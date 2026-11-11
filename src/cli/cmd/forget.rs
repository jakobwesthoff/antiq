// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `forget` command: deletes a version, or all versions of a piece.

use crate::cli::console::block::Block;
use crate::cli::console::rich::Rich;
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::{Context, ForgetArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::forget::{
    Action, ForgetPlan, ForgetReport, ForgetRequest, VersionEntry, Which, forget, plan,
};
use antiq::report::CommandReport;
use antiq::shown::escape_controls;
use antiq::versions::naming::{local_labels, system_zone};
use anyhow::Context as _;
use jiff::tz::TimeZone;

/// The exit code of a run in which a version could not be deleted.
const PARTIAL: u8 = 3;

pub fn run(ctx: &Context, args: &ForgetArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let request = ForgetRequest {
        collection: &collection,
        cwd: &ctx.start,
        piece: &args.piece,
        which: match &args.version {
            Some(version) => Which::One(version),
            None => Which::All,
        },
        force: args.force,
    };
    let zone = system_zone();

    if args.dry_run {
        let plan = plan(&request, &SystemClock, &zone)
            .map_err(antiq::Error::from)
            .context("plan forgetting the versions")?;
        if ctx.json {
            out.json(Some("forget"), &plan).context("print the plan")?;
        } else {
            let lines = render_plan(&plan, out.stdout_style().terminal, &zone);
            out.dry_plan_explained(
                &lines.stdout,
                deletes_a_version(&plan).then(explanation).as_ref(),
                "deleted",
                &rerun_text(&plan),
            )
            .context("print the plan")?;
            out.notices(&lines.stderr).context("print the notes")?;
        }
        return Ok(0);
    }

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
        out.notices(&lines.stderr).context("print the notes")?;
    }
    Ok(if report.partial() { PARTIAL } else { 0 })
}

/// How many versions the plan deletes. Repairs are not counted.
fn versions_deleted(plan: &ForgetPlan) -> usize {
    match plan {
        ForgetPlan::Held { .. } => 0,
        ForgetPlan::Listed { actions, .. } => actions
            .iter()
            .filter(|action| matches!(action, Action::ForgetVersion(_)))
            .count(),
    }
}

fn deletes_a_version(plan: &ForgetPlan) -> bool {
    versions_deleted(plan) > 0
}

/// Tells a terminal that the deletion cannot be undone. A plan of repairs
/// only deletes nothing and gets no such explanation.
fn explanation() -> Block {
    Block::info(
        Rich::new()
            .cmd("forget")
            .text(" deletes a version for good."),
    )
    .text(
        Rich::new()
            .cmd("unearth")
            .text(" and ")
            .cmd("fork")
            .text(" cannot bring it back afterwards."),
    )
}

/// What running the plan again without `-n` would do, for the footer.
fn rerun_text(plan: &ForgetPlan) -> String {
    let versions = versions_deleted(plan);
    match versions {
        0 => "make these repairs".to_string(),
        1 => "delete the version".to_string(),
        _ => "delete these versions".to_string(),
    }
}

/// What a run prints.
#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<Notice>,
}

/// How each entry names its version, after `Deleted` or `Would delete`. A
/// complete version is named by its local time on a terminal and by its
/// stamp in a pipe, which a script can hand to the next `forget`. An
/// incomplete version has no time to name it and is always named by its
/// stamp.
fn version_names(entries: &[&VersionEntry], terminal: bool, zone: &TimeZone) -> Vec<String> {
    let complete: Vec<_> = entries
        .iter()
        .filter(|entry| !entry.incomplete)
        .map(|entry| entry.stamp)
        .collect();
    let mut labels = local_labels(&complete, zone).into_iter();
    entries
        .iter()
        .map(|entry| {
            if entry.incomplete {
                return format!("incomplete version {}", entry.stamp);
            }
            let label = labels.next().expect("a label for each complete version");
            let name = if terminal {
                label
            } else {
                entry.stamp.to_string()
            };
            format!("version {name}")
        })
        .collect()
}

