// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` command.

use super::status::{Color, mid, pad, pad_left, paint, sanitize, spaces, wrap};
use crate::cli::output::{Output, Style};
use crate::cli::{Context, DryRunArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::id::RandomIds;
use antiq::ops::archive::{ArchiveReport, ArchiveRequest, KeptHow, archive, plan, size_text};
use antiq::pack::pool::Pool;
use antiq::report::CommandReport;
use anyhow::Context as _;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::thread;
use unicode_width::UnicodeWidthStr;

pub fn run(ctx: &Context, args: &DryRunArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let report = if args.dry_run {
        plan(&collection, &SystemClock)
            .map_err(antiq::Error::from)
            .context("plan the archive")?
    } else {
        let pool = Pool::for_machine(collection.config().collection.memory_percent())
            .map_err(antiq::Error::from)
            .context("size the compression pool")?;
        let request = ArchiveRequest {
            collection: &collection,
            clock: &SystemClock,
            ids: &RandomIds,
            pool: &pool,
            workers: thread::available_parallelism().map_or(1, NonZeroUsize::get),
            dry_run: false,
        };
        archive(&request)
            .map_err(antiq::Error::from)
            .context("archive the pieces")?
    };

    // Under `--json` the warnings are part of the object and nothing goes
    // to stderr.
    if ctx.json {
        out.json(Some("archive"), &report)
            .context("print the result")?;
        return Ok(exit_code(&report));
    }
    let lines = render(&report, out.stdout_style());
    for line in &lines.stdout {
        out.line(line).context("print the result")?;
    }
    for line in &lines.stderr {
        out.note(line).context("print the notes")?;
    }
    Ok(exit_code(&report))
}

fn exit_code(report: &ArchiveReport) -> u8 {
    if report.partial() { 3 } else { 0 }
}

/// What a run prints on a console.
#[derive(Debug, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

fn render(report: &ArchiveReport, style: Style) -> Lines {
    let stdout = match &report.plan {
        Some(plan) => plan.lines(),
        None => finished_lines(report, style),
    };
    let stderr = report
        .warnings()
        .iter()
        .map(|warning| format!("note: {}", sanitize(&warning.message)))
        .collect();
    Lines { stdout, stderr }
}

// =========================================================
// The rows of a finished run
// =========================================================

/// The word that tells how a piece ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Word {
    Removed,
    Kept,
    Skipped,
}

impl Word {
    fn text(self) -> &'static str {
        match self {
            Word::Removed => "removed",
            Word::Kept => "kept",
            Word::Skipped => "skipped",
        }
    }

    fn color(self) -> Color {
        match self {
            Word::Removed => Color::Green,
            Word::Kept => Color::Yellow,
            Word::Skipped => Color::Red,
        }
    }
}

/// One finished piece as a row: its path, its size when the report knows
/// it, the word and one detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub path: String,
    pub size: Option<u64>,
    pub word: Word,
    pub detail: String,
}

/// From this width on, every row stays on one line.
const WIDE_FROM: usize = 100;
/// The indent of a row.
const INDENT: usize = 2;
/// The column gap.
const GAP: usize = 2;
/// The widest word, `skipped` and `removed`.
const WORD_WIDTH: usize = 7;

fn rows(report: &ArchiveReport) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for done in report.archived.iter().flatten() {
        rows.push(Row {
            path: sanitize(&done.path.to_string()),
            size: Some(done.size),
            word: Word::Removed,
            detail: format!("{} archive, verified", size_text(done.archive_size)),
        });
    }
    for row in report.kept.iter().flatten() {
        let detail = match (row.how, &row.recovered_as) {
            (KeptHow::Unearthed, _) => "changed after it was verified, unearthed again".to_string(),
            (KeptHow::Recovered, Some(name)) => format!(
                "changed after it was verified, kept in .antiq/recovered/{}",
                sanitize(name)
            ),
            (KeptHow::Recovered, None) => {
                "changed after it was verified, kept in .antiq/recovered".to_string()
            }
        };
        rows.push(Row {
            path: sanitize(&row.path.to_string()),
            size: None,
            word: Word::Kept,
            detail,
        });
    }
    for row in report.skipped.iter().flatten() {
        rows.push(Row {
            path: sanitize(&row.path.to_string()),
            size: None,
            word: Word::Skipped,
            detail: sanitize(&row.message),
        });
    }
    // The report lists each outcome in path order; the console lists the
    // pieces in path order.
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows
}

