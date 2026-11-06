// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` operation.

mod remove;
mod report;
mod seams;
mod store;

pub use report::{
    ArchiveAction, ArchivePlan, ArchiveReport, ArchivedPiece, Outcome, Reason, RefusedPiece,
    SkipReason, SkippedPiece,
};

use crate::archive::PackError;
use crate::archive::pool::Pool;
use crate::archive::zstd::ZstdSettings;
use crate::clock::Clock;
use crate::collection::Collection;
use crate::commands::survey::Refusal;
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::FilesystemError;
use crate::filesystem::space::SpaceLedger;
use crate::filesystem::tree::TreeRoot;
use crate::idle::{self, IdleError, IdleVerdict};
use crate::listing::ListingError;
use crate::markers::RESTORED_FILE;
use crate::pin::PIN_FILE;
use crate::plan::HeldDryRun;
use crate::plan::Step;
use crate::reference::ReferenceError;
use crate::reference::{CandidateSet, resolve_piece, stored_spelling};
use crate::relpath::{Name, RelPath};
use crate::repair::RepairError;
use crate::repair::{Destination, Repair};
use crate::report::{Warning, setting_notes};
use crate::rules::PieceRules;
use crate::rules::RulesError;
use crate::run::{self, RunError, RunGuard, Started};
use crate::select::SelectError;
use crate::select::{Rules, select};
use crate::sysmem::SysmemError;
use crate::tree::PieceTree;
use crate::versions::VersionsError;
use crate::versions::stamp::Stamp;
use crate::walk::walk_piece_with;
use crate::walk::{Hooks, RealFs, WalkError};
use jiff::Timestamp;
use remove::{Outcome as Removed, RemoveContext, RemoveRequest, remove_folder};
use seams::{RealSeams, Seams};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use store::{StoreContext, StoreFailure, StoreRequest, store_version};

/// Which pieces to archive.
#[derive(Debug, Clone)]
pub enum Target {
    /// Every live piece that is due.
    Due,
    /// The piece `reference` names, however recently it changed. `cwd` is
    /// where a relative path starts.
    Piece { reference: OsString, cwd: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Request {
    pub target: Target,
    pub dry_run: bool,
}

/// What a run needs besides the collection.
pub(crate) struct Env<'a> {
    pub seams: &'a dyn Seams,
    pub pool: &'a Pool,
    /// The compression for the number of workers the pool granted.
    pub zstd: &'a (dyn Fn(u32) -> ZstdSettings + Sync),
    /// How many pieces run at a time.
    pub parallel: usize,
}

/// Archives the pieces `request` names, or lists what it would do.
pub fn archive(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
) -> Result<Outcome, ArchiveError> {
    if matches!(&request.target, Target::Piece { reference, .. } if reference.is_empty()) {
        return Err(ArchiveError::EmptyReference);
    }
    match run::start(collection, clock, request.dry_run)? {
        Started::DryRun {
            holder: Some(holder),
            ..
        } => Ok(Outcome::Held(HeldDryRun::new(&holder))),
        Started::DryRun {
            repairs, warnings, ..
        } => plan(
            collection,
            clock,
            &request.target,
            repairs,
            warnings,
            &RealFs,
        )
        .map(Outcome::Planned),
        Started::Guarded(guard) => {
            let pool = Pool::for_machine().map_err(ArchiveError::Pool)?;
            let zstd = |workers: u32| ZstdSettings::archive(workers);
            let env = Env {
                seams: &RealSeams,
                pool: &pool,
                zstd: &zstd,
                parallel: std::thread::available_parallelism().map_or(1, usize::from),
            };
            execute(collection, clock, &request.target, &guard, &env).map(Outcome::Archived)
        }
    }
}

/// The dry run: the repairs, the pieces it would archive and the ones it
/// refuses. The folders are judged where they lie now; a tree that repair 4
/// would give back to its live path is not judged.
pub(crate) fn plan(
    collection: &Collection,
    clock: &dyn Clock,
    target: &Target,
    repairs: Vec<Repair>,
    warnings: Vec<Warning>,
    hooks: &dyn Hooks,
) -> Result<ArchivePlan, ArchiveError> {
    let setup = Setup::new(collection, target)?;
    let now = clock.now();
    let mut actions: Vec<Step<ArchiveAction>> = repairs.into_iter().map(Step::Repair).collect();
    let mut refused = Vec::new();
    for piece in &setup.pieces {
        let tree = setup.walk(piece, hooks)?;
        match setup.judge(&tree, now) {
            Judged::Go => actions.push(Step::Command(ArchiveAction::Archive {
                piece: piece.to_string(),
            })),
            Judged::Quiet => {}
            Judged::Refused(piece) => refused.push(piece),
        }
    }
    let mut notes = setting_notes(collection);
    notes.extend(warnings);
    Ok(ArchivePlan::new(actions, refused, notes, setup.named))
}

/// The real run, with the repairs of `guard` done.
pub(crate) fn execute(
    collection: &Collection,
    clock: &dyn Clock,
    target: &Target,
    guard: &RunGuard,
    env: &Env<'_>,
) -> Result<ArchiveReport, ArchiveError> {
    let setup = Setup::new(collection, target)?;
    let shared = Shared {
        collection,
        clock,
        env,
        setup: &setup,
        now: clock.now(),
        ledger: SpaceLedger::new(),
        run_dirs: Mutex::new(()),
    };
    let done = run_pieces(&shared);

    let mut report = ArchiveReport {
        archived: Vec::new(),
        skipped: Vec::new(),
        refused: Vec::new(),
        repairs: guard.repaired().iter().cloned().map(Step::Repair).collect(),
        warnings: setting_notes(collection),
        named: setup.named,
    };
    report.warnings.extend(guard.warnings().iter().cloned());
    let mut exhausted = None;
    for item in done {
        report.warnings.extend(item.warnings);
        match item.finished {
            Finished::Archived(piece) => report.archived.push(piece),
            Finished::Skipped(piece) => report.skipped.push(piece),
            Finished::Refused(piece) => report.refused.push(piece),
            Finished::Quiet => {}
        }
        exhausted = exhausted.or(item.exhausted);
    }
    if let Some(cause) = exhausted {
        report.warnings.push(Warning::new(format!(
            "the run ran out of file descriptors ({cause}), so the pieces not yet started are skipped"
        )));
    }
    Ok(report)
}

// =========================================================
// What the real run and the dry run both look at
// =========================================================

struct Setup {
    root: TreeRoot,
    select_rules: Rules,
    piece_rules: PieceRules,
    settings: idle::Settings,
    /// The pieces to look at, sorted by path.
    pieces: Vec<RelPath>,
    /// Whether the command line names the piece.
    named: bool,
}

/// What `archive` makes of a piece.
enum Judged {
    Go,
    /// Not due, pinned or empty without being named: nothing to say.
    Quiet,
    Refused(RefusedPiece),
}

impl Setup {
    fn new(collection: &Collection, target: &Target) -> Result<Setup, ArchiveError> {
        let config = &collection.config().collection;
        let root = TreeRoot::open(collection.root()).map_err(|source| ArchiveError::Open {
            path: collection.root().display().to_string(),
            source,
        })?;
        let select_rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
        let selection = select(&root, &select_rules)?;
        let piece_rules = PieceRules::new(&config.ignore, &config.timeless)?;
        let (mut pieces, named) = match target {
            Target::Due => (selection.pieces().to_vec(), false),
            Target::Piece { reference, cwd } => {
                let candidates = CandidateSet::live(collection, &selection);
                let piece = resolve_piece(collection, cwd, reference, &candidates)?;
                (vec![piece], true)
            }
        };
        pieces.sort();
        Ok(Setup {
            root,
            select_rules,
            piece_rules,
            settings: idle::Settings::new(config.idle_days, config.soon_days),
            pieces,
            named,
        })
    }

