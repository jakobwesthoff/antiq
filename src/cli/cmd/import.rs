// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `import` command: packs a folder as a version.

use crate::cli::output::Output;
use crate::cli::{Context, ImportArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::import::{ImportReport, Outcome, Request, import};
use antiq::plan::PlanAction;
use antiq::report::Warning;
use anyhow::Context as _;

pub fn run(ctx: &Context, args: &ImportArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let request = Request {
        folder: args.folder.clone(),
        as_path: args.as_path.clone(),
        time: args.time.clone(),
        cwd: ctx.start.clone(),
        dry_run: args.dry_run,
    };
    let outcome = import(&collection, &SystemClock, &request)
        .map_err(antiq::Error::from)
        .context(if args.dry_run {
            "plan the import"
        } else {
            "import the folder"
        })?;

    match outcome {
        Outcome::Imported(report) => {
            print_report(ctx, &report, out)?;
            Ok(report.exit_code())
        }
        Outcome::Planned(plan) => {
            if ctx.json {
                out.json(Some("import"), &plan)
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
                out.json(Some("import"), &held)
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

fn print_report(ctx: &Context, report: &ImportReport, out: &mut Output) -> anyhow::Result<()> {
    if ctx.json {
        return out.json(Some("import"), report).context("print the result");
    }
    if let Some(version) = &report.imported {
        out.line(&format!(
            "imported {}: version {}",
            version.piece, version.stamp
        ))
        .context("print the result")?;
    }
    if let Some(skipped) = &report.skipped {
        out.line(&format!("skipped {}: {}", skipped.piece, skipped.message))
            .context("print the result")?;
    }
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
