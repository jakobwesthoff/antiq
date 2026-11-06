// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` command: archives due pieces, or one piece now.

use crate::cli::output::Output;
use crate::cli::{ArchiveArgs, Context};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::archive::{ArchiveReport, Outcome, Request, Target, archive};
use antiq::plan::PlanAction;
use antiq::report::Warning;
use anyhow::Context as _;

pub fn run(ctx: &Context, args: &ArchiveArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
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
    let outcome = archive(&collection, &SystemClock, &request)
        .map_err(antiq::Error::from)
        .context(if args.dry_run {
            "plan the archive"
        } else {
            "archive the pieces"
        })?;

    match outcome {
        Outcome::Archived(report) => {
            print_report(ctx, &report, out)?;
            Ok(report.exit_code())
        }
        Outcome::Planned(plan) => {
            if ctx.json {
                out.json(Some("archive"), &plan)
                    .context("print the result")?;
            } else {
                for line in plan.lines() {
                    out.line(&line).context("print the result")?;
                }
                notes(&plan.warnings, out)?;
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

fn print_report(ctx: &Context, report: &ArchiveReport, out: &mut Output) -> anyhow::Result<()> {
    if ctx.json {
        return out
            .json(Some("archive"), report)
            .context("print the result");
    }
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
    for repair in &report.repairs {
        out.note(&format!("note: repaired: {}", repair.describe()))
            .context("print the notes")?;
    }
    notes(&report.warnings, out)
}

fn notes(warnings: &[Warning], out: &mut Output) -> anyhow::Result<()> {
    for warning in warnings {
        out.note(&format!("note: {}", warning.message))
            .context("print the notes")?;
    }
    Ok(())
}
