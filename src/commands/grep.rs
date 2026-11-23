// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `grep` operation: searches the paths of the files of the live pieces,
//! by the walk an archive would make, and of every archived file, in the
//! listings of the store.

use crate::collection::Collection;
use crate::commands::live::{LiveError, LivePieces, Scope, Spelled, Spelling, spell_as_stored};
use crate::commands::survey::Refusal;
use crate::config::CollectionConfig;
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::tree::TreeRoot;
use crate::listing;
use crate::relpath::{Name, RelPath};
use crate::report::{self, Warning};
use crate::rules::{IGNORE_FILE, PieceRules, RulesError};
use crate::shown::Shown;
use crate::tree::{Class, EntryKind, PieceTree, Problem};
use crate::versions::VersionsError;
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StoreVersion};
use crate::walk::{Hooks, RealFs, WalkError, walk_piece_with};
use regex::bytes::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Why grep failed. A listing that cannot be searched is not an error: its
/// version is skipped and reported in [`GrepReport::skipped`]. Neither is a
/// problem in the walk of a live piece: it is a warning of the report.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum GrepError {
    #[error("the pattern is not a valid regular expression: {0}")]
    InvalidPattern(#[source] regex::Error),
    #[error("cannot read the store: {0}")]
    Store(
        #[source]
        #[diagnostic_source]
        VersionsError,
    ),
    #[error("cannot select the live pieces")]
    Live(
        #[from]
        #[diagnostic_source]
        LiveError,
    ),
    #[error("cannot use the ignore and timeless rules: {0}")]
    Rules(
        #[from]
        #[diagnostic_source]
        RulesError,
    ),
    /// The walk of a live piece ran out of file descriptors.
    #[error("cannot walk a live piece: {0}")]
    Walk(
        #[from]
        #[diagnostic_source]
        WalkError,
    ),
}

impl HasKind for GrepError {
    fn kind(&self) -> ErrorKind {
        match self {
            // The pattern is part of the command line.
            GrepError::InvalidPattern(_) => ErrorKind::Usage,
            GrepError::Store(_) | GrepError::Live(_) | GrepError::Rules(_) | GrepError::Walk(_) => {
                ErrorKind::Other
            }
        }
    }
}

/// What a search found. Pieces are sorted by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrepReport {
    pub pieces: Vec<PieceHits>,
    pub skipped: Vec<SkippedVersion>,
    /// The notes on the settings of the collection and on its lock file, the
    /// directories selection could not read and what keeps a live piece from
    /// being searched whole.
    pub warnings: Vec<Warning>,
    /// The run that holds the lock, while there is one.
    pub note: Option<String>,
}

/// The hits in one piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceHits {
    pub piece: RelPath,
    /// The pattern matches the piece path itself (see [`is_name_match`]).
    /// The piece then has no `files`: every file in it would match too.
    pub name_match: bool,
    /// The piece is live. Its files are those of an archive of it.
    pub live: bool,
    /// The versions that were searched, newest first.
    pub versions: Vec<SearchedVersion>,
    /// Every version of the piece that has an archive, newest first,
    /// searched or not. A version argument is resolved against all of them,
    /// so a label that names a version must be unambiguous among them.
    pub all_stamps: Vec<Stamp>,
    /// Sorted by path bytes.
    pub files: Vec<FileHit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchedVersion {
    pub stamp: Stamp,
    /// The `files` figure of the listing's head. Only read, and so only
    /// set, for a piece whose name matches.
    pub files: Option<u64>,
}

/// A path that matched, with the versions that hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    /// The path as the filesystem holds it.
    pub path: Vec<u8>,
    /// The live piece holds the path.
    pub live: bool,
    /// The stored versions that hold the path, newest first.
    pub versions: Vec<Stamp>,
}

/// A complete version that was not searched, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedVersion {
    pub piece: RelPath,
    pub stamp: Stamp,
    pub reason: String,
}

/// Compiles the pattern of `grep`. The pattern is case sensitive and
/// matches bytes, so `(?-u)` can match a path that is not UTF-8.
pub fn compile(pattern: &str) -> Result<Regex, GrepError> {
    Regex::new(pattern).map_err(GrepError::InvalidPattern)
}

/// Searches the pieces of `collection` that `scope` covers, and adds the
/// notes on its settings and its lock. Nothing is locked, so a running
/// `archive` is only noted.
///
/// The live pieces are selected before the store is read.
pub fn grep_collection(
    collection: &Collection,
    pattern: &Regex,
    scope: Scope,
) -> Result<GrepReport, GrepError> {
    let live = scope
        .includes_live()
        .then(|| LivePieces::select(collection))
        .transpose()?;
    let config = &collection.config().collection;
    let rules = live.as_ref().map(|_| live_rules(config)).transpose()?;
    let side = live
        .as_ref()
        .zip(rules.as_ref())
        .map(|(pieces, rules)| LiveSide {
            pieces,
            rules,
            hooks: &RealFs,
        });
    let store = scope.includes_store().then(|| collection.pieces_dir());

    let mut found = search(store.as_deref(), side.as_ref(), pattern, workers())?;

    let mut warnings = report::setting_notes(collection);
    let (note, unreadable_lock) = report::lock_status(collection);
    warnings.extend(unreadable_lock);
    warnings.extend(live.iter().flat_map(LivePieces::unreadable_warnings));
    warnings.append(&mut found.warnings);
    found.warnings = warnings;
    found.note = note;
    Ok(found)
}

/// Searches the listings of every complete version below `pieces_dir`.
///
/// An incomplete version, one without an archive, is not searched and not
/// reported: it is the leftover of an interrupted run.
pub fn grep(pieces_dir: &Path, pattern: &Regex) -> Result<GrepReport, GrepError> {
    search(Some(pieces_dir), None, pattern, workers())
}

/// All cores but two, so that the machine stays usable during a search.
fn workers() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .saturating_sub(2)
        .max(1)
}

/// Whether the pattern matches the piece path itself: its leftmost match in
/// `<piece>/` ends before the slash.
///
/// The slash is part of the text so that the answer is the one a path below
/// the piece would get: `scanner$` does not match there, since a slash
/// follows the piece, and `scanner.*` runs on into the path, so it is a
/// match of the files below the piece and not of the piece.
fn is_name_match(pattern: &Regex, piece: &RelPath) -> bool {
    let mut text = piece.as_bytes().to_vec();
    text.push(b'/');
    pattern
        .find(&text)
        .is_some_and(|found| found.end() < text.len())
}

/// The rules a live piece is walked by: those of the collection, with the
/// directories its ignore rules match left unentered. Everything below such a
/// directory is ignored with it, so no hit is lost, and a `target/` that the
/// rules ignore costs the walk one entry.
fn live_rules(config: &CollectionConfig) -> Result<PieceRules, RulesError> {
    Ok(PieceRules::new(&config.ignore, &config.timeless)?.skipping_ignored_directories())
}

/// The live pieces of a search, with what walking them takes.
struct LiveSide<'a> {
    pieces: &'a LivePieces,
    rules: &'a PieceRules,
    hooks: &'a dyn Hooks,
}

/// One version to search.
struct Job<'a> {
    piece: &'a RelPath,
    version: &'a StoreVersion,
    /// Only the head is read: the piece name matches.
    head_only: bool,
}

/// One unit of the search: a listing to read or a live piece to walk.
enum Work<'a> {
    Version(Job<'a>),
    Live {
        piece: &'a Spelled,
        /// The piece name matches, so the piece is not walked.
        name_match: bool,
    },
}

/// What one unit of work came to.
enum Done {
    Version(Outcome),
    Live(Result<LiveOutcome, WalkError>),
}

/// What searching one version came to.
enum Outcome {
    Searched {
        files: Option<u64>,
        /// The matching paths, each once.
        paths: Vec<Vec<u8>>,
    },
    Skipped(String),
}

/// What walking one live piece came to.
#[derive(Default)]
struct LiveOutcome {
    /// The matching paths, each once, in path order.
    paths: Vec<Vec<u8>>,
    warnings: Vec<Warning>,
}

/// What is known of one piece while the hits are put together.
#[derive(Default)]
struct Gathered {
    live: bool,
    searched: Vec<SearchedVersion>,
    all_stamps: Vec<Stamp>,
    files: BTreeMap<Vec<u8>, FileHit>,
}

fn search(
    store_dir: Option<&Path>,
    live: Option<&LiveSide>,
    pattern: &Regex,
    workers: usize,
) -> Result<GrepReport, GrepError> {
    // The spelling is asked for only when the store was scanned.
    let spelling = |paths: &[RelPath], folders: &BTreeSet<&RelPath>| {
        spell_as_stored(
            store_dir.expect("a scanned store has a directory"),
            paths,
            folders,
        )
    };
    search_with(store_dir, live, pattern, workers, &spelling)
}

