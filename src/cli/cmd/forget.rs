// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `forget` command: deletes a version, or all versions of a piece.

use crate::cli::output::Output;
use crate::cli::table::sanitize;
use crate::cli::{Context, ForgetArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::forget::{ForgetReport, ForgetRequest, Which, forget};
use antiq::report::CommandReport;
use antiq::versions::naming::{local_labels, system_zone};
use anyhow::Context as _;
use jiff::tz::TimeZone;
use std::path::PathBuf;

/// The exit code of a run in which a version could not be deleted.
const PARTIAL: u8 = 3;

pub fn run(ctx: &Context, args: &ForgetArgs, out: &mut Output) -> anyhow::Result<u8> {
    if args.dry_run {
        return super::not_implemented(ctx, "forget --dry-run");
    }
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    // A piece given as a relative path starts where the collection was
    // looked for from.
    let cwd = start
        .canonicalize()
        .context("resolve the current directory")?;
    let request = ForgetRequest {
        collection: &collection,
        cwd: &cwd,
        piece: &args.piece,
        which: match &args.version {
            Some(version) => Which::One(version),
            None => Which::All,
        },
    };
    let zone = system_zone();
    let report = forget(&request, &SystemClock, &zone)
        .map_err(antiq::Error::from)
        .context("forget the versions")?;

    if ctx.json {
        out.json(Some("forget"), &report)
            .context("print the result")?;
    } else {
        let lines = render(&report, out.stdout_style().terminal, &zone);
        for line in &lines.stdout {
            out.line(line).context("print the result")?;
        }
        for line in &lines.stderr {
            out.note(line).context("print the notes")?;
        }
    }
    Ok(if report.partial() { PARTIAL } else { 0 })
}

/// What a run prints.
#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

/// A complete version is named by its local time on a terminal and by its
/// stamp in a pipe, which a script can hand to the next `forget`. An
/// incomplete version has no time to name it and is always named by its
/// stamp.
fn render(report: &ForgetReport, terminal: bool, zone: &TimeZone) -> Lines {
    let complete: Vec<_> = report
        .forgotten
        .iter()
        .filter(|entry| !entry.incomplete)
        .map(|entry| entry.stamp)
        .collect();
    let mut labels = local_labels(&complete, zone).into_iter();

    let mut stdout = Vec::new();
    for entry in &report.forgotten {
        let piece = sanitize(&entry.piece.to_string());
        if entry.incomplete {
            stdout.push(format!(
                "Deleted incomplete version {} of {piece}",
                entry.stamp
            ));
            continue;
        }
        let label = labels.next().expect("a label for each complete version");
        let name = if terminal {
            label
        } else {
            entry.stamp.to_string()
        };
        stdout.push(format!("Deleted version {name} of {piece}"));
    }

    let mut stderr = Vec::new();
    for action in &report.repairs {
        if let antiq::commands::forget::Action::Repair(repair) = action {
            stderr.push(format!("note: repaired: {}", repair.describe()));
        }
    }
    stderr.extend(
        report
            .warnings
            .iter()
            .map(|warning| format!("note: {}", warning.message)),
    );
    for failure in &report.failed {
        stderr.push(format!(
            "error: could not delete version {} of {}: {}",
            failure.stamp,
            sanitize(&failure.piece.to_string()),
            failure.reason
        ));
    }
    Lines { stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::commands::forget::{Action, Failed, VersionEntry};
    use antiq::relpath::{Name, RelPath};
    use antiq::repair::Repair;
    use antiq::report::Warning;
    use antiq::versions::stamp::Stamp;

    fn piece(path: &str) -> RelPath {
        RelPath::from_bytes(path.as_bytes().to_vec()).expect("a valid path")
    }

    fn entry(path: &str, stamp: &str, incomplete: bool) -> VersionEntry {
        let stamp = Stamp::parse(stamp).expect("a stamp");
        VersionEntry {
            piece: piece(path),
            stamp,
            version: stamp.time(),
            incomplete,
            files: Vec::new(),
        }
    }

    fn report(forgotten: Vec<VersionEntry>) -> ForgetReport {
        ForgetReport {
            forgotten,
            failed: Vec::new(),
            repairs: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn berlin() -> TimeZone {
        TimeZone::get("Europe/Berlin").expect("the time zone database knows Berlin")
    }

    #[test]
    fn a_pipe_names_the_version_by_its_stamp() {
        let lines = render(
            &report(vec![entry("tools/scan", "2026-11-04T090211-2", false)]),
            false,
            &berlin(),
        );
        assert_eq!(
            lines.stdout,
            ["Deleted version 2026-11-04T090211-2 of tools/scan"]
        );
        assert!(lines.stderr.is_empty());
    }

    #[test]
    fn a_terminal_names_the_version_by_its_local_time() {
        let lines = render(
            &report(vec![entry("tools/scan", "2026-11-04T090211", false)]),
            true,
            &berlin(),
        );
        assert_eq!(
            lines.stdout,
            ["Deleted version 2026-11-04 10:02:11 of tools/scan"]
        );
    }

    #[test]
    fn a_terminal_adds_the_offset_where_two_versions_read_alike() {
        // The hour from 02:00 to 03:00 happens twice on 2026-10-25 in Berlin.
        let lines = render(
            &report(vec![
                entry("tools/scan", "2026-10-25T003000", false),
                entry("tools/scan", "2026-10-25T013000", false),
            ]),
            true,
            &berlin(),
        );
        assert_eq!(
            lines.stdout,
            [
                "Deleted version 2026-10-25 02:30:00 +02:00 of tools/scan",
                "Deleted version 2026-10-25 02:30:00 +01:00 of tools/scan",
            ]
        );
    }

    #[test]
    fn an_incomplete_version_is_named_by_its_stamp_on_a_terminal_too() {
        let lines = render(
            &report(vec![
                entry("tools/scan", "2026-10-25T003000", true),
                entry("tools/scan", "2026-11-04T090211", false),
            ]),
            true,
            &berlin(),
        );
        assert_eq!(
            lines.stdout,
            [
                "Deleted incomplete version 2026-10-25T003000 of tools/scan",
                "Deleted version 2026-11-04 10:02:11 of tools/scan",
            ]
        );
    }

    #[test]
    fn a_name_cannot_move_the_cursor() {
        let lines = render(
            &report(vec![entry("a\u{1b}[31mb", "2026-11-04T090211", false)]),
            false,
            &berlin(),
        );
        assert_eq!(
            lines.stdout,
            ["Deleted version 2026-11-04T090211 of a%1B[31mb"]
        );
    }

    #[test]
    fn repairs_warnings_and_failures_go_to_stderr_in_this_order() {
        let mut done = report(Vec::new());
        done.repairs = vec![Action::Repair(Repair::DeleteTemporary {
            entry: Name::from_bytes(b"leftover".to_vec()).expect("a name"),
        })];
        done.warnings = vec![Warning::new("watch out")];
        done.failed = vec![Failed {
            piece: piece("tools/scan"),
            stamp: Stamp::parse("2026-11-04T090211").expect("a stamp"),
            reason: "cannot delete the file".to_string(),
        }];
        let lines = render(&done, false, &berlin());
        assert!(lines.stdout.is_empty());
        assert_eq!(
            lines.stderr,
            [
                "note: repaired: delete .antiq/tmp/leftover",
                "note: watch out",
                "error: could not delete version 2026-11-04T090211 of tools/scan: cannot delete the file",
            ]
        );
    }

    #[test]
    fn a_report_with_a_failure_is_partial() {
        let mut done = report(Vec::new());
        assert!(!done.partial());
        done.failed = vec![Failed {
            piece: piece("tools/scan"),
            stamp: Stamp::parse("2026-11-04T090211").expect("a stamp"),
            reason: "no".to_string(),
        }];
        assert!(done.partial());
    }
}
