// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `import` operation: packs a folder from outside the collection as a
//! version of a piece, with steps 2 to 4 of `archive`, and leaves the
//! folder where it is.

use crate::archive::pool::Pool;
use crate::archive::zstd::ZstdSettings;
use crate::clock::Clock;
use crate::collection::Collection;
use crate::commands::archive::seams::{RealSeams, Seams};
use crate::commands::archive::store::{
    Folder, StoreContext, StoreFailure, StoreRequest, apparent_size, remove_staging, staging_chain,
    store_version,
};
use crate::commands::archive::{ArchiveError, Reason, SkipReason, stored_spelling_of};
use crate::commands::survey::Refusal;
use crate::error::{ErrorKind, HasKind, chain_text};
use crate::filesystem::space::{ReserveError, SpaceLedger};
use crate::filesystem::tree::TreeRoot;
use crate::markers::RESTORED_FILE;
use crate::pin::{self, PIN_FILE, PinError};
use crate::plan::{HeldDryRun, PlanAction, Step};
use crate::reference::{ReferenceError, resolve_path};
use crate::relpath::{Name, RelPath, RelPathError};
use crate::repair::Repair;
use crate::report::{Warning, setting_notes};
use crate::rules::{PieceRules, RulesError};
use crate::run::{self, RunError, RunGuard, Started};
use crate::select::{Rules, SelectError};
use crate::sysmem::SysmemError;
use crate::tree::{Class, EntryKind, PieceTree, Problem};
use crate::versions::VersionsError;
use crate::versions::naming::system_zone;
use crate::versions::stamp::Stamp;
use crate::versions::store::fresh_stamp;
use crate::walk::{Hooks, KeptFolder, WalkError, only_blocks_removal, walk_piece_with};
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use serde::Serialize;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Mutex;

// =========================================================
// The request and what comes back
// =========================================================

#[derive(Debug, Clone)]
pub struct Request {
    /// The folder to pack, as given.
    pub folder: PathBuf,
    /// The path of the piece, as given. Without it the piece is named after
    /// the folder.
    pub as_path: Option<PathBuf>,
    /// The version time, as given.
    pub time: Option<String>,
    /// Where a relative `folder` and a relative `as_path` start.
    pub cwd: PathBuf,
    pub dry_run: bool,
}

/// The outcome of `import`.
#[derive(Debug)]
pub enum Outcome {
    /// A real run.
    Imported(ImportReport),
    /// A dry run.
    Planned(ImportPlan),
    /// A dry run that found the lock held.
    Held(HeldDryRun),
}

/// What a real run needs besides the collection.
pub(crate) struct Env<'a> {
    pub seams: &'a dyn Seams,
    pub pool: &'a Pool,
    /// The compression for the number of workers the pool granted.
    pub zstd: &'a (dyn Fn(u32) -> ZstdSettings + Sync),
}

/// A version in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportedVersion {
    pub piece: String,
    pub stamp: String,
    /// The time of the version, which the stamp spells.
    pub version: Timestamp,
    pub archive_bytes: u64,
    pub files: u64,
    pub bytes: u64,
}

/// An import that ended without a version, or whose version may not be
/// durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedImport {
    pub piece: String,
    pub reason: SkipReason,
    pub message: String,
}

/// What a real run did.
#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub imported: Option<ImportedVersion>,
    pub skipped: Option<SkippedImport>,
    /// The repairs at the start of the run that were done.
    pub repairs: Vec<Step<ImportAction>>,
    pub warnings: Vec<Warning>,
}

impl ImportReport {
    /// 3 when the import was skipped, else 0.
    pub fn exit_code(&self) -> u8 {
        if self.skipped.is_some() { 3 } else { 0 }
    }
}

/// What a dry run lists after the repairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportAction {
    Import {
        source: String,
        piece: String,
        stamp: String,
        version: Timestamp,
        /// What the version leaves out: ignored entries (a folder counts
        /// once, with a trailing `/`) and the `.antiq-restored` marker.
        left_out: Vec<String>,
        /// Set when the import would not fit; the action then stands for
        /// the import that is skipped.
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<SkippedImport>,
    },
}

impl PlanAction for ImportAction {
    fn describe(&self) -> String {
        match self {
            ImportAction::Import {
                source,
                piece,
                stamp,
                skipped: None,
                ..
            } => format!("import {source} as {piece}, version {stamp}"),
            ImportAction::Import {
                skipped: Some(skipped),
                ..
            } => format!("skip {}: {}", skipped.piece, skipped.message),
        }
    }
}

/// What a dry run would do: the repairs, then the import, which says so when
/// it would be skipped.
#[derive(Debug, Serialize)]
pub struct ImportPlan {
    dry_run: bool,
    pub actions: Vec<Step<ImportAction>>,
    pub warnings: Vec<Warning>,
}

