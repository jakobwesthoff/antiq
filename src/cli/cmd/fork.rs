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

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::commands::fork::SkippedNote;
    use antiq::plan::Plan;
    use antiq::relpath::RelPath;
    use antiq::repair::Repair;
    use antiq::report::Warning;
    use jiff::Timestamp;

    const PIPE: Style = Style {
        width: 80,
        color: false,
        terminal: false,
    };

    fn report(skipped: Option<SkippedNote>) -> ForkReport {
        ForkReport {
            forked: skipped.is_none(),
            piece: "projects/scanner".to_string(),
            stamp: "2026-11-04T090211".to_string(),
            version: Timestamp::UNIX_EPOCH,
            path: "tools/copy".to_string(),
            bytes: None,
            skipped,
            repairs: Vec::new(),
            warnings: vec![Warning::new("a warning".to_string())],
            unfinished: false,
            siblings: Vec::new(),
        }
    }

    #[test]
    fn a_skip_without_a_known_size_says_the_volume_ran_full() {
        let lines = done_lines(
            &report(Some(SkippedNote {
                reason: "no_space",
                needed: None,
                available: 5_000,
            })),
            PIPE,
            &TimeZone::UTC,
        );
        assert_eq!(lines.stdout, Vec::<String>::new());
        assert_eq!(
            lines.stderr,
            [
                "note: a warning",
                "Skipped projects/scanner: out of space during the extraction, 5.0 kB free"
            ]
        );
    }

    #[test]
    fn a_dry_run_lists_a_repair_first_and_says_when_there_is_nothing_to_do() {
        let repair = Repair::RemoveEmptyFolder {
            folder: RelPath::parse("projects/old").expect("a path"),
        };
        let mut plan = Plan::after_repairs(vec![repair]);
        let lines = dry_run_lines(
            &DryRunPlan {
                dry_run: true,
                actions: std::mem::take(&mut plan.actions),
                warnings: Vec::new(),
            },
            PIPE,
            &TimeZone::UTC,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("Would "), "{lines:?}");
        let empty = DryRunPlan {
            dry_run: true,
            actions: Vec::new(),
            warnings: Vec::new(),
        };
        assert_eq!(
            dry_run_lines(&empty, PIPE, &TimeZone::UTC),
            ["Nothing to do."]
        );
    }

    #[test]
    fn a_check_this_command_does_not_know_is_shown_by_its_name() {
        let action = ForkAction {
            kind: "fork",
            piece: "projects/scanner".to_string(),
            stamp: "2026-11-04T090211".to_string(),
            version: Timestamp::UNIX_EPOCH,
            path: "tools/copy".to_string(),
            bytes: None,
            checks: Vec::new(),
            damage: Vec::new(),
            skipped: None,
            siblings: Vec::new(),
        };
        assert_eq!(check_text("other", &action, PIPE, &TimeZone::UTC), "other");
    }
}
