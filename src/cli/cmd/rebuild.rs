// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `rebuild` command: repairs a damaged version.

use super::progress;
use super::unearth::description_with;
use crate::cli::console::block::Block;
use crate::cli::console::cards::{Count, Summary, Tone, sizes_or_bytes};
use crate::cli::console::rich::Rich;
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::table::sanitize;
use crate::cli::{Context, RebuildArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::rebuild::{
    Action, Basis, RebuildEntry, RebuildPlan, RebuildReport, RebuildRequest, plan, rebuild,
    rebuild_observed,
};
use antiq::progress::Operation;
use antiq::versions::naming::{local_labels, system_zone};
use anyhow::Context as _;
use jiff::tz::TimeZone;

/// Fits `both files rebuilt`.
const RESULT_WIDTH: usize = 18;

/// What the cards of a rebuild count: the content bytes of the files in the
/// archive, the unit of the size on the finished line. Where the total is
/// not known the count shows the bytes checked so far and no total.
const COUNTS: &[(Operation, Count)] = &[(
    Operation::Verifying,
    Count {
        text: sizes_or_bytes,
        percent: true,
    },
)];

pub fn run(ctx: &Context, args: &RebuildArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let request = RebuildRequest {
        collection: &collection,
        cwd: &ctx.start,
        piece: &args.piece,
        version: &args.version,
    };
    let zone = system_zone();

    if args.dry_run {
        let plan = plan(&request, &SystemClock, &zone)
            .map_err(antiq::Error::from)
            .context("plan the rebuild")?;
        if ctx.json {
            out.json(Some("rebuild"), &plan).context("print the plan")?;
        } else {
            let terminal = out.stdout_style().terminal;
            let lines = render_plan(&plan, terminal, &zone);
            let info = matches!(
                &plan,
                RebuildPlan::Listed { actions, .. }
                    if actions.iter().any(|action| matches!(action, Action::RebuildVersion(_)))
            );
            out.dry_plan_explained(
                &lines.stdout,
                info.then(explanation).as_ref(),
                "rebuilt",
                "rebuild the version",
            )
            .context("print the plan")?;
            out.notices(&lines.stderr).context("print the notes")?;
        }
        return Ok(0);
    }

    let style = out.stdout_style();
    // On a terminal the version shows as a card while it is checked, and its
    // line replaces the line of the pipe.
    let mut description = description_with(COUNTS, style, &zone);
    description.result_width = RESULT_WIDTH;
    let (result, shown) = progress::watch(
        ctx,
        out,
        false,
        description,
        |observer| rebuild_observed(&request, &SystemClock, &zone, observer),
        || rebuild(&request, &SystemClock, &zone),
        |_| Vec::new(),
    )?;
    let report = result
        .map_err(antiq::Error::from)
        .context("rebuild the version")?;
    if ctx.json {
        out.json(Some("rebuild"), &report)
            .context("print the result")?;
    } else {
        let lines = render(&report);
        if let Some(shown) = &shown {
            shown.print_summary(out, &summary(&report, shown.took()))?;
        } else {
            for line in &lines.stdout {
                out.line(line).context("print the result")?;
            }
        }
        out.notices(&lines.stderr).context("print the notes")?;
    }
    Ok(0)
}

/// The last line of a rebuild that showed cards. It names the roles of the
/// files because their names are long, and the line of a pipe has them.
fn summary(report: &RebuildReport, took: std::time::Duration) -> Summary {
    let text = match report.basis {
        Basis::ChecksumFile => "Rebuilt the listing",
        Basis::Listing => "Rebuilt the checksum file",
        Basis::ReadableOnly => "Rebuilt the listing and the checksum file",
    };
    Summary {
        tone: Tone::Ok,
        parts: vec![(text.to_string(), Tone::Ok)],
        took,
    }
}

/// What a run prints.
#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<Notice>,
}