/// [`search`] with the spelling of the live pieces given.
///
/// A live piece whose path the volume treats as the name of a store folder
/// is reported under that folder's spelling. A pattern that matches either
/// spelling finds it: the live files are matched under both, the versions of
/// the store under the stored one.
fn search_with(
    store_dir: Option<&Path>,
    live: Option<&LiveSide>,
    pattern: &Regex,
    workers: usize,
    spelling: Spelling<'_>,
) -> Result<GrepReport, GrepError> {
    let scan = store_dir
        .map(|dir| store::scan(dir).map_err(GrepError::Store))
        .transpose()?;

    let live_paths: Vec<RelPath> = live
        .iter()
        .flat_map(|side| side.pieces.pieces())
        .cloned()
        .collect();
    let (spelled, spelling_warnings) = match &scan {
        Some(scan) => {
            let folders: BTreeSet<&RelPath> = scan
                .pieces
                .iter()
                .chain(&scan.sidecar_only)
                .map(|folder| &folder.path)
                .collect();
            spelling(&live_paths, &folders)
        }
        None => (live_paths.iter().map(Spelled::alone).collect(), Vec::new()),
    };
    let other_spellings: BTreeMap<&RelPath, &RelPath> = spelled
        .iter()
        .filter_map(|piece| piece.other_spelling().map(|live| (&piece.stored, live)))
        .collect();
    let name_matches = |piece: &RelPath| {
        is_name_match(pattern, piece)
            || other_spellings
                .get(piece)
                .is_some_and(|live| is_name_match(pattern, live))
    };

    // Newest first, as every list of versions is reported.
    let mut complete: Vec<(&RelPath, Vec<&StoreVersion>)> = Vec::new();
    for piece in scan.iter().flat_map(|scan| &scan.pieces) {
        let mut versions: Vec<&StoreVersion> = piece
            .versions
            .iter()
            .filter(|version| !version.is_incomplete())
            .collect();
        versions.sort_by_key(|version| std::cmp::Reverse(version.stamp));
        complete.push((&piece.path, versions));
    }

    // The walks of the live pieces are the long units of work, so they are
    // queued first and the workers that finish early take the short ones.
    let mut work = Vec::new();
    for piece in &spelled {
        work.push(Work::Live {
            piece,
            name_match: name_matches(&piece.stored),
        });
    }
    for (piece, versions) in &complete {
        let head_only = name_matches(piece);
        for version in versions {
            work.push(Work::Version(Job {
                piece,
                version,
                head_only,
            }));
        }
    }

    // The outcomes come in the order of the work. Each kind is taken apart
    // from the other, so the n-th walk is the one of the n-th live piece and
    // the n-th outcome of a version the one of the n-th version.
    let mut outcomes = Vec::new();
    let mut walks = Vec::new();
    for done in run_work(store_dir, live, pattern, &work, workers)? {
        match done {
            Done::Version(outcome) => outcomes.push(outcome),
            Done::Live(walk) => walks.push(walk),
        }
    }
    let mut outcomes = outcomes.into_iter();

    let mut report = GrepReport {
        warnings: spelling_warnings,
        ..GrepReport::default()
    };
    let mut gathered: BTreeMap<&RelPath, Gathered> = BTreeMap::new();
    for (piece, versions) in &complete {
        let entry = gathered.entry(piece).or_default();
        entry.all_stamps = versions.iter().map(|version| version.stamp).collect();
        for version in versions {
            let outcome = outcomes.next().expect("every version has an outcome");
            match outcome {
                Outcome::Searched {
                    files: count,
                    paths,
                } => {
                    entry.searched.push(SearchedVersion {
                        stamp: version.stamp,
                        files: count,
                    });
                    for path in paths {
                        let hit = entry.files.entry(path.clone()).or_insert_with(|| FileHit {
                            path,
                            live: false,
                            versions: Vec::new(),
                        });
                        hit.versions.push(version.stamp);
                    }
                }
                Outcome::Skipped(reason) => report.skipped.push(SkippedVersion {
                    piece: (*piece).clone(),
                    stamp: version.stamp,
                    reason,
                }),
            }
        }
    }
    for (piece, walk) in spelled.iter().zip(walks) {
        let outcome = walk?;
        let entry = gathered.entry(&piece.stored).or_default();
        entry.live = true;
        for path in outcome.paths {
            let hit = entry.files.entry(path.clone()).or_insert_with(|| FileHit {
                path,
                live: false,
                versions: Vec::new(),
            });
            hit.live = true;
        }
        report.warnings.extend(outcome.warnings);
    }

    for (piece, entry) in gathered {
        let name_match = name_matches(piece);
        let found = if name_match {
            entry.live || !entry.searched.is_empty()
        } else {
            !entry.files.is_empty()
        };
        if found {
            report.pieces.push(PieceHits {
                piece: piece.clone(),
                name_match,
                live: entry.live,
                versions: entry.searched,
                all_stamps: entry.all_stamps,
                files: entry.files.into_values().collect(),
            });
        }
    }
    Ok(report)
}

/// Runs the work on `workers` threads (at most one per unit). The
/// outcomes come in the order of `work`.
fn run_work(
    store_dir: Option<&Path>,
    live: Option<&LiveSide>,
    pattern: &Regex,
    work: &[Work],
    workers: usize,
) -> Result<Vec<Done>, GrepError> {
    let store = match store_dir {
        Some(dir) if work.iter().any(|unit| matches!(unit, Work::Version(_))) => {
            Some(TreeRoot::open(dir).map_err(|source| {
                GrepError::Store(VersionsError::Io {
                    action: "open",
                    path: Shown(&dir).to_string(),
                    source,
                })
            })?)
        }
        _ => None,
    };
    Ok(parallel_map(work, workers, |unit| match unit {
        Work::Version(job) => Done::Version(search_version(
            store.as_ref().expect("a version is searched in a store"),
            pattern,
            job,
        )),
        Work::Live { piece, name_match } => Done::Live(search_live(
            live.expect("a live piece is searched with its side"),
            pattern,
            piece,
            *name_match,
        )),
    }))
}

/// Applies `work` to every job on `workers` threads (at most one per job)
/// and returns the results in the order of `jobs`.
fn parallel_map<J: Sync, R: Send>(
    jobs: &[J],
    workers: usize,
    work: impl Fn(&J) -> R + Sync,
) -> Vec<R> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new(jobs.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, jobs.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        break;
                    };
                    let result = work(job);
                    let mut results = results.lock().unwrap_or_else(PoisonError::into_inner);
                    results[index] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .map(|result| result.expect("every job was run"))
        .collect()
}

/// Walks a live piece the way an archive of it would, and matches the files
/// the archive would hold. A piece whose name matches is not walked: it has
/// no files in the report.
///
/// What the archive would not hold stays out, although the walk records it:
/// special files (the archive skips them), entries on the exclude list and
/// everything inside the store of a nested collection.
///
/// A path matches when `<piece>/<path>` does, for the spelling of the store
/// folder and for the spelling of the live folder when they differ.
fn search_live(
    live: &LiveSide,
    pattern: &Regex,
    piece: &Spelled,
    name_match: bool,
) -> Result<LiveOutcome, WalkError> {
    if name_match {
        return Ok(LiveOutcome::default());
    }
    let tree = walk_piece_with(
        live.pieces.root(),
        &piece.live,
        live.pieces.rules(),
        live.rules,
        live.hooks,
    )?;
    let mut excluded = Vec::new();
    let mut nested = Vec::new();
    for problem in tree.problems() {
        match problem {
            Problem::Excluded { path } => excluded.push(path),
            Problem::NestedCollection { root } => nested.push(root),
            _ => {}
        }
    }
    let is_kept_out = |path: &RelPath| {
        excluded.contains(&path) || nested.iter().any(|root| inside_inner_store(path, root))
    };

    // The prefixes of the text each path is matched in: a spelling of the
    // piece and a slash.
    let prefixes: Vec<Vec<u8>> = std::iter::once(&piece.stored)
        .chain(piece.other_spelling())
        .map(|spelling| {
            let mut prefix = spelling.as_bytes().to_vec();
            prefix.push(b'/');
            prefix
        })
        .collect();
    let mut haystack = Vec::new();
    let mut paths = Vec::new();
    for entry in tree.packed_entries() {
        if !matches!(entry.kind, EntryKind::File | EntryKind::Symlink) || is_kept_out(&entry.path) {
            continue;
        }
        let matched = prefixes.iter().any(|prefix| {
            haystack.clear();
            haystack.extend_from_slice(prefix);
            haystack.extend_from_slice(entry.path.as_bytes());
            pattern.is_match(&haystack)
        });
        if matched {
            paths.push(entry.path.as_bytes().to_vec());
        }
    }
    Ok(LiveOutcome {
        paths,
        warnings: walk_warnings(&tree),
    })
}

