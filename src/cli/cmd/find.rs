// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `find` command.
//!
//! The matches come from the library. This file lists them for a console:
//! a table of state, piece, size and ID, wide where the terminal has room
//! and narrow at 80 columns. Interactively it lists too; the picker is not
//! built yet.

use crate::cli::cmd::status::{Color, WIDE_FROM, mid, pad, pad_left, paint, sanitize};
use crate::cli::output::{Output, Style};
use crate::cli::{Context, FindArgs};
use antiq::collection::Collection;
use antiq::ops::find::{FindReport, FoundPiece, FoundState, find};
use anyhow::Context as _;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

pub fn run(ctx: &Context, args: &FindArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let report = find(&collection, args.query.as_deref()).context("search the pieces")?;

    // Under `--json` the notes are part of the object and nothing goes to
    // stderr.
    if ctx.json {
        out.json(Some("find"), &report)
            .context("print the result")?;
        return Ok(0);
    }
    if args.print {
        // A shell reads these back, so each is the path as the filesystem
        // holds it, not the escaped form of the table.
        for found in &report.matches {
            out.line_os(found.folder.as_os_str())
                .context("print the result")?;
        }
    } else {
        for line in table(&report.matches, out.stdout_style()) {
            out.line(&line).context("print the result")?;
        }
    }
    for line in notes(&report) {
        out.note(&line).context("print the notes")?;
    }
    Ok(0)
}

/// What stderr says: that nothing matched, and the warnings.
fn notes(report: &FindReport) -> Vec<String> {
    let mut lines = Vec::new();
    if report.matches.is_empty() {
        lines.push(match &report.query {
            Some(query) => format!("no piece matches \"{}\"", sanitize(query)),
            None => "no pieces".to_string(),
        });
    }
    lines.extend(
        report
            .warnings
            .iter()
            .map(|warning| format!("note: {}", sanitize(&warning.message))),
    );
    lines
}

// =========================================================
// The table
// =========================================================

/// The width the PIECE column keeps at least, as far as the line allows.
const PIECE_MIN: usize = 20;

/// The words of the STATE column: the names of spec "States".
fn state_word(state: FoundState) -> &'static str {
    match state {
        FoundState::Piece(state) => match state {
            antiq::tracking::PieceState::New => "new",
            antiq::tracking::PieceState::Live => "live",
            antiq::tracking::PieceState::Unearthed => "unearthed",
            antiq::tracking::PieceState::Gone => "gone",
            antiq::tracking::PieceState::Archived => "archived",
            antiq::tracking::PieceState::RemoteOnly => "remote-only",
            antiq::tracking::PieceState::LiveOnAnotherMachine => "live-on-another-machine",
        },
        FoundState::Error => "error",
    }
}

/// A size in bytes for people: `512 B`, `1.5 KiB`, `12 MiB`. A figure
/// below 10 keeps one decimal.
fn human_size(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "-".to_string();
    };
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

struct Row {
    state: &'static str,
    error: bool,
    path: String,
    size: String,
    created: String,
    id: String,
}