/// What the files written for `basis` can vouch for.
fn statement(basis: Basis) -> &'static str {
    match basis {
        Basis::ChecksumFile => "the archive is the one checksummed when it was stored",
        Basis::Listing => "every member of the archive matches the listing",
        Basis::ReadableOnly => {
            "this shows only that the archive reads without error, not that it is unchanged"
        }
    }
}

/// The line of a pipe, which names the version by its stamp so that a
/// script can hand it to the next command, and the notes that follow the
/// result on any output.
fn render(report: &RebuildReport) -> Lines {
    let name = report.stamp.to_string();
    let files: Vec<String> = report.written.iter().map(|file| sanitize(file)).collect();
    let stdout = vec![format!(
        "Rebuilt version {name} of {}: wrote {}",
        sanitize(&report.piece.to_string()),
        files.join(", ")
    )];

    let mut stderr = Vec::new();
    for action in &report.repairs {
        if let Action::Repair(repair) = action {
            stderr.push(Notice::Note(format!("repaired: {}", repair.describe())));
        }
    }
    stderr.extend(notice::warnings(&report.warnings));
    stderr.push(Notice::Note(statement(report.basis).to_string()));
    Lines { stdout, stderr }
}

/// What a terminal is told about the command after the plan.
fn explanation() -> Block {
    Block::info(Rich::new().cmd("rebuild").text(" repairs a damaged version by writing only what is missing."))
        .text(Rich::new().text("It checks the archive against what survives first and runs only when asked, never as a repair."))
}

/// The rows of a version on a terminal: what is checked, against what, and
/// which files are written, each under its role.
fn rows(entry: &RebuildEntry) -> Vec<String> {
    let row = |role: &str, file: &str| format!("  {role:<9}{}", sanitize(file));
    let mut rows = Vec::new();
    match (&entry.basis, &entry.against) {
        (Basis::ChecksumFile, Some(against)) => {
            rows.push(row("check", &entry.archive));
            rows.push(row("against", against));
        }
        (Basis::Listing, Some(against)) => {
            rows.push(row("check", &format!("every member of {}", entry.archive)));
            rows.push(row("against", against));
        }
        _ => rows.push(row("read", &format!("{} to its end", entry.archive))),
    }
    rows.extend(entry.write.iter().map(|file| row("write", file)));
    rows
}