    fn walk(&self, piece: &RelPath, hooks: &dyn Hooks) -> Result<PieceTree, ArchiveError> {
        walk_piece_with(
            &self.root,
            piece,
            &self.select_rules,
            &self.piece_rules,
            hooks,
        )
        .map_err(|source| ArchiveError::Walk {
            piece: piece.to_string(),
            source,
        })
    }

    /// Without a piece named, only an idle one goes, and only a walk that
    /// refuses the piece is worth a word. A named piece goes however
    /// recently it changed; a pin or no counted file refuses it.
    fn judge(&self, tree: &PieceTree, now: Timestamp) -> Judged {
        let top = tree.top();
        let refused = |reasons: Vec<Reason>| {
            Judged::Refused(RefusedPiece {
                piece: top.to_string(),
                reasons,
            })
        };
        let walk_reasons = || -> Vec<Reason> {
            tree.problems()
                .iter()
                .map(|problem| Reason::from(&Refusal::of(top, problem)))
                .collect()
        };
        let marker = |file: &str| Reason::from(&Refusal::unreadable_marker(top, file));
        match idle::check(&self.root, tree, &self.settings, now) {
            Ok(check) => match check.verdict {
                IdleVerdict::Idle { .. } => Judged::Go,
                IdleVerdict::Active { .. } | IdleVerdict::Soon { .. } if self.named => Judged::Go,
                IdleVerdict::Pinned if self.named => {
                    let pin = Name::from_bytes(PIN_FILE.as_bytes().to_vec())
                        .expect("the pin file name is a name");
                    refused(vec![Reason::pinned(&top.join(&pin))])
                }
                IdleVerdict::Empty if self.named => refused(vec![Reason::empty(top)]),
                _ => Judged::Quiet,
            },
            Err(IdleError::Problems) => refused(walk_reasons()),
            Err(IdleError::Pin(_)) => {
                let mut reasons = walk_reasons();
                reasons.push(marker(PIN_FILE));
                refused(reasons)
            }
            Err(IdleError::Marker(_)) => refused(vec![marker(RESTORED_FILE)]),
        }
    }
}

// =========================================================
// The pieces of a real run
// =========================================================

/// What the pieces of a run share.
struct Shared<'a> {
    collection: &'a Collection,
    clock: &'a dyn Clock,
    env: &'a Env<'a>,
    setup: &'a Setup,
    now: Timestamp,
    ledger: SpaceLedger,
    /// Held while a chain below `tmp/pack/`, `trash/` or `doomed/` is
    /// created, renamed into or removed.
    run_dirs: Mutex<()>,
}

enum Finished {
    Archived(ArchivedPiece),
    Skipped(SkippedPiece),
    Refused(RefusedPiece),
    Quiet,
}

/// How a piece ended, and what it has to say on the way.
struct Done {
    finished: Finished,
    warnings: Vec<Warning>,
    /// The cause, when the walk ran out of file descriptors.
    exhausted: Option<String>,
}

impl Done {
    fn new(finished: Finished) -> Done {
        Done {
            finished,
            warnings: Vec::new(),
            exhausted: None,
        }
    }
}

/// Runs the pieces on as many threads as `env.parallel` allows. A walk that
/// runs out of file descriptors stops the pieces not yet started.
fn run_pieces(shared: &Shared<'_>) -> Vec<Done> {
    let pieces = &shared.setup.pieces;
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let slots: Vec<Mutex<Option<Done>>> = pieces.iter().map(|_| Mutex::new(None)).collect();
    let threads = shared.env.parallel.clamp(1, pieces.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while !stopped.load(Ordering::SeqCst) {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(piece) = pieces.get(index) else {
                        break;
                    };
                    let done = process(shared, piece);
                    if done.exhausted.is_some() {
                        stopped.store(true, Ordering::SeqCst);
                    }
                    *slots[index].lock().unwrap_or_else(PoisonError::into_inner) = Some(done);
                }
            });
        }
    });
    slots
        .into_iter()
        .zip(pieces)
        .map(|(slot, piece)| {
            let done = slot.into_inner().unwrap_or_else(PoisonError::into_inner);
            done.unwrap_or_else(|| {
                skipped(
                    piece,
                    SkipReason::Error,
                    "not started: the run ran out of file descriptors".to_string(),
                )
            })
        })
        .collect()
}

fn skipped(piece: &RelPath, reason: SkipReason, message: String) -> Done {
    Done::new(Finished::Skipped(SkippedPiece {
        piece: piece.to_string(),
        reason,
        message,
    }))
}

