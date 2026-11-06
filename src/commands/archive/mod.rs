// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` operation.

mod remove;
mod report;
pub(crate) mod seams;
pub(crate) mod store;

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
use crate::error::{ErrorKind, HasKind, chain_text};
use crate::filesystem::FilesystemError;
use crate::filesystem::space::SpaceLedger;
use crate::filesystem::tree::TreeRoot;
use crate::idle::{self, IdleError, IdleVerdict};
use crate::listing::ListingError;
use crate::markers::RESTORED_FILE;
use crate::pin::PIN_FILE;
use crate::plan::HeldDryRun;
use crate::plan::Step;
use crate::progress::{Event, Observer, Outcome as Progress, Planned, Silent, Tracker};
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
use crate::tree::{EntryKind, PieceTree, TreeEntry};
use crate::versions::VersionsError;
use crate::versions::stamp::Stamp;
use crate::walk::walk_piece_with;
use crate::walk::{Hooks, RealFs, WalkError};
use jiff::Timestamp;
use remove::{Outcome as Removed, RemoveContext, RemoveRequest, remove_folder};
use seams::{RealSeams, Seams};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use store::{Folder, StoreContext, StoreFailure, StoreRequest, apparent_size, store_version};

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
    /// Told what the run does, when someone watches it.
    pub observer: Option<&'a dyn Observer>,
}

/// Archives the pieces `request` names, or lists what it would do.
pub fn archive(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
) -> Result<Outcome, ArchiveError> {
    run_archive(collection, clock, request, None)
}

/// [`archive`], telling `observer` what a real run does. A dry run has
/// nothing to tell.
pub fn archive_observed(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
    observer: &dyn Observer,
) -> Result<Outcome, ArchiveError> {
    run_archive(collection, clock, request, Some(observer))
}

fn run_archive(
    collection: &Collection,
    clock: &dyn Clock,
    request: &Request,
    observer: Option<&dyn Observer>,
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
                observer,
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
    notes.extend(setup.unreadable_notes());
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
    let (done, looked_out) = run_pieces(&shared);

    let mut report = ArchiveReport {
        archived: Vec::new(),
        skipped: Vec::new(),
        refused: Vec::new(),
        repairs: guard.repaired().iter().cloned().map(Step::Repair).collect(),
        warnings: setting_notes(collection),
        named: setup.named,
    };
    report.warnings.extend(setup.unreadable_notes());
    report.warnings.extend(guard.warnings().iter().cloned());
    let mut exhausted = looked_out;
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
    /// The directories selection had to look into and could not read.
    unreadable: Vec<RelPath>,
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
            unreadable: selection.unreadable().to_vec(),
        })
    }

    /// The warnings that name the directories selection could not read.
    fn unreadable_notes(&self) -> Vec<Warning> {
        self.unreadable
            .iter()
            .map(|directory| Warning::new(format!("unreadable: {directory}")))
            .collect()
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
/// runs out of file descriptors stops the pieces not yet started. The cause
/// comes back when that happens before any piece has started, in the look for
/// shared files.
///
/// Pieces that hold names of one hard-linked file run one after the other, in
/// path order, on one thread. Deleting the first changes the link count and so
/// the change time of the file, which the second would take for a change by
/// someone else between its verify and its re-walk. Run after the first has
/// ended, the second records the file as the deletion left it.
fn run_pieces(shared: &Shared<'_>) -> (Vec<Done>, Option<String>) {
    let pieces = &shared.setup.pieces;
    let looks = match look_at_pieces(shared) {
        Ok(looks) => looks,
        Err(cause) => {
            let done = pieces.iter().map(not_started).collect();
            return (done, Some(cause));
        }
    };
    let (links, expected): (Vec<PieceLinks>, Vec<Option<Planned>>) = looks
        .into_iter()
        .map(|look| (look.links, look.planned))
        .unzip();
    let announced: Vec<Planned> = expected.iter().flatten().cloned().collect();
    if let Some(observer) = shared.env.observer
        && !announced.is_empty()
    {
        observer.event(Event::Planned { items: &announced });
    }
    let groups = share_groups(&links);
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let slots: Vec<Mutex<Option<Done>>> = pieces.iter().map(|_| Mutex::new(None)).collect();
    let threads = shared.env.parallel.clamp(1, groups.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(group) = groups.get(next.fetch_add(1, Ordering::SeqCst)) {
                    for &index in group {
                        if stopped.load(Ordering::SeqCst) {
                            return;
                        }
                        let done = process(shared, &pieces[index], expected[index].is_some());
                        if done.exhausted.is_some() {
                            stopped.store(true, Ordering::SeqCst);
                        }
                        *slots[index].lock().unwrap_or_else(PoisonError::into_inner) = Some(done);
                    }
                }
            });
        }
    });
    let done = slots
        .into_iter()
        .zip(pieces)
        .map(|(slot, piece)| {
            let done = slot.into_inner().unwrap_or_else(PoisonError::into_inner);
            done.unwrap_or_else(|| not_started(piece))
        })
        .collect();
    (done, None)
}