fn finished_lines(report: &ArchiveReport, style: Style) -> Vec<String> {
    let rows = rows(report);
    if rows.is_empty() {
        return vec!["Nothing to do.".to_string()];
    }
    let mut lines = vec![header(report), String::new()];
    lines.extend(row_lines(&rows, style));
    lines
}

/// `archived 2 pieces \u{b7} 1.6 GB`, then the counts of the other rows.
fn header(report: &ArchiveReport) -> String {
    let archived = report.archived.as_deref().unwrap_or_default();
    let total: u64 = archived.iter().map(|done| done.size).sum();
    let noun = if archived.len() == 1 {
        "piece"
    } else {
        "pieces"
    };
    let mut header = format!(
        "archived {} {noun} \u{b7} {}",
        archived.len(),
        size_text(total)
    );
    let kept = report.kept.as_deref().map_or(0, <[_]>::len);
    if kept > 0 {
        header.push_str(&format!(", {kept} kept"));
    }
    let skipped = report.skipped.as_deref().map_or(0, <[_]>::len);
    if skipped > 0 {
        header.push_str(&format!(", {skipped} skipped"));
    }
    header
}

/// The lines of finished rows, aligned in columns. Below 100 columns a row
/// takes one line when it fits and two otherwise: path and size, then the
/// word and detail. From 100 columns every row takes one line and a long
/// detail continues below.
pub(super) fn row_lines(rows: &[Row], style: Style) -> Vec<String> {
    let sizes: Vec<String> = rows
        .iter()
        .map(|row| row.size.map_or_else(|| "-".to_string(), size_text))
        .collect();
    let size_width = sizes.iter().map(|size| size.width()).max().unwrap_or(0);
    let wide = style.width >= WIDE_FROM;
    let beside = INDENT + GAP + size_width + GAP + WORD_WIDTH + GAP;

    // Rows that stay on one line below 100 columns. Padding every path to
    // the longest one can push a row over the width, which sends it to two
    // lines and may shorten the longest path, so this repeats.
    let mut one_line: Vec<bool> = rows
        .iter()
        .map(|row| wide || row.path.width() + beside + row.detail.width() <= style.width)
        .collect();
    let path_width = loop {
        let longest = rows
            .iter()
            .zip(&one_line)
            .filter(|(_, one)| **one)
            .map(|(row, _)| row.path.width())
            .max()
            .unwrap_or(0);
        if wide {
            break longest.min(style.width / 2);
        }
        let before = one_line.clone();
        for (row, one) in rows.iter().zip(one_line.iter_mut()) {
            if *one && longest + beside + row.detail.width() > style.width {
                *one = false;
            }
        }
        if before == one_line {
            break longest;
        }
    };

    let mut lines = Vec::new();
    for ((row, size), one) in rows.iter().zip(&sizes).zip(&one_line) {
        let word = paint(style, row.word.color(), row.word.text());
        let word_cell = format!("{word}{}", spaces(WORD_WIDTH - row.word.text().len()));
        let size_cell = paint(style, Color::Dim, &pad_left(size, size_width));
        if *one {
            let lead = format!(
                "{}{}{}{size_cell}{}{word_cell}{}",
                spaces(INDENT),
                pad(&mid(&row.path, path_width), path_width),
                spaces(GAP),
                spaces(GAP),
                spaces(GAP)
            );
            let indent = path_width + beside;
            let detail = wrap(&row.detail, style.width.saturating_sub(indent).max(1));
            continue_lines(&mut lines, lead, indent, &detail);
        } else {
            let room = style.width.saturating_sub(INDENT + GAP + size_width);
            lines.push(format!(
                "{}{}{}{size_cell}",
                spaces(INDENT),
                pad(&mid(&row.path, room), room),
                spaces(GAP)
            ));
            let lead = format!("{}{word_cell}{}", spaces(2 * INDENT), spaces(GAP));
            let indent = 2 * INDENT + WORD_WIDTH + GAP;
            let detail = wrap(&row.detail, style.width.saturating_sub(indent).max(1));
            continue_lines(&mut lines, lead, indent, &detail);
        }
    }
    lines
}

