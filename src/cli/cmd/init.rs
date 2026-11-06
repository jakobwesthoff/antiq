// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `init` command.

use crate::cli::output::Output;
use crate::cli::{Context, InitArgs};
use antiq::clock::SystemClock;
use antiq::id::RandomIds;
use antiq::ops::init::{InitReport, InitRequest, init};
use antiq::report::CommandReport;
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &InitArgs, out: &mut Output) -> anyhow::Result<u8> {
    let directory = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let machine = antiq::machine::detect()
        .map_err(antiq::Error::from)
        .context("detect the machine name")?;
    let request = InitRequest {
        directory: &directory,
        patterns: &args.patterns,
        machine: &machine,
        yes: args.yes,
        dry_run: args.dry_run,
    };
    let mut interaction = ctx.interaction();
    let report = init(&request, interaction.as_mut(), &SystemClock, &RandomIds)
        .map_err(antiq::Error::from)
        .context("make the directory a collection")?;

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
    Ok(if report.partial() { 3 } else { 0 })
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

    // After an answer the user has seen the plan and its notes in the
    // question, so a decline leaves only the fact that nothing happened.
    let declined = report.answered && !report.created;
    if !declined {
        stdout.push(if report.created {
            format!("Created the collection at {}", report.path)
        } else {
            format!("Would create a collection at {}", report.path)
        });
        if let Some(id) = report.id {
            stdout.push(format!("  id: {id}"));
        }
        stdout.push(format!("  machine: {}", report.machine));
        stdout.push(format!("  patterns: {}", report.patterns.join(", ")));
        // init has no exclude option: the exclude list is the proposal.
        if !report.exclude.is_empty() {
            let excluded: Vec<String> = report.exclude.iter().map(ToString::to_string).collect();
            stdout.push(format!(
                "  exclude: {} (proposed: archive.sh layout)",
                excluded.join(", ")
            ));
        }
        if report.created {
            stdout.push(format!("  pieces: {}", report.pieces.len()));
        } else {
            stdout.push(format!("pieces ({}):", report.pieces.len()));
            stdout.extend(report.pieces.iter().map(|piece| format!("  {piece}")));
        }
    }

    if !report.answered {
        stderr.extend(
            report
                .warnings
                .iter()
                .map(|warning| format!("note: {}", warning.message)),
        );
    }
    if !report.created && !report.dry_run {
        stderr.push(if report.answered {
            "nothing was created".to_string()
        } else {
            "nothing was created; run with --yes to create the collection without asking"
                .to_string()
        });
    }
    Lines { stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::relpath::RelPath;
    use antiq::report::Warning;

    fn report() -> InitReport {
        InitReport {
            dry_run: false,
            created: false,
            id: None,
            path: "/work".into(),
            machine: "laptop".into(),
            patterns: vec!["a*".into(), "b*".into()],
            exclude: Vec::new(),
            pieces: vec![
                RelPath::parse("a1").expect("valid"),
                RelPath::parse("b%FF").expect("valid"),
            ],
            actions: Vec::new(),
            warnings: vec![Warning::new("watch out")],
            answered: false,
        }
    }

    fn texts(lines: &[String]) -> Vec<&str> {
        lines.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_plan_lists_the_settings_and_every_piece() {
        let lines = render(&report());
        assert_eq!(
            texts(&lines.stdout),
            [
                "Would create a collection at /work",
                "  machine: laptop",
                "  patterns: a*, b*",
                "pieces (2):",
                "  a1",
                "  b%FF",
            ]
        );
    }

    #[test]
    fn a_plan_without_a_terminal_says_how_to_go_on() {
        assert_eq!(
            texts(&render(&report()).stderr),
            [
                "note: watch out",
                "nothing was created; run with --yes to create the collection without asking"
            ]
        );
    }

    #[test]
    fn a_dry_run_has_notes_and_no_hint() {
        let lines = render(&InitReport {
            dry_run: true,
            ..report()
        });
        assert_eq!(texts(&lines.stderr), ["note: watch out"]);
    }

    #[test]
    fn a_declined_question_leaves_one_line() {
        let lines = render(&InitReport {
            answered: true,
            ..report()
        });
        assert!(lines.stdout.is_empty());
        assert_eq!(texts(&lines.stderr), ["nothing was created"]);
    }

    #[test]
    fn a_created_collection_has_a_summary_and_its_id() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("a ULID");
        let lines = render(&InitReport {
            created: true,
            id: Some(id),
            exclude: vec![RelPath::parse("archive").expect("valid")],
            ..report()
        });
        assert_eq!(
            texts(&lines.stdout),
            [
                "Created the collection at /work",
                "  id: 01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "  machine: laptop",
                "  patterns: a*, b*",
                "  exclude: archive (proposed: archive.sh layout)",
                "  pieces: 2",
            ]
        );
        assert_eq!(texts(&lines.stderr), ["note: watch out"]);
    }

    #[test]
    fn notes_are_not_repeated_after_an_answer() {
        let lines = render(&InitReport {
            created: true,
            answered: true,
            ..report()
        });
        assert!(lines.stderr.is_empty());
    }
}