impl ImportPlan {
    /// The lines the console prints: a `Would ...` line per action, a line
    /// per left-out path under the import, `Would skip ...` when the import
    /// would not fit.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for action in &self.actions {
            lines.push(format!("Would {}", action.describe()));
            if let Step::Command(ImportAction::Import {
                left_out,
                skipped: None,
                ..
            }) = action
            {
                lines.extend(
                    left_out
                        .iter()
                        .map(|path| format!("Would leave out {path}")),
                );
            }
        }
        lines
    }

    /// 3 when the import would be skipped, as the real run would exit.
    pub fn exit_code(&self) -> u8 {
        let skipped = self.actions.iter().any(|action| {
            matches!(
                action,
                Step::Command(ImportAction::Import {
                    skipped: Some(_),
                    ..
                })
            )
        });
        if skipped { 3 } else { 0 }
    }
}

// =========================================================
// Errors
// =========================================================

/// Why import failed.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot start the run")]
    Run(#[from] RunError),
    #[error("cannot resolve the path")]
    Reference(#[from] ReferenceError),
    #[error("cannot select the pieces")]
    Select(#[from] SelectError),
    #[error("cannot read the ignore and timeless rules")]
    Rules(#[from] RulesError),
    #[error("cannot size the pool of workers")]
    Pool(#[source] SysmemError),
    #[error("cannot walk {folder}")]
    Walk {
        folder: String,
        #[source]
        source: WalkError,
    },
    #[error("cannot look for a pin in {folder}")]
    Pin {
        folder: String,
        #[source]
        source: PinError,
    },
    #[error("the name of the folder is not a valid piece name")]
    Name(#[from] RelPathError),
    #[error("cannot prepare the import")]
    Archive(#[from] ArchiveError),
    #[error("cannot choose a version stamp")]
    Stamp(#[source] Box<VersionsError>),
    #[error("cannot look at {path}")]
    Open {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{given}` is not a time: {why}")]
    Time { given: String, why: &'static str },
    #[error("{path} is not a folder: a symlink, a file or a folder with no name of its own")]
    NotAFolder { path: String },
    #[error("{path} lies inside the collection")]
    InsideCollection { path: String },
    #[error("cannot use `{path}` as the piece: {reason}")]
    PiecePath { path: String, reason: String },
    #[error("cannot import {folder}: {}", reasons_text(.reasons))]
    Refused {
        folder: String,
        reasons: Vec<Reason>,
    },
    #[error("the newest file `{path}` is dated {time}, which is in the future")]
    FutureFile { path: String, time: Timestamp },
    #[error("the version time {time} is in the future")]
    FutureTime { time: Timestamp },
    #[error("the version time is outside the years a version stamp covers")]
    OutOfRange,
}

fn reasons_text(reasons: &[Reason]) -> String {
    let texts: Vec<&str> = reasons.iter().map(|reason| reason.text.as_str()).collect();
    texts.join(", ")
}

impl HasKind for ImportError {
    fn kind(&self) -> ErrorKind {
        match self {
            ImportError::Run(error) => error.kind(),
            ImportError::Reference(error) => error.kind(),
            ImportError::Time { .. } => ErrorKind::Usage,
            _ => ErrorKind::Other,
        }
    }
}

// =========================================================
// The command
// =========================================================

/// Imports the folder `request` names, or lists what it would do.
pub fn import(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
) -> Result<Outcome, ImportError> {
    let given = request
        .time
        .as_deref()
        .map(|text| parse_time(text, &system_zone()))
        .transpose()?;
    match run::start(collection, clock, request.dry_run)? {
        Started::DryRun {
            holder: Some(holder),
            ..
        } => Ok(Outcome::Held(HeldDryRun::new(&holder))),
        Started::DryRun {
            repairs, warnings, ..
        } => plan(
            collection, clock, request, given, repairs, warnings, &RealSeams,
        )
        .map(Outcome::Planned),
        Started::Guarded(guard) => {
            let pool = Pool::for_machine().map_err(ImportError::Pool)?;
            let zstd = |workers: u32| ZstdSettings::archive(workers);
            let env = Env {
                seams: &RealSeams,
                pool: &pool,
                zstd: &zstd,
            };
            execute(collection, clock, request, given, &guard, &env).map(Outcome::Imported)
        }
    }
}

/// The dry run: the repairs, then the import it would make, or the skip it
/// would end in.
pub(crate) fn plan(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
    given: Option<Timestamp>,
    repairs: Vec<Repair>,
    warnings: Vec<Warning>,
    seams: &dyn Seams,
) -> Result<ImportPlan, ImportError> {
    let prepared = prepare(collection, clock, request, given, seams.walk_hooks())?;
    let stored = stored_spelling_of(collection, &prepared.piece)?;
    let mut actions: Vec<Step<ImportAction>> = repairs.into_iter().map(Step::Repair).collect();

    // The room is measured as the real run measures it, so a dry run ends
    // with the exit code of the run it predicts.
    let antiq = collection.antiq_dir();
    let skipped = match SpaceLedger::new()
        .try_reserve(apparent_size(&prepared.packed), || seams.free(&antiq))
    {
        Ok(_reservation) => None,
        Err(ReserveError::Short { needed, available }) => {
            let error = ArchiveError::NoRoom {
                piece: stored.to_string(),
                needed,
                available,
            };
            Some(SkippedImport {
                piece: stored.to_string(),
                reason: SkipReason::NoRoom,
                message: chain_text(&error),
            })
        }
        Err(ReserveError::Free(source)) => return Err(ArchiveError::Space(source).into()),
    };
    let stamp = fresh_stamp(
        &collection.pieces_dir(),
        &collection.tmp_dir().join("pack"),
        &stored,
        prepared.version,
    )
    .map_err(|source| ImportError::Stamp(Box::new(source)))?;
    actions.push(Step::Command(ImportAction::Import {
        source: prepared.source.clone(),
        piece: stored.to_string(),
        stamp: stamp.to_string(),
        version: prepared.version,
        left_out: prepared.left_out.clone(),
        skipped,
    }));

    let mut notes = setting_notes(collection);
    notes.extend(warnings);
    Ok(ImportPlan {
        dry_run: true,
        actions,
        warnings: notes,
    })
}

/// The real run, with the repairs of `guard` done. The guard stays held
/// until the import has ended.
pub(crate) fn execute(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
    given: Option<Timestamp>,
    guard: &RunGuard,
    env: &Env<'_>,
) -> Result<ImportReport, ImportError> {
    let prepared = prepare(collection, clock, request, given, env.seams.walk_hooks())?;
    let stored = stored_spelling_of(collection, &prepared.piece)?;
    let piece = stored.to_string();
    let run_dirs = Mutex::new(());
    let ledger = SpaceLedger::new();
    let context = StoreContext {
        collection,
        clock,
        pool: env.pool,
        ledger: &ledger,
        seams: env.seams,
        run_dirs: &run_dirs,
        zstd: env.zstd,
    };
    let store = StoreRequest {
        folder: Folder::Outside(&prepared.root),
        stored: &stored,
        tree: &prepared.packed,
        rules: &prepared.rules,
        imported: Some(clock.now()),
        time: Some(prepared.version),
    };

    let mut report = ImportReport {
        imported: None,
        skipped: None,
        repairs: guard.repaired().iter().cloned().map(Step::Repair).collect(),
        warnings: setting_notes(collection),
    };
    report.warnings.extend(guard.warnings().iter().cloned());
    let skip = |reason, message| SkippedImport {
        piece: piece.clone(),
        reason,
        message,
    };
    match store_version(&context, &store) {
        Ok(stored_version) => {
            // The staging stays, empty, after a successful store: the folder
            // of an archived piece has still to move through it. Nothing
            // follows here, so it goes now.
            remove_staging(
                &run_dirs,
                &collection.tmp_dir(),
                &staging_chain(&stored_version.stamp, &stored),
            );
            report.warnings.extend(stored_version.notes);
            report.imported = Some(ImportedVersion {
                piece,
                stamp: stored_version.stamp.to_string(),
                version: stored_version.stamp.time(),
                archive_bytes: stored_version.archive_bytes,
                files: stored_version.files,
                bytes: stored_version.bytes,
            });
        }
        Err(StoreFailure::NoRoom(error)) => {
            report.skipped = Some(skip(SkipReason::NoRoom, chain_text(error.as_ref())));
        }
        Err(StoreFailure::Changed(error)) => {
            report.skipped = Some(skip(SkipReason::Differs, chain_text(error.as_ref())));
        }
        Err(StoreFailure::NotDurable(error)) => {
            report.skipped = Some(skip(
                SkipReason::Error,
                format!(
                    "the version was stored, but its folder may not be durable: {}",
                    chain_text(error.as_ref())
                ),
            ));
        }
        Err(StoreFailure::OutOfDescriptors(error) | StoreFailure::Failed(error)) => {
            report.skipped = Some(skip(SkipReason::Error, chain_text(error.as_ref())));
        }
    }
    Ok(report)
}

// =========================================================
// What the real run and the dry run both look at
// =========================================================

/// The folder, walked and judged.
struct Prepared {
    /// The folder as the output names it.
    source: String,
    /// The folder, opened as a root of its own.
    root: TreeRoot,
    /// The tree for the packer, which names the archive's top directory
    /// after the folder.
    packed: PieceTree,
    rules: PieceRules,
    /// The piece path as the patterns see it.
    piece: RelPath,
    version: Timestamp,
    left_out: Vec<String>,
}

/// Every check an import makes before it takes any room, in the order that
/// shows the user the most basic fault first.
fn prepare(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
    given: Option<Timestamp>,
    hooks: &dyn Hooks,
) -> Result<Prepared, ImportError> {
    let config = &collection.config().collection;
    let (canonical, name) = locate(request)?;
    let source = canonical.display().to_string();

    // A folder the collection already holds is a piece or part of one.
    match resolve_path(collection, &request.cwd, &canonical) {
        Ok(_) => return Err(ImportError::InsideCollection { path: source }),
        Err(ReferenceError::Outside { .. }) => {}
        Err(error) => return Err(error.into()),
    }

    let piece = match &request.as_path {
        Some(path) => resolve_path(collection, &request.cwd, path)?,
        None => RelPath::root().join(&Name::from_bytes(name.as_bytes().to_vec())?),
    };
    let select = Rules::new(&config.patterns, &config.exclude)?;
    check_path(&piece, &config.exclude, &select)?;

    // The collection's exclude list names paths of the collection, so it
    // does not apply inside the folder.
    let root = TreeRoot::open(&canonical).map_err(|source_error| ImportError::Open {
        path: source.clone(),
        source: source_error,
    })?;
    let rules = PieceRules::new(&config.ignore, &config.timeless)?;
    let top = RelPath::root();
    let walked = walk_piece_with(
        &root,
        &top,
        &Rules::new(&[], &[])?,
        &rules,
        &KeptFolder(hooks),
    )
    .map_err(|source_error| ImportError::Walk {
        folder: source.clone(),
        source: source_error,
    })?;

    // Nothing is moved or deleted in the folder, so what only stops a
    // removal does not stop an import.
    let problems: Vec<Problem> = walked
        .problems()
        .iter()
        .filter(|problem| !only_blocks_removal(problem))
        .cloned()
        .collect();
    let mut reasons: Vec<Reason> = problems
        .iter()
        .map(|problem| Reason::from(&Refusal::of(&top, problem)))
        .collect();
    let pinned = pin::is_pinned(&root, &top).map_err(|source_error| ImportError::Pin {
        folder: source.clone(),
        source: source_error,
    })?;
    if pinned {
        reasons.push(Reason {
            kind: "pinned",
            path: PIN_FILE.to_string(),
            text: format!("pinned: {PIN_FILE}"),
            line: None,
        });
    }

    // The newest counted file gives the version time. The first of equal
    // times in path order is the one the messages name.
    let newest = walked
        .entries()
        .iter()
        .filter(|entry| entry.kind == EntryKind::File && entry.class == Class::Counted)
        .max_by(|a, b| {
            a.mtime_ns
                .cmp(&b.mtime_ns)
                .then_with(|| b.path.cmp(&a.path))
        });
    if reasons.is_empty() && newest.is_none() {
        reasons.push(Reason {
            kind: "empty",
            path: ".".to_string(),
            text: "empty: no file counts".to_string(),
            line: None,
        });
    }
    if !reasons.is_empty() {
        return Err(ImportError::Refused {
            folder: source,
            reasons,
        });
    }
    let newest = newest.expect("a folder with no refusal has a counted file");

    let now = clock.now();
    let version = match given {
        Some(time) => {
            if time > now {
                return Err(ImportError::FutureTime { time });
            }
            time
        }
        None => {
            let time =
                Timestamp::from_nanosecond(newest.mtime_ns).map_err(|_| ImportError::OutOfRange)?;
            if time > now {
                return Err(ImportError::FutureFile {
                    path: newest.path.to_string(),
                    time,
                });
            }
            time
        }
    };
    Stamp::at(version).map_err(|_| ImportError::OutOfRange)?;

    let packed = PieceTree::new(
        RelPath::root().join(&Name::from_bytes(name.as_bytes().to_vec())?),
        walked.entries().to_vec(),
        problems,
    );
    Ok(Prepared {
        left_out: left_out(&walked),
        source,
        root,
        packed,
        rules,
        piece,
        version,
    })
}

/// The folder as an absolute path with its symlinks resolved above it, and
/// its own name. The folder itself must be a directory and no symlink: a
/// walk never follows a link.
fn locate(request: &Request) -> Result<(PathBuf, std::ffi::OsString), ImportError> {
    let absolute = request.cwd.join(&request.folder);
    let shown = absolute.display().to_string();
    let open_error = |source| ImportError::Open {
        path: shown.clone(),
        source,
    };
    let canonical = match absolute.file_name() {
        Some(name) => {
            let parent = absolute
                .parent()
                .expect("a path with a file name has a parent");
            std::fs::canonicalize(parent)
                .map_err(open_error)?
                .join(name)
        }
        // `..` at the end names a directory only once it is resolved.
        None => std::fs::canonicalize(&absolute).map_err(open_error)?,
    };
    let metadata = std::fs::symlink_metadata(&canonical).map_err(open_error)?;
    let name = canonical.file_name().map(ToOwned::to_owned);
    match name {
        Some(name) if metadata.is_dir() => Ok((canonical, name)),
        _ => Err(ImportError::NotAFolder {
            path: canonical.display().to_string(),
        }),
    }
}

/// The ignored entries that are not inside another ignored entry, and the
/// restored marker, sorted.
fn left_out(tree: &PieceTree) -> Vec<String> {
    let mut left = Vec::new();
    let mut last: Option<&RelPath> = None;
    for entry in tree.entries() {
        if entry.class == Class::Ignored {
            let inside = last.is_some_and(|folder| lies_below(&entry.path, folder));
            if !inside {
                let suffix = if entry.kind == EntryKind::Directory {
                    "/"
                } else {
                    ""
                };
                left.push(format!("{}{suffix}", entry.path));
                last = Some(&entry.path);
            }
        } else if entry.class == Class::Control && entry.path.as_bytes() == RESTORED_FILE.as_bytes()
        {
            left.push(RESTORED_FILE.to_string());
        }
    }
    left.sort();
    left
}

// =========================================================
// The path of the piece and the version time
// =========================================================

/// Why `path` cannot be the piece an import makes, if it cannot.
pub(crate) fn check_path(
    path: &RelPath,
    exclude: &[RelPath],
    rules: &Rules,
) -> Result<(), ImportError> {
    let refuse = |reason: String| ImportError::PiecePath {
        path: path.to_string(),
        reason,
    };
    if path.is_root() {
        return Err(refuse("the collection root cannot be a piece".into()));
    }
    if path.components().next() == Some(b".antiq".as_slice()) {
        return Err(refuse(
            "it is inside .antiq/, which holds the collection's own files".into(),
        ));
    }
    for excluded in exclude {
        if path == excluded || lies_below(path, excluded) {
            return Err(refuse(format!(
                "it is excluded from the collection (`{excluded}`)"
            )));
        }
    }
    // Where matches nest, the shallowest one is the piece, so a path below
    // a selected path is part of that piece.
    for ancestor in ancestors(path) {
        if rules.matches(&ancestor) {
            return Err(refuse(format!("it lies inside the piece `{ancestor}`")));
        }
    }
    if !rules.matches(path) {
        return Err(refuse(
            "the patterns of the collection do not select it".into(),
        ));
    }
    Ok(())
}

fn lies_below(path: &RelPath, parent: &RelPath) -> bool {
    path.as_bytes()
        .strip_prefix(parent.as_bytes())
        .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// The proper ancestors of `path`, shortest first, the root left out.
fn ancestors(path: &RelPath) -> Vec<RelPath> {
    let components: Vec<&[u8]> = path.components().collect();
    (1..components.len())
        .map(|length| {
            RelPath::from_bytes(components[..length].join(&b'/'))
                .expect("a prefix of a valid path is a valid path")
        })
        .collect()
}

/// The time `--time` names. `2026-11-04T090211` is a UTC version stamp
/// and `2026-11-04 10:02:11` a time in `zone`; nothing else is a time, as a
/// date alone or a prefix of a stamp names no second. A local time that
/// `zone` repeats or skips names no single second either.
pub(crate) fn parse_time(text: &str, zone: &TimeZone) -> Result<Timestamp, ImportError> {
    let refuse = |why: &'static str| ImportError::Time {
        given: text.to_string(),
        why,
    };
    if text.contains('T') {
        let stamp = Stamp::parse(text).map_err(|_| {
            refuse("a UTC time is written like the version stamp 2026-11-04T090211")
        })?;
        if stamp.counter() != 1 {
            return Err(refuse("a counter names a second version, not a time"));
        }
        return Ok(stamp.time());
    }
    let civil = DateTime::strptime("%Y-%m-%d %H:%M:%S", text)
        .map_err(|_| refuse("a local time is written like 2026-11-04 10:02:11"))?;
    zone.to_ambiguous_zoned(civil)
        .unambiguous()
        .map(|zoned| zoned.timestamp())
        .map_err(|_| refuse("daylight saving time repeats or skips that local time"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::filesystem::FilesystemError;
    use crate::filesystem::atomic::recording::{Flush, Recording};
    use crate::filesystem::atomic::{Durable, RealDurable};
    use crate::testing::shared_pool;
    use crate::testing::tree::TempCollection;
    use crate::walk::RealFs;
    use std::fs;
    use std::io;
    use std::path::Path;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn berlin() -> TimeZone {
        TimeZone::get("Europe/Berlin").expect("the zone exists")
    }

    // =========================================================
    // The time
    // =========================================================

    #[test]
    fn a_stamp_is_a_time_in_utc() {
        let time = parse_time("2026-11-04T090211", &berlin()).expect("a time");

        assert_eq!(time.to_string(), "2026-11-04T09:02:11Z");
    }

    #[test]
    fn a_local_time_is_read_in_the_zone() {
        let time = parse_time("2026-11-04 10:02:11", &berlin()).expect("a time");

        assert_eq!(time.to_string(), "2026-11-04T09:02:11Z");
    }

    #[test]
    fn what_names_no_single_second_is_no_time() {
        let refused = [
            "2026-11-04",
            "2026-11-04T09",
            "2026-11-04T090211-2",
            "2026-11-04 10:02",
            "soon",
            "",
            // 2026-10-25 02:30 happens twice in Berlin, 2026-03-29 02:30 never.
            "2026-10-25 02:30:00",
            "2026-03-29 02:30:00",
        ];
        for text in refused {
            let error = parse_time(text, &berlin()).expect_err(text);
            assert_eq!(error.kind(), ErrorKind::Usage, "{text:?}");
        }
    }

    // =========================================================
    // The path of the piece
    // =========================================================

    fn rules() -> Rules {
        Rules::new(&["*".to_string(), "kept/*".to_string()], &[path("skipped")]).expect("rules")
    }

    fn refusal(text: &str) -> String {
        match check_path(&path(text), &[path("skipped")], &rules()) {
            Ok(()) => panic!("{text} is refused"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn a_selected_path_is_a_piece() {
        check_path(&path("anything"), &[path("skipped")], &rules()).expect("selected");
        check_path(
            &path("kept/inner"),
            &[path("skipped")],
            &Rules::new(&["kept/*".to_string()], &[]).expect("rules"),
        )
        .expect("selected");
    }

    #[test]
    fn the_paths_that_cannot_be_pieces_say_why() {
        assert!(refusal("").contains("root"));
        assert!(refusal(".antiq/x").contains(".antiq/"));
        assert!(refusal("skipped").contains("excluded"));
        assert!(refusal("skipped/below").contains("excluded"));
        assert!(refusal("anything/inner").contains("inside the piece `anything`"));
        assert!(refusal(".dot").contains("do not select"));
    }

    // =========================================================
    // The runs
    // =========================================================

    type Change = Box<dyn Fn(&Path) + Send + Sync>;

    /// What a test changes about the real run.
    #[derive(Default)]
    struct Probe {
        free: Option<u64>,
        /// Reading the free space fails.
        free_fails: bool,
        /// The walk of the folder runs out of file descriptors.
        exhausts: bool,
        recording: Option<Recording>,
        before_verify: Option<Change>,
        /// Paths below the top that report a mount, `.` for the top itself.
        mounts: Vec<&'static str>,
    }

    impl Hooks for Probe {
        fn fault(&self, _path: &RelPath, _call: crate::walk::Call) -> Option<io::Error> {
            self.exhausts
                .then(|| io::Error::from_raw_os_error(libc::EMFILE))
        }

        fn mount_root(&self, path: &RelPath, reported: bool) -> bool {
            let name = if path.is_root() {
                ".".to_string()
            } else {
                path.to_string()
            };
            reported || self.mounts.iter().any(|mount| *mount == name)
        }
    }

    impl Seams for Probe {
        fn durable(&self) -> &dyn Durable {
            match &self.recording {
                Some(recording) => recording,
                None => &RealDurable,
            }
        }

        fn free(&self, dir: &Path) -> Result<u64, FilesystemError> {
            if self.free_fails {
                return Err(crate::filesystem::io_error("read the free space of", dir)(
                    io::Error::from_raw_os_error(libc::EIO),
                ));
            }
            match self.free {
                Some(free) => Ok(free),
                None => RealSeams.free(dir),
            }
        }

        fn walk_hooks(&self) -> &dyn Hooks {
            self
        }

        fn before_verify(&self, staging: &Path) {
            if let Some(change) = &self.before_verify {
                change(staging);
            }
        }
    }

    struct World {
        collection: TempCollection,
        source: tempfile::TempDir,
    }

    fn world() -> World {
        let source = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir_all(source.path().join("sub")).expect("a folder");
        fs::write(source.path().join("a.txt"), "alpha").expect("a file");
        fs::write(source.path().join("sub/b.txt"), "beta").expect("a file");
        World {
            collection: TempCollection::minimal(),
            source,
        }
    }

    fn collection(world: &World) -> Collection {
        Collection::find(world.collection.root()).expect("a collection")
    }

    fn request(world: &World, dry_run: bool) -> Request {
        Request {
            folder: world.source.path().to_path_buf(),
            as_path: Some(PathBuf::from("piece")),
            time: None,
            cwd: world.collection.root().to_path_buf(),
            dry_run,
        }
    }

    fn fast(_workers: u32) -> ZstdSettings {
        ZstdSettings::fast()
    }

    fn execute_with(world: &World, seams: &dyn Seams) -> Result<ImportReport, ImportError> {
        let collection = collection(world);
        let clock = FixedClock::new(Timestamp::now());
        let Started::Guarded(guard) = run::start(&collection, &clock, false).expect("starts")
        else {
            panic!("a real run is guarded");
        };
        let env = Env {
            seams,
            pool: shared_pool(),
            zstd: &fast,
        };
        execute(
            &collection,
            &clock,
            &request(world, false),
            None,
            &guard,
            &env,
        )
    }

    fn plan_with(world: &World, seams: &dyn Seams) -> Result<ImportPlan, ImportError> {
        let collection = collection(world);
        let clock = FixedClock::new(Timestamp::now());
        plan(
            &collection,
            &clock,
            &request(world, true),
            None,
            Vec::new(),
            Vec::new(),
            seams,
        )
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = match fs::read_dir(dir) {
            Ok(read) => read
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into()
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        found.sort();
        found
    }

    fn store_is_empty(world: &World) {
        let root = world.collection.root();
        assert_eq!(entries(&root.join(".antiq/pieces")), Vec::<String>::new());
        assert_eq!(entries(&root.join(".antiq/tmp/pack")), Vec::<String>::new());
    }

    #[test]
    fn a_folder_is_stored_and_the_staging_is_gone() {
        let world = world();

        let report = execute_with(&world, &RealSeams).expect("the import ends");

        let imported = report.imported.expect("imported");
        assert_eq!((imported.files, imported.bytes), (2, 9));
        assert_eq!(
            entries(&world.collection.root().join(".antiq/tmp/pack")),
            Vec::<String>::new()
        );
        assert_eq!(entries(world.source.path()), ["a.txt", "sub"]);
    }

    #[test]
    fn a_folder_that_does_not_fit_is_skipped_with_no_room_and_leaves_nothing() {
        let world = world();
        let probe = Probe {
            free: Some(1),
            ..Probe::default()
        };

        let report = execute_with(&world, &probe).expect("the import ends");

        let skipped = report.skipped.as_ref().expect("skipped");
        assert_eq!(skipped.reason, SkipReason::NoRoom);
        assert_eq!(report.exit_code(), 3);
        store_is_empty(&world);
        assert_eq!(entries(world.source.path()), ["a.txt", "sub"]);
    }

    #[test]
    fn a_dry_run_that_would_not_fit_says_so_and_exits_3() {
        let world = world();
        let probe = Probe {
            free: Some(1),
            ..Probe::default()
        };

        let plan = plan_with(&world, &probe).expect("the plan ends");

        assert_eq!(plan.exit_code(), 3);
        assert!(
            plan.lines()[0].starts_with("Would skip piece: "),
            "{:?}",
            plan.lines()
        );
        assert!(
            !plan
                .lines()
                .iter()
                .any(|line| line.starts_with("Would import")),
            "{:?}",
            plan.lines()
        );
        let json = serde_json::to_value(&plan).expect("the plan serialises");
        assert!(json.get("skipped").is_none(), "{json}");
        let action = json["actions"]
            .as_array()
            .and_then(|actions| actions.last())
            .expect("the import action is listed");
        assert_eq!(action["kind"], "import", "{action}");
        assert_eq!(action["skipped"]["reason"], "no_room", "{action}");
        assert!(
            action["skipped"]["message"].is_string() && action["stamp"].is_string(),
            "{action}"
        );
    }

    #[test]
    fn a_folder_that_changes_while_it_is_packed_is_skipped_with_differs() {
        let world = world();
        let source = world.source.path().to_path_buf();
        let probe = Probe {
            before_verify: Some(Box::new(move |_| {
                fs::write(source.join("a.txt"), "changed").expect("the file changes");
            })),
            ..Probe::default()
        };

        let report = execute_with(&world, &probe).expect("the import ends");

        assert_eq!(report.skipped.expect("skipped").reason, SkipReason::Differs);
        store_is_empty(&world);
    }

    #[test]
    fn a_version_whose_folder_may_not_be_durable_is_skipped_and_says_it_was_stored() {
        let counting = Probe {
            recording: Some(Recording::new()),
            ..Probe::default()
        };
        execute_with(&world(), &counting).expect("the import ends");
        let dir_flushes = counting
            .recording
            .as_ref()
            .expect("recording")
            .flushes()
            .iter()
            .filter(|flush| matches!(flush, Flush::Dir(_)))
            .count();
        let world = world();
        let recording = Recording::new();
        recording.fail_dirs_at(dir_flushes - 1);
        let probe = Probe {
            recording: Some(recording),
            ..Probe::default()
        };

        let report = execute_with(&world, &probe).expect("the import ends");

        let skipped = report.skipped.as_ref().expect("skipped");
        assert!(
            skipped.message.contains("version was stored"),
            "{}",
            skipped.message
        );
        assert_eq!(report.exit_code(), 3);
        assert_eq!(
            entries(&world.collection.root().join(".antiq/tmp/pack")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_folder_that_is_a_mount_root_is_imported() {
        let world = world();
        let probe = Probe {
            mounts: vec!["."],
            ..Probe::default()
        };

        let report = execute_with(&world, &probe).expect("the import ends");

        assert!(report.imported.is_some());
    }

    #[test]
    fn a_mount_below_the_folder_refuses() {
        let world = world();
        let probe = Probe {
            mounts: vec!["sub"],
            ..Probe::default()
        };

        let error = execute_with(&world, &probe).expect_err("refused");

        assert!(error.to_string().contains("mount point: sub"), "{error}");
        store_is_empty(&world);
    }

    #[test]
    fn an_ignored_folder_is_left_out_once() {
        let world = world();
        fs::create_dir_all(world.source.path().join("cache/deep")).expect("folders");
        fs::write(world.source.path().join("cache/deep/x"), "x").expect("a file");
        fs::write(world.source.path().join("debug.log"), "x").expect("a file");
        fs::write(world.source.path().join(".antiqignore"), "cache/\n*.log\n").expect("rules");

        let plan = plan_with(&world, &RealSeams).expect("the plan ends");

        let Some(Step::Command(ImportAction::Import { left_out, .. })) = plan.actions.last() else {
            panic!("an import is planned: {:?}", plan.actions);
        };
        assert_eq!(left_out, &["cache/", "debug.log"]);
    }
    #[test]
    fn free_space_that_cannot_be_read_is_an_error_in_a_dry_run_and_a_skip_in_a_real_run() {
        let world = world();
        let probe = Probe {
            free_fails: true,
            ..Probe::default()
        };

        let planned = plan_with(&world, &probe).expect_err("the dry run fails");
        let report = execute_with(&world, &probe).expect("the import ends");

        assert!(matches!(
            planned,
            ImportError::Archive(ArchiveError::Space(_))
        ));
        assert_eq!(report.skipped.expect("skipped").reason, SkipReason::Error);
        store_is_empty(&world);
    }

    #[test]
    fn a_walk_that_runs_out_of_descriptors_is_an_error() {
        let world = world();
        let probe = Probe {
            exhausts: true,
            ..Probe::default()
        };

        let error = execute_with(&world, &probe).expect_err("the walk fails");

        assert!(matches!(error, ImportError::Walk { .. }), "{error}");
        store_is_empty(&world);
    }

    #[test]
    fn a_folder_that_does_not_exist_is_an_error_naming_it() {
        let world = world();
        let mut missing = request(&world, true);
        missing.folder = world.source.path().join("gone/deeper");

        let error = prepare(
            &collection(&world),
            &FixedClock::new(Timestamp::now()),
            &missing,
            None,
            &RealFs,
        )
        .err()
        .expect("the folder is missing");

        assert!(matches!(error, ImportError::Open { .. }), "{error}");
        assert!(format!("{error:?}").contains("gone"), "{error:?}");
    }

    #[test]
    fn a_folder_given_with_dot_dot_is_resolved_before_it_is_used() {
        let world = world();
        let mut dotted = request(&world, true);
        dotted.folder = world.source.path().join("sub/..");

        let prepared = prepare(
            &collection(&world),
            &FixedClock::new(Timestamp::now()),
            &dotted,
            None,
            &RealFs,
        )
        .expect("the folder is the source");

        assert_eq!(
            PathBuf::from(&prepared.source),
            fs::canonicalize(world.source.path()).expect("the source exists")
        );
    }

    /// Gives the folder its mode back, also when a test fails.
    struct Mode(PathBuf, u32);

    impl Drop for Mode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
        }
    }

    #[test]
    fn a_folder_that_cannot_be_opened_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let world = world();
        let _restore = Mode(world.source.path().to_path_buf(), 0o755);
        fs::set_permissions(world.source.path(), fs::Permissions::from_mode(0o000))
            .expect("the mode changes");
        assert!(
            fs::read_dir(world.source.path()).is_err(),
            "the test runs as a user the mode applies to"
        );

        let error = execute_with(&world, &RealSeams).expect_err("the folder does not open");

        assert!(matches!(error, ImportError::Open { .. }), "{error}");
    }
}
