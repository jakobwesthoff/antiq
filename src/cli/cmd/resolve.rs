// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `resolve` command.

use super::archive::finished_lines;
use super::pin::reference_directory;
use super::status::{pad, sanitize};
use crate::cli::output::{Output, Style};
use crate::cli::{Context, ResolveArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::id::RandomIds;
use antiq::ops::archive::{ArchiveReport, Asker};
use antiq::ops::resolve::{
    PendingRow, ResolveReport, ResolveRequest, Resolved, Task, flag, label, resolve,
};
use antiq::pack::pool::Pool;
use antiq::report::{CommandReport, lock_note};
use antiq::tracking::Answer;
use anyhow::Context as _;
use std::path::PathBuf;
use std::sync::Mutex;
use unicode_width::UnicodeWidthStr;

pub fn run(ctx: &Context, args: &ResolveArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let cwd = reference_directory(ctx.directory.as_ref())?;
    let pool = Pool::for_machine(collection.config().collection.memory_percent())
        .map_err(antiq::Error::from)
        .context("size the compression pool")?;
    let asker: Option<Asker> = ctx.terminal().map(Mutex::new);
    let task = match (args.piece.as_deref(), answer_of(args)) {
        (Some(piece), Some(answer)) => Task::Answer { piece, answer },
        (piece, _) => Task::Walk { piece },
    };
    let request = ResolveRequest {
        collection: &collection,
        clock: &SystemClock,
        ids: &RandomIds,
        pool: &pool,
        interaction: asker.as_ref(),
        cwd: &cwd,
        task,
        dry_run: args.dry_run,
    };
    let resolved = resolve(&request).context("resolve the decisions")?;

    match resolved {
        Resolved::Planned {
            plan,
            notes,
            holder,
        } => {
            if ctx.json {
                out.json(Some("resolve"), &plan.dry_run())
                    .context("print the plan")?;
                return Ok(0);
            }
            for line in plan.lines() {
                out.line(&sanitize(&line)).context("print the plan")?;
            }
            for note in &notes {
                out.note(&format!("note: {}", sanitize(&note.message)))
                    .context("print the notes")?;
            }
            if let Some(holder) = holder {
                out.note(&format!("note: {}", sanitize(&lock_note(&holder).message)))
                    .context("print the notes")?;
            }
            Ok(0)
        }
        Resolved::Done(report) => {
            // Under `--json` the warnings are part of the object and
            // nothing goes to stderr.
            if ctx.json {
                out.json(Some("resolve"), &report)
                    .context("print the result")?;
            } else {
                for line in lines(&report, out.stdout_style()) {
                    out.line(&line).context("print the result")?;
                }
                for warning in report.warnings() {
                    out.note(&format!("note: {}", sanitize(&warning.message)))
                        .context("print the notes")?;
                }
            }
            Ok(if report.partial() { 3 } else { 0 })
        }
    }
}

/// The answer the flags give; the argument group allows one at most.
fn answer_of(args: &ResolveArgs) -> Option<Answer> {
    [
        (args.mine, Answer::Mine),
        (args.theirs, Answer::Theirs),
        (args.fork, Answer::Fork),
        (args.replace, Answer::Replace),
        (args.skip, Answer::Skip),
        (args.drop_superseded, Answer::DropSuperseded),
    ]
    .into_iter()
    .find_map(|(given, answer)| given.then_some(answer))
}

/// What a run prints on a console.
fn lines(report: &ResolveReport, style: Style) -> Vec<String> {
    let archive = ArchiveReport {
        dry_run: false,
        plan: None,
        archived: report.archived.clone(),
        reclaimed: report.reclaimed.clone(),
        skipped: report.skipped.clone(),
        kept: report.kept.clone(),
        unfinished: report.unfinished.clone(),
        forked: report.forked.clone(),
        needs_decision: None,
        warnings: Vec::new(),
    };
    let done = [
        report.archived.as_ref().map(Vec::len),
        report.reclaimed.as_ref().map(Vec::len),
        report.skipped.as_ref().map(Vec::len),
        report.kept.as_ref().map(Vec::len),
        report.unfinished.as_ref().map(Vec::len),
        report.forked.as_ref().map(Vec::len),
    ]
    .into_iter()
    .flatten()
    .any(|count| count > 0);

    let mut lines = Vec::new();
    if done {
        lines.extend(finished_lines(&archive, style));
    } else if let Some(resolved) = &report.resolved {
        lines.push(format!(
            "Resolved: {}, answered with {}",
            label(resolved.kind),
            flag(resolved.answer)
        ));
    }
    match report.pending.as_deref() {
        Some([]) | None => {}
        Some(pending) => {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.extend(pending_lines(pending));
        }
    }
    if lines.is_empty() {
        lines.push(
            if report.pending.is_some() {
                "Nothing to decide."
            } else {
                "Nothing to do."
            }
            .to_string(),
        );
    }
    lines
}

/// One line per pending decision: the piece, what it waits on and the
/// answers that fit.
fn pending_lines(pending: &[PendingRow]) -> Vec<String> {
    let paths: Vec<String> = pending
        .iter()
        .map(|row| sanitize(&row.path.to_string()))
        .collect();
    let width = paths.iter().map(|path| path.width()).max().unwrap_or(0);
    let noun = if pending.len() == 1 {
        "decision"
    } else {
        "decisions"
    };
    let mut lines = vec![format!("{} pending {noun}", pending.len()), String::new()];
    for (row, path) in pending.iter().zip(&paths) {
        let answers: Vec<&str> = row.options.iter().map(|answer| flag(*answer)).collect();
        let (last, rest) = answers.split_last().expect("a decision has answers");
        let choice = if rest.is_empty() {
            (*last).to_string()
        } else {
            format!("{} or {last}", rest.join(", "))
        };
        lines.push(format!(
            "  {}  {}: {choice}",
            pad(path, width),
            label(row.decision)
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::id::ObjectId;
    use antiq::ops::archive::{Archived, Forked, Skipped};
    use antiq::ops::resolve::Resolution;
    use antiq::relpath::RelPath;
    use antiq::tracking::DecisionKind;

    fn id(number: u64) -> ObjectId {
        ObjectId::from_ulid(ulid::Ulid::from_parts(
            1_700_000_000_000,
            u128::from(number),
        ))
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid")
    }

    fn style() -> Style {
        Style {
            width: 100,
            color: false,
        }
    }

    fn row(text: &str, decision: DecisionKind) -> PendingRow {
        PendingRow {
            id: id(1),
            path: path(text),
            decision,
            options: decision.options().to_vec(),
        }
    }

    fn empty() -> ResolveReport {
        ResolveReport::default()
    }

    #[test]
    fn pending_decisions_are_listed_with_their_answers() {
        let report = ResolveReport {
            pending: Some(vec![
                row("projects/a", DecisionKind::ReplacingWithLoss),
                row("b", DecisionKind::SupersededArchive),
                row("c", DecisionKind::DivergedFolder),
            ]),
            ..empty()
        };
        insta::assert_snapshot!(lines(&report, style()).join("\n"), @"
        3 pending decisions

          projects/a  replacing with loss: --replace, --fork or --skip
          b           superseded archive: --drop-superseded
          c           diverged folder: --theirs, --mine or --fork
        ");
    }

    #[test]
    fn one_pending_decision_is_counted_in_the_singular() {
        let report = ResolveReport {
            pending: Some(vec![row("a", DecisionKind::ReplacingWithLoss)]),
            ..empty()
        };
        assert_eq!(lines(&report, style())[0], "1 pending decision");
    }

    #[test]
    fn a_listing_of_nothing_says_there_is_nothing_to_decide() {
        let report = ResolveReport {
            pending: Some(Vec::new()),
            ..empty()
        };
        assert_eq!(lines(&report, style()), ["Nothing to decide."]);
    }

    #[test]
    fn an_answer_that_did_nothing_says_there_is_nothing_to_do() {
        let report = ResolveReport {
            archived: Some(Vec::new()),
            ..empty()
        };
        assert_eq!(lines(&report, style()), ["Nothing to do."]);
    }

    #[test]
    fn an_answer_without_rows_says_what_was_resolved() {
        let report = ResolveReport {
            resolved: Some(Resolution {
                kind: DecisionKind::ReplacingWithLoss,
                answer: Answer::Skip,
            }),
            archived: Some(Vec::new()),
            ..empty()
        };
        assert_eq!(
            lines(&report, style()),
            ["Resolved: replacing with loss, answered with --skip"]
        );
    }

    #[test]
    fn rows_and_what_is_still_pending_are_printed_together() {
        let report = ResolveReport {
            archived: Some(vec![Archived {
                id: id(2),
                path: path("projects/a"),
                size: 412_000_000,
                archive_size: 88_000_000,
                payload_version: 2,
            }]),
            forked: Some(vec![Forked {
                id: id(3),
                from: id(4),
                path: path("projects/b"),
            }]),
            skipped: Some(vec![Skipped {
                id: None,
                path: path("projects/c"),
                reason: antiq::ops::archive::SkipReason::Error,
                message: "bad".to_string(),
            }]),
            pending: Some(vec![row("projects/d", DecisionKind::ReplacingWithLoss)]),
            ..empty()
        };
        insta::assert_snapshot!(lines(&report, style()).join("\n"), @"
        archived 1 piece · 412 MB, 1 skipped, 1 forked

          projects/a  412 MB  removed  88 MB archive, verified
          projects/b       -  forked   the folder stays, new ID 01HF7YAT000000000000000003
          projects/c       -  skipped  bad

        1 pending decision

          projects/d  replacing with loss: --replace, --fork or --skip
        ");
    }

    #[test]
    fn answers_come_from_the_flags() {
        let mut args = ResolveArgs {
            piece: None,
            mine: false,
            theirs: false,
            fork: false,
            replace: false,
            skip: false,
            drop_superseded: false,
            force: false,
            dry_run: false,
        };
        assert_eq!(answer_of(&args), None);
        args.skip = true;
        assert_eq!(answer_of(&args), Some(Answer::Skip));
    }
}
