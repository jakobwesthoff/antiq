// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `resolve` command.

pub mod conflict;
pub mod diverged;
pub mod replace;
pub mod superseded;

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::{Error, ErrorKind, HasKind};
use crate::id::{IdSource, ObjectId};
use crate::index::Index;
use crate::lock::Holder;
use crate::ops::archive::{
    Archived, Asker, Forked, Kept, RealSeams, Reclaimed, Seams, Skipped, Unfinished, size_text,
};
use crate::pack::pool::Pool;
use crate::plan::{Plan, PlanAction};
use crate::reference;
use crate::relpath::RelPath;
use crate::report::{CommandReport, Warning};
use crate::run::{self, Started};
use crate::tracking::{self, Answer, DecisionKind, TrackingEntry};
use serde::Serialize;
use std::ffi::OsStr;
use std::path::Path;

/// Errors of the `resolve` command.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("{0} is not supported yet")]
    Unsupported(String),
    /// The answer fits none of the decisions the piece waits on. The
    /// message names them with their answers.
    #[error("{0}")]
    DoesNotFit(String),
    #[error("{path} is pinned")]
    Pinned { path: RelPath },
}

impl HasKind for ResolveError {
    fn kind(&self) -> ErrorKind {
        match self {
            ResolveError::Unsupported(_) => ErrorKind::Unsupported,
            ResolveError::DoesNotFit(_) => ErrorKind::Usage,
            ResolveError::Pinned { .. } => ErrorKind::Other,
        }
    }
}

/// What to do.
#[derive(Debug, Clone, Copy)]
pub enum Task<'a> {
    /// Go through the pending decisions of the piece, or of every piece:
    /// ask about each when there is someone to ask, else list them.
    Walk { piece: Option<&'a OsStr> },
    /// Answer the decision of the piece the answer fits.
    Answer { piece: &'a OsStr, answer: Answer },
}

/// What a run of `resolve` needs.
pub struct ResolveRequest<'a> {
    pub collection: &'a Collection,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdSource,
    pub pool: &'a Pool,
    /// Whoever answers questions; `None` without a terminal and under
    /// `--json`.
    pub interaction: Option<&'a Asker>,
    /// The directory relative piece references start at.
    pub cwd: &'a Path,
    pub task: Task<'a>,
    pub dry_run: bool,
}

/// A decision a piece waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingRow {
    pub id: ObjectId,
    pub path: RelPath,
    pub decision: DecisionKind,
    pub options: Vec<Answer>,
}

/// The decision that was executed and the answer that did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub kind: DecisionKind,
    pub answer: Answer,
}

/// What a run of `resolve` did. A list is `None` where the run could not
/// have filled it: a listing has no archived pieces, an answer lists no
/// pending decisions.
#[derive(Debug, Default, Serialize)]
pub struct ResolveReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<Vec<Archived>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaimed: Option<Vec<Reclaimed>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<Skipped>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<Vec<Kept>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unfinished: Option<Vec<Unfinished>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked: Option<Vec<Forked>>,
    /// The decisions still pending after the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<PendingRow>>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for ResolveReport {
    fn partial(&self) -> bool {
        let any = |list: Option<usize>| list.is_some_and(|len| len > 0);
        any(self.skipped.as_ref().map(Vec::len))
            || any(self.unfinished.as_ref().map(Vec::len))
            || any(self.pending.as_ref().map(Vec::len))
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// One step of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ResolveAction {
    Reclaim {
        path: RelPath,
        id: ObjectId,
        size: u64,
    },
    Replace {
        path: RelPath,
        id: ObjectId,
        size: u64,
        lost_count: usize,
    },
    Fork {
        path: RelPath,
        id: ObjectId,
    },
    Skip {
        path: RelPath,
        id: ObjectId,
    },
}