fn render(report: &ForgetReport, terminal: bool, zone: &TimeZone) -> Lines {
    let entries: Vec<&VersionEntry> = report.forgotten.iter().collect();
    let names = version_names(&entries, terminal, zone);
    let stdout = entries
        .iter()
        .zip(names)
        .map(|(entry, name)| {
            format!(
                "Deleted {name} of {}",
                escape_controls(&entry.piece.to_string())
            )
        })
        .collect();

    let mut stderr = Vec::new();
    for action in &report.repairs {
        if let antiq::commands::forget::Action::Repair(repair) = action {
            stderr.push(Notice::Note(format!("repaired: {}", repair.describe())));
        }
    }
    stderr.extend(notice::warnings(report.warnings()));
    for failure in &report.failed {
        stderr.push(Notice::Line(format!(
            "error: could not delete version {} of {}: {}",
            failure.stamp,
            escape_controls(&failure.piece.to_string()),
            failure.reason
        )));
    }
    Lines { stdout, stderr }
}

/// The action line of a version a terminal gets, and one line below it for
/// each file the version consists of, named by its part: `archive`,
/// `listing` or `checksum`.
fn files_under_action(name: &str, piece: &str, entry: &VersionEntry) -> Vec<String> {
    let count = match entry.files.len() {
        1 => "one file".to_string(),
        2 => "two files".to_string(),
        3 => "three files".to_string(),
        many => format!("{many} files"),
    };
    let mut lines = vec![format!("Would delete {name} of {piece}, {count}:")];
    let part = |file: &str| {
        if file.ends_with(".tar.zst.sha256") {
            "checksum"
        } else if file.ends_with(".sha256.zst") {
            "listing"
        } else if file.ends_with(".tar.zst") {
            "archive"
        } else {
            "file"
        }
    };
    // "checksum" is the longest part, so the file names line up whichever
    // files a version has left.
    for file in &entry.files {
        lines.push(format!("  {:<8}   {}", part(file), escape_controls(file)));
    }
    lines
}

