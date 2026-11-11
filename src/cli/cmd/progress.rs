// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What `archive`, `push`, `unearth`, `fork`, `rebuild` and `import` share when they show
//! cards on a terminal: when to show them, how they are set up, and the summary line
//! and the pieces that never sent an event.

use crate::cli::Context;
use crate::cli::console::cards::{Cards, Config, Count, Ready, Summary, Tone};
use crate::cli::console::live::{Label, Setup, observed};
use crate::cli::output::Output;
use antiq::commands::archive::{ArchiveReport, ArchivedPiece, RefusedPiece, SkippedPiece};
use antiq::commands::import::ImportReport;
use antiq::commands::push::PushReport;
use antiq::progress::{Observer, Operation, Outcome};
use antiq::shown::Shown as Named;
use antiq::shown::escape_controls;
use anyhow::Context as _;
use std::path::Path;
use std::time::Duration;

/// Fits `12 345 files`, `unfinished` and `skipped`, the results of `archive`,
/// `unearth` and `fork`.
const DEFAULT_RESULT_WIDTH: usize = 12;

/// How a command describes itself to the cards.
pub(super) struct Description {
    /// The operations whose count does not read `N/M files`.
    pub counts: &'static [(Operation, Count)],
    /// Printed before the name on the card of a running piece.
    pub name_prefix: &'static str,
    /// The width of the result column of a finished line, sized to the
    /// longest result the command usually leaves.
    pub result_width: usize,
    /// How the version of an unearth reads; the stamp for any other command.
    pub label: Label,
}

impl Description {
    pub(super) fn new(counts: &'static [(Operation, Count)]) -> Description {
        Description {
            counts,
            name_prefix: "",
            result_width: DEFAULT_RESULT_WIDTH,
            label: Box::new(|stamp, _| stamp.to_string()),
        }
    }
}

/// The cards of a run that showed them, kept for the last line.
pub(super) struct Shown {
    cards: Cards,
    took: Duration,
}

impl Shown {
    /// The closing line, painted like the lines above it.
    pub(super) fn summary_line(&self, summary: &Summary) -> String {
        self.cards.summary_line(summary)
    }

    /// An empty line, then the closing line.
    pub(super) fn print_summary(&self, out: &mut Output, summary: &Summary) -> anyhow::Result<()> {
        out.line("").context("print the result")?;
        out.line(&self.summary_line(summary))
            .context("print the result")
    }

    /// From the start of the run to its end.
    pub(super) fn took(&self) -> Duration {
        self.took
    }
}

/// Runs a command with cards when its stdout is a terminal that shows them,
/// and plainly otherwise. A pipe, `--json` and a dry run keep the lines they
/// always had.
///
/// `watched` is the library call that tells an observer what it does,
/// `unwatched` the one that does not. `missing` names the pieces of the
/// result that never sent an event, which get their line at the end.
pub(super) fn watch<T>(
    ctx: &Context,
    out: &mut Output,
    dry_run: bool,
    description: Description,
    watched: impl FnOnce(&dyn Observer) -> T,
    unwatched: impl FnOnce() -> T,
    missing: impl FnOnce(&T) -> Vec<Ready>,
) -> anyhow::Result<(T, Option<Shown>)> {
    if ctx.json || dry_run || !out.stdout_env().rich {
        return Ok((unwatched(), None));
    }
    let setup = Setup {
        config: Config {
            counts: description.counts,
            name_prefix: description.name_prefix,
            result_width: description.result_width,
            style: out.stdout_style(),
            directory: ctx.directory.clone(),
        },
        label: description.label,
        panic_hook: true,
    };
    let run = observed(out, setup, watched, missing).context("show the progress")?;
    let shown = Shown {
        cards: run.cards,
        took: run.took,
    };
    Ok((run.value, Some(shown)))
}

pub(super) fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

/// The mark of a summary follows the exit code of the run.
pub(super) fn exit_tone(code: u8) -> Tone {
    if code == 0 { Tone::Ok } else { Tone::Bad }
}

// =========================================================
// archive
// =========================================================

/// The pieces of the report in the order the pipe lists them. The ones that
/// sent no event get their line from these.
pub(super) fn archive_pieces(report: &ArchiveReport) -> Vec<Ready> {
    listed_pieces(&report.archived, &report.skipped, &report.refused)
}

fn listed_pieces(
    archived: &[ArchivedPiece],
    skipped: &[SkippedPiece],
    refused: &[RefusedPiece],
) -> Vec<Ready> {
    let ready = |name: &str, outcome| Ready {
        name: name.to_string(),
        bytes: None,
        label: None,
        outcome,
        elapsed: None,
    };
    let mut pieces = Vec::new();
    for piece in archived {
        pieces.push(ready(&piece.piece, Outcome::Files(piece.files)));
    }
    for piece in skipped {
        pieces.push(ready(&piece.piece, Outcome::Skipped(piece.message.clone())));
    }
    for piece in refused {
        let reasons = piece.reasons.iter().map(|r| r.text.clone()).collect();
        pieces.push(ready(&piece.piece, Outcome::Refused(reasons)));
    }
    pieces
}