fn not_started(piece: &RelPath) -> Done {
    skipped(
        piece,
        SkipReason::Error,
        "not started: the run ran out of file descriptors".to_string(),
    )
}

/// What identifies a file on disk: its device and inode.
type FileId = (u64, u64);

/// The files of one piece that have more than one name.
#[derive(Debug, Default)]
struct PieceLinks {
    /// Files with a name that goes into the archive.
    packed: HashSet<FileId>,
    /// Files all of whose names in the piece are ignored.
    ignored: HashSet<FileId>,
}

/// What the look at a piece before the run found: its linked files, and
/// the piece with its size when the run expects to work on it.
#[derive(Default)]
struct Look {
    links: PieceLinks,
    planned: Option<Planned>,
}

/// The linked files of each piece, or the cause when a walk ran out of file
/// descriptors, which ends the run like the walk of a piece does. With an
/// observer, each piece is judged on this walk as well, so the run can tell
/// beforehand which pieces it expects to work on.
///
/// No walk before this one reaches into the pieces, and the walk of a piece
/// belongs to the moment it starts, so this one only keeps what it needs
/// and drops the tree. It is skipped for fewer than two pieces, which cannot
/// share a file and are not announced. Running out of descriptors is the
/// only way such a walk fails.
///
/// The verdict is advisory: the piece is judged again when it starts.
fn look_at_pieces(shared: &Shared<'_>) -> Result<Vec<Look>, String> {
    let pieces = &shared.setup.pieces;
    if pieces.len() < 2 {
        return Ok(pieces.iter().map(|_| Look::default()).collect());
    }
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<String>> = Mutex::new(None);
    let slots: Vec<Mutex<Look>> = pieces.iter().map(|_| Mutex::new(Look::default())).collect();
    std::thread::scope(|scope| {
        for _ in 0..shared.env.parallel.clamp(1, pieces.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(piece) = pieces.get(index) else {
                        break;
                    };
                    match shared.setup.walk(piece, shared.env.seams.link_hooks()) {
                        Ok(tree) => {
                            let planned = shared.env.observer.is_some().then(|| {
                                match shared.setup.judge(&tree, shared.now) {
                                    Judged::Quiet => None,
                                    Judged::Go | Judged::Refused(_) => Some(Planned {
                                        name: piece.to_string(),
                                        bytes: apparent_size(&tree),
                                    }),
                                }
                            });
                            *slots[index].lock().unwrap_or_else(PoisonError::into_inner) = Look {
                                links: piece_links(&tree),
                                planned: planned.flatten(),
                            };
                        }
                        Err(error) => {
                            failed
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .get_or_insert_with(|| chain_text(&error));
                            break;
                        }
                    }
                }
            });
        }
    });
    if let Some(cause) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(cause);
    }
    Ok(slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap_or_else(PoisonError::into_inner))
        .collect())
}

/// The files of `tree` that have more than one name. A directory has several
/// links without sharing anything.
fn piece_links(tree: &PieceTree) -> PieceLinks {
    let linked = |entry: &&TreeEntry| entry.nlink > 1 && entry.kind != EntryKind::Directory;
    let id = |entry: &TreeEntry| (entry.device, entry.inode);
    let packed: HashSet<FileId> = tree.packed_entries().filter(linked).map(id).collect();
    let ignored = tree
        .entries()
        .iter()
        .filter(linked)
        .map(id)
        .filter(|file| !packed.contains(file))
        .collect();
    PieceLinks { packed, ignored }
}

