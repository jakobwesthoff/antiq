// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `rebuild` command: repairs a damaged version.

use crate::cli::output::Output;
use crate::cli::table::sanitize;
use crate::cli::{Context, RebuildArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::rebuild::{
    Action, Basis, RebuildPlan, RebuildReport, RebuildRequest, plan, rebuild,
};
use antiq::report::CommandReport;
use antiq::versions::naming::{local_labels, system_zone};
use anyhow::Context as _;
use jiff::tz::TimeZone;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &RebuildArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    // A piece given as a relative path starts where the collection was
    // looked for from.
    let cwd = start
        .canonicalize()
        .context("resolve the current directory")?;
    let request = RebuildRequest {
        collection: &collection,
        cwd: &cwd,
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
            let lines = render_plan(&plan);
            for line in &lines.stdout {
                out.line(line).context("print the plan")?;
            }
            for line in &lines.stderr {
                out.note(line).context("print the notes")?;
            }
        }
        return Ok(0);
    }

    let report = rebuild(&request, &SystemClock, &zone)
        .map_err(antiq::Error::from)
        .context("rebuild the version")?;
    if ctx.json {
        out.json(Some("rebuild"), &report)
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
    Ok(0)
}

/// What a run prints.
#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
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

/// The version is named by its local time on a terminal and by its stamp in
/// a pipe, which a script can hand to the next command.
fn render(report: &RebuildReport, terminal: bool, zone: &TimeZone) -> Lines {
    let name = if terminal {
        local_labels(&[report.stamp], zone).remove(0)
    } else {
        report.stamp.to_string()
    };
    let files: Vec<String> = report.written.iter().map(|file| sanitize(file)).collect();
    let stdout = vec![format!(
        "Rebuilt version {name} of {}: wrote {}",
        sanitize(&report.piece.to_string()),
        files.join(", ")
    )];

    let mut stderr = Vec::new();
    for action in &report.repairs {
        if let Action::Repair(repair) = action {
            stderr.push(format!("note: repaired: {}", repair.describe()));
        }
    }
    stderr.extend(
        report
            .warnings()
            .iter()
            .map(|warning| format!("note: {}", warning.message)),
    );
    stderr.push(format!("note: {}", statement(report.basis)));
    Lines { stdout, stderr }
}

/// The plan of a dry run: one `Would ...` line per action on stdout, or the
/// line about the holder of the lock.
fn render_plan(plan: &RebuildPlan) -> Lines {
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
    let stdout = actions
        .iter()
        .map(|action| match action {
            Action::Repair(repair) => format!("Would {}", repair.describe()),
            Action::RebuildVersion(entry) => {
                let archive = sanitize(&entry.archive);
                let against = entry.against.as_deref().map(sanitize).unwrap_or_default();
                let write: Vec<String> = entry.write.iter().map(|file| sanitize(file)).collect();
                let write = write.join(", ");
                match entry.basis {
                    Basis::ChecksumFile => {
                        format!("Would check {archive} against {against} and write {write}")
                    }
                    Basis::Listing => format!(
                        "Would check every member of {archive} against {against} and write {write}"
                    ),
                    Basis::ReadableOnly => {
                        format!("Would read {archive} to its end and write {write}")
                    }
                }
            }
        })
        .collect();
    let stderr = warnings
        .iter()
        .map(|warning| format!("note: {}", warning.message))
        .collect();
    Lines { stdout, stderr }
}
