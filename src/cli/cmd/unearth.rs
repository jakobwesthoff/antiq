// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `unearth` command.

use super::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, UnearthArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::index::Index;
use antiq::lock::Holder;
use antiq::ops::unearth::{Planned, Skipped, UnearthReport, UnearthRequest, apply, plan};
use antiq::reference::{resolve, resolve_path};
use antiq::report::CommandReport;
use antiq::run::{Started, start};
use anyhow::Context as _;
use std::fs;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &UnearthArgs, out: &mut Output) -> anyhow::Result<u8> {
    let directory = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&directory)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    // Relative references start where the command was told to start.
    let cwd = fs::canonicalize(&directory).context("resolve the starting directory")?;
    let clock = SystemClock;

    // The run starts before anything is read, so that what an interrupted
    // run left behind is repaired first.
    let started = start(&collection, &clock, args.dry_run)
        .map_err(antiq::Error::from)
        .context("start the run")?;

    let index = Index::compute(&collection)
        .map_err(antiq::Error::from)
        .context("read the pieces this machine knows")?;
    let id = resolve(&collection, &index, &cwd, &args.piece)
        .map_err(antiq::Error::from)
        .context("find the piece")?;
    let to = args
        .to
        .as_deref()
        .map(|to| resolve_path(&collection, &cwd, to))
        .transpose()
        .map_err(antiq::Error::from)
        .context("resolve the target")?;
    let request = UnearthRequest {
        id,
        to,
        version: args.version,
    };
    let planned = plan(&collection, &request)
        .map_err(antiq::Error::from)
        .context("work out what to unearth")?;

    match started {
        Started::DryRun { holder } => {
            let plan = planned.plan();
            if ctx.json {
                out.json(Some("unearth"), &plan.dry_run())
                    .context("print the plan")?;
                return Ok(0);
            }
            for line in plan.lines() {
                out.line(&line).context("print the plan")?;
            }
            if let Some(holder) = holder {
                out.note(&lock_note(&holder)).context("print the notes")?;
            }
            Ok(0)
        }
        Started::Guarded(guard) => {
            let report = match planned {
                Planned::AlreadyUnearthed { id, path } => UnearthReport::already_unearthed(
                    &collection,
                    id,
                    path,
                    guard.warnings().to_vec(),
                ),
                Planned::Unearth(plan) => apply(&collection, &plan, &guard, &clock)
                    .map_err(antiq::Error::from)
                    .context("unearth the piece")?,
            };
            print_report(ctx, &report, out)?;
            Ok(if report.partial() { 3 } else { 0 })
        }
    }
}

fn print_report(ctx: &Context, report: &UnearthReport, out: &mut Output) -> anyhow::Result<()> {
    // Under `--json` the warnings are part of the object and nothing goes
    // to stderr.
    if ctx.json {
        return out
            .json(Some("unearth"), report)
            .context("print the result");
    }
    let lines = render(report);
    for line in &lines.stdout {
        out.line(line).context("print the result")?;
    }
    for line in &lines.stderr {
        out.note(line).context("print the notes")?;
    }
    Ok(())
}

/// What a run prints on a console.
#[derive(Debug, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

fn render(report: &UnearthReport) -> Lines {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let path = sanitize(&report.path.to_string());
    match (report.skipped, report.payload_version) {
        (Some(Skipped::NoSpace { needed, available }), _) => stderr.push(format!(
            "{path} was not unearthed: it needs {needed} bytes and the volume has {available}"
        )),
        (None, Some(version)) => {
            stdout.push(format!("Unearthed {path} (archive version {version})"))
        }
        (None, None) => stdout.push(format!("{} is already unearthed at {path}", report.id)),
    }
    stderr.extend(
        report
            .warnings
            .iter()
            .map(|warning| format!("note: {}", sanitize(&warning.message))),
    );
    Lines { stdout, stderr }
}

/// What `status` says when another run holds the lock.
fn lock_note(holder: &Holder) -> String {
    format!(
        "note: another run holds the lock (pid {}, machine {}, started {})",
        holder.pid,
        sanitize(&holder.machine),
        holder.started
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::relpath::RelPath;
    use antiq::report::Warning;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

    fn report() -> UnearthReport {
        UnearthReport {
            unearthed: true,
            id: ID.parse().expect("an ID"),
            path: RelPath::parse("projects/a").expect("a path"),
            abs_path: "/work/projects/a".into(),
            payload_version: Some(2),
            skipped: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn an_unearthed_piece_is_named_with_its_archive() {
        let lines = render(&report());
        assert_eq!(lines.stdout, ["Unearthed projects/a (archive version 2)"]);
        assert!(lines.stderr.is_empty());
    }

    #[test]
    fn a_piece_that_was_unearthed_before_is_named_with_its_place() {
        let lines = render(&UnearthReport {
            unearthed: false,
            payload_version: None,
            ..report()
        });
        assert_eq!(
            lines.stdout,
            [format!("{ID} is already unearthed at projects/a")]
        );
    }

    #[test]
    fn a_piece_skipped_for_space_says_what_was_missing_and_prints_nothing_else() {
        let lines = render(&UnearthReport {
            unearthed: false,
            payload_version: None,
            skipped: Some(Skipped::NoSpace {
                needed: 100,
                available: 40,
            }),
            ..report()
        });
        assert!(lines.stdout.is_empty());
        assert_eq!(
            lines.stderr,
            ["projects/a was not unearthed: it needs 100 bytes and the volume has 40"]
        );
    }

    #[test]
    fn warnings_are_notes_with_control_characters_shown_escaped() {
        let lines = render(&UnearthReport {
            warnings: vec![Warning::new("odd\nname")],
            ..report()
        });
        assert_eq!(lines.stderr, ["note: odd%0Aname"]);
    }

    #[test]
    fn the_lock_note_names_the_holder() {
        let note = lock_note(&Holder {
            pid: 4242,
            machine: "laptop".into(),
            started: "2026-11-04T10:00:00Z".parse().expect("a time"),
        });
        assert_eq!(
            note,
            "note: another run holds the lock (pid 4242, machine laptop, started 2026-11-04T10:00:00Z)"
        );
    }
}
