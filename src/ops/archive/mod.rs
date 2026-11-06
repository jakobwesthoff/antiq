// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` command.
//!
//! The candidates are the pieces `status` marks `idle` that are new or
//! live, so the command and `status` share one definition of what the next
//! `archive` takes.

pub mod durable;
pub mod new;
pub mod unearthed;

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::fsx::FsxError;
use crate::fsx::device::{self, FsKind};
use crate::fsx::space::available_space;
use crate::fsx::tree::TreeRoot;
use crate::id::{IdSource, ObjectId};
use crate::idle::{self, IdleError, IdleVerdict};
use crate::index;
use crate::interact::Interaction;
use crate::machine::MachineError;
use crate::ops::status::{self, Flag, PieceKind, StatusError, StatusRequest};
use crate::pack::pool::Pool;
use crate::pack::{PackError, Packager, TarZstd};
use crate::plan::{Plan, PlanAction};
use crate::relpath::{Name, RelPath};
use crate::remote::RemoteState;
use crate::repair::{Effects, RealEffects};
use crate::report::{CommandReport, Warning};
use crate::run::{self, RunError, Started};
use crate::select::{Rules, SelectError};
use crate::store::StoreError;
use crate::tracking::{Answer, DecisionKind, TrackingError};
use crate::tree::{EntryKind, PieceTree};
use crate::walk;
use jiff::Timestamp;
use serde::Serialize;
use std::collections::HashSet;
use std::io;
use std::panic::resume_unwind;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

/// Errors of the `archive` command. Only what stops the whole run is an
/// error; whatever goes wrong with one piece is a `skipped` row of the
/// report.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error(transparent)]
    Status(#[from] StatusError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Idle(#[from] crate::idle::IdleError),
    #[error(transparent)]
    Walk(#[from] walk::WalkError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
}

impl HasKind for ArchiveError {
    fn kind(&self) -> ErrorKind {
        match self {
            ArchiveError::Run(error) => error.kind(),
            ArchiveError::Machine(error) => error.kind(),
            ArchiveError::Store(error) => error.kind(),
            ArchiveError::Tracking(error) => error.kind(),
            _ => ErrorKind::Other,
        }
    }
}

// =========================================================
// Seams
// =========================================================

/// The steps of archiving one piece, in the order they run. A test seam is
/// asked before each step, and an error it returns is that step's own
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Walk,
    Pack,
    Verify,
    StorePayload,
    StoreListing,
    WriteMetadata,
    WriteEntry,
    MarkRemoving,
    MoveToTrash,
    Rewalk,
    DeleteInTrash,
    MarkGone,
    /// The rename to the trash was refused and the folder is deleted where
    /// it stands.
    DeleteInPlace,
    /// The trashed tree differs from the archive and goes back to its path.
    GiveBackMove,
    GiveBackEntry,
    /// The trashed tree differs and its path is not free.
    MoveRecovered,
    RecoveredEntry,
}

/// The calls of a run that a test can answer for. The defaults are the
/// real ones.
pub(crate) trait Seams: Sync {
    /// What the walks see of the filesystem.
    fn walk_hooks(&self) -> &(dyn walk::Hooks + Sync) {
        &walk::RealFs
    }

    /// The bytes free on the volume holding `dir`, for the piece `piece`.
    fn free_space(&self, _piece: &RelPath, dir: &Path) -> Result<u64, FsxError> {
        available_space(dir)
    }

    fn packager(&self) -> &(dyn Packager + Sync) {
        &TarZstd
    }

    /// The changes to the trash, the folder and its parent.
    fn effects(&self) -> &dyn Effects {
        &RealEffects
    }

    /// Called before `step` works on `piece`.
    fn at(&self, _step: Step, _piece: &RelPath) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct RealSeams;

impl Seams for RealSeams {}

/// What `status` asks the world while it picks the candidates: the seams'
/// walks, the real filesystem kind, and no remote. A remote that is away
/// changes nothing about which pieces are idle.
struct CandidateProbes<'a>(&'a dyn Seams);

impl status::Probes for CandidateProbes<'_> {
    fn remote_state(&self, _: &Collection) -> RemoteState {
        RemoteState::None
    }

    fn walk_hooks(&self) -> &dyn walk::Hooks {
        self.0.walk_hooks()
    }

    fn fs_kind(&self, root: &Path) -> Result<FsKind, FsxError> {
        device::fs_kind(root)
    }

    fn away_warning(
        &self,
        _: &Collection,
        _: RemoteState,
        _: bool,
        _: Timestamp,
    ) -> Option<Warning> {
        None
    }
}