/// The error and the causes below it, as one line. A cause that the text
/// already ends with is not said twice: a message that quotes its source
/// has said it.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut link = error.source();
    while let Some(cause) = link {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        link = cause.source();
    }
    text
}

/// Steps 1 to 7 for one piece: walk and judge it again, store its version,
/// remove its folder.
fn process(shared: &Shared<'_>, piece: &RelPath) -> Done {
    let env = shared.env;
    let setup = shared.setup;
    let config = &shared.collection.config().collection;

    // The walk and the verdict are those of the moment the piece starts.
    let tree = match setup.walk(piece, env.seams.walk_hooks()) {
        Ok(tree) => tree,
        Err(error) => {
            let mut done = skipped(piece, SkipReason::Error, chain(&error));
            done.exhausted = Some(chain(&error));
            return done;
        }
    };
    match setup.judge(&tree, shared.now) {
        Judged::Go => {}
        Judged::Quiet => return Done::new(Finished::Quiet),
        Judged::Refused(refused) => return Done::new(Finished::Refused(refused)),
    }

    let stored_path = match stored_spelling_of(shared.collection, piece) {
        Ok(path) => path,
        Err(error) => return skipped(piece, SkipReason::Error, chain(&error)),
    };

    // =========================================================
    // Steps 2 to 4: the version
    // =========================================================

    let store_context = StoreContext {
        collection: shared.collection,
        clock: shared.clock,
        pool: env.pool,
        ledger: &shared.ledger,
        seams: env.seams,
        run_dirs: &shared.run_dirs,
        zstd: env.zstd,
    };
    let request = StoreRequest {
        live: piece,
        stored: &stored_path,
        tree: &tree,
        rules: &setup.piece_rules,
        imported: None,
    };
    let stored = match store_version(&store_context, &request) {
        Ok(stored) => stored,
        Err(StoreFailure::NoRoom(error)) => {
            return skipped(piece, SkipReason::NoRoom, chain(error.as_ref()));
        }
        Err(StoreFailure::Changed(error)) => {
            return skipped(piece, SkipReason::Differs, chain(error.as_ref()));
        }
        Err(StoreFailure::OutOfDescriptors(error)) => {
            let mut done = skipped(piece, SkipReason::Error, chain(error.as_ref()));
            done.exhausted = Some(chain(error.as_ref()));
            return done;
        }
        Err(StoreFailure::NotDurable(error) | StoreFailure::Failed(error)) => {
            return skipped(piece, SkipReason::Error, chain(error.as_ref()));
        }
    };

    // =========================================================
    // Steps 5 to 7: the folder
    // =========================================================

    let removal = remove_folder(
        &RemoveContext {
            collection: shared.collection,
            seams: env.seams,
            run_dirs: &shared.run_dirs,
        },
        &RemoveRequest {
            live: piece,
            stored: &stored_path,
            stamp: &stored.stamp,
            names: &stored.names,
            verified: &stored.verified,
            rules: &setup.piece_rules,
            exclude: &config.exclude,
        },
    );
    let mut done = match removal.outcome {
        Removed::Removed => Done::new(Finished::Archived(ArchivedPiece {
            piece: piece.to_string(),
            stamp: stored.stamp.to_string(),
            version: stored.stamp.time(),
            archive_bytes: stored.archive_bytes,
            files: stored.files,
            bytes: stored.bytes,
        })),
        Removed::Differs {
            destination,
            reason,
        } => skipped(
            piece,
            SkipReason::Differs,
            format!(
                "the folder changed ({reason}) and {}",
                went(destination, &stored.stamp, piece)
            ),
        ),
        Removed::OutOfDescriptors { destination, cause } => {
            let message = format!(
                "cannot compare the folder with its version ({cause}) and {}",
                went(destination, &stored.stamp, piece)
            );
            let mut done = skipped(piece, SkipReason::Error, message);
            done.exhausted = Some(cause);
            done
        }
        Removed::Kept(error) => skipped(piece, SkipReason::Error, chain(error.as_ref())),
    };
    done.warnings = stored.notes;
    done.warnings.extend(removal.warnings);
    done
}

/// Where the folder of a piece that changed after the verify ended up.
fn went(destination: Destination, stamp: &Stamp, piece: &RelPath) -> String {
    match destination {
        Destination::Live => "went back to its path".to_string(),
        Destination::Recovered => format!("went to recovered/{stamp}/{piece}"),
    }
}

/// The store folder of `piece` in the spelling the volume already holds.
fn stored_spelling_of(collection: &Collection, piece: &RelPath) -> Result<RelPath, ArchiveError> {
    let pieces_dir = collection.pieces_dir();
    let pieces = TreeRoot::open(&pieces_dir).map_err(|source| ArchiveError::OpenStore {
        path: pieces_dir.display().to_string(),
        source,
    })?;
    stored_spelling(&pieces, piece).map_err(|source| ArchiveError::Spelling {
        piece: piece.to_string(),
        source,
    })
}

