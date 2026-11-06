// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fork` command.

use super::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, ForkArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::id::RandomIds;
use antiq::index::Index;
use antiq::lock::Holder;
use antiq::ops::fork::{ForkReport, ForkRequest, apply, plan};
use antiq::plan::Plan;
use antiq::reference::resolve;
use antiq::report::{self, CommandReport};
use antiq::run::{Started, start};
use anyhow::Context as _;
use std::fs;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &ForkArgs, out: &mut Output) -> anyhow::Result<u8> {
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
    let request = ForkRequest {
        id,
        name: args.name.clone(),
    };
    let planned = plan(&collection, &request)
        .map_err(antiq::Error::from)
        .context("work out what to fork")?;

    match started {
        Started::DryRun { holder } => {
            let mut actions = Plan::new();
            actions.push(planned.action());
            if ctx.json {
                out.json(Some("fork"), &actions.dry_run())
                    .context("print the plan")?;
                return Ok(0);
            }
            for line in actions.lines() {
                out.line(&sanitize(&line)).context("print the plan")?;
            }
            if let Some(holder) = holder {
                out.note(&lock_note(&holder)).context("print the notes")?;
            }
            Ok(0)
        }
        Started::Guarded(guard) => {
            let report = apply(&collection, &planned, &guard, &clock, &RandomIds)
                .map_err(antiq::Error::from)
                .context("fork the piece")?;
            print_report(ctx, &report, out).context("print the report")?;
            Ok(0)
        }
    }
}

fn print_report(ctx: &Context, report: &ForkReport, out: &mut Output) -> anyhow::Result<()> {
    // Under `--json` the warnings are part of the object and nothing goes
    // to stderr.
    if ctx.json {
        return out.json(Some("fork"), report).context("print the result");
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

fn render(report: &ForkReport) -> Lines {
    let old_path = sanitize(&report.old_path.to_string());
    let moved = if report.path == report.old_path {
        String::new()
    } else {
        format!(" to {}", sanitize(&report.path.to_string()))
    };
    let stdout = vec![format!("Forked {old_path}{moved} (new ID {})", report.id)];
    let stderr = report
        .warnings()
        .iter()
        .map(|warning| format!("note: {}", sanitize(&warning.message)))
        .collect();
    Lines { stdout, stderr }
}

/// The note every command prints while another run holds the lock, with
/// the machine name made safe for the terminal.
fn lock_note(holder: &Holder) -> String {
    format!("note: {}", sanitize(&report::lock_note(holder).message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::relpath::RelPath;
    use antiq::report::Warning;

    const OLD: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
    const NEW: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

    fn report() -> ForkReport {
        ForkReport {
            id: NEW.parse().expect("an ID"),
            from: OLD.parse().expect("an ID"),
            path: RelPath::parse("projects/a").expect("a path"),
            old_path: RelPath::parse("projects/a").expect("a path"),
            abs_path: "/work/projects/a".into(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_forked_piece_is_named_with_its_new_id() {
        let lines = render(&report());
        assert_eq!(lines.stdout, [format!("Forked projects/a (new ID {NEW})")]);
        assert!(lines.stderr.is_empty());
    }

    #[test]
    fn a_renamed_piece_is_named_with_its_new_place() {
        let lines = render(&ForkReport {
            path: RelPath::parse("projects/b").expect("a path"),
            ..report()
        });
        assert_eq!(
            lines.stdout,
            [format!("Forked projects/a to projects/b (new ID {NEW})")]
        );
    }

    #[test]
    fn warnings_are_notes_with_control_characters_shown_escaped() {
        let lines = render(&ForkReport {
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