// =========================================================
// The report
// =========================================================

/// One piece a dry run would archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ArchiveAction {
    Archive {
        path: RelPath,
        /// `null` for a piece that has no ID yet.
        id: Option<ObjectId>,
        /// Apparent size in bytes.
        size: u64,
    },
    /// An unearthed piece that equals its archive: the folder goes and no
    /// new archive is made.
    Reclaim {
        path: RelPath,
        id: ObjectId,
        /// Apparent size in bytes.
        size: u64,
    },
    /// An unearthed piece that changed without losing anything: a new
    /// archive replaces the old one without asking.
    Replace {
        path: RelPath,
        id: ObjectId,
        /// Apparent size in bytes.
        size: u64,
    },
    /// An unearthed piece whose replacement would lose `lost_count`
    /// entries: the run asks what to do.
    Ask {
        path: RelPath,
        id: ObjectId,
        /// Apparent size in bytes.
        size: u64,
        lost_count: usize,
    },
}

impl PlanAction for ArchiveAction {
    fn describe(&self) -> String {
        match self {
            ArchiveAction::Archive { path, size, .. } => {
                format!("archive {path} ({})", size_text(*size))
            }
            ArchiveAction::Reclaim { path, size, .. } => {
                format!("reclaim {path} ({})", size_text(*size))
            }
            ArchiveAction::Replace { path, size, .. } => {
                format!("replace the archive of {path} ({})", size_text(*size))
            }
            ArchiveAction::Ask {
                path, lost_count, ..
            } => {
                format!(
                    "ask whether to replace the archive of {path}, which would lose {lost_count} {}",
                    if *lost_count == 1 { "entry" } else { "entries" }
                )
            }
        }
    }
}

/// A piece that was archived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Archived {
    pub id: ObjectId,
    pub path: RelPath,
    /// Apparent size of the folder in bytes.
    pub size: u64,
    /// Bytes of the stored payload.
    pub archive_size: u64,
    pub payload_version: u32,
}

/// A piece whose folder was removed because it equals the archive it was
/// unearthed from. No archive was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reclaimed {
    pub id: ObjectId,
    pub path: RelPath,
    /// Apparent size of the folder in bytes.
    pub size: u64,
}

/// A piece that was forked instead of archived: the answer to a
/// replacement that would lose content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Forked {
    /// The new ID.
    pub id: ObjectId,
    /// The ID the piece had.
    pub from: ObjectId,
    pub path: RelPath,
}

/// One entry the archive holds that a replacement would lose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lost {
    pub path: RelPath,
    /// `removed` or `changed`.
    pub change: &'static str,
}

/// A piece waiting for a decision that nobody could give: nothing was
/// written for it but the pending decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NeedsDecision {
    pub id: ObjectId,
    pub path: RelPath,
    pub decision: DecisionKind,
    pub options: Vec<Answer>,
    /// How many entries a replacement would lose.
    pub lost_count: usize,
    /// The first of them, in path order.
    pub lost: Vec<Lost>,
}

/// Why a piece was not archived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Unreadable,
    MountPoint,
    Excluded,
    NestedCollection,
    /// The folder differs from what was packed from it.
    Differs,
    NoSpace,
    /// The archive is not proven good: the local payload fails its check
    /// and no check of the remote copy is recorded.
    PendingRemote,
    /// The answer to the question about a replacement was to skip.
    Declined,
    Error,
}

/// A piece that was left alone, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// Present only for a piece that had an ID before the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub path: RelPath,
    pub reason: SkipReason,
    pub message: String,
}

/// How a piece that was stored stays on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeptHow {
    /// The folder is back at its path and counts as unearthed.
    Unearthed,
    /// The folder is below `.antiq/recovered/`.
    Recovered,
}

/// A piece that was archived but whose folder was not deleted, because it
/// differs from what the archive holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub id: ObjectId,
    pub path: RelPath,
    pub how: KeptHow,
    /// The name below `.antiq/recovered/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered_as: Option<String>,
}

/// Where the tree of an unfinished piece is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnfinishedWhere {
    /// Below `.antiq/trash/`.
    Trash,
    /// Below `.antiq/recovered/`.
    Recovered,
    /// At the piece's path.
    InPlace,
}

/// A piece that was archived but whose removal stopped half way: the entry
/// stays `removing` and the repairs of the next run finish it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unfinished {
    pub id: ObjectId,
    pub path: RelPath,
    #[serde(rename = "where")]
    pub place: UnfinishedWhere,
    pub message: String,
}

