// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fork` command: extracts a working copy of a version.

use super::unearth::label;
use crate::cli::output::{Output, Style};
use crate::cli::table::{decimal_size, sanitize};
use crate::cli::{Context, ForkArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::fork::{DryRunPlan, ForkAction, ForkReport, ForkRequest, Outcome, fork};
use antiq::plan::Step;
use antiq::report::CommandReport;
use antiq::versions::naming::system_zone;
use anyhow::Context as _;
use jiff::tz::TimeZone;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &ForkArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let request = ForkRequest {
        cwd: start,
        piece: args.piece.clone(),
        version: args.version.clone(),
        name: args.name.clone(),
        dry_run: args.dry_run,
    };
    let zone = system_zone();
    let outcome = fork(&collection, &request, &SystemClock, &zone)
        .map_err(antiq::Error::from)
        .context("fork the version")?;
    let style = out.stdout_style();

    match outcome {
        Outcome::Held(held) => {
            if ctx.json {
                out.json(Some("fork"), &held).context("print the result")?;
            } else {
                for line in held.lines() {
                    out.line(&line).context("print the result")?;
                }
            }
            Ok(0)
        }
        Outcome::DryRun(plan) => {
            if ctx.json {
                out.json(Some("fork"), &plan).context("print the result")?;
            } else {
                for line in dry_run_lines(&plan, style, &zone) {
                    out.line(&line).context("print the result")?;
                }
                for warning in &plan.warnings {
                    out.note(&format!("note: {}", warning.message))
                        .context("print the notes")?;
                }
            }
            Ok(if plan.skips() { 3 } else { 0 })
        }
        Outcome::Done(report) => {
            if ctx.json {
                out.json(Some("fork"), &report)
                    .context("print the result")?;
            } else {
                let lines = done_lines(&report, style, &zone);
                for line in &lines.stdout {
                    out.line(line).context("print the result")?;
                }
                for line in &lines.stderr {
                    out.note(line).context("print the notes")?;
                }
            }
            Ok(if report.partial() { 3 } else { 0 })
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

fn done_lines(report: &ForkReport, style: Style, zone: &TimeZone) -> Lines {
    let piece = sanitize(&report.piece);
    let mut lines = Lines::default();
    for repair in &report.repairs {
        lines
            .stderr
            .push(format!("note: repaired: {}", repair.describe()));
    }
    for warning in &report.warnings {
        lines.stderr.push(format!("note: {}", warning.message));
    }
    if let Some(skipped) = &report.skipped {
        lines.stderr.push(match skipped.needed {
            Some(needed) => format!(
                "Skipped {piece}: {} needed, {} free",
                decimal_size(needed),
                decimal_size(skipped.available)
            ),
            None => format!(
                "Skipped {piece}: out of space during the extraction, {} free",
                decimal_size(skipped.available)
            ),
        });
    } else {
        lines.stdout.push(format!(
            "Forked {piece} (version {}) to {}",
            label(&report.stamp, &report.siblings, style, zone),
            sanitize(&report.path)
        ));
    }
    lines
}

fn check_text(check: &str, action: &ForkAction, style: Style, zone: &TimeZone) -> String {
    let version = label(&action.stamp, &action.siblings, style, zone);
    match check {
        "version" if action.damage.is_empty() => format!("the version {version} is sound"),
        "version" => format!("the version {version} is damaged and is extracted with a warning"),
        "destination" => format!("the destination {} is free", sanitize(&action.path)),
        "free_space" => format!(
            "there is room for {}",
            decimal_size(action.bytes.expect("the free space is checked with a size"))
        ),
        "member_names" => "the member names fit the volume (not run by -n)".to_string(),
        "extracted_tree" => "the extracted tree equals the listing (not run by -n)".to_string(),
        other => other.to_string(),
    }
}

fn dry_run_lines(plan: &DryRunPlan, style: Style, zone: &TimeZone) -> Vec<String> {
    let mut lines = Vec::new();
    for step in &plan.actions {
        match step {
            Step::Repair(repair) => lines.push(format!("Would {}", repair.describe())),
            Step::Command(action) => {
                let piece = sanitize(&action.piece);
                let version = label(&action.stamp, &action.siblings, style, zone);
                if let Some(skipped) = &action.skipped {
                    let needed = skipped
                        .needed
                        .expect("a dry run skips only for a size it read from the listing");
                    lines.push(format!(
                        "Would skip {piece} (version {version}): {} needed, {} free",
                        decimal_size(needed),
                        decimal_size(skipped.available)
                    ));
                    continue;
                }
                lines.push(format!(
                    "Would fork {piece} (version {version}) to {}",
                    sanitize(&action.path)
                ));
                for check in &action.checks {
                    lines.push(format!(
                        "  check {}",
                        check_text(check, action, style, zone)
                    ));
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push("Nothing to do.".to_string());
    }
    lines
}
