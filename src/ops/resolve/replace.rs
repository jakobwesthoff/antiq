// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Replacing an archive that loses content.
//!
//! The decision is never stored: every answer works out the loss again
//! from the folder as it is now, under the run lock.

use super::{
    Ctx, PendingRow, Resolution, ResolveAction, ResolveError, ResolveReport, Resolved, flag,
};
use crate::error::Error;
use crate::fsx::tree::TreeRoot;
use crate::lock::RunLock;
use crate::meta::HistoryAction;
use crate::ops::archive::new::{Mode, PieceOutcome, PieceResult, Run};
use crate::ops::archive::unearthed::fork_unearthed;
use crate::ops::archive::{ArchiveAction, Candidate, SkipReason, archive_one, nobody, plan_one};
use crate::ops::status::PieceKind;
use crate::pin;
use crate::report::Warning;
use crate::run::Started;
use crate::tracking::{self, Answer, DecisionKind, TrackingEntry};

const KIND: DecisionKind = DecisionKind::ReplacingWithLoss;

fn resolution(answer: Answer) -> Option<Resolution> {
    Some(Resolution { kind: KIND, answer })
}

/// A report that lists every outcome of the archive step as empty.
fn report_of(warnings: Vec<Warning>) -> ResolveReport {
    ResolveReport {
        archived: Some(Vec::new()),
        reclaimed: Some(Vec::new()),
        skipped: Some(Vec::new()),
        kept: Some(Vec::new()),
        unfinished: Some(Vec::new()),
        forked: Some(Vec::new()),
        warnings,
        ..ResolveReport::default()
    }
}

/// The piece of `entry` as the archive step takes it.
fn candidate_of(entry: &TrackingEntry) -> Candidate {
    Candidate {
        path: entry.path.clone(),
        id: Some(entry.id),
        kind: PieceKind::Unearthed,
        unearthed: entry.unearthed,
    }
}

/// Executes `answer` for `entry`, or plans it on a dry run.
pub(super) fn route(
    ctx: &Ctx<'_, '_>,
    started: Started,
    entry: TrackingEntry,
    answer: Answer,
) -> Result<Resolved, Error> {
    match started {
        Started::DryRun { holder } => {
            let (action, notes) = plan(ctx, &entry, answer)?;
            let mut plan = crate::plan::Plan::new();
            if let Some(action) = action {
                plan.push(action);
            }
            Ok(Resolved::Planned {
                plan,
                notes,
                holder,
            })
        }
        Started::Guarded(guard) => {
            let mut report = report_of(guard.warnings().to_vec());
            execute(ctx, guard.lock(), entry, answer, &mut report)?;
            Ok(Resolved::Done(report))
        }
    }
}

fn plan(
    ctx: &Ctx<'_, '_>,
    entry: &TrackingEntry,
    answer: Answer,
) -> Result<(Option<ResolveAction>, Vec<Warning>), Error> {
    let (path, id) = (entry.path.clone(), entry.id);
    if answer == Answer::Skip {
        return Ok((Some(ResolveAction::Skip { path, id }), Vec::new()));
    }
    if answer == Answer::Fork {
        return Ok((Some(ResolveAction::Fork { path, id }), Vec::new()));
    }
    let (action, notes) = plan_one(ctx.request.collection, ctx.seams, &candidate_of(entry))?;
    let action = action.map(|action| match action {
        ArchiveAction::Reclaim { path, id, size } => ResolveAction::Reclaim { path, id, size },
        ArchiveAction::Replace { path, id, size } => ResolveAction::Replace {
            path,
            id,
            size,
            lost_count: 0,
        },
        ArchiveAction::Ask {
            path,
            id,
            size,
            lost_count,
        } => ResolveAction::Replace {
            path,
            id,
            size,
            lost_count,
        },
        ArchiveAction::Archive { .. } => {
            unreachable!("an unearthed piece is reclaimed, replaced or asked about")
        }
    });
    Ok((action, notes))
}