/// The first line of a detail after `lead`, the others indented by `indent`.
fn continue_lines(lines: &mut Vec<String>, lead: String, indent: usize, detail: &[String]) {
    for (index, text) in detail.iter().enumerate() {
        if index == 0 {
            lines.push(format!("{lead}{text}"));
        } else {
            lines.push(format!("{}{text}", spaces(indent)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::id::ObjectId;
    use antiq::ops::archive::{ArchiveAction, Archived, Kept, KeptHow, SkipReason, Skipped};
    use antiq::plan::Plan;
    use antiq::relpath::RelPath;
    use antiq::report::Warning;

    fn narrow() -> Style {
        Style {
            width: 80,
            color: false,
        }
    }

    fn wide() -> Style {
        Style {
            width: 100,
            color: false,
        }
    }

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
            archived: None,
            skipped: None,
            kept: None,
            warnings: vec![Warning::new("the collection is on an NFS mount")],
        };
        let lines = render(&report, narrow());
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
            archived: None,
            skipped: None,
            kept: None,
            warnings: Vec::new(),
        };
        assert_eq!(render(&report, narrow()).stdout, ["Nothing to do."]);
    }

    // =========================================================
    // A real run
    // =========================================================

    fn id(number: u64) -> ObjectId {
        ObjectId::from_ulid(ulid::Ulid::from_parts(
            1_600_000_000_000,
            u128::from(number),
        ))
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid")
    }

    fn archived(text: &str, size: u64, archive_size: u64) -> Archived {
        Archived {
            id: id(1),
            path: path(text),
            size,
            archive_size,
            payload_version: 1,
        }
    }

    fn kept(text: &str, how: KeptHow, recovered_as: Option<&str>) -> Kept {
        Kept {
            id: id(2),
            path: path(text),
            how,
            recovered_as: recovered_as.map(str::to_string),
        }
    }

    fn skipped(text: &str, reason: SkipReason, message: &str) -> Skipped {
        Skipped {
            id: None,
            path: path(text),
            reason,
            message: message.to_string(),
        }
    }

    fn run_report(
        archived: Vec<Archived>,
        kept: Vec<Kept>,
        skipped: Vec<Skipped>,
    ) -> ArchiveReport {
        ArchiveReport {
            dry_run: false,
            plan: None,
            archived: Some(archived),
            skipped: Some(skipped),
            kept: Some(kept),
            warnings: Vec::new(),
        }
    }

    fn every_outcome() -> ArchiveReport {
        run_report(
            vec![archived("projects/a", 412_000_000, 88_000_000)],
            vec![
                kept("projects/b", KeptHow::Unearthed, None),
                kept(
                    "projects/c",
                    KeptHow::Recovered,
                    Some("01k0-20261101T120000Z"),
                ),
            ],
            vec![
                skipped(
                    "projects/d",
                    SkipReason::NoSpace,
                    "the piece is 1.2 GB and 800 MB are free",
                ),
                skipped(
                    "projects/e",
                    SkipReason::Differs,
                    "the folder differs from the package",
                ),
            ],
        )
    }

    #[test]
    fn rows_at_80_columns_take_two_lines_when_the_detail_does_not_fit() {
        let lines = render(&every_outcome(), narrow());
        insta::assert_snapshot!(lines.stdout.join("\n"), @"
        archived 1 piece \u{b7} 412 MB, 2 kept, 2 skipped

          projects/a  412 MB  removed  88 MB archive, verified
          projects/b       -  kept     changed after it was verified, unearthed again
          projects/c                                                                   -
            kept     changed after it was verified, kept in
                     .antiq/recovered/01k0-20261101T120000Z
          projects/d       -  skipped  the piece is 1.2 GB and 800 MB are free
          projects/e       -  skipped  the folder differs from the package
        ");
    }

    #[test]
    fn rows_at_100_columns_keep_their_columns_and_wrap_the_detail() {
        let lines = render(&every_outcome(), wide());
        insta::assert_snapshot!(lines.stdout.join("\n"), @"
        archived 1 piece \u{b7} 412 MB, 2 kept, 2 skipped

          projects/a  412 MB  removed  88 MB archive, verified
          projects/b       -  kept     changed after it was verified, unearthed again
          projects/c       -  kept     changed after it was verified, kept in
                                       .antiq/recovered/01k0-20261101T120000Z
          projects/d       -  skipped  the piece is 1.2 GB and 800 MB are free
          projects/e       -  skipped  the folder differs from the package
        ");
    }

    #[test]
    fn a_path_that_does_not_fit_goes_above_its_row() {
        let report = run_report(
            vec![archived(
                "2026_09_02_print-server-ideas",
                412_000_000,
                88_000_000,
            )],
            Vec::new(),
            Vec::new(),
        );
        insta::assert_snapshot!(render(&report, narrow()).stdout.join("\n"), @"
        archived 1 piece \u{b7} 412 MB

          2026_09_02_print-server-ideas              412 MB
            removed  88 MB archive, verified
        ");
    }

    #[test]
    fn a_kept_row_without_a_recovered_name_names_the_directory() {
        let report = run_report(
            Vec::new(),
            vec![kept("a", KeptHow::Recovered, None)],
            Vec::new(),
        );
        assert!(
            render(&report, narrow())
                .stdout
                .join("\n")
                .ends_with("kept in .antiq/recovered")
        );
    }

    #[test]
    fn a_run_without_pieces_has_nothing_to_do() {
        let report = run_report(Vec::new(), Vec::new(), Vec::new());
        assert_eq!(render(&report, narrow()).stdout, ["Nothing to do."]);
    }

    #[test]
    fn a_skipped_row_makes_the_run_partial_and_a_kept_row_alone_does_not() {
        assert_eq!(exit_code(&every_outcome()), 3);
        let kept_only = run_report(
            Vec::new(),
            vec![kept("a", KeptHow::Unearthed, None)],
            Vec::new(),
        );
        assert_eq!(exit_code(&kept_only), 0);
    }

    #[test]
    fn control_characters_in_a_path_or_a_message_are_shown_escaped() {
        let report = run_report(
            Vec::new(),
            Vec::new(),
            vec![skipped("a\u{1b}b", SkipReason::Error, "bad\nthing")],
        );
        let text = render(&report, narrow()).stdout.join("\n");
        assert!(
            !text.contains('\u{1b}') && !text.contains("bad\nthing"),
            "{text}"
        );
        assert!(
            text.contains("a%1Bb") && text.contains("bad%0Athing"),
            "{text}"
        );
    }

    #[test]
    fn colour_is_used_for_the_word_only_on_a_coloured_style() {
        let report = run_report(vec![archived("a", 1, 1)], Vec::new(), Vec::new());
        let plain = render(&report, narrow()).stdout.join("\n");
        assert!(!plain.contains('\u{1b}'));
        let coloured = render(
            &report,
            Style {
                width: 80,
                color: true,
            },
        )
        .stdout
        .join("\n");
        assert!(
            coloured.contains("\u{1b}[32mremoved\u{1b}[0m"),
            "{coloured:?}"
        );
    }

    // The longest path is padded to for every row on one line, which can
    // leave no room for the detail of a row with a short path.
    #[test]
    fn padding_to_the_longest_path_can_send_a_short_row_to_two_lines() {
        let report = run_report(
            vec![archived("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", 1, 1)],
            vec![kept("b", KeptHow::Unearthed, None)],
            Vec::new(),
        );
        insta::assert_snapshot!(render(&report, narrow()).stdout.join("\n"), @"
        archived 1 piece \u{b7} 1 B, 1 kept

          b                                                                            -
            kept     changed after it was verified, unearthed again
          zzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  1 B  removed  1 B archive, verified
        ");
    }
}