/// `None` when the run had no piece to tell about.
pub(super) fn archive_summary(report: &ArchiveReport, took: Duration) -> Option<Summary> {
    let (archived, skipped, refused) = (
        report.archived.len(),
        report.skipped.len(),
        report.refused.len(),
    );
    if archived + skipped + refused == 0 {
        return None;
    }
    let count = |count: usize, word: &str, tone| {
        (
            format!("{count} {word}"),
            if count > 0 { tone } else { Tone::Plain },
        )
    };
    Some(Summary {
        tone: exit_tone(report.exit_code()),
        parts: vec![
            count(archived, "archived", Tone::Ok),
            count(skipped, "skipped", Tone::Warn),
            count(refused, "refused", Tone::Bad),
        ],
        took,
    })
}

// =========================================================
// import
// =========================================================

/// The closing line of an import that showed its card: the version that
/// came about, or that the folder was skipped.
pub(super) fn import_summary(report: &ImportReport, took: Duration) -> Summary {
    let (text, tone) = if let Some(version) = &report.imported {
        let piece = escape_controls(&version.piece);
        (
            format!("Imported {piece} as version {}", version.stamp),
            Tone::Ok,
        )
    } else {
        let skipped = (report.skipped.as_ref()).expect("an import ends with a version or a skip");
        (
            format!("Skipped {}", escape_controls(&skipped.piece)),
            Tone::Warn,
        )
    };
    Summary {
        tone,
        parts: vec![(text, tone)],
        took,
    }
}

// =========================================================
// push
// =========================================================

pub(super) fn push_pieces(report: &PushReport) -> Vec<Ready> {
    report
        .changes
        .pieces
        .iter()
        .map(|piece| Ready {
            name: piece.piece.to_string(),
            bytes: None,
            label: None,
            outcome: Outcome::Changes {
                created: piece.counts.created,
                updated: piece.counts.updated,
                deleted: piece.counts.deleted,
            },
            elapsed: None,
        })
        .collect()
}

/// The summary of a push that changed pieces. `target` is named when the
/// push claimed it.
pub(super) fn push_summary(report: &PushReport, target: Option<&str>, took: Duration) -> Summary {
    let counts = &report.changes;
    let files: u64 = counts
        .pieces
        .iter()
        .map(|piece| piece.counts)
        .chain([counts.other])
        .map(|c| c.created + c.updated + c.deleted)
        .sum();
    let files = usize::try_from(files).unwrap_or(usize::MAX);
    let mut parts = Vec::new();
    if let Some(target) = target {
        parts.push((format!("Claimed {target}"), Tone::Ok));
    }
    parts.push((
        plural(counts.pieces.len(), "piece pushed", "pieces pushed"),
        Tone::Ok,
    ));
    parts.push((plural(files, "file changed", "files changed"), Tone::Plain));
    Summary {
        tone: Tone::Ok,
        parts,
        took,
    }
}

// =========================================================
// unearth
// =========================================================

/// Where the piece came back to, as a path to type from `cwd`. `path` is
/// relative to the collection `root`.
pub(super) fn destination(root: &Path, path: &str, cwd: &Path) -> String {
    let absolute = root.join(path);
    match absolute.strip_prefix(cwd) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
        Ok(rest) => format!("./{}", Named(&rest)),
        Err(_) => Named(&absolute).to_string(),
    }
}

/// What a command that brings a version back tells at the end of a run.
pub(super) struct Restored<'a> {
    /// `Unearthed` or `Forked`.
    pub verb: &'a str,
    pub piece: &'a str,
    /// The folder that came about, relative to the collection root.
    pub path: &'a str,
    pub skipped: bool,
    pub unfinished: bool,
}

/// The closing line of an unearth or a fork that showed cards: where the
/// folder came back to, or that the version stayed in the store.
pub(super) fn restore_summary(
    restored: &Restored,
    root: &Path,
    cwd: &Path,
    took: Duration,
) -> Summary {
    let (text, tone) = if restored.skipped {
        (
            format!("Skipped {}", escape_controls(restored.piece)),
            Tone::Warn,
        )
    } else {
        let destination = escape_controls(&destination(root, restored.path, cwd));
        let tone = if restored.unfinished {
            Tone::Bad
        } else {
            Tone::Ok
        };
        (format!("{} to {destination}", restored.verb), tone)
    };
    Summary {
        tone,
        parts: vec![(text, tone)],
        took,
    }
}

