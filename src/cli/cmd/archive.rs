// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` command: archives due pieces, or one piece now.

use super::progress;
use super::progress::{Description, Shown};
use crate::cli::console::cards::{Count, items, sizes};
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::{ArchiveArgs, Context};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::archive::{
    ArchivePlan, ArchiveReport, Outcome, Request, Target, archive, archive_observed,
};
use antiq::plan::{PlanAction, Step};
use antiq::progress::Operation;
use anyhow::Context as _;

/// The packing and the verifying of a piece count the content bytes of its
/// files, as the size on the card does. The removing of a piece counts the
/// entries of its listing, and every other operation counts files.
pub(super) const COUNTS: &[(Operation, Count)] = &[
    (
        Operation::Packing,
        Count {
            text: sizes,
            percent: true,
        },
    ),
    (
        Operation::Verifying,
        Count {
            text: sizes,
            percent: true,
        },
    ),
    (
        Operation::Removing,
        Count {
            text: |done, total| items(done, total, "entries"),
            percent: true,
        },
    ),
];

pub fn run(ctx: &Context, args: &ArchiveArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let target = match &args.piece {
        None => Target::Due,
        Some(reference) => Target::Piece {
            reference: reference.clone(),
            cwd: ctx.start.clone(),
        },
    };
    let request = Request {
        target,
        dry_run: args.dry_run,
    };
    // On a terminal the pieces show as cards while they work, and the lines
    // they leave behind replace the lines of the pipe.
    let (result, shown) = progress::watch(
        ctx,
        out,
        args.dry_run,
        Description::new(COUNTS),
        |observer| archive_observed(&collection, &SystemClock, &request, observer),
        || archive(&collection, &SystemClock, &request),
        |result| match result {
            Ok(Outcome::Archived(report)) => progress::archive_pieces(report),
            _ => Vec::new(),
        },
    )?;
    let outcome = result
        .map_err(antiq::Error::from)
        .context(if args.dry_run {
            "plan the archive"
        } else {
            "archive the pieces"
        })?;

    match outcome {
        Outcome::Archived(report) => {
            match shown {
                Some(shown) => print_cards_report(&shown, &report, out)?,
                None => print_report(ctx, &report, out)?,
            }
            Ok(report.exit_code())
        }
        Outcome::Planned(plan) => {
            if ctx.json {
                out.json(Some("archive"), &plan)
                    .context("print the result")?;
            } else {
                out.dry_plan(&plan.lines(), "archived", &rerun_text(&plan))
                    .context("print the result")?;
                out.notices(&notice::warnings(&plan.warnings))
                    .context("print the notes")?;
            }
            Ok(plan.exit_code())
        }
        Outcome::Held(held) => {
            if ctx.json {
                out.json(Some("archive"), &held)
                    .context("print the result")?;
            } else {
                for line in held.lines() {
                    out.line(&line).context("print the result")?;
                }
            }
            Ok(0)
        }
    }
}

/// What running the plan again without `-n` would do, for the footer.
fn rerun_text(plan: &ArchivePlan) -> String {
    let pieces = plan
        .actions
        .iter()
        .filter(|step| matches!(step, Step::Command(_)))
        .count();
    match pieces {
        0 => "make these repairs".to_string(),
        1 => "archive the piece".to_string(),
        _ => "archive these pieces".to_string(),
    }
}

/// The lines of the pieces are on the screen already: a blank line, the
/// summary, and the notes. A run without a piece prints the text of the pipe.
fn print_cards_report(
    shown: &Shown,
    report: &ArchiveReport,
    out: &mut Output,
) -> anyhow::Result<()> {
    let Some(summary) = progress::archive_summary(report, shown.took()) else {
        return print_pipe_lines(report, out);
    };
    shown.print_summary(out, &summary)?;
    print_notices(report, out)
}

fn print_notices(report: &ArchiveReport, out: &mut Output) -> anyhow::Result<()> {
    let mut notices: Vec<Notice> = report
        .repairs
        .iter()
        .map(|repair| Notice::Note(format!("repaired: {}", repair.describe())))
        .collect();
    notices.extend(notice::warnings(&report.warnings));
    out.notices(&notices).context("print the notes")
}

fn print_report(ctx: &Context, report: &ArchiveReport, out: &mut Output) -> anyhow::Result<()> {
    if ctx.json {
        return out
            .json(Some("archive"), report)
            .context("print the result");
    }
    print_pipe_lines(report, out)
}

/// One line for each piece, then the totals.
fn print_pipe_lines(report: &ArchiveReport, out: &mut Output) -> anyhow::Result<()> {
    for piece in &report.archived {
        out.line(&format!("archived {}", piece.piece))
            .context("print the result")?;
    }
    for piece in &report.skipped {
        out.line(&format!("skipped {}: {}", piece.piece, piece.message))
            .context("print the result")?;
    }
    for piece in &report.refused {
        out.line(&format!(
            "refused {}: {}",
            piece.piece,
            piece.reasons_text()
        ))
        .context("print the result")?;
    }
    out.line(&format!(
        "Archived {}, skipped {}, refused {}.",
        report.archived.len(),
        report.skipped.len(),
        report.refused.len()
    ))
    .context("print the result")?;
    print_notices(report, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cards_of_an_archive_name_each_operation_and_what_it_counts() {
        for (operation, word, count) in [
            (Operation::Packing, "packing", "1 B/4 B"),
            (Operation::Verifying, "verifying", "1 B/4 B"),
            (Operation::Storing, "storing", "1/4 files"),
            (Operation::Removing, "removing", "1/4 entries"),
        ] {
            let [first, second] = progress::card_of(COUNTS, operation, 1, 4);
            assert!(first.ends_with(&format!("  {word}")), "{first:?}");
            let expected = if count.is_empty() {
                "25%".to_string()
            } else {
                format!("25%  {count}")
            };
            assert!(second.ends_with(&expected), "{second:?}");
        }
    }
}