/// The plan of a dry run: one `Would ...` line per action on stdout, or the
/// line about the holder of the lock. A pipe gets one sentence for a
/// version, a terminal the action and its rows.
fn render_plan(plan: &RebuildPlan, terminal: bool, zone: &TimeZone) -> Lines {
    let (actions, warnings) = match plan {
        RebuildPlan::Held { lock_held, .. } => {
            return Lines {
                stdout: vec![format!(
                    "The lock is held by pid {}, started {}. No repairs are listed.",
                    lock_held.pid, lock_held.started
                )],
                stderr: Vec::new(),
            };
        }
        RebuildPlan::Listed {
            actions, warnings, ..
        } => (actions, warnings),
    };
    let mut stdout = Vec::new();
    for action in actions {
        match action {
            Action::Repair(repair) => stdout.push(format!("Would {}", repair.describe())),
            Action::RebuildVersion(entry) if terminal => {
                let version = local_labels(&[entry.stamp], zone).remove(0);
                stdout.push(format!(
                    "Would rebuild version {version} of {}",
                    sanitize(&entry.piece.to_string())
                ));
                stdout.extend(rows(entry));
            }
            Action::RebuildVersion(entry) => {
                let archive = sanitize(&entry.archive);
                let against = entry.against.as_deref().map(sanitize).unwrap_or_default();
                let write: Vec<String> = entry.write.iter().map(|file| sanitize(file)).collect();
                let write = write.join(", ");
                stdout.push(match entry.basis {
                    Basis::ChecksumFile => {
                        format!("Would check {archive} against {against} and write {write}")
                    }
                    Basis::Listing => format!(
                        "Would check every member of {archive} against {against} and write {write}"
                    ),
                    Basis::ReadableOnly => {
                        format!("Would read {archive} to its end and write {write}")
                    }
                });
            }
        }
    }
    let stderr = notice::warnings(warnings);
    Lines { stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::relpath::RelPath;
    use antiq::versions::stamp::Stamp;
    use std::time::Duration;

    fn entry(basis: Basis, against: Option<&str>, write: &[&str]) -> RebuildEntry {
        let stamp = Stamp::parse("2026-11-04T090211").expect("a stamp");
        RebuildEntry {
            piece: RelPath::parse("projects/scanner").expect("a piece"),
            stamp,
            version: stamp.time(),
            basis,
            archive: "a.tar.zst".to_string(),
            against: against.map(str::to_string),
            write: write.iter().map(|file| file.to_string()).collect(),
        }
    }

    #[test]
    fn a_missing_checksum_file_is_checked_against_the_listing_member_by_member() {
        let rows = rows(&entry(
            Basis::Listing,
            Some("a.sha256.zst"),
            &["a.tar.zst.sha256"],
        ));
        assert_eq!(
            rows,
            [
                "  check    every member of a.tar.zst",
                "  against  a.sha256.zst",
                "  write    a.tar.zst.sha256",
            ]
        );
    }

    #[test]
    fn an_archive_without_sidecars_is_only_read() {
        let rows = rows(&entry(
            Basis::ReadableOnly,
            None,
            &["a.sha256.zst", "a.tar.zst.sha256"],
        ));
        assert_eq!(rows[0], "  read     a.tar.zst to its end");
        assert_eq!(rows.len(), 3);
    }

    fn report(basis: Basis, written: &[&str]) -> RebuildReport {
        let stamp = Stamp::parse("2026-11-04T090211").expect("a stamp");
        RebuildReport {
            piece: RelPath::parse("projects/scanner").expect("a piece"),
            stamp,
            version: stamp.time(),
            basis,
            written: written.iter().map(|file| file.to_string()).collect(),
            repairs: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn the_closing_line_names_the_files_that_were_written_by_their_role() {
        let line = |report: &RebuildReport| {
            let summary = summary(report, Duration::from_secs(67));
            (summary.tone, summary.parts[0].0.clone())
        };
        assert_eq!(
            line(&report(Basis::ChecksumFile, &["a.sha256.zst"])),
            (Tone::Ok, "Rebuilt the listing".to_string())
        );
        assert_eq!(
            line(&report(Basis::Listing, &["a.tar.zst.sha256"])),
            (Tone::Ok, "Rebuilt the checksum file".to_string())
        );
        assert_eq!(
            line(&report(
                Basis::ReadableOnly,
                &["a.sha256.zst", "a.tar.zst.sha256"]
            )),
            (
                Tone::Ok,
                "Rebuilt the listing and the checksum file".to_string()
            )
        );
    }

    #[test]
    fn the_card_counts_content_bytes_against_the_size_of_the_listing() {
        let [first, second] = progress::card_of(COUNTS, Operation::Verifying, 1, 4);
        assert!(first.ends_with("  verifying"), "{first:?}");
        assert!(second.ends_with("25%  1 B/4 B"), "{second:?}");
    }

    #[test]
    fn without_a_total_the_card_counts_the_bytes_checked_under_the_moving_bar() {
        let [_, second] = progress::card_of(COUNTS, Operation::Verifying, 412_000_000, 0);
        assert!(second.ends_with("412 MB"), "{second:?}");
        assert!(!second.contains('%'), "{second:?}");
        assert!(!second.contains('/'), "{second:?}");
    }

    #[test]
    fn before_any_byte_is_checked_the_card_shows_no_count() {
        let [_, second] = progress::card_of(COUNTS, Operation::Verifying, 0, 0);
        assert!(!second.contains(" B"), "{second:?}");
        assert!(second.trim_end().ends_with('─'), "{second:?}");
    }
}
