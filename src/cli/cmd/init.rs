// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `init` command.

use crate::cli::output::Output;
use crate::cli::{Context, InitArgs};
use antiq::clock::SystemClock;
use antiq::commands::init::{InitError, InitReport, InitRequest, init};
use antiq::id::RandomIds;
use antiq::select::SelectError;
use anyhow::Context as _;

pub fn run(ctx: &Context, args: &InitArgs, out: &mut Output) -> anyhow::Result<u8> {
    let request = InitRequest {
        directory: &ctx.start,
        patterns: &args.patterns,
        dry_run: args.dry_run,
    };
    let report = init(&request, &SystemClock, &RandomIds)
        .map_err(|error| match error {
            // A `--pattern` that does not parse is a flaw of the command
            // line, unlike the same pattern read from a config file.
            error @ InitError::Select(SelectError::InvalidPattern { .. }) => {
                antiq::Error::Usage(error.to_string())
            }
            error => error.into(),
        })
        .context("make the directory an antiq collection")?;

    if ctx.json {
        out.json(Some("init"), &report)
            .context("print the result")?;
    } else {
        let lines = render(&report);
        for line in &lines.stdout {
            out.line(line).context("print the result")?;
        }
        for line in &lines.stderr {
            out.note(line).context("print the notes")?;
        }
    }
    Ok(0)
}

/// What a run prints on a console.
#[derive(Debug, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

fn render(report: &InitReport) -> Lines {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    if let Some(config) = &report.config {
        // The config is the data of a dry run; a shell can redirect it.
        stdout.extend(config.lines().map(str::to_string));
        stderr.push(format!("Would create a collection at {}", report.path));
    } else {
        stdout.push(format!("Created the collection at {}", report.path));
        if let Some(id) = report.id {
            stdout.push(format!("  id: {id}"));
        }
        stdout.push(format!("  patterns: {}", report.patterns.join(", ")));
        if let Some(due) = report.due {
            let folders = if due == 1 { "folder" } else { "folders" };
            stdout.push(format!("The next archive would archive {due} {folders}."));
        }
    }
    stderr.extend(
        report
            .warnings
            .iter()
            .map(|warning| format!("note: {}", warning.message)),
    );
    Lines { stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::report::Warning;

    fn report() -> InitReport {
        InitReport {
            dry_run: false,
            created: true,
            id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("a ULID")),
            path: "/work".into(),
            patterns: vec!["a*".into(), "b*".into()],
            due: Some(2),
            config: None,
            actions: Vec::new(),
            warnings: vec![Warning::new("watch out")],
        }
    }

    fn texts(lines: &[String]) -> Vec<&str> {
        lines.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_created_collection_has_a_summary_and_its_id() {
        let lines = render(&report());
        assert_eq!(
            texts(&lines.stdout),
            [
                "Created the collection at /work",
                "  id: 01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "  patterns: a*, b*",
                "The next archive would archive 2 folders.",
            ]
        );
        assert_eq!(texts(&lines.stderr), ["note: watch out"]);
    }

    #[test]
    fn one_folder_is_singular_and_no_count_prints_no_line() {
        let one = render(&InitReport {
            due: Some(1),
            ..report()
        });
        assert_eq!(
            one.stdout.last().map(String::as_str),
            Some("The next archive would archive 1 folder.")
        );
        let none = render(&InitReport {
            due: None,
            ..report()
        });
        assert_eq!(none.stdout.len(), 3);
    }

    #[test]
    fn a_dry_run_prints_the_config_and_says_where_it_would_go() {
        let lines = render(&InitReport {
            dry_run: true,
            created: false,
            id: None,
            due: None,
            config: Some("[collection]\nid = \"X\"\n".into()),
            ..report()
        });
        assert_eq!(texts(&lines.stdout), ["[collection]", "id = \"X\""]);
        assert_eq!(
            texts(&lines.stderr),
            ["Would create a collection at /work", "note: watch out"]
        );
    }
}