/// Why archiving a piece failed. Each variant names the step and the
/// place, and carries the cause as its source.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("cannot start the run")]
    Run(#[from] RunError),
    #[error("cannot resolve the piece")]
    Reference(#[from] ReferenceError),
    #[error("the piece to archive is empty")]
    EmptyReference,
    #[error("cannot open the collection {path}")]
    Open {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot select the pieces")]
    Select(#[from] SelectError),
    #[error("cannot read the ignore and timeless rules")]
    Rules(#[from] RulesError),
    #[error("cannot size the pool of workers")]
    Pool(#[source] SysmemError),
    #[error("cannot open {path}")]
    OpenStore {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot look for the stored spelling of {piece}")]
    Spelling {
        piece: String,
        #[source]
        source: ReferenceError,
    },
    #[error("cannot walk {piece}")]
    Walk {
        piece: String,
        #[source]
        source: WalkError,
    },
    #[error("{piece} needs {needed} bytes in .antiq/ and {available} are free")]
    NoRoom {
        piece: String,
        needed: u64,
        available: u64,
    },
    #[error("cannot read the free space of .antiq/")]
    Space(#[source] FilesystemError),
    #[error("cannot choose a version stamp for {piece}")]
    Stamp {
        piece: String,
        #[source]
        source: Box<VersionsError>,
    },
    #[error("cannot prepare tmp/pack/{stamp}/{piece}")]
    Stage {
        piece: String,
        stamp: String,
        #[source]
        source: FilesystemError,
    },
    #[error("cannot pack {piece} into tmp/pack/{stamp}")]
    Pack {
        piece: String,
        stamp: String,
        #[source]
        source: PackError,
    },
    #[error("cannot write {file} in tmp/pack/{stamp}/{piece}")]
    Write {
        piece: String,
        stamp: String,
        file: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write the listing of {piece} in tmp/pack/{stamp}")]
    Listing {
        piece: String,
        stamp: String,
        #[source]
        source: ListingError,
    },
    #[error("cannot make {file} in tmp/pack/{stamp}/{piece} durable")]
    Flush {
        piece: String,
        stamp: String,
        file: String,
        #[source]
        source: Box<FilesystemError>,
    },
    #[error("cannot verify the packing of {piece} in tmp/pack/{stamp}")]
    Verify {
        piece: String,
        stamp: String,
        #[source]
        source: PackError,
    },
    #[error("cannot move the version {stamp} of {piece} into the store")]
    MoveIn {
        piece: String,
        stamp: String,
        #[source]
        source: Box<VersionsError>,
    },
    #[error("cannot move {piece} into trash/{stamp}")]
    Trash {
        piece: String,
        stamp: String,
        #[source]
        source: FilesystemError,
    },
    #[error(
        "cannot put the version {stamp} of {piece} back into tmp/pack after it failed ({cause}), so it stays in the store"
    )]
    Rollback {
        piece: String,
        stamp: String,
        cause: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("cannot give {piece} back from trash/{stamp} after it changed ({reason})")]
    GiveBack {
        piece: String,
        stamp: String,
        reason: String,
        #[source]
        source: Box<RepairError>,
    },
    #[error("cannot move {piece} into doomed/{stamp}")]
    Doomed {
        piece: String,
        stamp: String,
        #[source]
        source: FilesystemError,
    },
    #[error("cannot move {piece} into doomed/{stamp}, so the folder {went}")]
    DoomedRefused {
        piece: String,
        stamp: String,
        went: String,
        #[source]
        source: Box<FilesystemError>,
    },
    #[error("cannot delete doomed/{stamp}/{piece}")]
    Delete {
        piece: String,
        stamp: String,
        #[source]
        source: FilesystemError,
    },
}

impl HasKind for ArchiveError {
    fn kind(&self) -> ErrorKind {
        match self {
            ArchiveError::Run(error) => error.kind(),
            ArchiveError::Reference(error) => error.kind(),
            ArchiveError::EmptyReference => ErrorKind::Usage,
            _ => ErrorKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::commands::survey::{Verdict, survey};
    use crate::error::Error;
    use crate::filesystem::tree::TreeRoot;
    use crate::plan::Step;
    use crate::relpath::RelPath;
    use crate::report::CommandReport;
    use crate::testing::shared_pool;
    use crate::testing::tree::{COLLECTION_ID, TempCollection, snapshot};
    use crate::walk::Call;
    use jiff::Timestamp;
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    const WHEN: &str = "2026-11-04T09:02:11Z";
    const DAY: i64 = 86_400;

    fn now() -> Timestamp {
        WHEN.parse().expect("a time")
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn fast(_workers: u32) -> ZstdSettings {
        ZstdSettings::fast()
    }

    type Change = Box<dyn Fn(&Path) + Send + Sync>;

    /// What a test changes about the real run.
    #[derive(Default)]
    struct Probe {
        /// The first walk of a piece runs out of file descriptors.
        exhausts_first_walk: bool,
        /// The walk of the trashed tree runs out of file descriptors.
        exhausts_rewalk: bool,
        tripped: std::sync::atomic::AtomicBool,
        /// Set once a folder was renamed into `trash/`.
        trashed: std::sync::atomic::AtomicBool,
        /// The bytes free in `.antiq/`.
        free: Option<u64>,
        failing_delete: bool,
        /// Runs after the folder was renamed into `trash/`.
        after_trash_rename: Option<Change>,
    }

    impl Hooks for Probe {
        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            let first = call == Call::Stat
                && path.is_root()
                && self.exhausts_first_walk
                && !self.tripped.swap(true, Ordering::SeqCst);
            let rewalk = call == Call::Stat
                && path.is_root()
                && self.exhausts_rewalk
                && self.trashed.load(Ordering::SeqCst);
            (first || rewalk).then(|| io::Error::from_raw_os_error(libc::EMFILE))
        }
    }

    impl Seams for Probe {
        fn free(&self, dir: &Path) -> Result<u64, FilesystemError> {
            match self.free {
                Some(free) => Ok(free),
                None => RealSeams.free(dir),
            }
        }

        fn walk_hooks(&self) -> &dyn Hooks {
            self
        }

        fn after_trash_rename(&self, tree: &Path) {
            self.trashed.store(true, Ordering::SeqCst);
            if let Some(change) = &self.after_trash_rename {
                change(tree);
            }
        }

        fn remove(&self, root: &TreeRoot, path: &RelPath) -> Result<(), FilesystemError> {
            if self.failing_delete {
                return Err(crate::filesystem::io_error("remove", Path::new("doomed"))(
                    io::Error::from_raw_os_error(libc::EACCES),
                ));
            }
            RealSeams.remove(root, path)
        }
    }

    fn world(extra: &str) -> TempCollection {
        let temp = TempCollection::minimal();
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{extra}\n"),
        )
        .expect("the config is written");
        temp
    }

    /// A piece of two files, the newest `age_days` old.
    fn add(world: &mut TempCollection, piece: &str, age_days: i64) {
        let mtime = now().as_second() - age_days * DAY;
        world
            .tree()
            .file_with(&format!("{piece}/a.txt"), "alpha", 0o644, mtime);
        world
            .tree()
            .file_with(&format!("{piece}/sub/b.txt"), "beta", 0o600, mtime);
    }

    fn collection(world: &TempCollection) -> Collection {
        Collection::find(world.root()).expect("a collection")
    }

    fn named(world: &TempCollection, reference: &str) -> Target {
        Target::Piece {
            reference: reference.into(),
            cwd: world.root().to_path_buf(),
        }
    }

    fn execute_with(
        world: &TempCollection,
        target: &Target,
        seams: &dyn Seams,
        parallel: usize,
    ) -> Result<ArchiveReport, ArchiveError> {
        let collection = collection(world);
        let clock = FixedClock::new(now());
        let Started::Guarded(guard) = run::start(&collection, &clock, false).expect("starts")
        else {
            panic!("a real run is guarded");
        };
        let env = Env {
            seams,
            pool: shared_pool(),
            zstd: &fast,
            parallel,
        };
        execute(&collection, &clock, target, &guard, &env)
    }

    fn run_all(world: &TempCollection) -> ArchiveReport {
        execute_with(world, &Target::Due, &RealSeams, 2).expect("the run ends")
    }

    fn plan_of(world: &TempCollection, target: &Target) -> ArchivePlan {
        let collection = collection(world);
        let clock = FixedClock::new(now());
        let request = Request {
            target: target.clone(),
            dry_run: true,
        };
        match archive(&collection, &clock, &request).expect("the dry run ends") {
            Outcome::Planned(plan) => plan,
            other => panic!("a dry run plans, not {other:?}"),
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = match fs::read_dir(dir) {
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
        names.sort();
        names
    }

    fn antiq(world: &TempCollection, below: &str) -> std::path::PathBuf {
        world.root().join(".antiq").join(below)
    }

    fn kinds(refused: &RefusedPiece) -> Vec<&'static str> {
        refused.reasons.iter().map(|reason| reason.kind).collect()
    }

    fn pieces_of(report: &ArchiveReport) -> Vec<&str> {
        report
            .archived
            .iter()
            .map(|item| item.piece.as_str())
            .collect()
    }

    fn assert_no_leftovers(world: &TempCollection) {
        for below in ["trash", "doomed", "tmp/pack"] {
            assert_eq!(
                entries(&antiq(world, below)),
                Vec::<String>::new(),
                "{below}"
            );
        }
    }

    // =========================================================
    // The pipeline in all mode
    // =========================================================

    #[test]
    fn an_idle_piece_ends_as_three_files_and_no_folder() {
        let mut world = world("");
        add(&mut world, "piece", 40);

        let report = run_all(&world);

        assert_eq!(pieces_of(&report), ["piece"]);
        let archived = &report.archived[0];
        assert_eq!(archived.stamp, "2026-11-04T090211");
        assert_eq!(archived.version, now());
        assert_eq!(archived.files, 2);
        assert_eq!(archived.bytes, 9);
        assert!(archived.archive_bytes > 0);
        assert!(report.skipped.is_empty() && report.refused.is_empty());
        assert_eq!(report.exit_code(), 0);
        assert!(!world.root().join("piece").exists());
        assert_eq!(entries(&antiq(&world, "pieces/piece")).len(), 3);
        assert_no_leftovers(&world);
    }

    #[test]
    fn a_piece_that_is_not_due_is_left_alone_and_not_reported() {
        let mut world = world("");
        add(&mut world, "recent", 1);
        add(&mut world, "soon", 29);

        let report = run_all(&world);

        assert!(report.archived.is_empty() && report.skipped.is_empty());
        assert!(report.refused.is_empty());
        assert!(world.root().join("recent/a.txt").exists());
        assert!(world.root().join("soon/a.txt").exists());
    }

    #[test]
    fn the_verdicts_match_those_of_the_survey() {
        let mut world = world("");
        add(&mut world, "due", 40);
        add(&mut world, "soon", 29);
        add(&mut world, "pinned", 40);
        fs::write(world.root().join("pinned/.antiq-pin"), "").expect("the pin is written");
        add(&mut world, "refused", 40);
        fs::create_dir_all(world.root().join("refused/inner/.antiq")).expect("a nested collection");
        fs::create_dir(world.root().join("empty")).expect("a folder");
        fs::write(world.root().join("empty/.DS_Store"), "").expect("an ignored file");
        let surveyed = survey(&collection(&world), now()).expect("the survey ends");
        let due: Vec<String> = surveyed
            .pieces
            .iter()
            .filter(|item| item.verdict == Verdict::Due)
            .map(|item| item.piece.to_string())
            .collect();
        let refused: Vec<String> = surveyed
            .pieces
            .iter()
            .filter(|item| matches!(item.verdict, Verdict::Refused(_)))
            .map(|item| item.piece.to_string())
            .collect();
        assert_eq!(
            (due.as_slice(), refused.as_slice()),
            (&["due".to_string()][..], &["refused".to_string()][..])
        );

        let report = run_all(&world);

        assert_eq!(pieces_of(&report), due);
        let named: Vec<&str> = report
            .refused
            .iter()
            .map(|item| item.piece.as_str())
            .collect();
        assert_eq!(named, refused);
        assert_eq!(kinds(&report.refused[0]), ["nested_collection"]);
        assert_eq!(report.exit_code(), 0, "refusals do not change the code");
    }

    #[test]
    fn pieces_below_one_parent_archive_in_parallel() {
        let mut world = world("patterns = [\"*/*\"]");
        for piece in ["group/one", "group/two", "group/three", "solo/four"] {
            add(&mut world, piece, 40);
        }

        let report = execute_with(&world, &Target::Due, &RealSeams, 4).expect("the run ends");

        assert_eq!(report.archived.len(), 4, "{:?}", report.skipped);
        assert_no_leftovers(&world);
        assert!(
            !world.root().join("group").exists() || entries(&world.root().join("group")).is_empty()
        );
    }

    #[test]
    fn a_stored_folder_in_another_spelling_is_the_one_archived_into() {
        let mut world = world("");
        add(&mut world, "Piece", 40);
        let folds_case = world.root().join("PIECE").exists();
        assert!(
            folds_case || cfg!(not(target_os = "macos")),
            "macOS volumes of the tests fold case"
        );
        if !folds_case {
            return;
        }
        run_all(&world);
        add(&mut world, "piece", 40);

        let report = run_all(&world);

        assert_eq!(pieces_of(&report), ["piece"], "{:?}", report.skipped);
        assert_eq!(entries(&antiq(&world, "pieces")), ["Piece"]);
        assert_eq!(entries(&antiq(&world, "pieces/Piece")).len(), 6);
    }

    // =========================================================
    // Skips
    // =========================================================

    #[test]
    fn a_piece_without_room_is_skipped_and_leaves_nothing() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let seams = Probe {
            free: Some(3),
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        assert!(report.archived.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::NoRoom);
        assert!(
            report.skipped[0].message.contains("needs 9 bytes"),
            "{:?}",
            report.skipped[0]
        );
        assert_eq!(report.exit_code(), 3);
        assert!(world.root().join("piece/a.txt").exists());
        assert_no_leftovers(&world);
    }

    #[test]
    fn a_folder_that_changes_after_the_verify_goes_back_and_is_skipped_as_differs() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let seams = Probe {
            after_trash_rename: Some(Box::new(|tree| {
                fs::write(tree.join("a.txt"), "changed later").expect("the file is written");
            })),
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        assert_eq!(report.skipped.len(), 1, "{report:?}");
        let skipped = &report.skipped[0];
        assert_eq!(skipped.reason, SkipReason::Differs);
        assert!(
            skipped.message.contains("a.txt") && skipped.message.contains("went back"),
            "{skipped:?}"
        );
        assert_eq!(
            fs::read_to_string(world.root().join("piece/a.txt")).expect("back"),
            "changed later"
        );
        assert_eq!(
            entries(&antiq(&world, "pieces/piece")).len(),
            3,
            "the version stays"
        );
        assert!(report.partial());
    }

    #[test]
    fn a_folder_whose_path_is_taken_goes_to_recovered_and_the_message_says_so() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let root = world.root().to_path_buf();
        let seams = Probe {
            after_trash_rename: Some(Box::new(move |tree| {
                fs::write(tree.join("a.txt"), "changed later").expect("the file is written");
                fs::create_dir(root.join("piece")).expect("the path is taken");
            })),
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        let skipped = &report.skipped[0];
        assert_eq!(skipped.reason, SkipReason::Differs);
        assert!(
            skipped
                .message
                .contains("recovered/2026-11-04T090211/piece"),
            "{skipped:?}"
        );
    }

    #[test]
    fn a_failed_deletion_is_skipped_as_error_and_the_next_run_finishes_it() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let seams = Probe {
            failing_delete: true,
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        let skipped = &report.skipped[0];
        assert_eq!(skipped.reason, SkipReason::Error);
        assert!(
            skipped
                .message
                .starts_with("cannot delete doomed/2026-11-04T090211/piece"),
            "{skipped:?}"
        );
        assert!(antiq(&world, "doomed/2026-11-04T090211/piece/a.txt").exists());

        let second = run_all(&world);
        assert!(
            second
                .repairs
                .iter()
                .any(|repair| format!("{repair:?}").contains("Doomed")),
            "{:?}",
            second.repairs
        );
        assert_no_leftovers(&world);
    }

    #[test]
    fn the_chain_of_causes_does_not_repeat_what_a_message_already_shows() {
        let error = ArchiveError::Delete {
            piece: "piece".into(),
            stamp: "2026-11-04T090211".into(),
            source: crate::filesystem::io_error("remove", Path::new("doomed"))(
                io::Error::from_raw_os_error(libc::EACCES),
            ),
        };

        assert_eq!(
            chain(&error),
            "cannot delete doomed/2026-11-04T090211/piece: cannot remove doomed: Permission denied (os error 13)"
        );
    }

    #[test]
    fn running_out_of_descriptors_skips_the_piece_and_the_rest_with_one_warning() {
        let mut world = world("");
        for piece in ["a", "b", "c"] {
            add(&mut world, piece, 40);
        }
        let seams = Probe {
            exhausts_first_walk: true,
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        assert!(report.archived.is_empty());
        let skipped: Vec<(&str, SkipReason)> = report
            .skipped
            .iter()
            .map(|item| (item.piece.as_str(), item.reason))
            .collect();
        assert_eq!(
            skipped,
            [
                ("a", SkipReason::Error),
                ("b", SkipReason::Error),
                ("c", SkipReason::Error)
            ]
        );
        let warnings: Vec<&str> = report
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .filter(|text| text.contains("file descriptors"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", report.warnings);
        assert_eq!(report.exit_code(), 3);
        assert!(world.root().join("c/a.txt").exists());
    }

    #[test]
    fn running_out_of_descriptors_in_the_rewalk_gives_the_folder_back_and_stops_the_run() {
        let mut world = world("");
        for piece in ["a", "b"] {
            add(&mut world, piece, 40);
        }
        let seams = Probe {
            exhausts_rewalk: true,
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        assert!(report.archived.is_empty());
        let skipped: Vec<(&str, SkipReason)> = report
            .skipped
            .iter()
            .map(|item| (item.piece.as_str(), item.reason))
            .collect();
        assert_eq!(
            skipped,
            [("a", SkipReason::Error), ("b", SkipReason::Error)]
        );
        assert!(
            report.skipped[0].message.ends_with("went back to its path"),
            "{:?}",
            report.skipped[0]
        );
        assert!(report.skipped[1].message.starts_with("not started"));
        let warnings = report
            .warnings
            .iter()
            .filter(|warning| warning.message.contains("file descriptors"))
            .count();
        assert_eq!(warnings, 1, "{:?}", report.warnings);
        assert_eq!(report.exit_code(), 3);
        assert!(world.root().join("a/a.txt").exists());
        assert_eq!(
            entries(&antiq(&world, "pieces/a")).len(),
            3,
            "the version stays"
        );
    }

    // =========================================================
    // A named piece
    // =========================================================

    #[test]
    fn a_named_piece_is_archived_however_recently_it_changed() {
        let mut world = world("");
        add(&mut world, "piece", 1);
        add(&mut world, "other", 1);

        let report = execute_with(&world, &named(&world, "piece"), &RealSeams, 2).expect("ends");

        assert_eq!(pieces_of(&report), ["piece"]);
        assert!(world.root().join("other/a.txt").exists());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn a_search_word_that_matches_one_piece_archives_it() {
        let mut world = world("patterns = [\"*/*\"]");
        add(&mut world, "projects/scanner", 1);
        add(&mut world, "projects/mailer", 1);

        let report = execute_with(&world, &named(&world, "scan"), &RealSeams, 2).expect("ends");

        assert_eq!(pieces_of(&report), ["projects/scanner"]);
    }

    #[test]
    fn a_named_pinned_piece_is_refused_with_exit_1() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        fs::write(world.root().join("piece/.antiq-pin"), "").expect("the pin is written");

        let report = execute_with(&world, &named(&world, "piece"), &RealSeams, 1).expect("ends");

        assert!(report.archived.is_empty() && report.skipped.is_empty());
        assert_eq!(kinds(&report.refused[0]), ["pinned"]);
        assert_eq!(report.refused[0].reasons[0].path, "piece/.antiq-pin");
        assert_eq!(report.exit_code(), 1);
        assert!(world.root().join("piece/a.txt").exists());
    }

    #[test]
    fn a_named_piece_without_a_counted_file_is_refused_as_empty_with_exit_1() {
        let world = world("");
        fs::create_dir(world.root().join("piece")).expect("a folder");
        fs::write(world.root().join("piece/.DS_Store"), "").expect("an ignored file");

        let report = execute_with(&world, &named(&world, "piece"), &RealSeams, 1).expect("ends");

        assert_eq!(kinds(&report.refused[0]), ["empty"]);
        assert_eq!(report.refused[0].reasons[0].path, "piece");
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn a_named_piece_the_walk_refuses_is_refused_with_exit_1() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        fs::create_dir_all(world.root().join("piece/inner/.antiq")).expect("a nested collection");

        let report = execute_with(&world, &named(&world, "piece"), &RealSeams, 1).expect("ends");

        assert_eq!(kinds(&report.refused[0]), ["nested_collection"]);
        assert_eq!(report.refused[0].reasons[0].path, "piece/inner");
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn a_named_piece_with_a_broken_marker_is_refused_as_unreadable_marker() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        fs::write(world.root().join("piece/.antiq-restored"), "not a time\n").expect("a marker");

        let report = execute_with(&world, &named(&world, "piece"), &RealSeams, 1).expect("ends");

        assert_eq!(kinds(&report.refused[0]), ["unreadable_marker"]);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn a_pin_that_is_a_folder_is_refused_as_not_a_file() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        fs::create_dir(world.root().join("piece/.antiq-pin")).expect("a pin that is a folder");

        let report = execute_with(&world, &Target::Due, &RealSeams, 1).expect("ends");

        let refused = &report.refused[0];
        assert_eq!(refused.piece, "piece");
        assert_eq!(kinds(refused), ["not_a_file"], "{refused:?}");
        assert_eq!(refused.reasons[0].path, "piece/.antiq-pin");
        assert!(world.root().join("piece/a.txt").exists());
    }

    #[test]
    fn a_restore_marker_of_yesterday_keeps_an_old_piece_from_being_due() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let yesterday = now().as_second() - DAY;
        let marker = Timestamp::from_second(yesterday).expect("a time");
        fs::write(
            world.root().join("piece/.antiq-restored"),
            format!("restored {}\n", marker.strftime("%Y-%m-%dT%H:%M:%SZ")),
        )
        .expect("the marker is written");

        let report = execute_with(&world, &Target::Due, &RealSeams, 1).expect("ends");

        assert!(report.archived.is_empty() && report.refused.is_empty());
        assert!(world.root().join("piece/a.txt").exists());
    }

    #[test]
    fn two_matches_are_an_ambiguous_reference_with_the_candidates() {
        let mut world = world("patterns = [\"*/*\"]");
        add(&mut world, "projects/scanner", 1);
        add(&mut world, "projects/scan-old", 1);

        let error =
            execute_with(&world, &named(&world, "scan"), &RealSeams, 1).expect_err("ambiguous");

        assert_eq!(error.kind(), ErrorKind::AmbiguousReference);
        let details = Error::from(error).details();
        let candidates = details["candidates"].as_array().expect("candidates").len();
        assert_eq!(candidates, 2);
    }

    #[test]
    fn no_match_is_an_unknown_reference() {
        let mut world = world("");
        add(&mut world, "piece", 1);

        let error =
            execute_with(&world, &named(&world, "nothing"), &RealSeams, 1).expect_err("unknown");

        assert_eq!(error.kind(), ErrorKind::UnknownReference);
    }

    #[test]
    fn an_empty_argument_is_a_usage_error() {
        let world = world("");
        let request = Request {
            target: named(&world, ""),
            dry_run: false,
        };

        let error =
            archive(&collection(&world), &FixedClock::new(now()), &request).expect_err("usage");

        assert!(matches!(error, ArchiveError::EmptyReference));
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    // =========================================================
    // The lock and the repairs
    // =========================================================

    #[test]
    fn a_held_lock_fails_a_real_run_with_the_holder_in_the_details() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let _held = crate::testing::hold_lock(world.root());
        let request = Request {
            target: Target::Due,
            dry_run: false,
        };

        let error =
            archive(&collection(&world), &FixedClock::new(now()), &request).expect_err("held");

        assert_eq!(error.kind(), ErrorKind::LockHeld);
        assert!(std::error::Error::source(&error).is_some_and(|source| source.is::<RunError>()));
        let error = Error::from(error);
        assert_eq!(error.kind(), ErrorKind::LockHeld);
        let details = error.details();
        assert_eq!(details["pid"], std::process::id());
        assert_eq!(details["started"], "2026-11-04T10:00:00Z");
        assert!(world.root().join("piece/a.txt").exists());
    }

    #[test]
    fn a_held_lock_in_a_dry_run_names_the_holder_and_lists_nothing() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let _held = crate::testing::hold_lock(world.root());
        let request = Request {
            target: Target::Due,
            dry_run: true,
        };

        let outcome =
            archive(&collection(&world), &FixedClock::new(now()), &request).expect("ends");

        let Outcome::Held(held) = outcome else {
            panic!("the lock is held");
        };
        assert!(held.lines()[0].contains(&format!("pid {}", std::process::id())));
    }

    #[test]
    fn the_repairs_of_the_start_are_in_the_report() {
        let world = world("");
        fs::create_dir_all(antiq(&world, "doomed/2026-11-03T000000/old")).expect("a leftover");
        fs::write(antiq(&world, "doomed/2026-11-03T000000/old/f"), "x").expect("a file");

        let report = run_all(&world);

        assert_eq!(report.repairs.len(), 2, "{:?}", report.repairs);
        assert!(entries(&antiq(&world, "doomed")).is_empty());
        let json = serde_json::to_value(&report).expect("serialises");
        assert_eq!(json["repairs"][0]["kind"], "repair");
        assert_eq!(json["repairs"][0]["repair"], "delete_doomed");
    }

    // =========================================================
    // The dry run
    // =========================================================

    #[test]
    fn a_dry_run_lists_the_repairs_then_the_archives_then_the_refusals_and_changes_nothing() {
        let mut world = world("");
        add(&mut world, "due", 40);
        add(&mut world, "refused", 40);
        fs::create_dir_all(world.root().join("refused/inner/.antiq")).expect("a nested collection");
        fs::create_dir_all(antiq(&world, "doomed/2026-11-03T000000/old")).expect("a leftover");
        fs::write(antiq(&world, "doomed/2026-11-03T000000/old/f"), "x").expect("a file");
        let before = snapshot(world.root());

        let plan = plan_of(&world, &Target::Due);

        assert!(matches!(plan.actions[0], Step::Repair(_)));
        assert!(
            matches!(&plan.actions[2], Step::Command(ArchiveAction::Archive { piece }) if piece == "due")
        );
        assert!(matches!(plan.actions[1], Step::Repair(_)));
        assert_eq!(plan.actions.len(), 3);
        let lines = plan.lines();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[0].starts_with("Would "), "{lines:?}");
        assert_eq!(lines[2], "Would archive due");
        assert!(
            lines[3].starts_with("Refused refused: nested collection: refused/inner"),
            "{lines:?}"
        );
        assert_eq!(plan.exit_code(), 0);
        assert_eq!(snapshot(world.root()), before);
    }

    #[test]
    fn a_dry_run_with_nothing_to_do_says_so() {
        let mut world = world("");
        add(&mut world, "recent", 1);

        let plan = plan_of(&world, &Target::Due);

        assert_eq!(plan.lines(), ["Nothing to do."]);
    }

    #[test]
    fn a_dry_run_of_a_named_pinned_piece_exits_1_and_names_the_reason() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        fs::write(world.root().join("piece/.antiq-pin"), "").expect("the pin is written");

        let plan = plan_of(&world, &named(&world, "piece"));

        assert_eq!(plan.lines(), ["Refused piece: pinned: piece/.antiq-pin"]);
        assert_eq!(plan.exit_code(), 1);
    }

    // =========================================================
    // Report and errors
    // =========================================================

    #[test]
    fn the_report_serialises_with_the_keys_of_the_spec() {
        let mut world = world("");
        add(&mut world, "due", 40);
        add(&mut world, "refused", 40);
        fs::create_dir_all(world.root().join("refused/inner/.antiq")).expect("a nested collection");

        let report = run_all(&world);

        let json = serde_json::to_value(&report).expect("serialises");
        let archived = &json["archived"][0];
        assert_eq!(archived["piece"], "due");
        assert_eq!(archived["stamp"], "2026-11-04T090211");
        assert_eq!(archived["version"], "2026-11-04T09:02:11Z");
        for key in ["archive_bytes", "files", "bytes"] {
            assert!(archived[key].is_number(), "{key}");
        }
        assert_eq!(json["skipped"], serde_json::json!([]));
        let reason = &json["refused"][0]["reasons"][0];
        assert_eq!(reason["kind"], "nested_collection");
        assert_eq!(reason["path"], "refused/inner");
        assert_eq!(reason["text"], "nested collection: refused/inner");
        assert!(reason.get("line").is_none());
        assert!(json["repairs"].is_array() && json["warnings"].is_array());
        assert!(json.get("named").is_none());
    }

    #[test]
    fn the_dry_run_serialises_with_the_keys_of_the_spec() {
        let mut world = world("");
        add(&mut world, "due", 40);

        let json = serde_json::to_value(plan_of(&world, &Target::Due)).expect("serialises");

        assert_eq!(json["dry_run"], true);
        assert_eq!(
            json["actions"],
            serde_json::json!([{"kind": "archive", "piece": "due"}])
        );
        assert_eq!(json["refused"], serde_json::json!([]));
        assert!(json["warnings"].is_array());
    }

    #[test]
    fn a_skipped_piece_makes_the_report_partial_and_the_code_3() {
        let report = ArchiveReport {
            archived: Vec::new(),
            skipped: vec![SkippedPiece {
                piece: "piece".into(),
                reason: SkipReason::Error,
                message: "cannot pack".into(),
            }],
            refused: vec![RefusedPiece {
                piece: "other".into(),
                reasons: vec![Reason::empty(&path("other"))],
            }],
            repairs: Vec::new(),
            warnings: Vec::new(),
            named: true,
        };

        assert!(report.partial());
        assert_eq!(report.exit_code(), 3);
    }

    #[test]
    fn each_error_has_its_kind_and_names_its_cause_as_the_source() {
        let reference = ArchiveError::from(ReferenceError::Unknown {
            reference: "x".into(),
        });
        assert_eq!(reference.kind(), ErrorKind::UnknownReference);
        assert!(
            std::error::Error::source(&reference)
                .is_some_and(|source| source.is::<ReferenceError>())
        );
        assert_eq!(reference.to_string(), "cannot resolve the piece");
        assert_eq!(
            ArchiveError::EmptyReference.to_string(),
            "the piece to archive is empty"
        );
        let walk = ArchiveError::Walk {
            piece: "piece".into(),
            source: WalkError::OutOfDescriptors {
                piece: path("piece"),
                path: path("piece"),
                source: io::Error::from_raw_os_error(libc::EMFILE),
            },
        };
        assert_eq!(walk.to_string(), "cannot walk piece");
        assert_eq!(walk.kind(), ErrorKind::Other);
        let open = ArchiveError::Open {
            path: "/x".into(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert_eq!(open.to_string(), "cannot open the collection /x");
    }
}