/// Whether `path` is the `.antiq` directory of the nested collection at
/// `holder`, or lies below it. The name is compared without regard to case,
/// as the walk does.
fn inside_inner_store(path: &RelPath, holder: &RelPath) -> bool {
    let mut parts = path.components();
    for part in holder.components() {
        if parts.next() != Some(part) {
            return false;
        }
    }
    parts
        .next()
        .is_some_and(|part| part.eq_ignore_ascii_case(b".antiq"))
}

/// One warning for each problem of the walk that changes what the search
/// sees. A lock, or a marker that is no file, only matters to removing the
/// piece. A top that is a mount point is walked completely. A directory that
/// the rules ignore is not entered, so a mount point or an entry of the
/// exclude list that is such a directory leaves nothing out of the search. A rule file that
/// cannot be used is not applied, so the paths it would ignore are searched:
/// that is a warning for an ignore file, and none for a timeless file, which
/// only decides the idle clock.
fn walk_warnings(tree: &PieceTree) -> Vec<Warning> {
    let top = tree.top();
    let is_ignored = |path: &RelPath| {
        tree.entries()
            .iter()
            .any(|entry| &entry.path == path && entry.class == Class::Ignored)
    };
    let cannot_search_all = |problem: &Problem| {
        let reason = Refusal::of(top, problem);
        Some(Warning::new(format!(
            "cannot search all of the live piece {}: {reason}",
            Shown(top)
        )))
    };
    tree.problems()
        .iter()
        .filter_map(|problem| match problem {
            Problem::Excluded { path } if is_ignored(path) => None,
            Problem::Unreadable { .. }
            | Problem::Excluded { .. }
            | Problem::NestedCollection { .. } => cannot_search_all(problem),
            Problem::MountPoint { other } if !other.is_root() && !is_ignored(other) => {
                cannot_search_all(problem)
            }
            Problem::MountPoint { .. }
            | Problem::Locked { .. }
            | Problem::LockedParent { .. }
            | Problem::MarkerNotAFile { .. } => None,
            Problem::RuleFile { path, line } => {
                let is_ignore_file = path
                    .components()
                    .last()
                    .is_some_and(|name| name == IGNORE_FILE);
                is_ignore_file.then(|| {
                    let line = line.map(|line| format!(":{line}")).unwrap_or_default();
                    Warning::new(format!(
                        "cannot use the rules of {}{line}, so the live piece {} is searched without them",
                        Shown(&top.concat(path)),
                        Shown(top)
                    ))
                })
            }
        })
        .collect()
}

fn search_version(store: &TreeRoot, pattern: &Regex, job: &Job) -> Outcome {
    match read_version(store, pattern, job) {
        Ok(outcome) => outcome,
        Err(reason) => Outcome::Skipped(reason),
    }
}

