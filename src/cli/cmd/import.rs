// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `import` command: packs a folder as a version.

use super::archive::COUNTS;
use super::progress::{self, Description};
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::{Context, ImportArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::import::{ImportReport, Outcome, Request, import, import_observed};
use antiq::plan::PlanAction;
use antiq::shown::escape_controls;
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
    // On a terminal the piece shows as a card while it is packed, as for an
    // archive, and its closing line replaces the line of the pipe.
    let mut description = Description::new(COUNTS);
    description.stored_size = true;
    let (result, shown) = progress::watch(
        ctx,
        out,
        args.dry_run,
        description,
        |observer| import_observed(&collection, &SystemClock, &request, observer),
        || import(&collection, &SystemClock, &request),
        |_| Vec::new(),
    )?;
    let outcome = result
        .map_err(antiq::Error::from)
        .context(if args.dry_run {
            "plan the import"
        } else {
            "import the folder"
        })?;

    match outcome {
        Outcome::Imported(report) => {
            match &shown {
                Some(shown) => {
                    let summary = progress::import_summary(&report, shown.took());
                    shown.print_summary(out, &summary)?;
                    print_notes(&report, out)?;
                }
                None => print_report(ctx, &report, out)?,
            }
            Ok(report.exit_code())
        }
        Outcome::Planned(plan) => {
            if ctx.json {
                out.json(Some("import"), &plan)
                    .context("print the result")?;
            } else {
                out.dry_plan(&plan.lines(), "imported", "import the folder")
                    .context("print the result")?;
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
    for line in report_lines(report) {
        out.line(&line).context("print the result")?;
    }
    print_notes(report, out)
}

/// The lines of a pipe: the version that came about and the skip, if any.
/// The piece and the message name paths from the user's files, so their
/// control characters are escaped.
fn report_lines(report: &ImportReport) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(version) = &report.imported {
        lines.push(format!(
            "imported {}: version {}",
            escape_controls(&version.piece),
            version.stamp
        ));
    }
    if let Some(skipped) = &report.skipped {
        lines.push(format!(
            "skipped {}: {}",
            escape_controls(&skipped.piece),
            escape_controls(&skipped.message)
        ));
    }
    lines
}

/// The repairs and the warnings of the run.
fn print_notes(report: &ImportReport, out: &mut Output) -> anyhow::Result<()> {
    let mut notices: Vec<Notice> = report
        .repairs
        .iter()
        .map(|repair| Notice::Note(format!("repaired: {}", repair.describe())))
        .collect();
    notices.extend(notice::warnings(&report.warnings));
    out.notices(&notices).context("print the notes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::commands::archive::SkipReason;
    use antiq::commands::import::SkippedImport;

    #[test]
    fn a_control_character_in_a_skipped_piece_is_escaped() {
        let report = ImportReport {
            imported: None,
            skipped: Some(SkippedImport {
                piece: "data\nskipped fake".into(),
                reason: SkipReason::Error,
                message: "cannot move data\nx".into(),
            }),
            repairs: Vec::new(),
            warnings: Vec::new(),
        };
        assert_eq!(
            report_lines(&report),
            ["skipped data%0Askipped fake: cannot move data%0Ax"]
        );
    }
}
