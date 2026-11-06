// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `import` command: packs a folder as a version.

use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::{Context, ImportArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::import::{ImportReport, Outcome, Request, import};
use antiq::plan::PlanAction;
use anyhow::Context as _;

pub fn run(ctx: &Context, args: &ImportArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
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
                out.notices(&notice::warnings(&plan.warnings))
                    .context("print the notes")?;
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
    let mut notices: Vec<Notice> = report
        .repairs
        .iter()
        .map(|repair| Notice::Note(format!("repaired: {}", repair.describe())))
        .collect();
    notices.extend(notice::warnings(&report.warnings));
    out.notices(&notices).context("print the notes")
}