/// Searches the one listing of a version. The error is the reason the
/// version cannot be searched.
fn read_version(store: &TreeRoot, pattern: &Regex, job: &Job) -> Result<Outcome, String> {
    let name = match job.version.listings.as_slice() {
        [] => return Err("the version has no listing file".into()),
        [name] => name,
        _ => return Err("the version has two listing files".into()),
    };
    let name =
        Name::from_bytes(name.as_bytes().to_vec()).expect("a scanned file name is a valid name");
    let file = store
        .open_read(&job.piece.join(&name))
        .map_err(|error| format!("cannot open the listing: {error}"))?;

    if job.head_only {
        let head = listing::read_head(file).map_err(|error| error.to_string())?;
        if let Some(reason) = head.damage() {
            return Err(reason);
        }
        let files = head
            .files
            .ok_or("the `files` line of the head is missing or unparsable")?;
        return Ok(Outcome::Searched {
            files: Some(files),
            paths: Vec::new(),
        });
    }

    let decoded = listing::decode_all(file).map_err(|error| error.to_string())?;
    let mut paths = Vec::new();
    // A prefix of the text each path is matched in: the piece and a slash.
    let mut haystack = job.piece.as_bytes().to_vec();
    haystack.push(b'/');
    let prefix = haystack.len();
    for entry in listing::scan_paths(&decoded) {
        // A failure part of the way through discards the hits so far: the
        // version is skipped as a whole.
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.kind == listing::ListingType::Dir {
            continue;
        }
        haystack.truncate(prefix);
        haystack.extend_from_slice(&entry.path);
        if pattern.is_match(&haystack) {
            paths.push(entry.path);
        }
    }
    Ok(Outcome::Searched { files: None, paths })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::Tree;
    use crate::testing::versions::{Store, StoredVersion};
    use crate::versions::names::VersionFile;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // =========================================================
    // Fixtures
    // =========================================================

    const NEW: &str = "2026-11-04T100211";
    const OLD: &str = "2026-09-03T121500";

    fn stamp(text: &str) -> Stamp {
        text.parse().expect("a valid stamp")
    }

    fn piece(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    /// A source tree with the given files, named from its root.
    fn source(files: &[&str]) -> Tree {
        let mut tree = Tree::new();
        for file in files {
            tree.file(file, "x");
        }
        tree
    }

    fn found(store: &Store, pattern: &str) -> GrepReport {
        let pattern = compile(pattern).expect("the pattern compiles");
        search(Some(&store.pieces_dir()), None, &pattern, 2).expect("the search runs")
    }

    fn paths(hits: &PieceHits) -> Vec<String> {
        hits.files
            .iter()
            .map(|hit| String::from_utf8(hit.path.clone()).expect("UTF-8 paths"))
            .collect()
    }

    fn pieces(report: &GrepReport) -> Vec<String> {
        report
            .pieces
            .iter()
            .map(|hits| hits.piece.to_string())
            .collect()
    }

    /// A store with the piece `projects/scanner` in two versions and
    /// `tools/scan-old` in one.
    fn two_pieces() -> Store {
        let mut store = Store::new();
        let old = source(&[
            "projects/scanner/src/main.rs",
            "projects/scanner/tools/bench/main.rs",
        ]);
        store.add_version(&old, "projects/scanner", OLD);
        let new = source(&["projects/scanner/src/main.rs", "projects/scanner/README"]);
        store.add_version(&new, "projects/scanner", NEW);
        let other = source(&["tools/scan-old/cli/src/main.rs"]);
        store.add_version(&other, "tools/scan-old", "2025-02-11T194007");
        store
    }

    // =========================================================
    // What is found
    // =========================================================

    #[test]
    fn finds_a_path_in_two_versions_of_two_pieces() {
        let report = found(&two_pieces(), r"main\.rs$");

        assert_eq!(pieces(&report), ["projects/scanner", "tools/scan-old"]);
        let scanner = &report.pieces[0];
        assert!(!scanner.name_match);
        assert_eq!(
            scanner.versions.iter().map(|v| v.stamp).collect::<Vec<_>>(),
            [stamp(NEW), stamp(OLD)]
        );
        assert_eq!(paths(scanner), ["src/main.rs", "tools/bench/main.rs"]);
        assert_eq!(scanner.files[0].versions, [stamp(NEW), stamp(OLD)]);
        assert_eq!(scanner.files[1].versions, [stamp(OLD)]);
        assert_eq!(scanner.all_stamps, [stamp(NEW), stamp(OLD)]);
        assert_eq!(paths(&report.pieces[1]), ["cli/src/main.rs"]);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn a_pattern_is_matched_in_the_piece_and_the_path_together() {
        let store = two_pieces();

        let across = found(&store, "scanner/src");
        assert_eq!(pieces(&across), ["projects/scanner"]);
        assert_eq!(paths(&across.pieces[0]), ["src/main.rs"]);

        // The path alone starts with `src`, but the text starts with the piece.
        let anchored = found(&store, "^src");
        assert!(anchored.pieces.is_empty());
        let anchored_piece = found(&store, "^projects/scanner/README");
        assert_eq!(paths(&anchored_piece.pieces[0]), ["README"]);
    }

    #[test]
    fn a_piece_name_match_is_one_hit_without_files() {
        let store = two_pieces();
        for (pattern, name_matches) in [
            ("scanner", vec!["projects/scanner"]),
            ("scanner|main", vec!["projects/scanner"]),
            ("^projects", vec!["projects/scanner"]),
            ("scan", vec!["projects/scanner", "tools/scan-old"]),
        ] {
            let report = found(&store, pattern);
            let named: Vec<String> = report
                .pieces
                .iter()
                .filter(|hits| hits.name_match)
                .map(|hits| hits.piece.to_string())
                .collect();
            assert_eq!(named, name_matches, "pattern {pattern}");
            assert!(
                report
                    .pieces
                    .iter()
                    .filter(|hits| hits.name_match)
                    .all(|hits| hits.files.is_empty()),
                "pattern {pattern}"
            );
        }
    }

    #[test]
    fn a_match_that_runs_past_the_piece_path_lists_files_instead() {
        let store = two_pieces();
        for pattern in ["scanner.*", ".*", r"scanner$"] {
            let report = found(&store, pattern);
            let scanner = report
                .pieces
                .iter()
                .find(|hits| hits.piece == piece("projects/scanner"));
            match (pattern, scanner) {
                (r"scanner$", None) => {}
                (_, Some(hits)) => {
                    assert!(!hits.name_match, "pattern {pattern}");
                    assert_eq!(
                        paths(hits),
                        ["README", "src/main.rs", "tools/bench/main.rs"],
                        "pattern {pattern}"
                    );
                }
                (_, None) => panic!("pattern {pattern} found no files"),
            }
        }
    }

    #[test]
    fn a_name_match_reads_only_the_heads() {
        let mut store = Store::new();
        let tree = source(&["projects/scanner/a", "projects/scanner/b"]);
        let version = store.add_version(&tree, "projects/scanner", NEW);
        version.cut_listing();

        let report = found(&store, "scanner");

        assert!(report.skipped.is_empty());
        let hits = &report.pieces[0];
        assert!(hits.name_match);
        assert_eq!(
            hits.versions,
            [SearchedVersion {
                stamp: stamp(NEW),
                files: Some(2)
            }]
        );
        // The same cut listing cannot be searched for its files.
        let files = found(&store, "scanner/");
        assert_eq!(files.skipped.len(), 1);
    }

    #[test]
    fn matches_the_unescaped_bytes_of_a_name_with_a_tab_and_a_backslash() {
        let mut store = Store::new();
        let tree = source(&["notes/a\tb\\c", "notes/plain"]);
        store.add_version(&tree, "notes", NEW);

        let report = found(&store, r"a\tb\\c");

        assert_eq!(report.pieces[0].files[0].path, b"a\tb\\c");
        assert_eq!(report.pieces[0].files.len(), 1);
        // The escaped spelling of the listing is not what is searched.
        assert!(found(&store, r"a\\tb").pieces.is_empty());
    }

    #[test]
    fn a_directory_is_no_hit_but_a_file_of_that_name_is() {
        let mut store = Store::new();
        let tree = source(&["notes/sub/inside", "notes/x/sub"]);
        store.add_version(&tree, "notes", NEW);

        let report = found(&store, "sub$");

        assert_eq!(paths(&report.pieces[0]), ["x/sub"]);
    }

    #[test]
    fn links_are_hits() {
        let mut store = Store::new();
        let mut tree = source(&["notes/real"]);
        tree.symlink("notes/soft-link", "real");
        tree.hard_link("notes/real", "notes/hard-link");
        store.add_version(&tree, "notes", NEW);

        let report = found(&store, "link$");

        assert_eq!(paths(&report.pieces[0]), ["hard-link", "soft-link"]);
    }

    #[test]
    fn nothing_found_is_an_empty_report() {
        let report = found(&two_pieces(), "qxzv");
        assert_eq!(report, GrepReport::default());
    }

    #[test]
    fn a_missing_store_is_an_empty_store() {
        let pattern = compile("x").expect("compiles");
        let report = search(
            Some(Path::new("/nonexistent-antiq-pieces")),
            None,
            &pattern,
            1,
        )
        .expect("a missing store is empty");
        assert_eq!(report, GrepReport::default());
    }

    #[test]
    fn a_store_that_cannot_be_scanned_is_an_error() {
        let mut tree = Tree::new();
        let not_a_directory = tree.file("pieces", "a file where the store should be");
        let pattern = compile("x").expect("compiles");

        let error = search(Some(&not_a_directory), None, &pattern, 1).expect_err("the scan fails");

        assert!(matches!(error, GrepError::Store(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    // =========================================================
    // Versions that are not searched
    // =========================================================

    /// Two versions of `notes` that both hold `needle`, and the older one
    /// for `damage` to spoil.
    fn spoilable() -> (Store, StoredVersion) {
        let mut store = Store::new();
        let tree = source(&["notes/needle"]);
        let spoiled = store.add_version(&tree, "notes", OLD);
        store.add_version(&tree, "notes", NEW);
        (store, spoiled)
    }

    fn assert_skipped_only(report: &GrepReport, reason: &str) {
        assert_eq!(report.skipped.len(), 1, "{report:?}");
        let skipped = &report.skipped[0];
        assert_eq!(skipped.piece, piece("notes"));
        assert_eq!(skipped.stamp, stamp(OLD));
        assert!(
            skipped.reason.contains(reason),
            "`{}` does not contain `{reason}`",
            skipped.reason
        );
        let hits = &report.pieces[0];
        assert_eq!(hits.files[0].versions, [stamp(NEW)]);
        assert_eq!(hits.all_stamps, [stamp(NEW), stamp(OLD)]);
    }

    #[test]
    fn an_incomplete_version_is_left_out_silently() {
        let (store, spoiled) = spoilable();
        spoiled.remove(VersionFile::Archive);

        let report = found(&store, "needle");

        assert!(report.skipped.is_empty());
        let hits = &report.pieces[0];
        assert_eq!(hits.files[0].versions, [stamp(NEW)]);
        assert_eq!(hits.all_stamps, [stamp(NEW)]);
    }

    #[test]
    fn a_cut_listing_is_skipped_with_its_reason() {
        let (store, spoiled) = spoilable();
        spoiled.cut_listing();
        assert_skipped_only(&found(&store, "needle"), "incomplete frame");
    }

    #[test]
    fn a_listing_of_a_newer_format_is_skipped() {
        let (store, spoiled) = spoilable();
        spoiled.listing_from_text(b"# antiq listing 2\n");
        assert_skipped_only(&found(&store, "needle"), "format 2");
    }

    #[test]
    fn a_missing_listing_is_skipped() {
        let (store, spoiled) = spoilable();
        spoiled.remove(VersionFile::Listing);
        assert_skipped_only(&found(&store, "needle"), "no listing file");
    }

    #[test]
    fn two_listing_files_are_skipped() {
        let (store, spoiled) = spoilable();
        spoiled.add_twin(VersionFile::Listing, "other");
        assert_skipped_only(&found(&store, "needle"), "two listing files");
    }

    #[test]
    fn an_unreadable_listing_is_skipped() {
        let (store, spoiled) = spoilable();
        let path = spoiled.path(VersionFile::Listing);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("the mode is set");
        let _restore = RestoreMode(path.clone());
        assert!(
            fs::File::open(&path).is_err(),
            "the test needs a user who cannot read a mode 000 file"
        );

        let report = found(&store, "needle");

        assert_skipped_only(&report, "cannot open the listing");
    }

    /// Gives the file its mode back, also when the test panics, so the
    /// temp directory can be removed.
    struct RestoreMode(std::path::PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
        }
    }

    #[test]
    fn a_name_match_needs_a_head_with_its_figures() {
        for (line, reason) in [
            ("archive-bytes", "archive-bytes"),
            ("files", "`files` line"),
        ] {
            let (store, spoiled) = spoilable();
            spoiled.listing_without(line);

            let report = found(&store, "notes");

            assert_eq!(report.skipped.len(), 1, "without {line}");
            assert!(
                report.skipped[0].reason.contains(reason),
                "without {line}: {}",
                report.skipped[0].reason
            );
            assert_eq!(report.pieces[0].versions.len(), 1, "without {line}");
        }
    }

    #[test]
    fn a_bad_entry_line_discards_the_hits_before_it() {
        let (store, spoiled) = spoilable();
        // The matching entry is read first; the line after it is not an entry.
        spoiled.edit_listing(|text| format!("{text}# file  not an entry\n"));
        assert_skipped_only(&found(&store, "needle"), "line 11 of the listing");
    }

    #[test]
    fn a_doubled_archive_does_not_stop_the_search() {
        let (store, spoiled) = spoilable();
        spoiled.add_twin(VersionFile::Archive, "other");

        let report = found(&store, "needle");

        assert!(report.skipped.is_empty());
        assert_eq!(report.pieces[0].files[0].versions, [stamp(NEW), stamp(OLD)]);
    }

    #[test]
    fn a_missing_checksum_file_does_not_stop_the_search() {
        let (store, spoiled) = spoilable();
        spoiled.remove(VersionFile::Checksum);

        let report = found(&store, "needle");

        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(report.pieces[0].files[0].versions, [stamp(NEW), stamp(OLD)]);
    }

    #[test]
    fn an_archive_of_another_size_than_the_listing_records_does_not_stop_the_search() {
        let (store, spoiled) = spoilable();
        let size = std::fs::metadata(spoiled.path(VersionFile::Archive))
            .expect("the archive exists")
            .len();
        spoiled.truncate(VersionFile::Archive, size - 1);

        let report = found(&store, "needle");

        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(report.pieces[0].files[0].versions, [stamp(NEW), stamp(OLD)]);
    }

    // =========================================================
    // The pattern and the workers
    // =========================================================

    #[test]
    fn an_invalid_pattern_is_a_usage_error() {
        let error = compile("(unclosed").expect_err("the pattern is invalid");
        assert!(matches!(error, GrepError::InvalidPattern(_)));
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn the_report_does_not_depend_on_the_number_of_workers() {
        let mut store = Store::new();
        let tree = source(&["notes/needle", "notes/other"]);
        for day in 1..=12 {
            store.add_version(&tree, "notes", &format!("2026-10-{day:02}T100000"));
        }
        let pattern = compile("needle").expect("compiles");

        let one = search(Some(&store.pieces_dir()), None, &pattern, 1).expect("one worker");
        let four = search(Some(&store.pieces_dir()), None, &pattern, 4).expect("four workers");

        assert_eq!(one, four);
        assert_eq!(one.pieces[0].files[0].versions.len(), 12);
        assert_eq!(
            one.pieces[0].files[0].versions[0],
            stamp("2026-10-12T100000")
        );
    }

    // =========================================================
    // Live pieces
    // =========================================================

    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use crate::walk::Call;
    use rustix::io::Errno;
    use std::io;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Condvar, Mutex as StdMutex};
    use std::time::Duration;

    fn collection(temp: &TempCollection, config_lines: &str) -> Collection {
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{config_lines}\n"),
        )
        .expect("a config can be written");
        Collection::find(temp.root()).expect("a collection")
    }

    /// A collection whose live pieces hold the given files.
    fn live_world(files: &[&str]) -> TempCollection {
        let mut temp = TempCollection::minimal();
        for file in files {
            temp.tree().file(file, "x");
        }
        temp
    }

    fn searched_live(temp: &TempCollection, config: &str, pattern: &str) -> GrepReport {
        grep_collection(
            &collection(temp, config),
            &compile(pattern).expect("the pattern compiles"),
            Scope::Live,
        )
        .expect("the search runs")
    }

    fn warning_texts(report: &GrepReport) -> Vec<&str> {
        report
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect()
    }

    /// Replaces the mount roots and fails calls of a walk, at paths relative
    /// to the top of the piece. It cannot tell which piece it is asked for.
    #[derive(Default)]
    struct Staged {
        mount_roots: Vec<RelPath>,
        faults: Vec<(RelPath, Call, Errno)>,
    }

    impl Hooks for Staged {
        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            self.faults
                .iter()
                .find(|(at, kind, _)| at == path && *kind == call)
                .map(|(_, _, errno)| io::Error::from_raw_os_error(errno.raw_os_error()))
        }

        fn mount_root(&self, path: &RelPath, reported: bool) -> bool {
            reported || self.mount_roots.contains(path)
        }
    }

    /// Searches the live pieces with `hooks` on `workers` threads.
    fn live_with(
        temp: &TempCollection,
        config: &str,
        hooks: &dyn Hooks,
        pattern: &str,
        workers: usize,
    ) -> Result<GrepReport, GrepError> {
        let collection = collection(temp, config);
        let live = LivePieces::select(&collection).expect("the pieces are selected");
        let rules = live_rules(&collection.config().collection).expect("valid rules");
        let side = LiveSide {
            pieces: &live,
            rules: &rules,
            hooks,
        };
        let pattern = compile(pattern).expect("the pattern compiles");
        search(None, Some(&side), &pattern, workers)
    }

    #[test]
    fn a_live_file_is_found() {
        let temp = live_world(&["notes/a.txt", "notes/b.md"]);

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(pieces(&report), ["notes"]);
        let hits = &report.pieces[0];
        assert!(hits.live && !hits.name_match);
        assert!(hits.versions.is_empty() && hits.all_stamps.is_empty());
        assert_eq!(paths(hits), ["a.txt"]);
        assert!(hits.files[0].live && hits.files[0].versions.is_empty());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn the_search_of_a_store_directory_finds_the_paths_of_its_versions() {
        let store = two_pieces();
        let pattern = compile(r"main\.rs$").expect("the pattern compiles");

        let report = grep(&store.pieces_dir(), &pattern).expect("the search runs");

        assert_eq!(pieces(&report), ["projects/scanner", "tools/scan-old"]);
        assert!(report.pieces.iter().all(|hits| !hits.live));
    }

    #[test]
    fn a_pattern_is_matched_in_the_live_piece_and_the_path_together() {
        let temp = live_world(&["projects/scanner/src/main.rs"]);

        let across = searched_live(&temp, "patterns = [\"*/*\"]", "scanner/src");
        assert_eq!(paths(&across.pieces[0]), ["src/main.rs"]);

        let anchored = searched_live(&temp, "patterns = [\"*/*\"]", "^src");
        assert!(anchored.pieces.is_empty());
    }

    #[test]
    fn a_path_the_config_ignores_is_not_searched() {
        let temp = live_world(&["notes/debug.log", "notes/kept.txt"]);

        let report = searched_live(&temp, "ignore = [\"*.log\"]", "notes/");

        assert_eq!(paths(&report.pieces[0]), ["kept.txt"]);
    }

    #[test]
    fn a_path_an_antiqignore_file_ignores_is_not_searched() {
        let mut temp = live_world(&["notes/secret.txt", "notes/kept.txt"]);
        temp.tree().file("notes/.antiqignore", "secret.txt\n");

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["kept.txt"]);
    }

    #[test]
    fn the_restored_marker_is_not_searched_but_the_rule_files_are() {
        let mut temp = live_world(&["notes/a.txt"]);
        temp.tree().file("notes/.antiq-restored", "");
        temp.tree().file("notes/.antiqignore", "nothing\n");

        let report = searched_live(&temp, "", r"\.antiq");

        assert_eq!(paths(&report.pieces[0]), [".antiqignore"]);
    }

    #[test]
    fn a_directory_is_no_hit_but_a_symlink_is() {
        let mut temp = live_world(&["notes/sub/inside", "notes/x/sub"]);
        temp.tree().symlink("notes/soft-sub", "x/sub");

        let report = searched_live(&temp, "", "sub$");

        assert_eq!(paths(&report.pieces[0]), ["soft-sub", "x/sub"]);
    }

    #[test]
    fn a_fifo_and_a_socket_are_not_listed_as_the_archive_does_not_hold_them() {
        let mut temp = live_world(&["notes/a.txt"]);
        temp.tree().fifo("notes/pipe");
        temp.tree().socket("notes/sock");

        let report = searched_live(&temp, "", "pipe|sock");

        assert!(report.pieces.is_empty(), "{report:?}");
    }

    #[test]
    fn an_entry_of_the_exclude_list_is_not_listed_and_the_piece_is_warned_about() {
        let temp = live_world(&["notes/secret.txt", "notes/kept.txt"]);

        let report = searched_live(&temp, "exclude = [\"notes/secret.txt\"]", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["kept.txt"]);
        assert_eq!(
            warning_texts(&report),
            ["cannot search all of the live piece notes: exclude entry: notes/secret.txt"]
        );
    }

    #[test]
    fn the_store_of_a_nested_collection_is_not_listed_and_the_piece_is_warned_about() {
        let temp = live_world(&["notes/sub/.antiq/config.toml", "notes/sub/file"]);

        let report = searched_live(&temp, "", "config|file");

        assert_eq!(paths(&report.pieces[0]), ["sub/file"]);
        assert_eq!(
            warning_texts(&report),
            ["cannot search all of the live piece notes: nested antiq collection: notes/sub"]
        );
    }

    #[test]
    fn a_name_that_differs_only_in_case_is_the_nested_store_too() {
        let temp = live_world(&["notes/sub/.ANTIQ/config.toml", "notes/sub/file"]);

        let report = searched_live(&temp, "", "config|file");

        assert_eq!(paths(&report.pieces[0]), ["sub/file"]);
    }

    #[test]
    fn a_piece_that_is_the_nested_collection_itself_is_warned_about() {
        let temp = live_world(&["notes/.antiq/config.toml", "notes/file"]);

        let report = searched_live(&temp, "", "config|file");

        assert_eq!(paths(&report.pieces[0]), ["file"]);
        assert_eq!(
            warning_texts(&report),
            ["cannot search all of the live piece notes: nested antiq collection: notes"]
        );
    }

    #[test]
    fn an_unreadable_directory_is_warned_about_and_the_rest_is_searched() {
        let mut temp = live_world(&["notes/a.txt"]);
        temp.tree().unreadable_dir("notes/locked");
        assert_not_root(&temp.root().join("notes/locked"));

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["a.txt"]);
        assert_eq!(
            warning_texts(&report),
            ["cannot search all of the live piece notes: unreadable entry: notes/locked"]
        );
    }

    fn assert_not_root(directory: &Path) {
        assert!(
            fs::read_dir(directory).is_err(),
            "the test needs a user that mode 000 locks out"
        );
    }

    #[test]
    fn a_rule_file_that_cannot_be_used_is_warned_about() {
        let mut temp = live_world(&["notes/a.txt"]);
        temp.tree().file("notes/.antiqignore", "broken\\\n");

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["a.txt"]);
        assert_eq!(
            warning_texts(&report),
            [
                "cannot use the rules of notes/.antiqignore:1, so the live piece notes is searched without them"
            ]
        );
    }

    #[test]
    fn the_paths_an_unusable_ignore_file_would_have_ignored_are_searched() {
        let mut temp = live_world(&["notes/a.txt", "notes/secret.txt"]);
        temp.tree()
            .file("notes/.antiqignore", "secret.txt\nbroken\\\n");

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["a.txt", "secret.txt"]);
    }

    #[test]
    fn an_ignore_file_that_cannot_be_read_is_a_warning_without_a_line() {
        let temp = live_world(&["notes/a.txt", "notes/.antiqignore"]);
        let hooks = Staged {
            faults: vec![(piece(".antiqignore"), Call::Read, Errno::ACCESS)],
            ..Staged::default()
        };

        let report = live_with(&temp, "", &hooks, r"\.txt$", 1).expect("the search runs");

        assert_eq!(
            warning_texts(&report),
            [
                "cannot use the rules of notes/.antiqignore, so the live piece notes is searched without them"
            ]
        );
    }

    #[test]
    fn a_timeless_file_that_cannot_be_used_is_no_warning() {
        let mut temp = live_world(&["notes/a.txt"]);
        temp.tree().file("notes/.antiq-timeless", "broken\\\n");

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["a.txt"]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_marker_that_is_no_file_is_not_warned_about() {
        let mut temp = live_world(&["notes/a.txt"]);
        temp.tree().dir("notes/.antiq-pin");

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["a.txt"]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_fifo_named_like_a_rule_file_does_not_stop_the_search() {
        let mut temp = live_world(&["notes/a.txt"]);
        temp.tree().fifo("notes/.antiqignore");

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(paths(&report.pieces[0]), ["a.txt"]);
    }

    #[test]
    fn a_pinned_piece_is_searched_without_a_word_about_it() {
        let mut temp = live_world(&["notes/a.txt", "pinned/b.txt"]);
        temp.tree().file("pinned/.antiq-pin", "");

        let report = searched_live(&temp, "", r"\.txt$");

        assert_eq!(pieces(&report), ["notes", "pinned"]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_mount_point_below_the_top_is_not_entered_and_is_warned_about() {
        let temp = live_world(&["notes/a.txt", "notes/sub/inner.txt"]);
        let hooks = Staged {
            mount_roots: vec![piece("sub")],
            ..Staged::default()
        };

        let report = live_with(&temp, "", &hooks, r"\.txt$", 1).expect("the search runs");

        assert_eq!(paths(&report.pieces[0]), ["a.txt"]);
        assert_eq!(
            warning_texts(&report),
            ["cannot search all of the live piece notes: mount point: notes/sub"]
        );
    }

    #[test]
    fn a_top_that_is_a_mount_point_is_searched_completely_and_without_a_warning() {
        let temp = live_world(&["notes/a.txt", "notes/sub/inner.txt"]);
        let hooks = Staged {
            mount_roots: vec![RelPath::root()],
            ..Staged::default()
        };

        let report = live_with(&temp, "", &hooks, r"\.txt$", 1).expect("the search runs");

        assert_eq!(paths(&report.pieces[0]), ["a.txt", "sub/inner.txt"]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_piece_that_vanishes_during_the_walk_is_warned_about() {
        let temp = live_world(&["notes/a.txt"]);
        let hooks = Staged {
            faults: vec![(RelPath::root(), Call::Stat, Errno::NOENT)],
            ..Staged::default()
        };

        let report = live_with(&temp, "", &hooks, r"\.txt$", 1).expect("the search runs");

        assert!(report.pieces.is_empty());
        assert_eq!(
            warning_texts(&report),
            ["cannot search all of the live piece notes: unreadable entry: notes"]
        );
    }

    #[test]
    fn running_out_of_file_descriptors_fails_the_search() {
        let temp = live_world(&["notes/a.txt"]);
        let hooks = Staged {
            faults: vec![(RelPath::root(), Call::Open, Errno::MFILE)],
            ..Staged::default()
        };

        let error = live_with(&temp, "", &hooks, r"\.txt$", 1).expect_err("the walk fails");

        assert!(matches!(error, GrepError::Walk(_)), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_live_piece_whose_name_matches_is_not_walked() {
        let temp = live_world(&["notes/a.txt"]);
        let hooks = Staged {
            faults: vec![(RelPath::root(), Call::Stat, Errno::NOENT)],
            ..Staged::default()
        };

        let report = live_with(&temp, "", &hooks, "notes", 1).expect("the search runs");

        assert_eq!(pieces(&report), ["notes"]);
        let hits = &report.pieces[0];
        assert!(hits.name_match && hits.live && hits.files.is_empty());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    /// Copies the files below `from` to `to`.
    fn copy_below(from: &Path, to: &Path) {
        for entry in fs::read_dir(from).expect("the directory lists") {
            let entry = entry.expect("an entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("a type").is_dir() {
                fs::create_dir_all(&target).expect("the directory is made");
                copy_below(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).expect("the file is copied");
            }
        }
    }

    // A guard: the scope decides which side is read at all.
    #[test]
    fn a_live_search_reads_no_listing_and_a_store_search_walks_nothing() {
        let temp = live_world(&["notes/a.txt"]);
        let mut store = Store::new();
        let version = store.add_version(&source(&["notes/a.txt"]), "notes", NEW);
        version.cut_listing();
        copy_below(&store.pieces_dir(), &temp.root().join(".antiq/pieces"));
        let collection = collection(&temp, "");
        let pattern = compile(r"\.txt$").expect("compiles");
        let in_scope = |scope| grep_collection(&collection, &pattern, scope).expect("it runs");

        let live_only = in_scope(Scope::Live);
        assert!(live_only.skipped.is_empty());
        assert_eq!(pieces(&live_only), ["notes"]);
        assert!(live_only.pieces.iter().all(|hits| hits.versions.is_empty()));

        let archived = in_scope(Scope::Archived);
        assert_eq!(archived.skipped.len(), 1);
        assert!(archived.pieces.is_empty(), "nothing is walked");

        let both = in_scope(Scope::Both);
        assert_eq!(both.skipped.len(), 1);
        assert_eq!(pieces(&both), ["notes"]);
    }

    /// What a report holds of each piece: its name, whether it is live, the
    /// searched versions and the files with their live flag and versions.
    #[allow(clippy::type_complexity)]
    fn contents(
        report: &GrepReport,
    ) -> Vec<(String, bool, Vec<Stamp>, Vec<(String, bool, Vec<Stamp>)>)> {
        report
            .pieces
            .iter()
            .map(|hits| {
                let files = hits
                    .files
                    .iter()
                    .map(|file| {
                        let path = String::from_utf8(file.path.clone()).expect("UTF-8 paths");
                        (path, file.live, file.versions.clone())
                    })
                    .collect();
                let versions = hits.versions.iter().map(|version| version.stamp).collect();
                (hits.piece.to_string(), hits.live, versions, files)
            })
            .collect()
    }

    // A guard: the live walks and the listings queue together, and each kind
    // of outcome goes to the pieces of its own kind.
    #[test]
    fn a_mixed_search_puts_each_unit_in_its_own_piece() {
        let mut store = Store::new();
        store.add_version(&source(&["b/only.txt", "b/both.txt"]), "b", OLD);
        store.add_version(&source(&["b/both.txt"]), "b", NEW);
        store.add_version(&source(&["c/c.txt"]), "c", NEW);
        let temp = live_world(&["a/x.txt", "c/c.txt", "c/live.txt"]);
        let collection = collection(&temp, "");
        let live = LivePieces::select(&collection).expect("the pieces are selected");
        let rules = PieceRules::new(&[], &[]).expect("valid rules");
        let side = LiveSide {
            pieces: &live,
            rules: &rules,
            hooks: &RealFs,
        };
        let pattern = compile(r"\.txt$").expect("compiles");
        let (new, old) = (stamp(NEW), stamp(OLD));
        let expected = vec![
            (
                "a".to_string(),
                true,
                vec![],
                vec![("x.txt".to_string(), true, vec![])],
            ),
            (
                "b".to_string(),
                false,
                vec![new, old],
                vec![
                    ("both.txt".to_string(), false, vec![new, old]),
                    ("only.txt".to_string(), false, vec![old]),
                ],
            ),
            (
                "c".to_string(),
                true,
                vec![new],
                vec![
                    ("c.txt".to_string(), true, vec![new]),
                    ("live.txt".to_string(), true, vec![]),
                ],
            ),
        ];

        for workers in [1, 4] {
            let report = search(Some(&store.pieces_dir()), Some(&side), &pattern, workers)
                .expect("the search runs");
            assert_eq!(contents(&report), expected);
        }
    }

    #[test]
    fn a_piece_that_is_live_and_stored_is_one_hit_with_both() {
        let mut store = Store::new();
        store.add_version(&source(&["notes/a.txt"]), "notes", OLD);
        store.add_version(&source(&["notes/a.txt", "notes/gone.txt"]), "notes", NEW);
        store.add_version(&source(&["tools/old/run.txt"]), "tools/old", NEW);
        let temp = live_world(&["notes/a.txt", "notes/b.txt", "zeta/z.txt"]);
        let collection = collection(&temp, "");
        let live = LivePieces::select(&collection).expect("the pieces are selected");
        let rules = PieceRules::new(&[], &[]).expect("valid rules");
        let side = LiveSide {
            pieces: &live,
            rules: &rules,
            hooks: &RealFs,
        };
        let pattern = compile(r"\.txt$").expect("compiles");

        let report =
            search(Some(&store.pieces_dir()), Some(&side), &pattern, 2).expect("the search runs");

        assert_eq!(pieces(&report), ["notes", "tools/old", "zeta"]);
        let notes = &report.pieces[0];
        assert!(notes.live);
        assert_eq!(
            notes.versions.iter().map(|v| v.stamp).collect::<Vec<_>>(),
            [stamp(NEW), stamp(OLD)]
        );
        assert_eq!(paths(notes), ["a.txt", "b.txt", "gone.txt"]);
        assert_eq!(
            notes
                .files
                .iter()
                .map(|file| (file.live, file.versions.len()))
                .collect::<Vec<_>>(),
            [(true, 2), (true, 0), (false, 1)]
        );
        assert!(!report.pieces[1].live);
        assert!(report.pieces[2].live && report.pieces[2].versions.is_empty());
    }

    #[test]
    fn a_name_match_of_a_live_and_stored_piece_keeps_the_file_count_of_its_newest_version() {
        let mut store = Store::new();
        store.add_version(&source(&["notes/a", "notes/b"]), "notes", NEW);
        let temp = live_world(&["notes/a"]);
        let collection = collection(&temp, "");
        let live = LivePieces::select(&collection).expect("the pieces are selected");
        let rules = PieceRules::new(&[], &[]).expect("valid rules");
        let side = LiveSide {
            pieces: &live,
            rules: &rules,
            hooks: &RealFs,
        };
        let pattern = compile("notes").expect("compiles");

        let report =
            search(Some(&store.pieces_dir()), Some(&side), &pattern, 1).expect("the search runs");

        let hits = &report.pieces[0];
        assert!(hits.name_match && hits.live);
        assert_eq!(hits.versions[0].files, Some(2));
    }

    #[test]
    fn a_directory_selection_cannot_read_is_a_warning_before_the_walk_problems() {
        let mut temp = live_world(&["notes/sub/a.txt"]);
        temp.tree().unreadable_dir("projects");
        assert_not_root(&temp.root().join("projects"));

        let report = grep_collection(
            &collection(&temp, "patterns = [\"*/*\"]"),
            &compile(r"\.txt$").expect("compiles"),
            Scope::Both,
        )
        .expect("the search runs");

        assert_eq!(warning_texts(&report), ["unreadable: projects"]);
    }

    #[test]
    fn patterns_that_cannot_be_used_fail_a_search_that_selects() {
        let temp = live_world(&["notes/a.txt"]);
        let collection = collection(&temp, "patterns = [\"[\"]");
        let pattern = compile("a").expect("compiles");

        let error = grep_collection(&collection, &pattern, Scope::Both)
            .expect_err("the pattern of the config is invalid");
        assert!(matches!(error, GrepError::Live(_)), "{error:?}");

        grep_collection(&collection, &pattern, Scope::Archived)
            .expect("a search of the store selects nothing");
    }

    #[test]
    fn an_ignore_line_that_cannot_be_used_fails_a_search_of_live_pieces() {
        let temp = live_world(&["notes/a.txt"]);
        let collection = collection(&temp, "ignore = [\"broken\\\\\"]");
        let pattern = compile("a").expect("compiles");

        let error = grep_collection(&collection, &pattern, Scope::Live)
            .expect_err("the ignore line is invalid");
        assert!(matches!(error, GrepError::Rules(_)), "{error:?}");
    }

    #[test]
    fn the_live_pieces_come_in_the_order_of_their_paths_whatever_the_number_of_workers() {
        let names: Vec<String> = (0..12).map(|number| format!("piece-{number:02}")).collect();
        let files: Vec<String> = names.iter().map(|name| format!("{name}/needle")).collect();
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let temp = live_world(&refs);

        let one = live_with(&temp, "", &Staged::default(), "needle", 1).expect("one worker");
        let four = live_with(&temp, "", &Staged::default(), "needle", 4).expect("four workers");

        assert_eq!(one, four);
        assert_eq!(pieces(&one), names);
    }

    // =========================================================
    // The workers
    // =========================================================

    /// Lets a unit of work wait until others have reached the same point.
    /// A wait that is never answered ends after ten seconds with `false`.
    struct Meeting {
        arrived: StdMutex<usize>,
        changed: Condvar,
    }

    impl Meeting {
        fn new() -> Meeting {
            Meeting {
                arrived: StdMutex::new(0),
                changed: Condvar::new(),
            }
        }

        fn arrive(&self) {
            *self.arrived.lock().expect("the meeting is not poisoned") += 1;
            self.changed.notify_all();
        }

        fn wait_for(&self, count: usize) -> bool {
            let arrived = self.arrived.lock().expect("the meeting is not poisoned");
            let (_guard, outcome) = self
                .changed
                .wait_timeout_while(arrived, Duration::from_secs(10), |arrived| *arrived < count)
                .expect("the meeting is not poisoned");
            !outcome.timed_out()
        }
    }

    #[test]
    fn jobs_run_at_the_same_time_on_several_workers() {
        let meeting = Meeting::new();

        let met = parallel_map(&[0, 1], 2, |_| {
            meeting.arrive();
            meeting.wait_for(2)
        });

        assert_eq!(met, [true, true], "the two jobs never ran together");
    }

    #[test]
    fn results_come_in_the_order_of_the_jobs_not_of_their_end() {
        let meeting = Meeting::new();

        let results = parallel_map(&[0, 1, 2, 3], 4, |job| {
            if *job == 0 {
                // The first job ends after the other three.
                assert!(meeting.wait_for(3), "the other jobs never ran");
            } else {
                meeting.arrive();
            }
            *job
        });

        assert_eq!(results, [0, 1, 2, 3]);
    }

    #[test]
    fn no_job_gives_no_result_and_starts_no_worker() {
        let none: Vec<u8> = parallel_map(&[] as &[u8], 4, |job| *job);
        assert!(none.is_empty());
    }

    /// Holds the walk of every piece at the opening of its top until a
    /// second walk has reached that point too, and remembers whether every
    /// walk found one.
    struct Gate {
        meeting: Meeting,
        always_met: AtomicBool,
    }

    impl Hooks for Gate {
        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            if call == Call::Open && path.is_root() {
                self.meeting.arrive();
                let met = self.meeting.wait_for(2);
                self.always_met.fetch_and(met, Ordering::SeqCst);
            }
            None
        }
    }

    #[test]
    fn two_live_pieces_are_walked_at_the_same_time() {
        let temp = live_world(&["one/needle", "two/needle"]);

        let gate = Gate {
            meeting: Meeting::new(),
            always_met: AtomicBool::new(true),
        };

        let report = live_with(&temp, "", &gate, "needle", 2).expect("the search runs");

        assert!(
            gate.always_met.load(Ordering::SeqCst),
            "a walk waited for the other and it never came"
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(pieces(&report), ["one", "two"]);
    }

    // =========================================================
    // Spelling
    // =========================================================

    use crate::commands::live::stored_spellings;
    use crate::testing::tree::folds_case;

    /// A volume that folds case: the store folder is the lower case of the
    /// live path.
    fn folding(live: &[RelPath], folders: &BTreeSet<&RelPath>) -> (Vec<Spelled>, Vec<Warning>) {
        stored_spellings(live, folders, &|path| {
            Ok(Some(piece(&path.to_string().to_lowercase())))
        })
    }

    /// A search of the live pieces of `temp` and the store of `store`, with
    /// the spelling given.
    fn spelled_search(
        temp: &TempCollection,
        store: &Store,
        pattern: &str,
        spelling: Spelling<'_>,
    ) -> GrepReport {
        let collection = collection(temp, "");
        let live = LivePieces::select(&collection).expect("the pieces are selected");
        let rules = PieceRules::new(&[], &[]).expect("valid rules");
        let side = LiveSide {
            pieces: &live,
            rules: &rules,
            hooks: &RealFs,
        };
        let pattern = compile(pattern).expect("the pattern compiles");
        search_with(
            Some(&store.pieces_dir()),
            Some(&side),
            &pattern,
            1,
            spelling,
        )
        .expect("the search runs")
    }

    /// `notes` in the store with two files, `Notes` live with one of them.
    fn notes_in_two_spellings() -> (TempCollection, Store) {
        let mut store = Store::new();
        store.add_version(&source(&["notes/a.txt", "notes/gone.txt"]), "notes", NEW);
        (live_world(&["Notes/a.txt"]), store)
    }

    #[test]
    fn a_live_piece_is_reported_under_the_spelling_of_its_store_folder() {
        let (temp, store) = notes_in_two_spellings();

        let report = spelled_search(&temp, &store, r"\.txt$", &folding);

        assert_eq!(pieces(&report), ["notes"]);
        let hits = &report.pieces[0];
        assert!(hits.live);
        assert_eq!(hits.versions.len(), 1);
        assert_eq!(
            hits.files
                .iter()
                .map(|file| (file.live, file.versions.len()))
                .collect::<Vec<_>>(),
            [(true, 1), (false, 1)]
        );
    }

    #[test]
    fn a_pattern_finds_a_live_file_by_either_spelling_of_its_piece() {
        let (temp, store) = notes_in_two_spellings();

        let live_spelling = spelled_search(&temp, &store, "^Notes/a", &folding);
        assert_eq!(pieces(&live_spelling), ["notes"]);
        let hit = &live_spelling.pieces[0].files[0];
        assert!(hit.live && hit.versions.is_empty(), "{hit:?}");

        let stored_spelling = spelled_search(&temp, &store, "^notes/a", &folding);
        let hit = &stored_spelling.pieces[0].files[0];
        assert!(hit.live && hit.versions.len() == 1, "{hit:?}");
    }

    #[test]
    fn a_pattern_that_matches_the_name_of_either_spelling_is_a_name_match() {
        let (temp, store) = notes_in_two_spellings();

        for pattern in ["^Notes", "^notes"] {
            let report = spelled_search(&temp, &store, pattern, &folding);
            let hits = &report.pieces[0];
            assert!(hits.name_match && hits.live, "{pattern}");
            assert!(hits.files.is_empty(), "{pattern}");
            assert_eq!(hits.versions[0].files, Some(2), "{pattern}");
        }
    }

    #[test]
    fn a_live_piece_takes_the_spelling_of_a_folder_that_holds_sidecars_only() {
        let temp = live_world(&["Notes/a.txt"]);
        let mut lonely = Store::new();
        let version = lonely.add_version(&source(&["notes/b.txt"]), "notes", NEW);
        version.remove(VersionFile::Archive);

        let report = spelled_search(&temp, &lonely, r"\.txt$", &folding);

        assert_eq!(pieces(&report), ["notes"]);
        let hits = &report.pieces[0];
        assert!(hits.live && hits.versions.is_empty());
        assert_eq!(paths(hits), ["a.txt"]);
    }

    #[test]
    fn a_spelling_that_could_not_be_compared_is_the_first_warning() {
        let (temp, store) = notes_in_two_spellings();
        let failing = |live: &[RelPath], _: &BTreeSet<&RelPath>| {
            (
                live.iter().map(Spelled::alone).collect(),
                vec![Warning::new("cannot compare the spelling of Notes: boom")],
            )
        };

        let report = spelled_search(&temp, &store, r"\.txt$", &failing);

        assert_eq!(pieces(&report), ["Notes", "notes"]);
        assert_eq!(
            warning_texts(&report),
            ["cannot compare the spelling of Notes: boom"]
        );
    }

    #[test]
    fn the_live_scope_keeps_the_live_spelling_and_reads_no_store() {
        let (temp, _) = notes_in_two_spellings();
        let hooks = Staged::default();

        let report = live_with(&temp, "", &hooks, r"\.txt$", 1).expect("the search runs");

        assert_eq!(pieces(&report), ["Notes"]);
    }

    #[test]
    fn on_a_folding_volume_the_search_reports_the_store_spelling() {
        let (temp, store) = notes_in_two_spellings();
        let folds = folds_case(&store.pieces_dir());
        let pattern = compile(r"\.txt$").expect("compiles");
        let collection = collection(&temp, "");
        let live = LivePieces::select(&collection).expect("the pieces are selected");
        let rules = PieceRules::new(&[], &[]).expect("valid rules");
        let side = LiveSide {
            pieces: &live,
            rules: &rules,
            hooks: &RealFs,
        };

        let report =
            search(Some(&store.pieces_dir()), Some(&side), &pattern, 1).expect("the search runs");

        let expected: [&[&str]; 2] = [&["Notes", "notes"], &["notes"]];
        assert_eq!(pieces(&report), expected[usize::from(folds)]);
    }

    // =========================================================
    // Ignored directories
    // =========================================================

    /// Notes every call the walk makes for a path of the piece.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<(RelPath, Call)>>);

    impl Hooks for Recorded {
        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            self.0
                .lock()
                .expect("the record is not poisoned")
                .push((path.clone(), call));
            None
        }
    }

    impl Recorded {
        /// The calls that enter `directory` or reach something inside it:
        /// its own `Open` and `List` and every call below it, by components.
        fn calls_into(&self, directory: &str) -> Vec<String> {
            let directory = piece(directory);
            let inside = |at: &RelPath| {
                let mut parts = at.components();
                directory
                    .components()
                    .all(|part| parts.next() == Some(part))
            };
            self.0
                .lock()
                .expect("the record is not poisoned")
                .iter()
                .filter(|(at, call)| {
                    inside(at) && (at != &directory || matches!(call, Call::Open | Call::List))
                })
                .map(|(at, call)| format!("{at} {call:?}"))
                .collect()
        }
    }

    #[test]
    fn a_directory_the_rules_ignore_is_not_entered_and_the_hits_are_the_same() {
        let mut temp = live_world(&[
            "notes/keep.txt",
            "notes/target/debug/a.txt",
            "notes/build/deep/b.txt",
            "notes/build-tools/c.txt",
        ]);
        temp.tree().file("notes/.antiqignore", "build/\n");
        let recorded = Recorded::default();

        let report = live_with(&temp, "ignore = [\"target/\"]", &recorded, r"\.txt$", 1)
            .expect("the search runs");

        assert_eq!(paths(&report.pieces[0]), ["build-tools/c.txt", "keep.txt"]);
        assert_eq!(recorded.calls_into("target"), Vec::<String>::new());
        assert_eq!(recorded.calls_into("build"), Vec::<String>::new());
        assert!(!recorded.calls_into("build-tools").is_empty());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn an_ignored_directory_that_is_a_mount_or_on_the_exclude_list_is_no_warning() {
        let temp = live_world(&["notes/keep.txt", "notes/cache/x.txt", "notes/skipped/y.txt"]);
        let hooks = Staged {
            mount_roots: vec![piece("cache")],
            ..Staged::default()
        };
        let config = "ignore = [\"cache/\", \"skipped/\"]\nexclude = [\"notes/skipped\"]";

        let report = live_with(&temp, config, &hooks, r"\.txt$", 1).expect("the search runs");

        assert_eq!(paths(&report.pieces[0]), ["keep.txt"]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_mount_or_an_excluded_entry_that_no_rule_ignores_is_still_a_warning() {
        let temp = live_world(&["notes/keep.txt", "notes/cache/x.txt", "notes/skipped/y.txt"]);
        let hooks = Staged {
            mount_roots: vec![piece("cache")],
            ..Staged::default()
        };

        let report = live_with(&temp, "exclude = [\"notes/skipped\"]", &hooks, r"\.txt$", 1)
            .expect("the search runs");

        assert_eq!(
            warning_texts(&report),
            [
                "cannot search all of the live piece notes: mount point: notes/cache",
                "cannot search all of the live piece notes: exclude entry: notes/skipped"
            ]
        );
    }
}
