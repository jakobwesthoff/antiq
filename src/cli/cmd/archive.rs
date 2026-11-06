// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` command.

use crate::cli::output::Output;
use crate::cli::{Context, DryRunArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::ops::archive::{ArchiveReport, plan};
use antiq::report::CommandReport;
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &DryRunArgs, out: &mut Output) -> anyhow::Result<u8> {
    if !args.dry_run {
        return super::not_implemented("archive");
    }
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let report = plan(&collection, &SystemClock)
        .map_err(antiq::Error::from)
        .context("plan the archive")?;

    // Under `--json` the warnings are part of the object and nothing goes
    // to stderr.
    if ctx.json {
        out.json(Some("archive"), &report)
            .context("print the result")?;
        return Ok(if report.partial() { 3 } else { 0 });
    }
    let lines = render(&report);
    for line in &lines.stdout {
        out.line(line).context("print the result")?;
    }
    for line in &lines.stderr {
        out.note(line).context("print the notes")?;
    }
    Ok(if report.partial() { 3 } else { 0 })
}

/// What a run prints on a console.
#[derive(Debug, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

fn render(report: &ArchiveReport) -> Lines {
    let stdout = report
        .plan
        .as_ref()
        .map_or_else(Vec::new, |plan| plan.lines());
    let stderr = report
        .warnings()
        .iter()
        .map(|warning| format!("note: {}", warning.message))
        .collect();
    Lines { stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::ops::archive::ArchiveAction;
    use antiq::plan::Plan;
    use antiq::relpath::RelPath;
    use antiq::report::Warning;

    fn action(path: &str, size: u64) -> ArchiveAction {
        ArchiveAction::Archive {
            path: RelPath::parse(path).expect("valid"),
            id: None,
            size,
        }
    }

    #[test]
    fn a_plan_is_one_would_line_per_piece_and_the_warnings_are_notes() {
        let mut plan = Plan::new();
        plan.push(action("projects/a", 412_000_000));
        plan.push(action("projects/b", 1_600_000_000));
        let report = ArchiveReport {
            dry_run: true,
            plan: Some(plan),
            warnings: vec![Warning::new("the collection is on an NFS mount")],
        };
        let lines = render(&report);
        insta::assert_debug_snapshot!(lines, @r#"
        Lines {
            stdout: [
                "Would archive projects/a (412 MB)",
                "Would archive projects/b (1.6 GB)",
            ],
            stderr: [
                "note: the collection is on an NFS mount",
            ],
        }
        "#);
    }

    #[test]
    fn nothing_to_do_is_said_once() {
        let report = ArchiveReport {
            dry_run: true,
            plan: Some(Plan::new()),
            warnings: Vec::new(),
        };
        assert_eq!(render(&report).stdout, ["Nothing to do."]);
    }
}