fn table(matches: &[FoundPiece], style: Style) -> Vec<String> {
    if matches.is_empty() {
        return Vec::new();
    }
    let wide = style.width >= WIDE_FROM;
    let rows: Vec<Row> = matches
        .iter()
        .map(|found| Row {
            state: state_word(found.state),
            error: found.state == FoundState::Error,
            path: sanitize(&found.path.to_string()),
            size: human_size(found.size),
            created: found.created.strftime("%Y-%m-%d").to_string(),
            id: if wide {
                found.id.to_string()
            } else {
                found.short_id.clone()
            },
        })
        .collect();

    let widest = |header: &str, cell: fn(&Row) -> usize| {
        rows.iter().map(cell).max().unwrap_or(0).max(header.len())
    };
    let state_w = widest("STATE", |row| row.state.width());
    let size_w = widest("SIZE", |row| row.size.width());
    let created_w = if wide {
        widest("CREATED", |row| row.created.width())
    } else {
        0
    };
    let id_w = widest("ID", |row| row.id.width());
    let path_longest = widest("PIECE", |row| row.path.width());

    // Every column but PIECE keeps its width; PIECE gets what is left, down
    // to a floor, and a path that does not fit loses its middle.
    let others = state_w + size_w + id_w + if wide { created_w + 8 } else { 6 };
    let piece_w = path_longest.min(style.width.saturating_sub(others).max(PIECE_MIN));

    let cells = |state: String, piece: String, size: String, created: String, id: &str| {
        let mut line = format!("{}  {}  {}", state, pad(&piece, piece_w), size);
        if wide {
            line.push_str(&format!("  {}", pad(&created, created_w)));
        }
        line.push_str(&format!("  {id}"));
        line
    };

    let header = cells(
        pad("STATE", state_w),
        "PIECE".to_string(),
        pad_left("SIZE", size_w),
        "CREATED".to_string(),
        "ID",
    );
    let mut lines = vec![paint(style, Color::Dim, &header)];
    for row in &rows {
        let state = paint(
            style,
            if row.error { Color::Red } else { Color::Plain },
            row.state,
        );
        let state = format!("{state}{}", " ".repeat(state_w - row.state.width()));
        lines.push(cells(
            state,
            mid(&row.path, piece_w),
            pad_left(&row.size, size_w),
            row.created.clone(),
            &row.id,
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::id::ObjectId;
    use antiq::relpath::{Name, RelPath};
    use antiq::report::Warning;
    use antiq::tracking::PieceState;

    fn style(width: usize, color: bool) -> Style {
        Style { width, color }
    }

    fn found(number: u64, path: &str, state: FoundState, size: Option<u64>) -> FoundPiece {
        let rel = RelPath::parse(path).expect("a valid path");
        let name = Name::from_bytes(rel.components().last().expect("a name").to_vec())
            .expect("a valid name");
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_000 + number, 1));
        FoundPiece {
            id,
            abs_path: format!("/work/{path}"),
            folder: PathBuf::from(format!("/work/{path}")),
            short_id: id.to_string()[..10].to_string(),
            path: rel,
            name,
            state,
            created: "2026-09-03T08:00:00Z".parse().expect("a time"),
            size,
        }
    }

    fn matches() -> Vec<FoundPiece> {
        vec![
            found(
                1,
                "2026_09_03_data-extraction",
                FoundState::Piece(PieceState::Archived),
                Some(12_582_912),
            ),
            found(
                2,
                "2026_10_30_todo-run",
                FoundState::Piece(PieceState::RemoteOnly),
                Some(512),
            ),
            found(3, "scratch", FoundState::Piece(PieceState::Live), None),
        ]
    }

    fn text(lines: Vec<String>) -> String {
        lines.join("\n")
    }

    #[test]
    fn at_80_columns_a_long_path_loses_its_middle() {
        insta::assert_snapshot!(text(table(&matches(), style(80, false))), @r"
        STATE        PIECE                                           SIZE  ID
        archived     2026_09_03_data-extraction                    12 MiB  01EJ3PX001
        remote-only  2026_10_30_todo-run   512 B  01EJ3PX002
        live         scratch                                            -  01EJ3PX003
        ");
    }

    #[test]
    fn a_path_that_does_not_fit_is_shortened_in_the_middle() {
        let narrow = text(table(&matches(), style(60, false)));
        insta::assert_snapshot!(narrow, @r"
        STATE        PIECE                          SIZE  ID
        archived     2026_09_03_data-extraction   12 MiB  01EJ3PX001
        remote-only  2026_10_30_todo-run…f-by-agents   512 B  01EJ3PX002
        live         scratch                           -  01EJ3PX003
        ");
    }

    #[test]
    fn at_120_columns_the_table_adds_the_creation_date_and_the_whole_id() {
        insta::assert_snapshot!(text(table(&matches(), style(120, false))), @r"
        STATE        PIECE                                           SIZE  CREATED     ID
        archived     2026_09_03_data-extraction                    12 MiB  2026-09-03  01EJ3PX0010000000000000001
        remote-only  2026_10_30_todo-run   512 B  2026-09-03  01EJ3PX0020000000000000001
        live         scratch                                            -  2026-09-03  01EJ3PX0030000000000000001
        ");
    }

    #[test]
    fn an_object_in_error_is_red_and_the_header_is_dim_on_a_terminal() {
        let mut list = matches();
        list[2].state = FoundState::Error;
        let lines = table(&list, style(80, true));
        assert!(lines[0].starts_with("\x1b[2mSTATE"), "{:?}", lines[0]);
        let red = lines[3].starts_with("\x1b[31merror\x1b[0m");
        assert!(red, "{lines:?}");
        assert!(!lines[1].contains("\x1b[31m"), "{lines:?}");
    }

    #[test]
    fn a_control_character_in_a_path_is_shown_escaped() {
        let list = vec![found(
            1,
            "a\nb",
            FoundState::Piece(PieceState::Live),
            Some(1),
        )];
        assert!(text(table(&list, style(80, false))).contains("a%0Ab"));
    }

    #[test]
    fn no_match_means_no_table() {
        assert!(table(&[], style(80, false)).is_empty());
    }

    #[test]
    fn sizes_are_given_in_the_largest_unit_below_1024() {
        let sizes = [
            (None, "-"),
            (Some(0), "0 B"),
            (Some(1023), "1023 B"),
            (Some(1024), "1.0 KiB"),
            (Some(1536), "1.5 KiB"),
            (Some(10 * 1024), "10 KiB"),
            (Some(12 * 1024 * 1024), "12 MiB"),
            (Some(3 * 1024 * 1024 * 1024), "3.0 GiB"),
            (Some(5 * 1024 * 1024 * 1024 * 1024), "5.0 TiB"),
            (Some(2048 * 1024 * 1024 * 1024 * 1024), "2048 TiB"),
        ];
        for (bytes, shown) in sizes {
            assert_eq!(human_size(bytes), shown, "{bytes:?}");
        }
    }

    #[test]
    fn every_state_has_its_word() {
        let states = [
            (PieceState::New, "new"),
            (PieceState::Live, "live"),
            (PieceState::Unearthed, "unearthed"),
            (PieceState::Gone, "gone"),
            (PieceState::Archived, "archived"),
            (PieceState::RemoteOnly, "remote-only"),
            (PieceState::LiveOnAnotherMachine, "live-on-another-machine"),
        ];
        for (state, word) in states {
            assert_eq!(state_word(FoundState::Piece(state)), word);
        }
        assert_eq!(state_word(FoundState::Error), "error");
    }

    fn report(query: Option<&str>, matches: Vec<FoundPiece>, warnings: &[&str]) -> FindReport {
        FindReport {
            query: query.map(str::to_string),
            matches,
            warnings: warnings.iter().map(|text| Warning::new(*text)).collect(),
        }
    }

    #[test]
    fn nothing_found_is_said_on_stderr_with_the_query() {
        assert_eq!(
            notes(&report(Some("wo\nlf"), Vec::new(), &[])),
            ["no piece matches \"wo%0Alf\""]
        );
        assert_eq!(notes(&report(None, Vec::new(), &[])), ["no pieces"]);
    }

    #[test]
    fn warnings_follow_as_notes_and_a_found_piece_needs_no_extra_line() {
        assert_eq!(
            notes(&report(None, matches(), &["watch\nout"])),
            ["note: watch%0Aout"]
        );
    }
}