/// The pieces, by index, grouped so that pieces which one deletion can make
/// differ from their verify are in one group. A file joins its holders when
/// at least one of them packs it: the step 6 comparison skips ignored
/// entries, so a file that every holder ignores can change without any piece
/// noticing, while the deletion of an ignored name still changes a file that
/// another piece packs.
///
/// A group lists its pieces in ascending order, and the groups are ordered by
/// their first piece, so a run without shared files keeps the order of the
/// paths.
fn share_groups(links: &[PieceLinks]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..links.len()).collect();
    fn root(parent: &mut [usize], mut at: usize) -> usize {
        while parent[at] != at {
            parent[at] = parent[parent[at]];
            at = parent[at];
        }
        at
    }
    // Per file: the first piece that holds it, and whether any holder packs it.
    let mut holders: HashMap<FileId, (usize, bool)> = HashMap::new();
    for (index, piece) in links.iter().enumerate() {
        for (file, packs) in piece
            .packed
            .iter()
            .map(|file| (file, true))
            .chain(piece.ignored.iter().map(|file| (file, false)))
        {
            let entry = holders.entry(*file).or_insert((index, false));
            entry.1 |= packs;
        }
    }
    for (index, piece) in links.iter().enumerate() {
        for file in piece.packed.iter().chain(&piece.ignored) {
            let (first, packed_somewhere) = holders[file];
            if packed_somewhere {
                let (a, b) = (root(&mut parent, first), root(&mut parent, index));
                // The lower index stays the root, so a group is found by its
                // first piece.
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..links.len() {
        groups
            .entry(root(&mut parent, index))
            .or_default()
            .push(index);
    }
    groups.into_values().collect()
}

fn skipped(piece: &RelPath, reason: SkipReason, message: String) -> Done {
    Done::new(Finished::Skipped(SkippedPiece {
        piece: piece.to_string(),
        reason,
        message,
    }))
}

/// Steps 1 to 7 for one piece: walk and judge it again, store its version,
/// remove its folder. `planned` tells whether the observer was told to
/// expect the piece.
fn process(shared: &Shared<'_>, piece: &RelPath, planned: bool) -> Done {
    let env = shared.env;
    let setup = shared.setup;

    // The walk and the verdict are those of the moment the piece starts.
    let tree = match setup.walk(piece, env.seams.walk_hooks()) {
        Ok(tree) => tree,
        Err(error) => {
            let mut done = skipped(piece, SkipReason::Error, chain_text(&error));
            done.exhausted = Some(chain_text(&error));
            return done;
        }
    };
    let name = piece.to_string();
    let tracker = Tracker::new(env.observer.unwrap_or(&Silent), &name);
    let bytes = apparent_size(&tree);
    match setup.judge(&tree, shared.now) {
        Judged::Go => {}
        Judged::Quiet => {
            if planned {
                tracker.dropped();
            }
            return Done::new(Finished::Quiet);
        }
        Judged::Refused(refused) => {
            let reasons = refused
                .reasons
                .iter()
                .map(|reason| reason.text.clone())
                .collect();
            tracker.finished(bytes, &Progress::Refused(reasons));
            return Done::new(Finished::Refused(refused));
        }
    }

    tracker.started(Some(bytes), None, &[]);
    let done = store_and_remove(shared, piece, &tree, tracker);
    let outcome = match &done.finished {
        Finished::Archived(archived) => Progress::Files(archived.files),
        Finished::Skipped(skipped) => Progress::Skipped(skipped.message.clone()),
        Finished::Refused(_) | Finished::Quiet => {
            unreachable!("a piece that went on is archived or skipped")
        }
    };
    tracker.finished(bytes, &outcome);
    done
}

/// Steps 2 to 7 of a piece that is due.
fn store_and_remove(
    shared: &Shared<'_>,
    piece: &RelPath,
    tree: &PieceTree,
    tracker: Tracker<'_>,
) -> Done {
    let env = shared.env;
    let setup = shared.setup;
    let config = &shared.collection.config().collection;

    let stored_path = match stored_spelling_of(shared.collection, piece) {
        Ok(path) => path,
        Err(error) => return skipped(piece, SkipReason::Error, chain_text(&error)),
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
        tracker,
    };
    let request = StoreRequest {
        folder: Folder::Live(piece),
        stored: &stored_path,
        tree,
        rules: &setup.piece_rules,
        imported: None,
        time: None,
    };
    let stored = match store_version(&store_context, &request) {
        Ok(stored) => stored,
        Err(StoreFailure::NoRoom(error)) => {
            return skipped(piece, SkipReason::NoRoom, chain_text(error.as_ref()));
        }
        Err(StoreFailure::Changed(error)) => {
            return skipped(piece, SkipReason::Differs, chain_text(error.as_ref()));
        }
        Err(StoreFailure::OutOfDescriptors(error)) => {
            let mut done = skipped(piece, SkipReason::Error, chain_text(error.as_ref()));
            done.exhausted = Some(chain_text(error.as_ref()));
            return done;
        }
        Err(StoreFailure::NotDurable(error) | StoreFailure::Failed(error)) => {
            return skipped(piece, SkipReason::Error, chain_text(error.as_ref()));
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
            tracker,
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
        Removed::Kept(error) => skipped(piece, SkipReason::Error, chain_text(error.as_ref())),
        Removed::KeptOutOfDescriptors { error, cause } => {
            let mut done = skipped(piece, SkipReason::Error, chain_text(error.as_ref()));
            done.exhausted = Some(cause);
            done
        }
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
pub(crate) fn stored_spelling_of(
    collection: &Collection,
    piece: &RelPath,
) -> Result<RelPath, ArchiveError> {
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
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ArchiveError {
    #[error("cannot start the run")]
    Run(
        #[from]
        #[diagnostic_source]
        RunError,
    ),
    #[error("cannot resolve the piece")]
    Reference(
        #[from]
        #[diagnostic_source]
        ReferenceError,
    ),
    #[error("the piece to archive is empty")]
    EmptyReference,
    #[error("cannot open the collection {path}")]
    Open {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot select the pieces")]
    Select(
        #[from]
        #[diagnostic_source]
        SelectError,
    ),
    #[error("cannot read the ignore and timeless rules")]
    Rules(
        #[from]
        #[diagnostic_source]
        RulesError,
    ),
    #[error("cannot size the pool of workers")]
    Pool(
        #[source]
        #[diagnostic_source]
        SysmemError,
    ),
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
        #[diagnostic_source]
        source: ReferenceError,
    },
    #[error("cannot walk {piece}")]
    Walk {
        piece: String,
        #[source]
        #[diagnostic_source]
        source: WalkError,
    },
    #[error("{piece} needs {needed} bytes in .antiq/ and {available} are free")]
    NoRoom {
        piece: String,
        needed: u64,
        available: u64,
    },
    #[error("cannot read the free space of .antiq/")]
    Space(
        #[source]
        #[diagnostic_source]
        FilesystemError,
    ),
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
        #[diagnostic_source]
        source: FilesystemError,
    },
    #[error("cannot pack {piece} into tmp/pack/{stamp}")]
    Pack {
        piece: String,
        stamp: String,
        #[source]
        #[diagnostic_source]
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
        #[diagnostic_source]
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
        #[diagnostic_source]
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
        #[diagnostic_source]
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
        #[diagnostic_source]
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
        #[diagnostic_source]
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

    #[test]
    fn an_unreported_physical_memory_is_an_error_of_the_kind_other_with_the_cause_in_the_text() {
        let error = ArchiveError::Pool(SysmemError::Unavailable);
        assert_eq!(
            crate::error::chain_text(&error),
            "cannot size the pool of workers: the operating system does not report the physical memory"
        );
        assert_eq!(error.kind(), ErrorKind::Other);
    }
    use crate::clock::FixedClock;
    use crate::commands::survey::{Verdict, survey};
    use crate::error::Error;
    use crate::filesystem::tree::TreeRoot;
    use crate::plan::Step;
    use crate::progress::Counter;
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
        /// Runs after the archive is written, before it is read back.
        before_read_back: Option<Change>,
        /// Runs after the archive is read back, before it is compared with
        /// the folder.
        before_verify: Option<Change>,
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

        fn before_read_back(&self, staging: &Path) {
            if let Some(change) = &self.before_read_back {
                change(staging);
            }
        }

        fn before_verify(&self, staging: &Path) {
            if let Some(change) = &self.before_verify {
                change(staging);
            }
        }

        fn remove(
            &self,
            root: &TreeRoot,
            path: &RelPath,
            counter: &Counter,
        ) -> Result<(), FilesystemError> {
            if self.failing_delete {
                return Err(crate::filesystem::io_error("remove", Path::new("doomed"))(
                    io::Error::from_raw_os_error(libc::EACCES),
                ));
            }
            RealSeams.remove(root, path, counter)
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
        execute_observed(world, target, seams, parallel, None)
    }

    fn execute_observed(
        world: &TempCollection,
        target: &Target,
        seams: &dyn Seams,
        parallel: usize,
        observer: Option<&dyn Observer>,
    ) -> Result<ArchiveReport, ArchiveError> {
        let collection = collection(world);
        let clock = FixedClock::new(now());
        // A run that ended just before has released the lock, but a forked
        // child of another test may still hold a copy of it for a moment.
        let started = crate::lock::retry_while_held(
            |result| matches!(result, Err(RunError::Lock(_))),
            || run::start(&collection, &clock, false),
        );
        let Started::Guarded(guard) = started.expect("starts") else {
            panic!("a real run is guarded");
        };
        let env = Env {
            seams,
            pool: shared_pool(),
            zstd: &fast,
            parallel,
            observer,
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
            chain_text(&error),
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

    #[test]
    fn a_give_back_that_fails_after_a_rewalk_out_of_descriptors_also_stops_the_run() {
        let mut world = world("");
        for piece in ["a", "b"] {
            add(&mut world, piece, 40);
        }
        let root = world.root().to_path_buf();
        let seams = Probe {
            exhausts_rewalk: true,
            after_trash_rename: Some(Box::new(move |_| {
                // The path is taken, and `recovered/` cannot be made.
                fs::create_dir(root.join("a")).expect("the path is taken");
                let recovered = root.join(".antiq/recovered");
                fs::remove_dir_all(&recovered).ok();
                fs::write(&recovered, "in the way").expect("recovered/ is a file");
            })),
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        let skipped: Vec<&str> = report
            .skipped
            .iter()
            .map(|item| item.piece.as_str())
            .collect();
        assert_eq!(
            skipped,
            ["a", "b"],
            "the piece that is not started is skipped"
        );
        assert!(report.skipped[1].message.starts_with("not started"));
        let warnings = report
            .warnings
            .iter()
            .filter(|warning| warning.message.contains("file descriptors"))
            .count();
        assert_eq!(warnings, 1, "{:?}", report.warnings);
        assert_eq!(report.exit_code(), 3);
    }

    /// A directory that selection has to look into and cannot read.
    fn world_with_unreadable_directory() -> TempCollection {
        let mut world = world("patterns = [\"*/*\"]");
        add(&mut world, "b/c", 40);
        let locked = world.tree().unreadable_dir("a");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        world
    }

    #[test]
    fn an_unreadable_directory_of_the_selection_is_named_in_a_warning() {
        let world = world_with_unreadable_directory();

        let report = run_all(&world);

        let archived: Vec<&str> = report
            .archived
            .iter()
            .map(|item| item.piece.as_str())
            .collect();
        assert_eq!(archived, ["b/c"]);
        let texts: Vec<&str> = report.warnings.iter().map(|w| w.message.as_str()).collect();
        assert_eq!(texts, ["unreadable: a"], "the run goes on and exits 0");
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn a_dry_run_names_the_unreadable_directory_too() {
        let world = world_with_unreadable_directory();

        let plan = plan_of(&world, &Target::Due);

        let texts: Vec<&str> = plan.warnings.iter().map(|w| w.message.as_str()).collect();
        assert_eq!(texts, ["unreadable: a"]);
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
            closest: Vec::new(),
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

    // =========================================================
    // Other ways a piece ends
    // =========================================================

    #[test]
    fn a_folder_that_changes_while_it_is_packed_is_skipped_as_differs() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let live = world.root().join("piece/a.txt");
        let seams = Probe {
            before_verify: Some(Box::new(move |_| {
                fs::write(&live, "changed during the pack").expect("the file is written");
            })),
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert_eq!(report.skipped[0].reason, SkipReason::Differs);
        assert!(world.root().join("piece/a.txt").exists());
        assert!(entries(&antiq(&world, "pieces/piece")).is_empty());
        assert_no_leftovers(&world);
    }

    #[test]
    fn an_archive_that_reads_back_differently_is_skipped_as_error() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let seams = Probe {
            before_read_back: Some(Box::new(|staging| {
                let archive = find_archive(staging).expect("the archive is staged");
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(archive)
                    .expect("the archive opens");
                io::Write::write_all(&mut file, b"x").expect("a byte is added");
            })),
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 1).expect("the run ends");

        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert_eq!(report.skipped[0].reason, SkipReason::Error);
        assert!(world.root().join("piece/a.txt").exists());
        assert_no_leftovers(&world);
    }

    fn find_archive(dir: &Path) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = find_archive(&path) {
                    return Some(found);
                }
            } else if path.to_string_lossy().ends_with(".tar.zst") {
                return Some(path);
            }
        }
        None
    }

    #[test]
    fn a_store_that_cannot_be_opened_skips_the_piece() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        fs::remove_dir(antiq(&world, "pieces")).expect("pieces/ is empty");
        fs::write(antiq(&world, "pieces"), "").expect("a file takes the place of pieces/");

        let report = execute_with(&world, &Target::Due, &RealSeams, 1).expect("the run ends");

        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert_eq!(report.skipped[0].reason, SkipReason::Error);
        assert!(
            report.skipped[0].message.starts_with("cannot open "),
            "{:?}",
            report.skipped[0]
        );
        assert!(world.root().join("piece/a.txt").exists());
    }

    /// Restores the mode of a directory when dropped.
    struct Mode(std::path::PathBuf);

    impl Drop for Mode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    #[test]
    fn a_piece_whose_pin_cannot_be_looked_at_is_refused_as_an_unreadable_marker() {
        use std::os::unix::fs::PermissionsExt;
        let mut world = world("");
        add(&mut world, "piece", 40);
        let top = world.root().join("piece");
        let _restore = Mode(top.clone());
        fs::set_permissions(&top, fs::Permissions::from_mode(0o000)).expect("the piece is locked");
        assert!(
            fs::read_dir(&top).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        let report = execute_with(&world, &Target::Due, &RealSeams, 1).expect("the run ends");

        let refused = &report.refused[0];
        assert!(kinds(refused).contains(&"unreadable_entry"), "{refused:?}");
        assert_eq!(
            kinds(refused).last(),
            Some(&"unreadable_marker"),
            "{refused:?}"
        );
    }

    #[test]
    fn the_reports_hand_their_warnings_to_the_binary() {
        let mut world = world("mystery = 1");
        add(&mut world, "recent", 1);
        let report = run_all(&world);
        let plan = plan_of(&world, &Target::Due);

        assert!(!plan.partial());
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].message.contains("mystery"));
        assert_eq!(CommandReport::warnings(&plan), plan.warnings.as_slice());
        assert_eq!(CommandReport::warnings(&report), report.warnings.as_slice());
    }

    // =========================================================
    // Pieces that share a hard-linked file
    // =========================================================

    /// Holds `remover` after its rename into `trash/` until `waiter` has
    /// reached the same point, and `waiter` there until `remover` is deleted.
    /// Run side by side, the verify of `waiter` then falls before the
    /// deletion of `remover`, and its re-walk after it, whatever the timing.
    /// Run one after the other, which is what the fix does, `remover` waits
    /// for a `waiter` that cannot start before it ends, so every passing run
    /// takes the whole timeout. The wait is what makes the failing run fail
    /// every time, so the cost stays.
    struct Linked {
        waiter: &'static str,
        remover: &'static str,
        state: Mutex<(bool, bool)>,
        changed: std::sync::Condvar,
    }

    impl Linked {
        fn new(waiter: &'static str, remover: &'static str) -> Linked {
            Linked {
                waiter,
                remover,
                state: Mutex::new((false, false)),
                changed: std::sync::Condvar::new(),
            }
        }

        /// Waits until `done` holds of (waiter arrived, remover deleted). The
        /// timeout is long, since a short one lets the failing run pass under
        /// load; it only ends a wait that cannot succeed.
        fn wait_for(&self, done: impl Fn(&(bool, bool)) -> bool) {
            let state = self.state.lock().expect("the gate is not poisoned");
            let _ = self
                .changed
                .wait_timeout_while(state, std::time::Duration::from_secs(3), |state| {
                    !done(state)
                })
                .expect("the gate is not poisoned");
        }

        fn update(&self, change: impl FnOnce(&mut (bool, bool))) {
            change(&mut self.state.lock().expect("the gate is not poisoned"));
            self.changed.notify_all();
        }
    }

    impl Seams for Linked {
        fn after_trash_rename(&self, tree: &Path) {
            if tree.ends_with(self.waiter) {
                self.update(|state| state.0 = true);
                self.wait_for(|state| state.1);
            } else if tree.ends_with(self.remover) {
                self.wait_for(|state| state.0);
            }
        }

        fn remove(
            &self,
            root: &TreeRoot,
            path: &RelPath,
            counter: &Counter,
        ) -> Result<(), FilesystemError> {
            RealSeams.remove(root, path, counter)?;
            if Path::new(&path.to_string()).ends_with(self.remover) {
                self.update(|state| state.1 = true);
            }
            Ok(())
        }
    }

    #[test]
    fn pieces_that_share_a_hard_link_are_both_archived_whatever_the_timing() {
        let mut world = world("");
        add(&mut world, "a", 40);
        add(&mut world, "b", 40);
        let linked = world.tree().path("b/a.txt");
        fs::remove_file(&linked).expect("the copy goes");
        world.tree().hard_link("a/a.txt", "b/a.txt");
        let seams = Linked::new("b", "a");

        let report = execute_with(&world, &Target::Due, &seams, 2).expect("the run ends");

        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(pieces_of(&report), ["a", "b"]);
        assert_no_leftovers(&world);
    }

    fn files(ids: &[u64]) -> PieceLinks {
        PieceLinks {
            packed: ids.iter().map(|inode| (1, *inode)).collect(),
            ignored: HashSet::new(),
        }
    }

    #[test]
    fn pieces_without_a_shared_file_are_groups_of_one_in_path_order() {
        let keys = [files(&[]), files(&[1]), files(&[2])];

        assert_eq!(share_groups(&keys), [[0], [1], [2]]);
    }

    #[test]
    fn pieces_with_a_file_in_common_form_one_group_ordered_by_its_first_piece() {
        let keys = [
            files(&[1]),
            files(&[]),
            files(&[1]),
            files(&[2]),
            files(&[3]),
        ];

        assert_eq!(share_groups(&keys), [vec![0, 2], vec![1], vec![3], vec![4]]);
    }

    #[test]
    fn sharing_is_transitive_and_a_device_tells_inodes_apart() {
        let keys = [
            files(&[1]),
            files(&[2]),
            files(&[1, 2]),
            PieceLinks {
                packed: HashSet::from([(2, 1)]),
                ignored: HashSet::new(),
            },
        ];

        assert_eq!(share_groups(&keys), [vec![0, 1, 2], vec![3]]);
    }

    #[test]
    fn a_file_that_every_holder_ignores_joins_no_pieces_but_one_that_a_holder_packs_does() {
        let ignoring = |inode: u64| PieceLinks {
            packed: HashSet::new(),
            ignored: HashSet::from([(1, inode)]),
        };

        assert_eq!(share_groups(&[ignoring(1), ignoring(1)]), [[0], [1]]);
        assert_eq!(
            share_groups(&[ignoring(1), ignoring(2), files(&[1])]),
            [vec![0, 2], vec![1]]
        );
    }

    #[test]
    fn the_linked_files_of_a_walk_are_the_files_with_more_than_one_name() {
        let mut world = world("ignore = [\"node_modules/\"]");
        add(&mut world, "a", 40);
        add(&mut world, "b", 40);
        world.tree().hard_link("a/a.txt", "b/shared");
        world.tree().hard_link("a/sub/b.txt", "a/twin.txt");
        world
            .tree()
            .file_with("a/node_modules/x.js", "ignored", 0o644, 0);
        world.tree().hard_link("a/node_modules/x.js", "b/dep.js");
        let collection = collection(&world);
        let setup = Setup::new(&collection, &Target::Due).expect("the setup");
        let id_of = |relative: &str| {
            let meta = fs::metadata(world.root().join(relative)).expect("the file is there");
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };

        let a = piece_links(&setup.walk(&path("a"), &RealFs).expect("walks"));
        let b = piece_links(&setup.walk(&path("b"), &RealFs).expect("walks"));

        assert_eq!(
            a.packed,
            HashSet::from([id_of("a/a.txt"), id_of("a/sub/b.txt")])
        );
        assert_eq!(a.ignored, HashSet::from([id_of("a/node_modules/x.js")]));
        assert_eq!(
            b.packed,
            HashSet::from([id_of("a/a.txt"), id_of("a/node_modules/x.js")])
        );
        assert!(b.ignored.is_empty());
        let directory = id_of("a/sub");
        assert!(!a.packed.contains(&directory) && !a.ignored.contains(&directory));
    }

    #[test]
    fn linked_pieces_run_in_path_order_on_one_thread() {
        let mut world = world("");
        for piece in ["a", "b", "c"] {
            add(&mut world, piece, 40);
        }
        fs::remove_file(world.root().join("c/a.txt")).expect("the copy goes");
        world.tree().hard_link("a/a.txt", "c/a.txt");
        let order = Order::default();

        let report = execute_with(&world, &Target::Due, &order, 3).expect("the run ends");

        assert_eq!(pieces_of(&report), ["a", "b", "c"]);
        let seen = order.seen.lock().expect("not poisoned").clone();
        let at = |piece: &str| {
            seen.iter()
                .position(|(name, _)| name == piece)
                .expect("seen")
        };
        assert!(at("a") < at("c"), "{seen:?}");
        assert_eq!(seen[at("a")].1, seen[at("c")].1, "one thread: {seen:?}");
    }

    /// Records which thread deleted which piece, in order.
    #[derive(Default)]
    struct Order {
        seen: Mutex<Vec<(String, std::thread::ThreadId)>>,
    }

    impl Seams for Order {
        fn remove(
            &self,
            root: &TreeRoot,
            path: &RelPath,
            counter: &Counter,
        ) -> Result<(), FilesystemError> {
            RealSeams.remove(root, path, counter)?;
            let piece = path
                .to_string()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            self.seen
                .lock()
                .expect("not poisoned")
                .push((piece, std::thread::current().id()));
            Ok(())
        }
    }

    #[test]
    fn running_out_of_descriptors_in_the_look_for_shared_files_ends_the_run_before_any_piece_starts()
     {
        struct Blind;
        impl Hooks for Blind {
            fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
                (call == Call::Stat && path.is_root())
                    .then(|| io::Error::from_raw_os_error(libc::EMFILE))
            }
        }
        impl Seams for Blind {
            fn link_hooks(&self) -> &dyn Hooks {
                self
            }
        }
        let mut world = world("");
        add(&mut world, "a", 40);
        add(&mut world, "b", 40);

        let report = execute_with(&world, &Target::Due, &Blind, 2).expect("the run ends");

        assert!(report.archived.is_empty(), "{report:?}");
        assert_eq!(report.skipped.len(), 2, "{report:?}");
        for skipped in &report.skipped {
            assert_eq!(skipped.reason, SkipReason::Error);
            assert!(skipped.message.starts_with("not started"), "{skipped:?}");
        }
        let notes: Vec<&str> = report.warnings.iter().map(|w| w.message.as_str()).collect();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("ran out of file descriptors"),
            "{notes:?}"
        );
        assert_eq!(report.exit_code(), 3);
        assert!(world.root().join("a/a.txt").exists() && world.root().join("b/a.txt").exists());
    }

    #[test]
    fn running_out_of_descriptors_in_a_group_leaves_its_later_pieces_not_started() {
        let mut world = world("");
        add(&mut world, "a", 40);
        add(&mut world, "b", 40);
        fs::remove_file(world.root().join("b/a.txt")).expect("the copy goes");
        world.tree().hard_link("a/a.txt", "b/a.txt");
        let seams = Probe {
            exhausts_first_walk: true,
            ..Probe::default()
        };

        let report = execute_with(&world, &Target::Due, &seams, 2).expect("the run ends");

        assert!(report.archived.is_empty(), "{report:?}");
        let messages: Vec<&str> = report.skipped.iter().map(|s| s.message.as_str()).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[1].starts_with("not started"), "{messages:?}");
        assert!(world.root().join("b/a.txt").exists());
    }

    // =========================================================
    // Progress events
    // =========================================================

    use crate::testing::recording::Recorder;

    #[test]
    fn a_lone_piece_reports_its_operations_with_counters_that_reach_their_totals() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let recorder = Recorder::new();

        execute_observed(&world, &Target::Due, &RealSeams, 1, Some(&recorder))
            .expect("the run ends");

        assert_eq!(
            recorder.lines(),
            [
                "started piece 9",
                "piece Packing 2/2",
                "piece Verifying 2/2",
                "piece Storing 0/0",
                "piece Removing 4/4",
                "finished piece 9 Files(2)",
            ]
        );
    }

    #[test]
    fn the_removal_counts_the_ignored_entries_it_deletes_too() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let mtime = now().as_second() - 40 * DAY;
        for path in ["piece/node_modules/x.js", "piece/node_modules/deep/y.js"] {
            world.tree().file_with(path, "ignored", 0o644, mtime);
        }
        let recorder = Recorder::new();

        execute_observed(&world, &Target::Due, &RealSeams, 1, Some(&recorder))
            .expect("the run ends");

        // The piece folder, a.txt, sub, sub/b.txt, node_modules, x.js, deep
        // and y.js.
        assert_eq!(recorder.lines_of("piece")[4], "piece Removing 8/8");
    }

    #[test]
    fn a_hard_linked_file_is_counted_once_in_the_verify_and_per_name_in_the_packing() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        world.tree().hard_link("piece/a.txt", "piece/link.txt");
        let recorder = Recorder::new();

        execute_observed(&world, &Target::Due, &RealSeams, 1, Some(&recorder))
            .expect("the run ends");

        // a.txt, link.txt and sub/b.txt are packed; the link is a member
        // that the verify does not read again.
        assert_eq!(
            recorder.lines_of("piece")[1..3],
            ["piece Packing 3/3", "piece Verifying 2/2"]
        );
    }

    #[test]
    fn pieces_are_planned_without_the_ones_that_are_not_due() {
        let mut world = world("");
        add(&mut world, "due", 40);
        add(&mut world, "recent", 1);
        add(&mut world, "refused", 40);
        fs::create_dir_all(world.root().join("refused/inner/.antiq")).expect("a nested collection");
        let recorder = Recorder::new();

        execute_observed(&world, &Target::Due, &RealSeams, 1, Some(&recorder))
            .expect("the run ends");

        let lines = recorder.lines();
        assert_eq!(lines[0], "planned due refused");
        assert!(
            lines.iter().all(|line| !line.contains("recent")),
            "{lines:?}"
        );
        assert_eq!(
            recorder.lines_of("due").first().map(String::as_str),
            Some("started due 9")
        );
        let refused = recorder.lines_of("refused");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(
            refused[0].starts_with("finished refused ") && refused[0].contains("Refused"),
            "{refused:?}"
        );
    }

    /// Makes the second piece recent while the first one is being walked.
    struct Refreshing {
        touched: std::sync::atomic::AtomicBool,
        piece: std::path::PathBuf,
    }

    impl Seams for Refreshing {
        fn walk_hooks(&self) -> &dyn Hooks {
            if !self.touched.swap(true, Ordering::SeqCst) {
                fs::File::options()
                    .write(true)
                    .open(&self.piece)
                    .expect("the file opens")
                    .set_modified(std::time::SystemTime::from(now()))
                    .expect("the time is set");
            }
            &RealFs
        }
    }

    #[test]
    fn a_planned_piece_that_is_no_longer_due_when_its_turn_comes_is_dropped() {
        let mut world = world("");
        add(&mut world, "a", 40);
        add(&mut world, "b", 40);
        let seams = Refreshing {
            touched: std::sync::atomic::AtomicBool::new(false),
            piece: world.root().join("b/sub/b.txt"),
        };
        let recorder = Recorder::new();

        execute_observed(&world, &Target::Due, &seams, 1, Some(&recorder)).expect("the run ends");

        assert_eq!(recorder.lines()[0], "planned a b");
        assert_eq!(recorder.lines_of("b"), ["dropped b"]);
        assert_eq!(
            recorder.lines_of("a").last().map(String::as_str),
            Some("finished a 9 Files(2)")
        );
    }

    #[test]
    fn a_folder_that_changes_after_the_verify_ends_as_a_skipped_piece() {
        let mut world = world("");
        add(&mut world, "piece", 40);
        let seams = Probe {
            after_trash_rename: Some(Box::new(|tree| {
                fs::write(tree.join("a.txt"), "changed later").expect("the file is written");
            })),
            ..Probe::default()
        };
        let recorder = Recorder::new();

        execute_observed(&world, &Target::Due, &seams, 1, Some(&recorder)).expect("the run ends");

        let lines = recorder.lines();
        assert_eq!(lines[0], "started piece 9");
        let last = lines.last().expect("events came");
        assert!(
            last.starts_with("finished piece 9 Skipped(") && last.contains("went back"),
            "{last}"
        );
    }
}
