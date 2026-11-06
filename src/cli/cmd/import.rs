// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `import` command.

use super::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, ImportArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::id::RandomIds;
use antiq::ops::import::{ImportReport, ImportRequest, SkipReason, import};
use antiq::plan::Plan;
use antiq::report::{self, CommandReport};
use anyhow::Context as _;
use std::fs;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &ImportArgs, out: &mut Output) -> anyhow::Result<u8> {
    // One path cannot name several pieces.
    if args.path.is_some() && args.files.len() > 1 {
        return Err(antiq::Error::Usage(
            "--path names one piece, so it takes exactly one file".to_string(),
        )
        .into());
    }
    let directory = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&directory)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    // Relative files and paths start where the command was told to start.
    let cwd = fs::canonicalize(&directory).context("resolve the starting directory")?;
    let request = ImportRequest {
        collection: &collection,
        cwd: &cwd,
        files: &args.files,
        path: args.path.as_deref(),
    };
    let report = import(&request, args.dry_run, &SystemClock, &RandomIds)
        .map_err(antiq::Error::from)
        .context("import the archives")?;

    // Under `--json` the warnings are part of the object and nothing goes
    // to stderr.
    if ctx.json {
        out.json(Some("import"), &report)
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

fn render(report: &ImportReport) -> Lines {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    if report.dry_run {
        let plan = Plan {
            actions: report.actions.clone().unwrap_or_default(),
            brake: None,
        };
        // The lines about skipped and refused files already say there is
        // nothing to do.
        if !plan.is_empty() || (report.skipped.is_empty() && report.refused.is_empty()) {
            stdout.extend(plan.lines().iter().map(|line| sanitize(line)));
        }
    }
    for imported in &report.imported {
        stdout.push(format!(
            "Imported {} as {} ({})",
            sanitize(&imported.file),
            sanitize(&imported.path.to_string()),
            imported.id
        ));
    }
    let skip = if report.dry_run {
        "Would skip"
    } else {
        "Skipped"
    };
    for skipped in &report.skipped {
        let file = sanitize(&skipped.file);
        let id = skipped
            .id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "an earlier file".to_string());
        stdout.push(match skipped.reason {
            SkipReason::AlreadyImported => format!("{skip} {file}: already imported as {id}"),
            SkipReason::EntryCompleted => format!(
                "{skip} {file}: already imported as {id}; its tracking entry is written now"
            ),
            SkipReason::SameArchiveInThisRun => {
                format!("{skip} {file}: the same archive as an earlier file of this run")
            }
        });
    }
    for refused in &report.refused {
        stderr.push(format!(
            "{} was not imported: {}",
            sanitize(&refused.file),
            sanitize(&refused.reason)
        ));
    }
    stderr.extend(
        report
            .warnings
            .iter()
            .map(|warning| format!("note: {}", sanitize(&warning.message))),
    );
    if let Some(holder) = &report.lock_holder {
        // The machine name comes from a file; it is made safe for the
        // terminal like every other name.
        stderr.push(format!(
            "note: {}",
            sanitize(&report::lock_note(holder).message)
        ));
    }
    Lines { stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::id::ObjectId;
    use antiq::ops::import::{ImportAction, Imported, Refused, Skipped};
    use antiq::relpath::{Name, RelPath};
    use antiq::report::Warning;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

    fn report() -> ImportReport {
        ImportReport {
            dry_run: false,
            actions: None,
            imported: Vec::new(),
            skipped: Vec::new(),
            refused: Vec::new(),
            warnings: Vec::new(),
            lock_holder: None,
        }
    }

    fn skipped(reason: SkipReason, id: Option<ObjectId>) -> Skipped {
        Skipped {
            file: "a.tar.zst".into(),
            id,
            reason,
        }
    }

    #[test]
    fn every_reason_to_skip_has_its_words() {
        let id: ObjectId = ID.parse().expect("an ID");
        let lines = render(&ImportReport {
            skipped: vec![
                skipped(SkipReason::AlreadyImported, Some(id)),
                skipped(SkipReason::EntryCompleted, Some(id)),
                skipped(SkipReason::SameArchiveInThisRun, None),
            ],
            ..report()
        });
        assert_eq!(
            lines.stdout,
            [
                format!("Skipped a.tar.zst: already imported as {ID}"),
                format!(
                    "Skipped a.tar.zst: already imported as {ID}; its tracking entry is written now"
                ),
                "Skipped a.tar.zst: the same archive as an earlier file of this run".to_string(),
            ]
        );
    }

    #[test]
    fn names_are_shown_without_control_characters() {
        let lines = render(&ImportReport {
            imported: vec![Imported {
                id: ID.parse().expect("an ID"),
                file: "a\nb.tar.zst".into(),
                path: RelPath::parse("p").expect("a path"),
                name: Name::from_bytes(b"p".to_vec()).expect("a name"),
                created: "2001-09-09T01:46:40Z".parse().expect("a time"),
                size: 1,
                sha256: "0".repeat(64),
                payload_version: 1,
            }],
            refused: vec![Refused {
                file: "c\rd".into(),
                reason: "e\u{1b}f".into(),
            }],
            warnings: vec![Warning::new("g\u{7}h")],
            ..report()
        });
        assert_eq!(
            lines.stdout,
            [format!("Imported a%0Ab.tar.zst as p ({ID})")]
        );
        assert_eq!(
            lines.stderr,
            [
                "c%0Dd was not imported: e%1Bf".to_string(),
                "note: g%07h".to_string()
            ]
        );
    }

    #[test]
    fn a_dry_run_that_refused_everything_prints_no_plan() {
        let lines = render(&ImportReport {
            dry_run: true,
            refused: vec![Refused {
                file: "x".into(),
                reason: "no".into(),
            }],
            ..report()
        });
        assert!(lines.stdout.is_empty());
        assert_eq!(lines.stderr, ["x was not imported: no"]);
    }

    #[test]
    fn a_dry_run_with_actions_prints_them_as_a_plan() {
        let lines = render(&ImportReport {
            dry_run: true,
            actions: Some(vec![ImportAction::Import {
                file: "a.tar.zst".into(),
                path: RelPath::parse("p").expect("a path"),
            }]),
            ..report()
        });
        assert_eq!(lines.stdout, ["Would import a.tar.zst as p"]);
    }

    #[test]
    fn a_dry_run_with_nothing_at_all_says_so() {
        let lines = render(&ImportReport {
            dry_run: true,
            ..report()
        });
        assert_eq!(lines.stdout, ["Nothing to do."]);
    }
}