/// What `archive` did, or with `--dry-run` would do.
#[derive(Debug, Serialize)]
pub struct ArchiveReport {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan<ArchiveAction>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<Vec<Archived>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaimed: Option<Vec<Reclaimed>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<Skipped>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<Vec<Kept>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unfinished: Option<Vec<Unfinished>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked: Option<Vec<Forked>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_decision: Option<Vec<NeedsDecision>>,
    pub warnings: Vec<Warning>,
}

#[cfg(test)]
impl ArchiveReport {
    pub(crate) fn partial_for_test(&self) -> bool {
        self.partial()
    }
}

impl CommandReport for ArchiveReport {
    fn partial(&self) -> bool {
        let skipped = self.skipped.as_deref().unwrap_or_default();
        let unfinished = self.unfinished.as_deref().unwrap_or_default();
        let undecided = self.needs_decision.as_deref().unwrap_or_default();
        !skipped.is_empty() || !unfinished.is_empty() || !undecided.is_empty()
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// A size in decimal units with one decimal below 10: `88 MB`, `1.2 GB`.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && value.round() >= 1000.0 {
        value /= 1000.0;
        unit += 1;
    }
    if unit > 0 && (value * 10.0).round() < 100.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

// =========================================================
// Planning
// =========================================================

/// A piece the next real run archives.
pub(crate) struct Candidate {
    pub(crate) path: RelPath,
    pub(crate) id: Option<ObjectId>,
    /// `New`, `Live` or `Unearthed`.
    pub(crate) kind: PieceKind,
    /// When the piece was last unearthed.
    pub(crate) unearthed: Option<Timestamp>,
}

/// The idle, unpinned pieces in state `new`, `live` or `unearthed`, in path
/// order, with the warnings `status` has for the collection.
fn candidates(
    collection: &Collection,
    clock: &dyn Clock,
    seams: &dyn Seams,
    own_lock: bool,
) -> Result<(Vec<Candidate>, Vec<Warning>), ArchiveError> {
    let report = status::status_with(&StatusRequest {
        collection,
        clock,
        probes: &CandidateProbes(seams),
        own_lock,
    })?;
    let candidates = report
        .pieces
        .iter()
        .filter(|piece| {
            matches!(
                piece.state,
                PieceKind::New | PieceKind::Live | PieceKind::Unearthed
            )
        })
        .filter(|piece| piece.flags.contains(&Flag::Idle))
        .map(|piece| Candidate {
            path: piece.path.clone(),
            id: piece.id,
            kind: piece.state,
            unearthed: piece.unearthed,
        })
        .collect();
    Ok((candidates, report.warnings))
}

/// The sum of the sizes of the regular files, a file with several names
/// counted once.
pub(crate) fn apparent_size(tree: &PieceTree) -> u64 {
    let mut seen = HashSet::new();
    tree.entries()
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .filter(|entry| entry.nlink < 2 || seen.insert((entry.device, entry.inode)))
        .map(|entry| entry.size)
        .sum()
}

/// What a run would archive, as a dry run lists it. Nothing is locked and
/// nothing is written.
pub fn plan(collection: &Collection, clock: &dyn Clock) -> Result<ArchiveReport, ArchiveError> {
    plan_with(collection, clock, &RealSeams)
}

pub(crate) fn plan_with(
    collection: &Collection,
    clock: &dyn Clock,
    seams: &dyn Seams,
) -> Result<ArchiveReport, ArchiveError> {
    let (candidates, mut warnings) = candidates(collection, clock, seams, false)?;
    let config = &collection.config().collection;
    let root = open_root(collection)?;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    // `status` has warned about the junk list already.
    let junk = status::junk_names(&config.junk, &mut Vec::new());
    let mut plan = Plan::new();
    for candidate in candidates {
        let junk = junk_for(collection, &candidate, &junk);
        let tree =
            walk::walk_piece_with(&root, &candidate.path, &junk, &rules, seams.walk_hooks())?;
        let size = apparent_size(&tree);
        if candidate.kind != PieceKind::Unearthed {
            plan.push(ArchiveAction::Archive {
                size,
                path: candidate.path,
                id: candidate.id,
            });
            continue;
        }
        let (action, found) = plan_unearthed(collection, &junk, &root, &candidate, size);
        if let Some(action) = action {
            plan.push(action);
        }
        warnings.extend(found);
    }
    Ok(ArchiveReport {
        dry_run: true,
        plan: Some(plan),
        archived: None,
        reclaimed: None,
        skipped: None,
        kept: None,
        unfinished: None,
        forked: None,
        needs_decision: None,
        warnings,
    })
}

/// The junk names a run uses for `candidate`: an unearthed piece has its
/// own list (see `unearthed::junk_of_piece`), any other piece the
/// configured one.
fn junk_for(collection: &Collection, candidate: &Candidate, configured: &[Name]) -> Vec<Name> {
    match (candidate.kind, candidate.id) {
        (PieceKind::Unearthed, Some(id)) => unearthed::junk_of_piece(collection, id),
        _ => configured.to_vec(),
    }
}

/// What a run would do with the unearthed piece `candidate` of `size`
/// bytes: the action, or the warning that it would be skipped.
fn plan_unearthed(
    collection: &Collection,
    junk: &[Name],
    root: &TreeRoot,
    candidate: &Candidate,
    size: u64,
) -> (Option<ArchiveAction>, Vec<Warning>) {
    match unearthed::plan_action(collection, junk, root, candidate, size) {
        Ok((action, warning)) => (Some(action), warning.into_iter().collect()),
        Err(refusal) => (
            None,
            vec![Warning::new(format!(
                "`{}` would be skipped: {}",
                candidate.path, refusal.message
            ))],
        ),
    }
}

/// The note for a piece that is not idle when it is judged again.
pub(crate) const IN_USE_NOTE: &str = "the piece is in use; the next archive takes it";

/// What a run would do with the one unearthed piece `candidate`, as
/// `resolve` plans it. Nothing is locked and nothing is written. A piece
/// that a real run would leave alone because it is in use, or skip because
/// the walk cannot read it, gets no action and a note.
pub(crate) fn plan_one(
    collection: &Collection,
    clock: &dyn Clock,
    seams: &dyn Seams,
    candidate: &Candidate,
) -> Result<(Option<ArchiveAction>, Vec<Warning>), ArchiveError> {
    let config = &collection.config().collection;
    let root = open_root(collection)?;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    let junk = junk_for(
        collection,
        candidate,
        &status::junk_names(&config.junk, &mut Vec::new()),
    );
    let tree = walk::walk_piece_with(&root, &candidate.path, &junk, &rules, seams.walk_hooks())?;
    let settings = idle::Settings::new(config.idle_days, &config.ignore)?;
    match idle::check(&root, &tree, candidate.unearthed, &settings, clock.now()) {
        Ok(check) if matches!(check.verdict, IdleVerdict::Idle { .. }) => {}
        Ok(_) => return Ok((None, vec![Warning::new(IN_USE_NOTE)])),
        Err(error @ IdleError::Problems) => {
            return Ok((
                None,
                vec![Warning::new(format!(
                    "`{}` would be skipped: {error}",
                    candidate.path
                ))],
            ));
        }
        Err(error) => return Err(error.into()),
    }
    Ok(plan_unearthed(
        collection,
        &junk,
        &root,
        candidate,
        apparent_size(&tree),
    ))
}

// =========================================================
// The run
// =========================================================

/// Whoever answers the questions of a run, shared by its workers. A
/// question needs `&mut` access to the answerer, which only the guard of
/// the mutex gives, so the answerer sees one question at a time.
pub type Asker = Mutex<Box<dyn Interaction>>;

/// An answerer that answers nothing, as a run without a terminal has.
pub(crate) fn nobody() -> &'static Asker {
    static NOBODY: std::sync::LazyLock<Asker> =
        std::sync::LazyLock::new(|| Mutex::new(Box::new(crate::interact::NonInteractive)));
    &NOBODY
}