impl PlanAction for ResolveAction {
    fn describe(&self) -> String {
        match self {
            ResolveAction::Reclaim { path, size, .. } => {
                format!("reclaim {path} ({})", size_text(*size))
            }
            ResolveAction::Replace {
                path,
                size,
                lost_count,
                ..
            } => format!(
                "replace the archive of {path}, losing {lost_count} {} ({})",
                if *lost_count == 1 { "entry" } else { "entries" },
                size_text(*size)
            ),
            ResolveAction::Fork { path, .. } => format!("fork {path}"),
            ResolveAction::Skip { path, .. } => format!("skip the decision about {path}"),
        }
    }
}

/// The result of [`resolve`].
#[derive(Debug)]
pub enum Resolved {
    /// A dry run: what would be done, what to tell about it and the run that
    /// holds the lock.
    Planned {
        plan: Plan<ResolveAction>,
        notes: Vec<Warning>,
        holder: Option<Holder>,
    },
    Done(ResolveReport),
}

/// The decisions in the order of the table in spec "Decisions", which is
/// the order an answer that fits several of them is tried in.
const TABLE: [DecisionKind; 4] = [
    DecisionKind::VersionConflict,
    DecisionKind::ReplacingWithLoss,
    DecisionKind::DivergedFolder,
    DecisionKind::SupersededArchive,
];

/// The name of a decision as the table in spec "Decisions" gives it.
pub fn label(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::VersionConflict => "version conflict",
        DecisionKind::ReplacingWithLoss => "replacing with loss",
        DecisionKind::DivergedFolder => "diverged folder",
        DecisionKind::SupersededArchive => "superseded archive",
    }
}

/// The command-line flag of an answer.
pub fn flag(answer: Answer) -> &'static str {
    match answer {
        Answer::Mine => "--mine",
        Answer::Theirs => "--theirs",
        Answer::Fork => "--fork",
        Answer::Replace => "--replace",
        Answer::Skip => "--skip",
        Answer::DropSuperseded => "--drop-superseded",
    }
}

/// What the routing of an answer needs besides the entry.
pub(crate) struct Ctx<'a, 'r> {
    pub(crate) request: &'r ResolveRequest<'a>,
    pub(crate) seams: &'r dyn Seams,
}

/// Goes through the pending decisions or executes one answer.
pub fn resolve(request: &ResolveRequest<'_>) -> Result<Resolved, Error> {
    resolve_with(request, &RealSeams)
}

pub(crate) fn resolve_with(
    request: &ResolveRequest<'_>,
    seams: &dyn Seams,
) -> Result<Resolved, Error> {
    let collection = request.collection;
    // The repairs of a real run come first: they can change what a piece
    // waits on.
    let started = run::start(collection, request.clock, request.dry_run)?;
    let index = Index::compute(collection)?;
    let (given, answer) = match request.task {
        Task::Walk { piece } => (piece, None),
        Task::Answer { piece, answer } => (Some(piece), Some(answer)),
    };
    let target = given
        .map(|given| reference::resolve(collection, &index, request.cwd, given))
        .transpose()?;
    let pending = pending_entries(collection, target)?;
    let ctx = Ctx { request, seams };

    match (answer, started) {
        (Some(answer), started) => {
            let (entry, kind) = fit(pending.into_iter().next(), answer)?;
            match kind {
                DecisionKind::ReplacingWithLoss => replace::route(&ctx, started, entry, answer),
                DecisionKind::VersionConflict => Err(conflict::answer(answer).into()),
                DecisionKind::DivergedFolder => Err(diverged::answer(answer).into()),
                DecisionKind::SupersededArchive => Err(superseded::answer(answer).into()),
            }
        }
        (None, Started::Guarded(guard)) if request.interaction.is_some() => {
            let mut report = ResolveReport {
                archived: Some(Vec::new()),
                reclaimed: Some(Vec::new()),
                skipped: Some(Vec::new()),
                kept: Some(Vec::new()),
                unfinished: Some(Vec::new()),
                forked: Some(Vec::new()),
                pending: Some(Vec::new()),
                warnings: guard.warnings().to_vec(),
                ..ResolveReport::default()
            };
            for entry in pending {
                for decision in entry.pending_decisions.clone() {
                    if decision.kind == DecisionKind::ReplacingWithLoss {
                        replace::walk(&ctx, guard.lock(), &entry, &mut report)?;
                    } else {
                        report.warnings.push(Warning::new(format!(
                            "`{}` waits on a {}, which cannot be resolved yet",
                            entry.path,
                            label(decision.kind)
                        )));
                        report
                            .pending
                            .get_or_insert_with(Vec::new)
                            .push(row_of(&entry, decision.kind));
                    }
                }
            }
            Ok(Resolved::Done(report))
        }
        (None, started) => {
            let warnings = match started {
                Started::Guarded(guard) => guard.warnings().to_vec(),
                Started::DryRun { .. } => Vec::new(),
            };
            let rows = pending
                .iter()
                .flat_map(|entry| {
                    entry
                        .pending_decisions
                        .iter()
                        .map(|decision| row_of(entry, decision.kind))
                })
                .collect();
            Ok(Resolved::Done(ResolveReport {
                pending: Some(rows),
                warnings,
                ..ResolveReport::default()
            }))
        }
    }
}