/// Executes one answer. A pin refuses the archive step, which the answer
/// `replace` runs.
fn execute(
    ctx: &Ctx<'_, '_>,
    lock: &RunLock,
    mut entry: TrackingEntry,
    answer: Answer,
    report: &mut ResolveReport,
) -> Result<(), Error> {
    let request = ctx.request;
    if answer == Answer::Skip {
        entry.resolve_decision(KIND);
        tracking::write(request.collection, lock, request.clock, &mut entry)?;
        report.resolved = resolution(Answer::Skip);
        return Ok(());
    }
    if answer == Answer::Fork {
        let forked = fork_unearthed(
            request.collection,
            lock,
            request.clock,
            request.ids,
            entry.id,
        )?;
        report.warnings.extend(forked.warnings);
        report
            .forked
            .get_or_insert_with(Vec::new)
            .push(crate::ops::archive::Forked {
                id: forked.id,
                from: forked.from,
                path: forked.path,
            });
        report.resolved = resolution(Answer::Fork);
        return Ok(());
    }
    if is_pinned(ctx, &entry)? {
        return Err(ResolveError::Pinned { path: entry.path }.into());
    }
    let result = archive_step(ctx, lock, &entry, Some(Answer::Replace))?;
    fold(report, result);
    Ok(())
}

fn is_pinned(ctx: &Ctx<'_, '_>, entry: &TrackingEntry) -> Result<bool, Error> {
    let collection = ctx.request.collection;
    let root = TreeRoot::open(collection.root())
        .map_err(crate::fsx::io_error("open", collection.root()))?;
    Ok(pin::is_pinned(&root, &entry.path)?)
}

/// The archive step for the piece of `entry`, as `resolve` runs it: a new
/// archive gets the history action `resolve`.
fn archive_step(
    ctx: &Ctx<'_, '_>,
    lock: &RunLock,
    entry: &TrackingEntry,
    answer: Option<Answer>,
) -> Result<PieceResult, Error> {
    let request = ctx.request;
    let run = Run {
        collection: request.collection,
        lock,
        clock: request.clock,
        ids: request.ids,
        pool: request.pool,
        interaction: request.interaction.unwrap_or(nobody()),
    };
    let mode = Mode {
        history: HistoryAction::Resolve,
        answer,
    };
    Ok(archive_one(&run, ctx.seams, &candidate_of(entry), mode)?)
}

/// Files what the archive step did, and the answer it amounts to.
fn fold(report: &mut ResolveReport, result: PieceResult) {
    report.warnings.extend(result.warnings);
    match result.outcome {
        PieceOutcome::Dropped => report.warnings.push(Warning::new(
            "the piece is in use; the next archive takes it",
        )),
        PieceOutcome::Skipped(row) if row.reason == SkipReason::Declined => {
            report.resolved = resolution(Answer::Skip);
        }
        PieceOutcome::Skipped(row) => report.skipped.get_or_insert_with(Vec::new).push(row),
        PieceOutcome::Archived(row) => {
            report.archived.get_or_insert_with(Vec::new).push(row);
            report.resolved = resolution(Answer::Replace);
        }
        PieceOutcome::Reclaimed(row) => {
            report.reclaimed.get_or_insert_with(Vec::new).push(row);
            report.resolved = resolution(Answer::Replace);
        }
        PieceOutcome::Kept(row) => {
            report.kept.get_or_insert_with(Vec::new).push(row);
            report.resolved = resolution(Answer::Replace);
        }
        PieceOutcome::Unfinished(row) => {
            report.unfinished.get_or_insert_with(Vec::new).push(row);
            report.resolved = resolution(Answer::Replace);
        }
        PieceOutcome::Forked(row) => {
            report.forked.get_or_insert_with(Vec::new).push(row);
            report.resolved = resolution(Answer::Fork);
        }
        PieceOutcome::NeedsDecision(row) => {
            report
                .pending
                .get_or_insert_with(Vec::new)
                .push(PendingRow {
                    id: row.id,
                    path: row.path,
                    decision: row.decision,
                    options: row.options,
                })
        }
    }
}

/// Goes through the decision of `entry` by asking, the way `archive` asks.
/// A pinned piece is left pending with a note.
pub(super) fn walk(
    ctx: &Ctx<'_, '_>,
    lock: &RunLock,
    entry: &TrackingEntry,
    report: &mut ResolveReport,
) -> Result<(), Error> {
    if is_pinned(ctx, entry)? {
        report.warnings.push(Warning::new(format!(
            "`{}` is pinned, so {} cannot be used on it",
            entry.path,
            flag(Answer::Replace)
        )));
        report
            .pending
            .get_or_insert_with(Vec::new)
            .push(PendingRow {
                id: entry.id,
                path: entry.path.clone(),
                decision: KIND,
                options: KIND.options().to_vec(),
            });
        return Ok(());
    }
    let result = archive_step(ctx, lock, entry, None)?;
    fold(report, result);
    Ok(())
}
