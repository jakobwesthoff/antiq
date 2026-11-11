// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `init` command.

use crate::cli::console::env::Env;
use crate::cli::console::symbols::Symbols;
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::table::sanitize;
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
        let env = out.stdout_env();
        let lines = render(&report, &env);
        if report.config.is_some() && env.rich {
            out.dry_plan(&lines.stdout, "written", "create the antiq collection")
                .context("print the result")?;
        } else {
            for line in &lines.stdout {
                out.line(line).context("print the result")?;
            }
        }
        out.notices(&lines.stderr).context("print the notes")?;
    }
    Ok(0)
}

/// What a run prints on a console.
#[derive(Debug, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<Notice>,
}

/// The config of a dry run with the line of the ID replaced by a comment.
/// The ID is minted by the real run, so the one in the report is not the
/// one the collection will get.
fn without_id(config: &str) -> impl Iterator<Item = &str> {
    config.lines().map(|line| {
        if line.starts_with("id = ") {
            "# id is minted by the real run"
        } else {
            line
        }
    })
}

fn render(report: &InitReport, env: &Env) -> Lines {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    if let Some(config) = &report.config {
        if env.rich {
            let frame = Symbols::for_env(env).frame;
            stdout.push(format!(
                "Would create the antiq collection at {}",
                report.path
            ));
            stdout.push(
                "Would write .antiq/config.toml, the settings of the collection:".to_string(),
            );
            stdout.extend(
                without_id(config).map(|line| format!("{frame} {line}").trim_end().to_string()),
            );
        } else {
            stdout.extend(without_id(config).map(str::to_string));
            stderr.push(Notice::Line(format!(
                "Would create a collection at {}",
                report.path
            )));
        }
    } else {
        stdout.push(format!(
            "Created the collection at {}",
            sanitize(&report.path)
        ));
        if let Some(id) = report.id {
            stdout.push(format!("  id: {id}"));
        }
        stdout.push(format!(
            "  patterns: {}",
            sanitize(&report.patterns.join(", "))
        ));
        if let Some(due) = report.due {
            let folders = if due == 1 { "folder" } else { "folders" };
            stdout.push(format!("The next archive would archive {due} {folders}."));
        }
    }
    stderr.extend(notice::warnings(&report.warnings));
    Lines { stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::notice::pipe_lines;
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
        let lines = render(&report(), &Env::pipe());
        assert_eq!(
            texts(&lines.stdout),
            [
                "Created the collection at /work",
                "  id: 01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "  patterns: a*, b*",
                "The next archive would archive 2 folders.",
            ]
        );
        assert_eq!(pipe_lines(&lines.stderr), ["warning: watch out"]);
    }

    #[test]
    fn one_folder_is_singular_and_no_count_prints_no_line() {
        let one = render(
            &InitReport {
                due: Some(1),
                ..report()
            },
            &Env::pipe(),
        );
        assert_eq!(
            one.stdout.last().map(String::as_str),
            Some("The next archive would archive 1 folder.")
        );
        let none = render(
            &InitReport {
                due: None,
                ..report()
            },
            &Env::pipe(),
        );
        assert_eq!(none.stdout.len(), 3);
    }

    #[test]
    fn a_dry_run_prints_the_config_and_says_where_it_would_go() {
        let lines = render(
            &InitReport {
                dry_run: true,
                created: false,
                id: None,
                due: None,
                config: Some("[collection]\nid = \"X\"\n".into()),
                ..report()
            },
            &Env::pipe(),
        );
        assert_eq!(
            texts(&lines.stdout),
            ["[collection]", "# id is minted by the real run"]
        );
        assert_eq!(
            pipe_lines(&lines.stderr),
            ["Would create a collection at /work", "warning: watch out"]
        );
    }
}