fn row_of(entry: &TrackingEntry, kind: DecisionKind) -> PendingRow {
    PendingRow {
        id: entry.id,
        path: entry.path.clone(),
        decision: kind,
        options: kind.options().to_vec(),
    }
}

/// The entries that wait on a decision, in path order: the one of the
/// piece `target`, or every one.
fn pending_entries(
    collection: &Collection,
    target: Option<ObjectId>,
) -> Result<Vec<TrackingEntry>, Error> {
    let entries = match target {
        Some(id) => tracking::read(collection, id)?.into_iter().collect(),
        None => tracking::read_all(collection)?.entries,
    };
    let mut waiting: Vec<TrackingEntry> = entries
        .into_iter()
        .filter(|entry| !entry.pending_decisions.is_empty())
        .collect();
    waiting.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(waiting)
}

/// The decision of `entry` that `answer` answers: the first in the order
/// of the table whose answers include it.
fn fit(
    entry: Option<TrackingEntry>,
    answer: Answer,
) -> Result<(TrackingEntry, DecisionKind), ResolveError> {
    let Some(entry) = entry else {
        return Err(ResolveError::DoesNotFit(format!(
            "the piece has no pending decision to answer with {}",
            flag(answer)
        )));
    };
    let waits_on = |kind: DecisionKind| {
        entry
            .pending_decisions
            .iter()
            .any(|decision| decision.kind == kind)
    };
    let fitting = TABLE
        .into_iter()
        .find(|kind| waits_on(*kind) && kind.options().contains(&answer));
    match fitting {
        Some(kind) => Ok((entry, kind)),
        None => {
            let waiting: Vec<String> = TABLE
                .into_iter()
                .filter(|kind| waits_on(*kind))
                .map(|kind| {
                    let options: Vec<&str> = kind.options().iter().map(|a| flag(*a)).collect();
                    format!("{} ({})", label(kind), options.join(", "))
                })
                .collect();
            Err(ResolveError::DoesNotFit(format!(
                "{} answers none of the decisions `{}` waits on: {}",
                flag(answer),
                entry.path,
                waiting.join("; ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{IdSource, SeqIds};
    use crate::interact::ScriptedInteraction;
    use crate::ops::archive::unearthed::tests::{At, FILE_TIME, PIECE, Scene, TITLE, asker, scene};
    use crate::ops::archive::{RealSeams, SkipReason, Step};
    use crate::ops::unearth::tests::{ID, id, path};
    use crate::testing::{rerun_while_lock_unattributed, shared_pool};
    use crate::tracking::PendingDecision;
    use std::ffi::OsString;
    use std::fs;
    use std::io;

    /// A piece that lost `notes.txt` and waits for the decision about it.
    fn undecided() -> Scene {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        scene.run(&RealSeams);
        assert_eq!(scene.entry().pending_decisions.len(), 1);
        scene
    }

    fn request<'a>(
        scene: &'a Scene,
        ids: &'a SeqIds,
        interaction: Option<&'a Asker>,
        task: Task<'a>,
        dry_run: bool,
    ) -> ResolveRequest<'a> {
        ResolveRequest {
            collection: &scene.collection,
            clock: &scene.clock,
            ids,
            pool: shared_pool(),
            interaction,
            cwd: scene.temp.root(),
            task,
            dry_run,
        }
    }

    fn answer_with(scene: &Scene, answer: Answer, seams: &dyn Seams) -> Result<Resolved, Error> {
        let ids = SeqIds::new(1);
        let piece = OsString::from(PIECE);
        rerun_while_lock_unattributed(|| {
            resolve_with(
                &request(
                    scene,
                    &ids,
                    None,
                    Task::Answer {
                        piece: &piece,
                        answer,
                    },
                    false,
                ),
                seams,
            )
        })
    }

    fn done(resolved: Result<Resolved, Error>) -> ResolveReport {
        match resolved.expect("the run finishes") {
            Resolved::Done(report) => report,
            other => panic!("expected a report, got {other:?}"),
        }
    }

    fn answered(scene: &Scene, answer: Answer) -> ResolveReport {
        done(answer_with(scene, answer, &RealSeams))
    }

    fn walked(
        scene: &Scene,
        piece: Option<&str>,
        interaction: Option<&Asker>,
        dry_run: bool,
    ) -> Result<Resolved, Error> {
        let ids = SeqIds::new(1);
        let piece = piece.map(OsString::from);
        rerun_while_lock_unattributed(|| {
            resolve_with(
                &request(
                    scene,
                    &ids,
                    interaction,
                    Task::Walk {
                        piece: piece.as_deref(),
                    },
                    dry_run,
                ),
                &RealSeams,
            )
        })
    }

    fn kind_of(error: Error) -> ErrorKind {
        error.kind()
    }

    fn resolution(kind: DecisionKind, answer: Answer) -> Option<Resolution> {
        Some(Resolution { kind, answer })
    }

    fn replaced() -> Option<Resolution> {
        resolution(DecisionKind::ReplacingWithLoss, Answer::Replace)
    }

    fn rows<T>(list: &Option<Vec<T>>) -> &[T] {
        list.as_deref().expect("the run lists them")
    }

    fn last_action(scene: &Scene) -> String {
        scene
            .metadata()
            .history
            .last()
            .expect("a history")
            .action
            .clone()
    }

    // =========================================================
    // Answers
    // =========================================================

    #[test]
    fn replace_archives_the_folder_with_the_loss_it_has_now() {
        let scene = undecided();
        // The loss has grown since the decision was recorded.
        fs::remove_file(scene.folder().join("src/main.rs")).expect("removed");
        fs::write(scene.folder().join("keep.txt"), "k").expect("a file");
        filetime::set_file_mtime(
            scene.folder().join("keep.txt"),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");
        filetime::set_file_mtime(
            scene.folder().join("src"),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");
        filetime::set_file_mtime(
            scene.folder(),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");

        let report = answered(&scene, Answer::Replace);

        assert_eq!(report.resolved, replaced());
        let archived = rows(&report.archived);
        assert_eq!(archived.len(), 1, "{report:?}");
        assert_eq!(archived[0].payload_version, 2);
        assert!(!scene.folder().exists());
        assert!(scene.entry().pending_decisions.is_empty());
        assert_eq!(last_action(&scene), "resolve");
        assert!(!report.partial());
    }

    #[test]
    fn replace_reclaims_a_piece_that_equals_its_archive_again() {
        let scene = undecided();
        fs::write(scene.folder().join("notes.txt"), "hello").expect("restored");
        filetime::set_file_mtime(
            scene.folder().join("notes.txt"),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");
        filetime::set_file_mtime(
            scene.folder(),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");

        let report = answered(&scene, Answer::Replace);

        assert_eq!(report.resolved, replaced());
        assert_eq!(rows(&report.reclaimed).len(), 1, "{report:?}");
        assert!(rows(&report.archived).is_empty());
        assert!(!scene.folder().exists());
        assert!(scene.entry().pending_decisions.is_empty());
    }

    #[test]
    fn a_piece_in_use_is_left_for_the_next_archive() {
        let scene = undecided();
        let before = (scene.metadata(), scene.entry());
        // The clock is at 2026-11-04T12:00:00Z.
        filetime::set_file_mtime(
            scene.folder().join("src/main.rs"),
            filetime::FileTime::from_unix_time(1_793_707_200 - 3_600, 0),
        )
        .expect("the time is set");

        let report = answered(&scene, Answer::Replace);

        assert_eq!(report.resolved, None);
        assert!(rows(&report.archived).is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message == "the piece is in use; the next archive takes it"),
            "{:?}",
            report.warnings
        );
        assert!(!report.partial());
        assert_eq!((scene.metadata(), scene.entry()), before);
        assert!(scene.folder().exists());
    }

    #[test]
    fn a_pinned_piece_is_refused() {
        let scene = undecided();
        let root = crate::fsx::tree::TreeRoot::open(scene.temp.root()).expect("opens");
        crate::pin::create(&root, &path(PIECE)).expect("pinned");
        let before = (scene.metadata(), scene.entry());

        let error = answer_with(&scene, Answer::Replace, &RealSeams).expect_err("refused");

        assert!(matches!(error, Error::Resolve(ResolveError::Pinned { .. })));
        assert_eq!(kind_of(error), ErrorKind::Other);
        assert_eq!((scene.metadata(), scene.entry()), before);
    }

    #[test]
    fn fork_gives_the_folder_a_new_id() {
        let scene = undecided();

        let report = answered(&scene, Answer::Fork);

        assert_eq!(
            report.resolved,
            resolution(DecisionKind::ReplacingWithLoss, Answer::Fork)
        );
        let forked = rows(&report.forked);
        assert_eq!(forked.len(), 1, "{report:?}");
        assert_eq!(forked[0].from, id(ID));
        assert!(scene.folder().exists());
        assert!(scene.entry().pending_decisions.is_empty());
        assert!(!report.partial());
    }

    #[test]
    fn skip_resolves_the_decision_and_succeeds() {
        let scene = undecided();
        let before = (scene.metadata(), scene.folder().exists());

        let report = answered(&scene, Answer::Skip);

        assert_eq!(
            report.resolved,
            resolution(DecisionKind::ReplacingWithLoss, Answer::Skip)
        );
        assert!(rows(&report.skipped).is_empty(), "{report:?}");
        assert!(!report.partial(), "skip exits 0");
        assert!(scene.entry().pending_decisions.is_empty());
        assert_eq!((scene.metadata(), scene.folder().exists()), before);
    }

    #[test]
    fn an_answer_that_fits_no_pending_decision_is_a_usage_error() {
        let scene = undecided();
        let before = scene.entry();

        let error = answer_with(&scene, Answer::Mine, &RealSeams).expect_err("does not fit");

        assert_eq!(kind_of(error), ErrorKind::Usage);
        let error = answer_with(&scene, Answer::Mine, &RealSeams).expect_err("does not fit");
        let message = error.to_string();
        assert!(
            message.contains("replacing with loss") && message.contains("--replace"),
            "{message}"
        );
        assert_eq!(scene.entry(), before);
    }

    #[test]
    fn a_piece_that_waits_on_nothing_takes_no_answer() {
        let scene = scene();

        let error = answer_with(&scene, Answer::Replace, &RealSeams).expect_err("nothing pending");

        assert_eq!(kind_of(error), ErrorKind::Usage);
        let error = answer_with(&scene, Answer::Replace, &RealSeams).expect_err("nothing pending");
        assert!(error.to_string().contains("no pending decision"), "{error}");
    }

    fn deciding(scene: &Scene, kind: DecisionKind) {
        scene.edit_entry(|entry| entry.pending_decisions = vec![PendingDecision { kind }]);
    }

    #[test]
    fn a_version_conflict_is_not_resolved_yet() {
        let scene = scene();
        deciding(&scene, DecisionKind::VersionConflict);
        let error = answer_with(&scene, Answer::Mine, &RealSeams).expect_err("unsupported");
        assert_eq!(kind_of(error), ErrorKind::Unsupported);
    }

    #[test]
    fn a_diverged_folder_is_not_resolved_yet() {
        let scene = scene();
        deciding(&scene, DecisionKind::DivergedFolder);
        let error = answer_with(&scene, Answer::Theirs, &RealSeams).expect_err("unsupported");
        assert_eq!(kind_of(error), ErrorKind::Unsupported);
    }

    #[test]
    fn a_superseded_archive_is_not_dropped_yet() {
        let scene = scene();
        deciding(&scene, DecisionKind::SupersededArchive);
        let error =
            answer_with(&scene, Answer::DropSuperseded, &RealSeams).expect_err("unsupported");
        assert_eq!(kind_of(error), ErrorKind::Unsupported);
    }

    #[test]
    fn an_answer_that_fits_two_decisions_goes_to_the_first_in_the_table() {
        let scene = undecided();
        scene.edit_entry(|entry| {
            entry.pending_decisions = vec![
                PendingDecision {
                    kind: DecisionKind::ReplacingWithLoss,
                },
                PendingDecision {
                    kind: DecisionKind::VersionConflict,
                },
            ];
        });

        // `--fork` answers a version conflict and a replacement; the
        // conflict comes first in the table.
        let error = answer_with(&scene, Answer::Fork, &RealSeams).expect_err("unsupported");

        assert_eq!(kind_of(error), ErrorKind::Unsupported);
        assert!(scene.folder().exists());
    }

    // =========================================================
    // Each outcome of the archive step
    // =========================================================

    #[test]
    fn a_kept_folder_is_reported_as_kept() {
        let scene = undecided();
        let seams = At(|at| match at {
            Step::Rewalk => Err(io::Error::other("the walk fails")),
            _ => Ok(()),
        });

        let report = done(answer_with(&scene, Answer::Replace, &seams));

        assert_eq!(rows(&report.kept).len(), 1, "{report:?}");
        assert_eq!(report.resolved, replaced());
        assert!(scene.folder().exists());
    }

    #[test]
    fn a_removal_that_stops_is_reported_as_unfinished_and_partial() {
        let scene = undecided();
        let seams = At(|at| match at {
            Step::DeleteInTrash => Err(io::Error::from_raw_os_error(libc::EIO)),
            _ => Ok(()),
        });

        let report = done(answer_with(&scene, Answer::Replace, &seams));

        assert_eq!(rows(&report.unfinished).len(), 1, "{report:?}");
        assert!(report.partial());
    }

    #[test]
    fn a_piece_that_cannot_be_trusted_is_reported_as_skipped_and_partial() {
        let scene = undecided();
        fs::write(scene.folder().join("notes.txt"), "hello").expect("restored");
        filetime::set_file_mtime(
            scene.folder().join("notes.txt"),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");
        filetime::set_file_mtime(
            scene.folder(),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");
        fs::remove_file(scene.object_file(&scene.payload_file())).expect("removed");

        let report = answered(&scene, Answer::Replace);

        let skipped = rows(&report.skipped);
        assert_eq!(skipped.len(), 1, "{report:?}");
        assert_eq!(skipped[0].reason, SkipReason::PendingRemote);
        assert!(report.partial());
    }

    // =========================================================
    // Listing and walking
    // =========================================================

    fn pending_paths(report: &ResolveReport) -> Vec<String> {
        rows(&report.pending)
            .iter()
            .map(|row| row.path.to_string())
            .collect()
    }

    #[test]
    fn without_someone_to_ask_the_pending_decisions_are_listed() {
        let scene = undecided();
        let before = scene.entry();

        let report = done(walked(&scene, None, None, false));

        assert_eq!(pending_paths(&report), [PIECE]);
        let row = &rows(&report.pending)[0];
        assert_eq!(row.id, id(ID));
        assert_eq!(row.decision, DecisionKind::ReplacingWithLoss);
        assert_eq!(row.options, [Answer::Replace, Answer::Fork, Answer::Skip]);
        assert!(report.partial());
        assert_eq!(report.resolved, None);
        assert_eq!(scene.entry(), before);
    }

    #[test]
    fn a_piece_without_an_answer_lists_its_own_decisions() {
        let scene = undecided();

        let report = done(walked(&scene, Some(PIECE), None, false));

        assert_eq!(pending_paths(&report), [PIECE]);
    }

    #[test]
    fn nothing_pending_is_an_empty_listing() {
        let scene = scene();

        let report = done(walked(&scene, None, None, false));

        assert!(rows(&report.pending).is_empty());
        assert!(!report.partial());
    }

    #[test]
    fn a_dry_run_of_a_walk_lists_and_changes_nothing() {
        let scene = undecided();
        let before = (scene.entry(), scene.metadata());
        // A dry run asks nobody; a panicking answerer proves it.
        let asked = asker(crate::interact::NonInteractive);
        let report = done(walked(&scene, None, Some(&asked), true));

        assert_eq!(pending_paths(&report), [PIECE]);
        assert_eq!((scene.entry(), scene.metadata()), before);
    }

    #[test]
    fn a_walk_asks_about_each_decision_and_executes_the_answer() {
        let scene = undecided();
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "replace");
        let asked = asker(script);

        let report = done(walked(&scene, None, Some(&asked), false));

        assert_eq!(report.resolved, replaced());
        assert_eq!(rows(&report.archived).len(), 1, "{report:?}");
        assert!(rows(&report.pending).is_empty());
        assert_eq!(last_action(&scene), "resolve");
    }

    #[test]
    fn a_skip_in_a_walk_resolves_the_decision_and_succeeds() {
        let scene = undecided();
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "skip");
        let asked = asker(script);

        let report = done(walked(&scene, Some(PIECE), Some(&asked), false));

        assert!(rows(&report.skipped).is_empty(), "{report:?}");
        assert!(!report.partial());
        assert!(scene.entry().pending_decisions.is_empty());
    }

    #[test]
    fn a_walk_nobody_answers_leaves_the_decision_pending() {
        let scene = undecided();
        let mut script = ScriptedInteraction::new();
        script.expect_no_answer(TITLE);
        let asked = asker(script);

        let report = done(walked(&scene, None, Some(&asked), false));

        assert_eq!(pending_paths(&report), [PIECE]);
        assert!(report.partial());
        assert_eq!(scene.entry().pending_decisions.len(), 1);
    }

    #[test]
    fn a_walk_leaves_a_decision_it_cannot_resolve_yet_with_a_note() {
        let scene = scene();
        deciding(&scene, DecisionKind::VersionConflict);
        let asked = asker(ScriptedInteraction::new());

        let report = done(walked(&scene, None, Some(&asked), false));

        assert_eq!(pending_paths(&report), [PIECE]);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("version conflict")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_walk_leaves_a_pinned_piece_pending_with_a_note() {
        let scene = undecided();
        let root = crate::fsx::tree::TreeRoot::open(scene.temp.root()).expect("opens");
        crate::pin::create(&root, &path(PIECE)).expect("pinned");
        let asked = asker(ScriptedInteraction::new());

        let report = done(walked(&scene, None, Some(&asked), false));

        assert_eq!(pending_paths(&report), [PIECE]);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("is pinned")),
            "{:?}",
            report.warnings
        );
        assert_eq!(scene.entry().pending_decisions.len(), 1);
    }

    // =========================================================
    // The dry run of an answer
    // =========================================================

    fn planned(scene: &Scene, answer: Answer) -> Vec<ResolveAction> {
        let ids = SeqIds::new(1);
        let piece = OsString::from(PIECE);
        let resolved = rerun_while_lock_unattributed(|| {
            resolve_with(
                &request(
                    scene,
                    &ids,
                    None,
                    Task::Answer {
                        piece: &piece,
                        answer,
                    },
                    true,
                ),
                &RealSeams,
            )
        })
        .expect("the plan is made");
        match resolved {
            Resolved::Planned { plan, .. } => plan.actions,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn a_dry_run_replace_names_the_loss_and_writes_nothing() {
        let scene = undecided();
        let before = (scene.entry(), scene.metadata());

        let actions = planned(&scene, Answer::Replace);

        assert_eq!(
            actions,
            [ResolveAction::Replace {
                path: path(PIECE),
                id: id(ID),
                size: 12,
                lost_count: 1,
            }]
        );
        assert_eq!((scene.entry(), scene.metadata()), before);
        assert!(scene.folder().exists());
    }

    #[test]
    fn a_dry_run_replace_of_an_unchanged_piece_reclaims() {
        let scene = scene();
        scene.edit_entry(|entry| {
            entry.pending_decisions = vec![PendingDecision {
                kind: DecisionKind::ReplacingWithLoss,
            }];
        });

        let actions = planned(&scene, Answer::Replace);

        assert_eq!(
            actions,
            [ResolveAction::Reclaim {
                path: path(PIECE),
                id: id(ID),
                size: 17,
            }]
        );
    }

    #[test]
    fn a_dry_run_fork_and_skip_name_the_piece() {
        let scene = undecided();
        assert_eq!(
            planned(&scene, Answer::Fork),
            [ResolveAction::Fork {
                path: path(PIECE),
                id: id(ID)
            }]
        );
        assert_eq!(
            planned(&scene, Answer::Skip),
            [ResolveAction::Skip {
                path: path(PIECE),
                id: id(ID)
            }]
        );
        assert_eq!(scene.entry().pending_decisions.len(), 1);
    }

    #[test]
    fn a_walk_answered_with_fork_forks_the_piece() {
        let scene = undecided();
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "fork");
        let asked = asker(script);

        let report = done(walked(&scene, None, Some(&asked), false));

        assert_eq!(
            report.resolved,
            resolution(DecisionKind::ReplacingWithLoss, Answer::Fork)
        );
        assert_eq!(rows(&report.forked).len(), 1, "{report:?}");
        assert!(scene.folder().exists());
    }

    #[test]
    fn a_fork_that_fails_is_an_error_and_changes_nothing() {
        let scene = undecided();
        // The ID the fork would mint is taken.
        let taken = SeqIds::new(1).mint(crate::clock::Clock::now(&scene.clock));
        fs::create_dir_all(scene.collection.object_dir(taken)).expect("an object directory");
        let before = (scene.entry(), scene.metadata());

        let error = answer_with(&scene, Answer::Fork, &RealSeams).expect_err("the fork fails");

        assert!(error.to_string().contains(&taken.to_string()), "{error}");
        assert_eq!((scene.entry(), scene.metadata()), before);
    }

    #[test]
    fn a_dry_run_replace_of_a_piece_that_lost_nothing_names_no_loss() {
        let scene = undecided();
        fs::write(scene.folder().join("notes.txt"), "hello").expect("restored");
        fs::write(scene.folder().join("added.txt"), "x").expect("added");
        for name in ["notes.txt", "added.txt"] {
            filetime::set_file_mtime(
                scene.folder().join(name),
                filetime::FileTime::from_unix_time(FILE_TIME, 0),
            )
            .expect("the time is set");
        }
        filetime::set_file_mtime(
            scene.folder(),
            filetime::FileTime::from_unix_time(FILE_TIME, 0),
        )
        .expect("the time is set");

        let actions = planned(&scene, Answer::Replace);

        assert_eq!(
            actions,
            [ResolveAction::Replace {
                path: path(PIECE),
                id: id(ID),
                size: 18,
                lost_count: 0,
            }]
        );
    }

    #[test]
    fn actions_say_what_they_do() {
        let lines: Vec<String> = [
            ResolveAction::Reclaim {
                path: path(PIECE),
                id: id(ID),
                size: 17,
            },
            ResolveAction::Replace {
                path: path(PIECE),
                id: id(ID),
                size: 12,
                lost_count: 2,
            },
            ResolveAction::Fork {
                path: path(PIECE),
                id: id(ID),
            },
            ResolveAction::Skip {
                path: path(PIECE),
                id: id(ID),
            },
        ]
        .iter()
        .map(PlanAction::describe)
        .collect();
        assert_eq!(
            lines,
            [
                "reclaim projects/work (17 B)",
                "replace the archive of projects/work, losing 2 entries (12 B)",
                "fork projects/work",
                "skip the decision about projects/work",
            ]
        );
    }
}
