// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fork` command: extracts a working copy of a version.

use super::progress::{self, Restored};
use super::unearth::{description, label};
use crate::cli::console::block::Block;
use crate::cli::console::cards::Summary;
use crate::cli::console::rich::Rich;
use crate::cli::notice::{self, Notice};
use crate::cli::output::{Output, Style};
use crate::cli::table::{decimal_size, sanitize};
use crate::cli::{Context, ForkArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::fork::{
    DryRunPlan, ForkAction, ForkReport, ForkRequest, Outcome, fork, fork_observed,
};
use antiq::plan::Step;
use antiq::report::CommandReport;
use antiq::versions::naming::system_zone;
use anyhow::Context as _;
use jiff::tz::TimeZone;

pub fn run(ctx: &Context, args: &ForkArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let request = ForkRequest {
        cwd: ctx.start.clone(),
        piece: args.piece.clone(),
        version: args.version.clone(),
        name: args.name.clone(),
        dry_run: args.dry_run,
    };
    let zone = system_zone();
    let style = out.stdout_style();
    // On a terminal the piece shows as a card while the copy is made, as for
    // an unearth, and its line replaces the line of the pipe.
    let (result, shown) = progress::watch(
        ctx,
        out,
        args.dry_run,
        description(style, &zone),
        |observer| fork_observed(&collection, &request, &SystemClock, &zone, observer),
        || fork(&collection, &request, &SystemClock, &zone),
        |_| Vec::new(),
    )?;
    let outcome = result
        .map_err(antiq::Error::from)
        .context("fork the version")?;

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
                let info = plan
                    .actions
                    .iter()
                    .any(|step| matches!(step, Step::Command(action) if action.skipped.is_none()));
                out.dry_plan_explained(
                    &dry_run_lines(&plan, style, &zone),
                    info.then(explanation).as_ref(),
                    "forked",
                    "fork the version",
                )
                .context("print the result")?;
                out.notices(&notice::warnings(&plan.warnings))
                    .context("print the notes")?;
            }
            Ok(if plan.skips() { 3 } else { 0 })
        }
        Outcome::Done(report) => {
            if ctx.json {
                out.json(Some("fork"), &report)
                    .context("print the result")?;
            } else {
                let lines = done_lines(&report, style, &zone);
                if let Some(shown) = &shown {
                    let summary =
                        done_summary(&report, collection.root(), &ctx.start, shown.took());
                    shown.print_summary(out, &summary)?;
                } else {
                    for line in &lines.stdout {
                        out.line(line).context("print the result")?;
                    }
                }
                out.notices(&lines.notices).context("print the notes")?;
                if shown.is_none() {
                    for line in &lines.stderr {
                        out.note(line).context("print the notes")?;
                    }
                }
            }
            Ok(if report.partial() { 3 } else { 0 })
        }
    }
}

/// The last line of a fork that showed cards: where the copy came about, or
/// that the version was not forked.
fn done_summary(
    report: &ForkReport,
    root: &std::path::Path,
    cwd: &std::path::Path,
    took: std::time::Duration,
) -> Summary {
    progress::restore_summary(
        &Restored {
            verb: "Forked",
            piece: &report.piece,
            path: &report.path,
            skipped: report.skipped.is_some(),
            unfinished: report.unfinished,
        },
        root,
        cwd,
        took,
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    /// Warnings and repairs, written before the lines of `stderr`.
    notices: Vec<Notice>,
    stderr: Vec<String>,
}

fn done_lines(report: &ForkReport, style: Style, zone: &TimeZone) -> Lines {
    let piece = sanitize(&report.piece);
    let mut lines = Lines::default();
    for repair in &report.repairs {
        lines
            .notices
            .push(Notice::Note(format!("repaired: {}", repair.describe())));
    }
    lines.notices.extend(notice::warnings(&report.warnings));
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

/// What a terminal is told about the command after the plan.
fn explanation() -> Block {
    Block::info(
        Rich::new()
            .cmd("fork")
            .text(" extracts a working copy of a stored version."),
    )
    .text(Rich::new().text("The version stays in the store unchanged."))
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
                // A terminal gets the checks as a list under a heading,
                // a pipe keeps one `  check ...` line for each.
                if style.terminal {
                    lines.push("Checks the real run makes first:".to_string());
                }
                for check in &action.checks {
                    let text = check_text(check, action, style, zone);
                    lines.push(if style.terminal {
                        format!("  {text}")
                    } else {
                        format!("  check {text}")
                    });
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
    use crate::cli::console::cards::Tone;
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
        utf8: true,
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

    fn summary(report: &ForkReport) -> (Tone, String) {
        let root = std::path::Path::new("/data");
        let summary = done_summary(report, root, root, std::time::Duration::from_secs(4));
        (summary.tone, summary.parts[0].0.clone())
    }

    #[test]
    fn a_fork_is_told_by_the_path_of_the_copy_and_not_by_the_piece() {
        assert_eq!(
            summary(&report(None)),
            (Tone::Ok, "Forked to ./tools/copy".to_string())
        );
    }

    #[test]
    fn a_copy_in_place_that_is_not_durable_is_told_as_a_failure() {
        let mut unfinished = report(None);
        unfinished.unfinished = true;
        assert_eq!(
            summary(&unfinished),
            (Tone::Bad, "Forked to ./tools/copy".to_string())
        );
    }

    #[test]
    fn a_skipped_fork_is_told_by_its_piece() {
        let skipped = report(Some(SkippedNote {
            reason: "no_space",
            needed: Some(9),
            available: 3,
        }));
        assert_eq!(
            summary(&skipped),
            (Tone::Warn, "Skipped projects/scanner".to_string())
        );
    }

    #[test]
    fn the_cards_of_a_fork_count_files_while_extracting_and_bytes_while_verifying() {
        use antiq::progress::Operation;
        for (operation, word, count) in [
            (Operation::Extracting, "extracting", "1 B/4 B"),
            (Operation::Verifying, "verifying", "1 B/4 B"),
        ] {
            let [first, second] = progress::card_of(super::super::unearth::COUNTS, operation, 1, 4);
            assert!(first.ends_with(&format!("  {word}")), "{first:?}");
            assert!(second.ends_with(&format!("25%  {count}")), "{second:?}");
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
            crate::cli::notice::pipe_lines(&lines.notices),
            ["warning: a warning"]
        );
        assert_eq!(
            lines.stderr,
            ["Skipped projects/scanner: out of space during the extraction, 5.0 kB free"]
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