/// The plan of a dry run: one `Would ...` line per action on stdout, or
/// the line about the holder of the lock.
fn render_plan(plan: &ForgetPlan, terminal: bool, zone: &TimeZone) -> Lines {
    let (actions, warnings) = match plan {
        ForgetPlan::Held { lock_held, .. } => {
            return Lines {
                stdout: vec![format!(
                    "The lock is held by pid {}, started {}. No repairs are listed.",
                    lock_held.pid, lock_held.started
                )],
                stderr: Vec::new(),
            };
        }
        ForgetPlan::Listed {
            actions, warnings, ..
        } => (actions, warnings),
    };
    let versions: Vec<&VersionEntry> = actions
        .iter()
        .filter_map(|action| match action {
            Action::ForgetVersion(entry) => Some(entry),
            Action::Repair(_) => None,
        })
        .collect();
    let mut names = version_names(&versions, terminal, zone).into_iter();

    let mut stdout = Vec::new();
    for action in actions {
        match action {
            Action::Repair(repair) => stdout.push(format!("Would {}", repair.describe())),
            Action::ForgetVersion(entry) => {
                let name = names.next().expect("a name for each version");
                let piece = escape_controls(&entry.piece.to_string());
                if terminal {
                    stdout.extend(files_under_action(&name, &piece, entry));
                } else {
                    let files: Vec<String> = entry
                        .files
                        .iter()
                        .map(|file| escape_controls(file))
                        .collect();
                    stdout.push(format!(
                        "Would delete {name} of {piece}: {}",
                        files.join(", ")
                    ));
                }
            }
        }
    }
    Lines {
        stdout,
        stderr: notice::warnings(warnings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::notice::pipe_lines;
    use antiq::commands::forget::Failed;
    use antiq::plan::LockHeld;
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
            pipe_lines(&lines.stderr),
            [
                "note: repaired: delete .antiq/tmp/leftover",
                "warning: watch out",
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

    fn listed(actions: Vec<Action>) -> ForgetPlan {
        ForgetPlan::Listed {
            dry_run: true,
            actions,
            warnings: vec![Warning::new("watch out")],
        }
    }

    fn with_files(mut entry: VersionEntry, files: &[&str]) -> VersionEntry {
        entry.files = files.iter().map(|file| file.to_string()).collect();
        entry
    }

    #[test]
    fn a_plan_lists_the_repairs_and_then_each_version_with_its_files() {
        let plan = listed(vec![
            Action::Repair(Repair::DeleteTemporary {
                entry: Name::from_bytes(b"leftover".to_vec()).expect("a name"),
            }),
            Action::ForgetVersion(with_files(
                entry("tools/scan", "2026-11-04T090211", false),
                &["a.tar.zst", "a.sha256.zst", "a.tar.zst.sha256"],
            )),
            Action::ForgetVersion(with_files(
                entry("tools/scan", "2026-11-03T090211", true),
                &["b.sha256.zst"],
            )),
        ]);
        let lines = render_plan(&plan, true, &berlin());
        assert_eq!(
            lines.stdout,
            [
                "Would delete .antiq/tmp/leftover",
                "Would delete version 2026-11-04 10:02:11 of tools/scan, three files:",
                "  archive    a.tar.zst",
                "  listing    a.sha256.zst",
                "  checksum   a.tar.zst.sha256",
                "Would delete incomplete version 2026-11-03T090211 of tools/scan, one file:",
                "  listing    b.sha256.zst",
            ]
        );
        assert_eq!(pipe_lines(&lines.stderr), ["warning: watch out"]);
    }

    #[test]
    fn a_plan_in_a_pipe_names_the_version_by_its_stamp() {
        let plan = listed(vec![Action::ForgetVersion(with_files(
            entry("tools/scan", "2026-11-04T090211", false),
            &["a\nb"],
        ))]);
        let lines = render_plan(&plan, false, &berlin());
        assert_eq!(
            lines.stdout,
            ["Would delete version 2026-11-04T090211 of tools/scan: a%0Ab"]
        );
    }

    #[test]
    fn a_plan_of_a_held_lock_is_the_line_about_the_holder() {
        let plan = ForgetPlan::Held {
            dry_run: true,
            lock_held: LockHeld {
                pid: 4711,
                started: "2026-11-04T09:02:11Z".parse().expect("a timestamp"),
            },
            actions: Vec::new(),
        };
        let lines = render_plan(&plan, false, &berlin());
        assert_eq!(
            lines.stdout,
            ["The lock is held by pid 4711, started 2026-11-04T09:02:11Z. No repairs are listed."]
        );
        assert!(lines.stderr.is_empty());
    }

    #[test]
    fn the_files_of_a_version_are_counted_and_named_by_their_part() {
        let mut version = entry("projects/scanner", "2026-11-04T090211", false);
        version.files = vec![
            "a--2026-11-04T090211.tar.zst".to_string(),
            "a--2026-11-04T090211.sha256.zst".to_string(),
        ];
        let lines = files_under_action("version 2026-11-04", "projects/scanner", &version);
        assert_eq!(
            lines,
            [
                "Would delete version 2026-11-04 of projects/scanner, two files:",
                "  archive    a--2026-11-04T090211.tar.zst",
                "  listing    a--2026-11-04T090211.sha256.zst",
            ]
        );

        version.files = (1..=4).map(|n| format!("stray-{n}")).collect();
        let lines = files_under_action("version 2026-11-04", "projects/scanner", &version);
        assert_eq!(
            lines[0],
            "Would delete version 2026-11-04 of projects/scanner, 4 files:"
        );
        assert_eq!(lines[1], "  file       stray-1");
    }
}