/// What a run of `archive` needs.
pub struct ArchiveRequest<'a> {
    pub collection: &'a Collection,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdSource,
    pub pool: &'a Pool,
    /// Whoever answers the question about a replacement that would lose
    /// content. Workers take turns: the lock is held only while a question
    /// is asked.
    pub interaction: &'a Asker,
    /// How many pieces are archived at the same time; at least one.
    pub workers: usize,
    pub dry_run: bool,
}

/// Archives every idle piece, or with `dry_run` lists them.
pub fn archive(request: &ArchiveRequest<'_>) -> Result<ArchiveReport, ArchiveError> {
    archive_with(request, &RealSeams)
}

pub(crate) fn archive_with(
    request: &ArchiveRequest<'_>,
    seams: &dyn Seams,
) -> Result<ArchiveReport, ArchiveError> {
    let collection = request.collection;
    let guard = match run::start(collection, request.clock, request.dry_run)? {
        Started::DryRun { .. } => return plan_with(collection, request.clock, seams),
        Started::Guarded(guard) => guard,
    };

    let (candidates, found) = candidates(collection, request.clock, seams, true)?;
    let mut warnings: Vec<Warning> = guard.warnings().to_vec();
    warnings.extend(found);
    let mut archived = Vec::new();
    let mut reclaimed = Vec::new();
    let mut skipped = Vec::new();
    let mut kept = Vec::new();
    let mut unfinished = Vec::new();
    let mut forked = Vec::new();
    let mut needs_decision = Vec::new();
    if !candidates.is_empty() {
        let run = new::Run {
            collection,
            lock: guard.lock(),
            clock: request.clock,
            ids: request.ids,
            pool: request.pool,
            interaction: request.interaction,
        };
        let cx = new::Context::for_run(
            &run,
            seams,
            candidates.iter().any(|candidate| candidate.id.is_none()),
        )?;
        let mut listing_stored = false;
        let slots = run_pieces(&cx, &candidates, request.workers);
        // The pieces that did not finish are skipped rows named after the
        // first failure in path order: a failure ends the run, and every
        // piece not yet started is left alone.
        let stopped_at = candidates
            .iter()
            .zip(&slots)
            .find(|(_, slot)| matches!(slot, Slot::Failed(_)))
            .map(|(candidate, _)| candidate.path.clone());
        for (candidate, slot) in candidates.iter().zip(slots) {
            let message = match slot {
                Slot::Done(result) => {
                    warnings.extend(result.warnings);
                    listing_stored |= result.listing_stored;
                    match result.outcome {
                        new::PieceOutcome::Dropped => {}
                        new::PieceOutcome::Skipped(row) => skipped.push(row),
                        new::PieceOutcome::Archived(row) => archived.push(row),
                        new::PieceOutcome::Reclaimed(row) => reclaimed.push(row),
                        new::PieceOutcome::Kept(row) => kept.push(row),
                        new::PieceOutcome::Unfinished(row) => unfinished.push(row),
                        new::PieceOutcome::Forked(row) => forked.push(row),
                        new::PieceOutcome::NeedsDecision(row) => needs_decision.push(row),
                    }
                    continue;
                }
                Slot::Failed(error) => {
                    let cause = new::chain(&error);
                    if stopped_at.as_ref() == Some(&candidate.path) {
                        warnings.push(Warning::new(format!(
                            "the run stopped at `{}`: {cause}",
                            candidate.path
                        )));
                    }
                    cause
                }
                Slot::NotStarted => match &stopped_at {
                    Some(path) => format!("not started because the run stopped at `{path}`"),
                    None => unreachable!("a piece is only left unstarted after a failure"),
                },
            };
            skipped.push(Skipped {
                id: candidate.id,
                path: candidate.path.clone(),
                reason: SkipReason::Error,
                message,
            });
        }
        if listing_stored && let Err(error) = index::update(collection, guard.lock()) {
            warnings.push(Warning::new(format!("cannot update the index: {error}")));
        }
    }
    Ok(ArchiveReport {
        dry_run: false,
        plan: None,
        archived: Some(archived),
        reclaimed: Some(reclaimed),
        skipped: Some(skipped),
        kept: Some(kept),
        unfinished: Some(unfinished),
        forked: Some(forked),
        needs_decision: Some(needs_decision),
        warnings,
    })
}

