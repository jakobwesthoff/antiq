// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `unearth` command: brings a version back and removes it.

use super::progress::{self, Description, Restored};
use crate::cli::console::block::Block;
use crate::cli::console::cards::{Count, Summary, sizes};
use crate::cli::console::rich::Rich;
use crate::cli::notice::{self, Notice};
use crate::cli::output::{Output, Style};
use crate::cli::table::{decimal_size, sanitize};
use crate::cli::{Context, UnearthArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::unearth::{
    DryRunPlan, Outcome, UnearthAction, UnearthReport, UnearthRequest, unearth, unearth_observed,
};
use antiq::plan::Step;
use antiq::progress::Operation;
use antiq::report::CommandReport;
use antiq::versions::naming::{local_labels, system_zone};
use antiq::versions::stamp::Stamp;
use anyhow::Context as _;
use jiff::tz::TimeZone;

/// The verifying of an unearth counts the content bytes of the files it
/// compares, as the size on the card does. Every other count is a
/// count of files, which is what the cards show for an operation without an
/// entry here. `fork` brings a version back the same way and uses the same
/// list.
pub(super) const COUNTS: &[(Operation, Count)] = &[(
    Operation::Verifying,
    Count {
        text: sizes,
        percent: true,
    },
)];

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
    let style = out.stdout_style();
    // On a terminal the piece shows as a card while it comes back, and its
    // line replaces the line of the pipe.
    let cards = description(style, &zone);
    let (result, shown) = progress::watch(
        ctx,
        out,
        args.dry_run,
        cards,
        |observer| unearth_observed(&collection, &request, &SystemClock, &zone, observer),
        || unearth(&collection, &request, &SystemClock, &zone),
        |_| Vec::new(),
    )?;
    let outcome = result
        .map_err(antiq::Error::from)
        .context("unearth the version")?;

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
                let info = plan
                    .actions
                    .iter()
                    .any(|step| matches!(step, Step::Command(action) if action.skipped.is_none()));
                out.dry_plan_explained(
                    &dry_run_lines(&plan, style, &zone),
                    info.then(explanation).as_ref(),
                    "unearthed",
                    "unearth the version",
                )
                .context("print the result")?;
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

/// The last line of an unearth that showed cards: where the piece came
/// back to, or that it stayed in the store.
fn done_summary(
    report: &UnearthReport,
    root: &std::path::Path,
    cwd: &std::path::Path,
    took: std::time::Duration,
) -> Summary {
    progress::restore_summary(
        &Restored {
            verb: "Unearthed",
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

/// The description of the cards of a command that brings a version back:
/// `unearth` and `fork` count and name their versions the same way.
pub(super) fn description(style: Style, zone: &TimeZone) -> Description {
    let zone = zone.clone();
    let mut description = Description::new(COUNTS);
    description.label = Box::new(move |stamp, siblings| {
        let siblings: Vec<Stamp> = siblings.iter().filter_map(|s| s.parse().ok()).collect();
        label(stamp, &siblings, style, &zone)
    });
    description
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

/// What a terminal is told about the command after the plan.
fn explanation() -> Block {
    Block::info(
        Rich::new()
            .cmd("unearth")
            .text(" brings a stored version back as a live folder."),
    )
    .text(
        Rich::new().text(
            "It removes the version from the store once the folder is complete and verified.",
        ),
    )
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
    use antiq::commands::unearth::SkippedNote;
    use std::path::Path;
    use std::time::Duration;

    fn report() -> UnearthReport {
        UnearthReport {
            unearthed: true,
            piece: "projects/scanner".to_string(),
            stamp: "2026-11-04T090211".to_string(),
            version: jiff::Timestamp::UNIX_EPOCH,
            path: "projects/scanner".to_string(),
            bytes: 9,
            skipped: None,
            repairs: Vec::new(),
            warnings: Vec::new(),
            unfinished: false,
            siblings: Vec::new(),
        }
    }

    #[test]
    fn the_cards_of_an_unearth_count_files_and_show_the_bytes_compared_as_sizes() {
        for (operation, word, count) in [
            (Operation::Extracting, "extracting", "1/4 files"),
            (Operation::Verifying, "verifying", "1 B/4 B"),
            (Operation::Removing, "removing", "1/4 files"),
        ] {
            let [first, second] = progress::card_of(COUNTS, operation, 1, 4);
            assert!(first.ends_with(&format!("  {word}")), "{first:?}");
            assert!(second.ends_with(&format!("25%  {count}")), "{second:?}");
        }
    }

    fn summary(report: &UnearthReport) -> (Tone, String) {
        let root = Path::new("/data");
        let summary = done_summary(report, root, root, Duration::from_secs(4));
        (summary.tone, summary.parts[0].0.clone())
    }

    #[test]
    fn an_unearthed_piece_is_told_by_its_destination() {
        assert_eq!(
            summary(&report()),
            (Tone::Ok, "Unearthed to ./projects/scanner".to_string())
        );
    }

    #[test]
    fn a_folder_in_place_with_its_version_left_is_told_as_a_failure() {
        let mut unfinished = report();
        unfinished.unfinished = true;
        assert_eq!(
            summary(&unfinished),
            (Tone::Bad, "Unearthed to ./projects/scanner".to_string())
        );
    }

    #[test]
    fn a_skipped_piece_is_told_as_skipped() {
        let mut skipped = report();
        skipped.unearthed = false;
        skipped.skipped = Some(SkippedNote {
            reason: "no_room",
            needed: 9,
            available: 3,
        });
        assert_eq!(
            summary(&skipped),
            (Tone::Warn, "Skipped projects/scanner".to_string())
        );
    }
}
