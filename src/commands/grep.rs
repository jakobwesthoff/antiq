// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `grep` operation: searches the paths of every archived file in the
//! listings of the store.

use crate::error::{ErrorKind, HasKind};
use crate::filesystem::tree::TreeRoot;
use crate::listing;
use crate::relpath::{Name, RelPath};
use crate::versions::VersionsError;
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StoreVersion};
use regex::bytes::Regex;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Why grep failed. A listing that cannot be searched is not an error: its
/// version is skipped and reported in [`GrepReport::skipped`].
#[derive(Debug, thiserror::Error)]
pub enum GrepError {
    #[error("the pattern is not a valid regular expression: {0}")]
    InvalidPattern(#[source] regex::Error),
    #[error("cannot read the store: {0}")]
    Store(#[source] VersionsError),
}

impl HasKind for GrepError {
    fn kind(&self) -> ErrorKind {
        match self {
            // The pattern is part of the command line.
            GrepError::InvalidPattern(_) => ErrorKind::Usage,
            GrepError::Store(_) => ErrorKind::Other,
        }
    }
}

/// What a search found. Pieces are sorted by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrepReport {
    pub pieces: Vec<PieceHits>,
    pub skipped: Vec<SkippedVersion>,
}

/// The hits in one piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceHits {
    pub piece: RelPath,
    /// The pattern matches the piece path itself (see [`is_name_match`]).
    /// The piece then has no `files`: every file in it would match too.
    pub name_match: bool,
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
    /// Newest first.
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

/// Searches the listings of every complete version below `pieces_dir`.
///
/// An incomplete version, one without an archive, is not searched and not
/// reported: it is the leftover of an interrupted run.
pub fn grep(pieces_dir: &Path, pattern: &Regex) -> Result<GrepReport, GrepError> {
    // All cores but two, so that the machine stays usable during a search.
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .saturating_sub(2)
        .max(1);
    search(pieces_dir, pattern, workers)
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

/// One version to search.
struct Job<'a> {
    piece: &'a RelPath,
    version: &'a StoreVersion,
    /// Only the head is read: the piece name matches.
    head_only: bool,
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

fn search(pieces_dir: &Path, pattern: &Regex, workers: usize) -> Result<GrepReport, GrepError> {
    let scan = store::scan(pieces_dir).map_err(GrepError::Store)?;

    // Newest first, as every list of versions is reported.
    let mut complete: Vec<(&RelPath, Vec<&StoreVersion>)> = Vec::new();
    for piece in &scan.pieces {
        let mut versions: Vec<&StoreVersion> = piece
            .versions
            .iter()
            .filter(|version| !version.is_incomplete())
            .collect();
        versions.sort_by_key(|version| std::cmp::Reverse(version.stamp));
        complete.push((&piece.path, versions));
    }
    let name_matches: Vec<bool> = complete
        .iter()
        .map(|(piece, _)| is_name_match(pattern, piece))
        .collect();

    let mut jobs = Vec::new();
    for ((piece, versions), head_only) in complete.iter().zip(&name_matches) {
        for version in versions {
            jobs.push(Job {
                piece,
                version,
                head_only: *head_only,
            });
        }
    }
    let outcomes = run_jobs(pieces_dir, pattern, &jobs, workers)?;

    // The outcomes come in the order of the jobs, so each piece reads its
    // own slice of them.
    let mut report = GrepReport::default();
    let mut outcomes = outcomes.into_iter();
    for ((piece, versions), name_match) in complete.iter().zip(name_matches) {
        let mut searched = Vec::new();
        let mut files: BTreeMap<Vec<u8>, Vec<Stamp>> = BTreeMap::new();
        for version in versions {
            match outcomes.next().expect("every job has an outcome") {
                Outcome::Searched {
                    files: count,
                    paths,
                } => {
                    searched.push(SearchedVersion {
                        stamp: version.stamp,
                        files: count,
                    });
                    for path in paths {
                        files.entry(path).or_default().push(version.stamp);
                    }
                }
                Outcome::Skipped(reason) => report.skipped.push(SkippedVersion {
                    piece: (*piece).clone(),
                    stamp: version.stamp,
                    reason,
                }),
            }
        }
        let found = if name_match {
            !searched.is_empty()
        } else {
            !files.is_empty()
        };
        if found {
            report.pieces.push(PieceHits {
                piece: (*piece).clone(),
                name_match,
                versions: searched,
                all_stamps: versions.iter().map(|version| version.stamp).collect(),
                files: files
                    .into_iter()
                    .map(|(path, versions)| FileHit { path, versions })
                    .collect(),
            });
        }
    }
    Ok(report)
}

/// Runs the jobs on `workers` threads (at most one per job). The outcomes
/// come in the order of `jobs`.
fn run_jobs(
    pieces_dir: &Path,
    pattern: &Regex,
    jobs: &[Job],
    workers: usize,
) -> Result<Vec<Outcome>, GrepError> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let store = TreeRoot::open(pieces_dir).map_err(|source| {
        GrepError::Store(VersionsError::Io {
            action: "open",
            path: pieces_dir.display().to_string(),
            source,
        })
    })?;
    let next = AtomicUsize::new(0);
    let outcomes: Mutex<Vec<Option<Outcome>>> = Mutex::new(jobs.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, jobs.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        break;
                    };
                    let outcome = search_version(&store, pattern, job);
                    let mut outcomes = outcomes.lock().unwrap_or_else(PoisonError::into_inner);
                    outcomes[index] = Some(outcome);
                }
            });
        }
    });
    Ok(outcomes
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .map(|outcome| outcome.expect("every job was run"))
        .collect())
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
        search(&store.pieces_dir(), &pattern, 2).expect("the search runs")
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
        let report = search(Path::new("/nonexistent-antiq-pieces"), &pattern, 1)
            .expect("a missing store is empty");
        assert_eq!(report, GrepReport::default());
    }

    #[test]
    fn a_store_that_cannot_be_scanned_is_an_error() {
        let mut tree = Tree::new();
        let not_a_directory = tree.file("pieces", "a file where the store should be");
        let pattern = compile("x").expect("compiles");

        let error = search(&not_a_directory, &pattern, 1).expect_err("the scan fails");

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

        let one = search(&store.pieces_dir(), &pattern, 1).expect("one worker");
        let four = search(&store.pieces_dir(), &pattern, 4).expect("four workers");

        assert_eq!(one, four);
        assert_eq!(one.pieces[0].files[0].versions.len(), 12);
        assert_eq!(
            one.pieces[0].files[0].versions[0],
            stamp("2026-10-12T100000")
        );
    }
}