/// Archives the one piece `candidate` under the lock `run` holds, the way
/// `archive` does, but as `mode` says and without `status` having picked it.
pub(crate) fn archive_one(
    run: &new::Run<'_>,
    seams: &dyn Seams,
    candidate: &Candidate,
    mode: new::Mode,
) -> Result<new::PieceResult, ArchiveError> {
    let cx = new::Context::for_run(run, seams, false)?;
    new::archive_piece(&cx, candidate, mode)
}

/// What became of one candidate.
enum Slot {
    Done(new::PieceResult),
    /// The piece hit what no piece can go on after.
    Failed(ArchiveError),
    /// A piece failed first, so this one was never started.
    NotStarted,
}

/// Archives the candidates on `workers` threads that take the next piece
/// in path order as they become free, and returns one slot per candidate in
/// path order. When a piece fails the run, no new piece is started; the
/// pieces running by then finish.
fn run_pieces(cx: &new::Context<'_>, candidates: &[Candidate], workers: usize) -> Vec<Slot> {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let finished: Vec<(usize, Result<new::PieceResult, ArchiveError>)> = thread::scope(|scope| {
        let threads: Vec<_> = (0..workers.clamp(1, candidates.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    while !failed.load(Ordering::SeqCst) {
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        let Some(candidate) = candidates.get(index) else {
                            break;
                        };
                        let result = new::archive_piece(cx, candidate, new::Mode::ARCHIVE);
                        if result.is_err() {
                            failed.store(true, Ordering::SeqCst);
                        }
                        mine.push((index, result));
                    }
                    mine
                })
            })
            .collect();
        threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap_or_else(|panic| resume_unwind(panic)))
            .collect()
    });
    let mut slots: Vec<Slot> = candidates.iter().map(|_| Slot::NotStarted).collect();
    for (index, result) in finished {
        slots[index] = match result {
            Ok(result) => Slot::Done(result),
            Err(error) => Slot::Failed(error),
        };
    }
    slots
}