/// The two lines of the card of a piece whose `operation` has counted
/// `done` of `total`, on a terminal of 96 columns without colour. The
/// tests of the commands use it to pin the words and counts their cards show.
#[cfg(test)]
pub(super) fn card_of(
    counts: &'static [(Operation, Count)],
    operation: Operation,
    done: u64,
    total: u64,
) -> [String; 2] {
    use crate::cli::console::cards::{Cards, Msg};
    use crate::cli::output::Style;
    use antiq::progress::Counter;

    let mut cards = Cards::new(Config {
        counts,
        name_prefix: "",
        result_width: DEFAULT_RESULT_WIDTH,
        style: Style {
            width: 96,
            color: false,
            terminal: true,
            utf8: true,
        },
        directory: None,
    });
    cards.apply(
        Msg::Started {
            name: "projects/scanner".to_string(),
            bytes: Some(10),
            label: None,
        },
        Duration::ZERO,
    );
    let counter = Counter::new();
    counter.set_total(total);
    counter.add(done);
    cards.apply(
        Msg::Operation {
            name: "projects/scanner".to_string(),
            operation,
            counter,
        },
        Duration::ZERO,
    );
    let region = cards.region(Duration::ZERO, 96, 24);
    [region[0].clone(), region[1].clone()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::commands::import::{ImportedVersion, SkippedImport};

    #[test]
    fn a_destination_below_the_directory_is_a_relative_path() {
        let root = Path::new("/data/collection");
        assert_eq!(
            destination(root, "projects/recipe-api", root),
            "./projects/recipe-api"
        );
        assert_eq!(destination(root, "", root), ".");
        assert_eq!(
            destination(
                root,
                "projects/recipe-api",
                Path::new("/data/collection/projects")
            ),
            "./recipe-api"
        );
        assert_eq!(
            destination(root, "projects/recipe-api", Path::new("/elsewhere")),
            "/data/collection/projects/recipe-api"
        );
    }

    #[test]
    fn the_pieces_of_a_report_are_listed_archived_then_skipped_then_refused() {
        use antiq::commands::archive::{Reason, SkipReason};
        let archived = [ArchivedPiece {
            piece: "a".to_string(),
            stamp: "2026-11-05T130211".to_string(),
            version: jiff::Timestamp::UNIX_EPOCH,
            archive_bytes: 3,
            files: 4,
            bytes: 9,
        }];
        let skipped = [SkippedPiece {
            piece: "b".to_string(),
            reason: SkipReason::Differs,
            message: "it changed".to_string(),
        }];
        let refused = [RefusedPiece {
            piece: "c".to_string(),
            reasons: vec![Reason {
                kind: "empty",
                path: "c".to_string(),
                text: "empty: c".to_string(),
                line: None,
            }],
        }];
        let listed: Vec<(String, Outcome)> = listed_pieces(&archived, &skipped, &refused)
            .into_iter()
            .map(|ready| (ready.name, ready.outcome))
            .collect();
        assert_eq!(
            listed,
            [
                ("a".to_string(), Outcome::Files(4)),
                ("b".to_string(), Outcome::Skipped("it changed".to_string())),
                (
                    "c".to_string(),
                    Outcome::Refused(vec!["empty: c".to_string()])
                ),
            ]
        );
    }

    #[test]
    fn a_count_of_one_is_singular() {
        assert_eq!(plural(1, "file changed", "files changed"), "1 file changed");
        assert_eq!(
            plural(0, "file changed", "files changed"),
            "0 files changed"
        );
        assert_eq!(
            plural(9, "file changed", "files changed"),
            "9 files changed"
        );
    }

    #[test]
    fn the_mark_of_a_summary_follows_the_exit_code() {
        assert_eq!(exit_tone(0), Tone::Ok);
        assert_eq!(exit_tone(1), Tone::Bad);
        assert_eq!(exit_tone(3), Tone::Bad);
    }

    fn imported_report(
        version: Option<ImportedVersion>,
        skipped: Option<SkippedImport>,
    ) -> ImportReport {
        ImportReport {
            imported: version,
            skipped,
            repairs: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn the_summary_of_an_import_names_the_piece_and_the_version() {
        let report = imported_report(
            Some(ImportedVersion {
                piece: "projects/scanner".to_string(),
                stamp: "2023-11-14T221320".to_string(),
                version: jiff::Timestamp::UNIX_EPOCH,
                archive_bytes: 3,
                files: 4,
                bytes: 9,
            }),
            None,
        );
        let summary = import_summary(&report, Duration::from_secs(5));
        assert_eq!(summary.tone, Tone::Ok);
        assert_eq!(
            summary.parts,
            [(
                "Imported projects/scanner as version 2023-11-14T221320".to_string(),
                Tone::Ok
            )]
        );
        assert_eq!(summary.took, Duration::from_secs(5));
    }

    #[test]
    fn the_summary_of_a_skipped_import_names_the_piece() {
        let report = imported_report(
            None,
            Some(SkippedImport {
                piece: "projects/scanner".to_string(),
                reason: antiq::commands::archive::SkipReason::NoRoom,
                message: "no room".to_string(),
            }),
        );
        let summary = import_summary(&report, Duration::from_secs(5));
        assert_eq!(summary.tone, Tone::Warn);
        assert_eq!(
            summary.parts,
            [("Skipped projects/scanner".to_string(), Tone::Warn)]
        );
    }
}
