// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `unearth` command: brings a version back and removes it.

use crate::cli::notice::{self, Notice};
use crate::cli::output::{Output, Style};
use crate::cli::table::{decimal_size, sanitize};
use crate::cli::{Context, UnearthArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::unearth::{
    DryRunPlan, Outcome, UnearthAction, UnearthReport, UnearthRequest, unearth,
};
use antiq::plan::Step;
use antiq::report::CommandReport;
use antiq::versions::naming::{local_labels, system_zone};
use antiq::versions::stamp::Stamp;
use anyhow::Context as _;
use jiff::tz::TimeZone;

pub fn run(ctx: &Context, args: &UnearthArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let request = UnearthRequest {
        cwd: ctx.start.clone(),
        piece: args.piece.clone(),
        version: args.version.clone(),
        dry_run: args.dry_run,
    };
    let zone = system_zone();
    let outcome = unearth(&collection, &request, &SystemClock, &zone)
        .map_err(antiq::Error::from)
        .context("unearth the version")?;
    let style = out.stdout_style();

    match outcome {
        Outcome::Held(held) => {
            if ctx.json {
                out.json(Some("unearth"), &held)
                    .context("print the result")?;
            } else {
                for line in held.lines() {
                    out.line(&line).context("print the result")?;
                }
            }
            Ok(0)
        }
        Outcome::DryRun(plan) => {
            if ctx.json {
                out.json(Some("unearth"), &plan)
                    .context("print the result")?;
            } else {
                for line in dry_run_lines(&plan, style, &zone) {
                    out.line(&line).context("print the result")?;
                }
                out.notices(&notice::warnings(&plan.warnings))
                    .context("print the notes")?;
            }
            Ok(if plan.skips() { 3 } else { 0 })
        }
        Outcome::Done(report) => {
            if ctx.json {
                out.json(Some("unearth"), &report)
                    .context("print the result")?;
            } else {
                let lines = done_lines(&report, style, &zone);
                for line in &lines.stdout {
                    out.line(line).context("print the result")?;
                }
                out.notices(&lines.notices).context("print the notes")?;
                for line in &lines.stderr {
                    out.note(line).context("print the notes")?;
                }
            }
            Ok(if report.partial() { 3 } else { 0 })
        }
    }
}

/// How the console names a version: its local time on a terminal, with
/// the offset where a sibling renders alike, and the stamp in a pipe.
pub(super) fn label(stamp: &str, siblings: &[Stamp], style: Style, zone: &TimeZone) -> String {
    let Ok(stamp) = stamp.parse::<Stamp>() else {
        return stamp.to_string();
    };
    if !style.terminal {
        return stamp.to_string();
    }
    let mut all = siblings.to_vec();
    if !all.contains(&stamp) {
        all.push(stamp);
        all.sort();
    }
    let position = all
        .iter()
        .position(|other| *other == stamp)
        .expect("the stamp is in the list");
    local_labels(&all, zone).swap_remove(position)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    /// Warnings and repairs, written before the lines of `stderr`.
    notices: Vec<Notice>,
    stderr: Vec<String>,
}

fn done_lines(report: &UnearthReport, style: Style, zone: &TimeZone) -> Lines {
    let piece = sanitize(&report.piece);
    let mut lines = Lines::default();
    for repair in &report.repairs {
        lines
            .notices
            .push(Notice::Note(format!("repaired: {}", repair.describe())));
    }
    lines.notices.extend(notice::warnings(&report.warnings));
    if let Some(skipped) = &report.skipped {
        lines.stderr.push(format!(
            "Skipped {piece}: {} needed, {} free",
            decimal_size(skipped.needed),
            decimal_size(skipped.available)
        ));
    } else {
        lines.stdout.push(format!(
            "Unearthed {piece} (version {})",
            label(&report.stamp, &report.siblings, style, zone)
        ));
    }
    lines
}

fn check_text(check: &str, action: &UnearthAction, style: Style, zone: &TimeZone) -> String {
    match check {
        "version" => format!(
            "the version {} is sound",
            label(&action.stamp, &action.siblings, style, zone)
        ),
        "destination" => format!("the destination {} is free", sanitize(&action.path)),
        "free_space" => format!("there is room for {}", decimal_size(action.bytes)),
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
                    lines.push(format!(
                        "Would skip {piece} (version {version}): {} needed, {} free",
                        decimal_size(skipped.needed),
                        decimal_size(skipped.available)
                    ));
                    continue;
                }
                lines.push(format!("Would unearth {piece} (version {version})"));
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