fn open_root(collection: &Collection) -> Result<TreeRoot, ArchiveError> {
    TreeRoot::open(collection.root()).map_err(|source| ArchiveError::Io {
        action: "open",
        path: collection.root().display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::meta::{CreatedSource, Lifecycle, Metadata};
    use crate::relpath::Name;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use crate::tracking::{EntryState, TrackingEntry, VersionTakenOut};
    use std::fs;

    pub(super) const NOW: &str = "2026-11-01T12:00:00Z";

    pub(super) fn clock() -> FixedClock {
        FixedClock::new(NOW.parse().expect("a time"))
    }

    pub(super) fn days_ago(days: i64) -> i64 {
        NOW.parse::<Timestamp>().expect("a time").as_second() - days * 86_400
    }

    /// A collection whose folders a test lays out.
    pub(super) struct World {
        pub(super) temp: TempCollection,
        pub(super) collection: Collection,
    }

    pub(super) fn world() -> World {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        World { temp, collection }
    }

    impl World {
        pub(super) fn file(&mut self, relative: &str, age_days: i64) {
            self.temp
                .tree()
                .file_with(relative, "x", 0o644, days_ago(age_days));
        }

        pub(super) fn configure(&mut self, lines: &str) {
            fs::write(
                self.temp.root().join(".antiq/config.toml"),
                format!("[collection]\nid = \"{COLLECTION_ID}\"\n{lines}\n"),
            )
            .expect("a config can be written");
            self.collection = Collection::find(self.temp.root()).expect("a collection");
        }

        fn plan(&self) -> ArchiveReport {
            plan_with(&self.collection, &clock(), &RealSeams).expect("the plan is made")
        }

        fn planned(&self) -> Vec<String> {
            self.plan()
                .plan
                .expect("a dry run has a plan")
                .actions
                .iter()
                .map(|action| match action {
                    ArchiveAction::Archive { path, .. }
                    | ArchiveAction::Reclaim { path, .. }
                    | ArchiveAction::Replace { path, .. }
                    | ArchiveAction::Ask { path, .. } => path.to_string(),
                })
                .collect()
        }
    }

    /// Live metadata of `owner` and an entry for the piece at `at`; the
    /// entry is changed by `change` before it is written.
    pub(super) fn live_piece(
        world: &World,
        number: u8,
        at: &str,
        owner: &str,
        change: impl FnOnce(&mut TrackingEntry),
    ) {
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(
            1_600_000_000_000 + u64::from(number),
            u128::from(number),
        ));
        let path = RelPath::parse(at).expect("a valid path");
        let name = Name::from_bytes(at.as_bytes().to_vec()).expect("a valid name");
        let when: Timestamp = "2026-10-01T10:00:00Z".parse().expect("a time");
        let metadata = Metadata {
            format: 1,
            id,
            collection: owner.parse().expect("an ID"),
            path: path.clone(),
            name,
            created: when,
            created_source: CreatedSource::Birth,
            lifecycle: Lifecycle::Live,
            payloads: Vec::new(),
            history: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let directory = world.collection.object_dir(id);
        fs::create_dir_all(&directory).expect("the object directory can be made");
        fs::write(
            directory.join("meta.1"),
            crate::meta::to_bytes(&metadata).expect("metadata serialises"),
        )
        .expect("metadata can be written");
        let mut entry = TrackingEntry::new(
            id,
            path,
            EntryState::Live,
            VersionTakenOut::Committed(1),
            when,
        );
        change(&mut entry);
        fs::write(
            world.collection.tracking_path(id),
            serde_json::to_vec(&entry).expect("an entry serialises"),
        )
        .expect("an entry can be written");
    }

    /// A second device below the piece at `path`, relative to its top.
    struct OtherDevice(&'static str);

    impl walk::Hooks for OtherDevice {
        fn device(&self, path: &RelPath, reported: u64) -> u64 {
            if path.to_string() == self.0 {
                reported + 1
            } else {
                reported
            }
        }

        fn fault(&self, _: &RelPath, _: walk::Call) -> Option<io::Error> {
            None
        }
    }

    struct Hooked(OtherDevice);

    impl Seams for Hooked {
        fn walk_hooks(&self) -> &(dyn walk::Hooks + Sync) {
            &self.0
        }
    }

    // =========================================================
    // Which pieces
    // =========================================================

    #[test]
    fn dry_run_lists_idle_new_and_live_pieces_in_path_order() {
        let mut world = world();
        world.file("c/note.txt", 30);
        world.file("a/note.txt", 30);
        world.file("busy/note.txt", 1);
        world.file("b/note.txt", 30);
        live_piece(&world, 1, "b", COLLECTION_ID, |_| {});
        let plan = world.plan();
        assert!(plan.dry_run);
        let actions = plan.plan.expect("a dry run has a plan").actions;
        let listed: Vec<(String, Option<ObjectId>)> = actions.iter().map(parts).collect();
        let live_id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1));
        assert_eq!(
            listed,
            [
                ("a".to_string(), None),
                ("b".to_string(), Some(live_id)),
                ("c".to_string(), None)
            ]
        );
    }

    /// The path and the ID of a planned action.
    pub(super) fn parts(action: &ArchiveAction) -> (String, Option<ObjectId>) {
        match action {
            ArchiveAction::Archive { path, id, .. } => (path.to_string(), *id),
            ArchiveAction::Reclaim { path, id, .. }
            | ArchiveAction::Replace { path, id, .. }
            | ArchiveAction::Ask { path, id, .. } => (path.to_string(), Some(*id)),
        }
    }

    /// `world` holds an idle piece `sibling` next to whatever the test
    /// made; only the sibling is planned.
    fn only_the_sibling(mut world: World) {
        world.file("sibling/note.txt", 30);
        assert_eq!(world.planned(), ["sibling"]);
    }

    #[test]
    fn a_pinned_piece_is_not_planned() {
        let mut world = world();
        world.file("p/.antiq-pin", 90);
        world.file("p/note.txt", 90);
        only_the_sibling(world);
    }

    #[test]
    fn an_empty_piece_is_not_planned() {
        let mut world = world();
        world.temp.tree().dir("e/sub");
        only_the_sibling(world);
    }

    #[test]
    fn an_unreadable_piece_is_not_planned() {
        let mut world = world();
        world.file("u/note.txt", 30);
        world.temp.tree().unreadable_dir("u/sub");
        assert!(
            fs::read_dir(world.temp.root().join("u/sub")).is_err(),
            "the test needs a directory that cannot be read; it must not run as root"
        );
        only_the_sibling(world);
    }

    #[test]
    fn a_piece_with_a_second_device_is_not_planned() {
        let mut world = world();
        world.file("m/note.txt", 30);
        world.temp.tree().dir("m/mnt");
        world.file("sibling/note.txt", 30);
        let report = plan_with(&world.collection, &clock(), &Hooked(OtherDevice("mnt")))
            .expect("the plan is made");
        let planned: Vec<String> = report
            .plan
            .expect("a plan")
            .actions
            .iter()
            .map(|action| match action {
                ArchiveAction::Archive { path, .. }
                | ArchiveAction::Reclaim { path, .. }
                | ArchiveAction::Replace { path, .. }
                | ArchiveAction::Ask { path, .. } => path.to_string(),
            })
            .collect();
        assert_eq!(planned, ["sibling"]);
    }

    #[test]
    fn a_piece_holding_a_collection_is_not_planned() {
        let mut world = world();
        world.file("n/note.txt", 30);
        world.temp.tree().dir("n/inner/.antiq");
        only_the_sibling(world);
    }

    #[test]
    fn a_piece_holding_an_exclude_entry_is_not_planned() {
        let mut world = world();
        world.configure("exclude = [\"x/skip\"]");
        world.file("x/note.txt", 30);
        world.temp.tree().dir("x/skip");
        only_the_sibling(world);
    }

    #[test]
    fn a_young_piece_is_not_planned() {
        let mut world = world();
        world.file("busy/note.txt", 3);
        only_the_sibling(world);
    }

    #[test]
    fn a_live_piece_unearthed_two_days_ago_is_not_planned() {
        let mut world = world();
        world.file("work/note.txt", 30);
        live_piece(&world, 1, "work", COLLECTION_ID, |entry| {
            entry.unearthed = Some("2026-10-30T10:00:00Z".parse().expect("a time"));
        });
        only_the_sibling(world);
    }

    #[test]
    fn a_diverged_live_piece_is_not_planned() {
        let mut world = world();
        world.file("work/note.txt", 30);
        live_piece(&world, 1, "work", COLLECTION_ID, |entry| {
            entry.diverged = true;
        });
        only_the_sibling(world);
    }

    #[test]
    fn a_live_piece_of_another_collection_is_not_planned() {
        let world = world();
        live_piece(&world, 1, "theirs", "01D39ZY06FGSCTVN4T2V9PKHFZ", |_| {});
        // Another collection's piece has no entry of ours.
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1));
        fs::remove_file(world.collection.tracking_path(id)).expect("the entry exists");
        only_the_sibling(world);
    }

    // =========================================================
    // A dry run changes nothing
    // =========================================================

    /// Every path below `root` with its size and modification time.
    fn snapshot(root: &Path) -> Vec<(String, u64, i64)> {
        use std::os::unix::fs::MetadataExt;
        let mut found = Vec::new();
        let mut open = vec![root.to_path_buf()];
        while let Some(directory) = open.pop() {
            for entry in fs::read_dir(&directory).expect("a directory") {
                let path = entry.expect("an entry").path();
                let metadata = fs::symlink_metadata(&path).expect("metadata");
                found.push((path.display().to_string(), metadata.len(), metadata.mtime()));
                if metadata.is_dir() {
                    open.push(path);
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn a_dry_run_writes_nothing_below_antiq() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let before = snapshot(world.temp.root());
        let _ = world.plan();
        assert_eq!(snapshot(world.temp.root()), before);
    }

    #[test]
    fn a_dry_run_names_the_lock_holder() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let _lock = crate::lock::acquire(&world.collection.lock_path(), "laptop", &clock())
            .expect("the lock is free");
        let warnings = world.plan().warnings;
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("another run holds the lock")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_junk_entry_that_is_no_file_name_warns_once_in_a_dry_run() {
        let mut world = world();
        world.file("a/note.txt", 30);
        world.configure("junk = [\"a/b\"]");
        let warnings = world.plan().warnings;
        let count = warnings
            .iter()
            .filter(|warning| warning.message.contains("collection.junk"))
            .count();
        assert_eq!(count, 1, "{warnings:?}");
    }

    // =========================================================
    // Sizes
    // =========================================================

    #[test]
    fn the_action_carries_the_apparent_size_in_bytes() {
        let mut world = world();
        let tree = world.temp.tree();
        tree.file_with("a/one.txt", "12345", 0o644, days_ago(30));
        tree.file_with("a/two.txt", "123", 0o644, days_ago(30));
        let actions = world.plan().plan.expect("a plan").actions;
        assert_eq!(
            actions,
            [ArchiveAction::Archive {
                path: RelPath::parse("a").expect("valid"),
                id: None,
                size: 8
            }]
        );
        assert_eq!(actions[0].describe(), "archive a (8 B)");
    }

    #[test]
    fn apparent_size_counts_a_hard_linked_file_once() {
        let mut world = world();
        let tree = world.temp.tree();
        tree.file_with("a/one.txt", "12345", 0o644, days_ago(30));
        tree.hard_link("a/one.txt", "a/two.txt");
        let actions = world.plan().plan.expect("a plan").actions;
        assert!(
            matches!(&actions[0], ArchiveAction::Archive { size: 5, .. }),
            "{actions:?}"
        );
    }

    #[test]
    fn sizes_are_written_in_decimal_units() {
        let cases = [
            (0, "0 B"),
            (999, "999 B"),
            (1_000, "1.0 kB"),
            (1_234, "1.2 kB"),
            (9_960_000, "10 MB"),
            (88_000_000, "88 MB"),
            (999_600, "1.0 MB"),
            (412_000_000, "412 MB"),
            (1_600_000_000, "1.6 GB"),
        ];
        for (bytes, text) in cases {
            assert_eq!(size_text(bytes), text, "{bytes} bytes");
        }
    }

    #[test]
    fn a_dry_run_serialises_dry_run_actions_and_warnings_in_that_order() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let text = serde_json::to_string(&world.plan()).expect("the report serialises");
        assert_eq!(
            text,
            r#"{"dry_run":true,"actions":[{"action":"archive","path":"a","id":null,"size":1}],"warnings":[]}"#
        );
    }

    #[test]
    fn an_unfinished_row_serialises_its_place_as_where() {
        let row = Unfinished {
            id: ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_000, 7)),
            path: RelPath::parse("a").expect("valid"),
            place: UnfinishedWhere::InPlace,
            message: "the entry stays removing".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&row).expect("the row serialises"),
            r#"{"id":"01EJ3PX0000000000000000007","path":"a","where":"in_place","message":"the entry stays removing"}"#
        );
    }
}
