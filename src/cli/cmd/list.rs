// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `list` command: lists the versions.

use crate::cli::output::{Output, Style};
use crate::cli::table::{Color, WIDE_FROM, decimal_size, mid, pad, pad_left, paint, sanitize};
use crate::cli::{Context, ListArgs};
use antiq::collection::Collection;
use antiq::commands::list::{ListReport, Row, State, list};
use antiq::versions::naming::{local_labels, system_zone};
use anyhow::Context as _;
use jiff::tz::TimeZone;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &ListArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let query = args.query.as_deref();
    let report = list(&collection, query)
        .map_err(antiq::Error::from)
        .context("list the versions")?;

    if ctx.json {
        out.json(Some("list"), &report)
            .context("print the result")?;
        // The empty result is data in the JSON, and a note for the person
        // who reads the console.
        if let Some(text) = empty_note(&report, query) {
            out.note(&format!("note: {text}"))
                .context("print the notes")?;
        }
    } else {
        let lines = render(&report, query, out.stdout_style(), &system_zone());
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

/// The distance between two columns of the table.
const GAP: &str = "   ";

/// The least columns the piece keeps when a narrow terminal cuts it.
const LEAST_PIECE_WIDTH: usize = 8;

fn render(report: &ListReport, query: Option<&str>, style: Style, zone: &TimeZone) -> Lines {
    let stdout = if style.terminal {
        table(&report.versions, style, zone)
    } else {
        report.versions.iter().map(pipe_line).collect()
    };
    Lines {
        stdout,
        stderr: notes(report, query),
    }
}

/// The notes on stderr, in the order: settings, the lock, nothing to list,
/// the damage of each damaged version.
fn notes(report: &ListReport, query: Option<&str>) -> Vec<String> {
    let mut notes: Vec<String> = report
        .warnings
        .iter()
        .map(|warning| format!("note: {}", warning.message))
        .collect();
    notes.extend(report.note.iter().map(|note| format!("note: {note}")));
    notes.extend(empty_note(report, query).map(|text| format!("note: {text}")));
    for row in &report.versions {
        for fault in &row.damage {
            notes.push(format!(
                "note: {} {} is damaged: {fault}",
                sanitize(&row.piece.to_string()),
                row.stamp
            ));
        }
    }
    notes
}

/// Why there is no row, when there is none.
fn empty_note(report: &ListReport, query: Option<&str>) -> Option<String> {
    if !report.versions.is_empty() {
        return None;
    }
    Some(if report.store_has_versions {
        format!("no piece matches `{}`", query.unwrap_or("").trim())
    } else {
        "no versions in the collection".to_string()
    })
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Ok => "ok",
        State::Damaged => "damaged",
        State::Incomplete => "incomplete",
        State::NewerFormat => "newer-format",
        State::Unreadable => "unreadable",
    }
}

fn pipe_line(row: &Row) -> String {
    let number = |number: Option<u64>| number.map_or("-".to_string(), |number| number.to_string());
    [
        sanitize(&row.piece.to_string()),
        row.stamp.to_string(),
        number(row.archive_bytes),
        number(row.files),
        state_name(row.state).to_string(),
    ]
    .join("\t")
}

/// The words of the last column, and their colour. A sound version has none.
fn mark(state: State) -> Option<(&'static str, Color)> {
    match state {
        State::Ok => None,
        State::Damaged => Some(("damaged", Color::Red)),
        State::Incomplete => Some(("incomplete", Color::Yellow)),
        State::NewerFormat => Some(("newer format", Color::Magenta)),
        State::Unreadable => Some(("unreadable", Color::Red)),
    }
}

/// How each row names its version. A version is named by its local time,
/// with the UTC offset where another version of the piece renders alike. An
/// incomplete version can be named by its stamp only, so it is shown by its
/// stamp and takes no part in telling the others apart.
fn version_labels(rows: &[Row], zone: &TimeZone) -> Vec<String> {
    let mut labels = vec![String::new(); rows.len()];
    let mut start = 0;
    while start < rows.len() {
        let end = rows[start..]
            .iter()
            .position(|row| row.piece != rows[start].piece)
            .map_or(rows.len(), |length| start + length);
        let named: Vec<usize> = (start..end)
            .filter(|index| rows[*index].state != State::Incomplete)
            .collect();
        let stamps: Vec<_> = named.iter().map(|index| rows[*index].stamp).collect();
        for (index, label) in named.into_iter().zip(local_labels(&stamps, zone)) {
            labels[index] = label;
        }
        for index in start..end {
            if rows[index].state == State::Incomplete {
                labels[index] = rows[index].stamp.to_string();
            }
        }
        start = end;
    }
    labels
}

fn table(rows: &[Row], style: Style, zone: &TimeZone) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let labels = version_labels(rows, zone);
    let pieces: Vec<String> = rows
        .iter()
        .map(|row| sanitize(&row.piece.to_string()))
        .collect();
    let sizes: Vec<String> = rows
        .iter()
        .map(|row| row.archive_bytes.map_or("-".to_string(), decimal_size))
        .collect();
    let files: Vec<String> = rows
        .iter()
        .map(|row| row.files.map_or("-".to_string(), thousands))
        .collect();
    let marks: Vec<Option<(&str, Color)>> = rows.iter().map(|row| mark(row.state)).collect();

    let widest = |header: &str, cells: &[String]| {
        cells
            .iter()
            .map(|cell| unicode_width::UnicodeWidthStr::width(cell.as_str()))
            .chain([header.len()])
            .max()
            .expect("the header is a cell")
    };
    let version_width = widest("VERSION", &labels);
    let size_width = widest("SIZE", &sizes);
    let files_width = widest("FILES", &files);
    let mark_width = marks
        .iter()
        .flatten()
        .map(|(text, _)| text.len())
        .max()
        .unwrap_or(0);

    // A wide terminal shows the pieces whole. A narrow one cuts the longest
    // to what the other columns leave, but never below a few columns.
    let gaps = if mark_width > 0 { 4 } else { 3 };
    let others = version_width + size_width + files_width + mark_width + gaps * GAP.len();
    let longest = widest("PIECE", &pieces);
    let piece_width = if style.width >= WIDE_FROM {
        longest
    } else {
        longest.min(style.width.saturating_sub(others).max(LEAST_PIECE_WIDTH))
    };

    let header = [
        pad("PIECE", piece_width),
        pad("VERSION", version_width),
        pad("SIZE", size_width),
        pad("FILES", files_width),
    ]
    .join(GAP);
    let mut lines = vec![header.trim_end().to_string()];
    for index in 0..rows.len() {
        let mut line = [
            pad(&mid(&pieces[index], piece_width), piece_width),
            pad(&labels[index], version_width),
            pad_left(&sizes[index], size_width),
            pad_left(&files[index], files_width),
        ]
        .join(GAP);
        if let Some((text, color)) = marks[index] {
            line.push_str(GAP);
            line.push_str(&paint(style, color, text));
        }
        lines.push(line);
    }
    lines
}

/// The count with a space between the thousands, `48 211`.
fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(' ');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::commands::list::{Row, State};
    use antiq::relpath::RelPath;
    use antiq::report::Warning;
    use antiq::versions::stamp::Stamp;
    use jiff::tz::{self, TimeZone};
    use unicode_width::UnicodeWidthStr;

    const TERMINAL: Style = Style {
        width: 80,
        color: false,
        terminal: true,
    };
    const PIPE: Style = Style {
        width: 80,
        color: false,
        terminal: false,
    };

    fn row(piece: &str, stamp: &str, state: State, sizes: Option<(u64, u64, u64)>) -> Row {
        let stamp: Stamp = stamp.parse().expect("a stamp");
        Row {
            piece: RelPath::parse(piece).expect("a piece"),
            stamp,
            version: stamp.time(),
            state,
            archive_bytes: sizes.map(|(archive, _, _)| archive),
            bytes: sizes.map(|(_, bytes, _)| bytes),
            files: sizes.map(|(_, _, files)| files),
            damage: Vec::new(),
        }
    }

    fn sound(piece: &str, stamp: &str, archive: u64, files: u64) -> Row {
        row(piece, stamp, State::Ok, Some((archive, archive * 2, files)))
    }

    fn report(versions: Vec<Row>) -> ListReport {
        ListReport {
            versions,
            warnings: Vec::new(),
            note: None,
            store_has_versions: true,
        }
    }

    fn plus_one() -> TimeZone {
        TimeZone::fixed(tz::offset(1))
    }

    fn auckland() -> TimeZone {
        TimeZone::get("Pacific/Auckland").expect("the time zone database knows Auckland")
    }

    fn texts(lines: &[String]) -> Vec<&str> {
        lines.iter().map(String::as_str).collect()
    }

    fn example() -> ListReport {
        report(vec![
            sound(
                "projects/scanner",
                "2026-11-04T090211",
                1_100_000_000,
                48_211,
            ),
            sound(
                "projects/scanner",
                "2026-09-03T121500",
                1_040_000_000,
                47_980,
            ),
            sound("tools/scan-old", "2025-02-11T184007", 40_000_000, 812),
        ])
    }

    #[test]
    fn the_terminal_table_shows_local_time_decimal_sizes_and_grouped_files() {
        let lines = render(&example(), Some("scan"), TERMINAL, &plus_one());
        insta::assert_snapshot!(lines.stdout.join("\n"), @r"
        PIECE              VERSION               SIZE     FILES
        projects/scanner   2026-11-04 10:02:11   1.1 GB   48 211
        projects/scanner   2026-09-03 13:15:00   1.0 GB   47 980
        tools/scan-old     2025-02-11 19:40:07    40 MB      812
        ");
        assert!(lines.stderr.is_empty(), "{:?}", lines.stderr);
    }

    #[test]
    fn the_pipe_form_has_tab_separated_stamps_and_raw_numbers_without_a_header() {
        let lines = render(&example(), None, PIPE, &plus_one());
        assert_eq!(
            texts(&lines.stdout),
            [
                "projects/scanner\t2026-11-04T090211\t1100000000\t48211\tok",
                "projects/scanner\t2026-09-03T121500\t1040000000\t47980\tok",
                "tools/scan-old\t2025-02-11T184007\t40000000\t812\tok",
            ]
        );
    }

    fn marked() -> ListReport {
        report(vec![
            row("a", "2026-11-04T090211", State::Damaged, Some((10, 20, 3))),
            row("b", "2026-11-04T090211", State::Incomplete, None),
            row("c", "2026-11-04T090211", State::NewerFormat, None),
            sound("d", "2026-11-04T090211", 5, 1),
            row("e", "2026-11-04T090211", State::Unreadable, None),
        ])
    }

    #[test]
    fn marks_follow_the_files_column_and_missing_numbers_are_dashes() {
        let lines = render(&marked(), None, TERMINAL, &plus_one());
        insta::assert_snapshot!(lines.stdout.join("\n"), @r"
        PIECE   VERSION               SIZE   FILES
        a       2026-11-04 10:02:11   10 B       3   damaged
        b       2026-11-04T090211        -       -   incomplete
        c       2026-11-04 10:02:11      -       -   newer format
        d       2026-11-04 10:02:11    5 B       1
        e       2026-11-04 10:02:11      -       -   unreadable
        ");
    }

    #[test]
    fn marks_are_coloured_on_a_coloured_terminal() {
        let style = Style {
            color: true,
            ..TERMINAL
        };
        let lines = render(&marked(), None, style, &plus_one());
        let out = lines.stdout.join("\n");
        assert!(out.contains("\x1b[31mdamaged\x1b[0m"), "{out:?}");
        assert!(out.contains("\x1b[33mincomplete\x1b[0m"), "{out:?}");
        assert!(out.contains("\x1b[35mnewer format\x1b[0m"), "{out:?}");
    }

    #[test]
    fn the_pipe_form_names_the_state() {
        let lines = render(&marked(), None, PIPE, &plus_one());
        assert_eq!(
            texts(&lines.stdout),
            [
                "a\t2026-11-04T090211\t10\t3\tdamaged",
                "b\t2026-11-04T090211\t-\t-\tincomplete",
                "c\t2026-11-04T090211\t-\t-\tnewer-format",
                "d\t2026-11-04T090211\t5\t1\tok",
                "e\t2026-11-04T090211\t-\t-\tunreadable",
            ]
        );
    }

    #[test]
    fn control_characters_of_a_piece_are_escaped_in_both_forms() {
        let mut tabbed = sound("a", "2026-11-04T090211", 5, 1);
        tabbed.piece = RelPath::from_bytes(b"a\tb\nc".to_vec()).expect("a valid path");
        let versions = vec![tabbed];
        let table = render(&report(versions.clone()), None, TERMINAL, &plus_one());
        assert!(
            table.stdout[1].starts_with("a%09b%0Ac "),
            "{:?}",
            table.stdout
        );
        let pipe = render(&report(versions), None, PIPE, &plus_one());
        assert_eq!(pipe.stdout, ["a%09b%0Ac\t2026-11-04T090211\t5\t1\tok"]);
    }

    fn long_piece() -> String {
        format!("projects/{}", "x".repeat(60))
    }

    #[test]
    fn a_wide_terminal_pads_to_the_longest_piece_and_a_narrow_one_cuts_it() {
        let versions = vec![
            sound(&long_piece(), "2026-11-04T090211", 1_100_000_000, 48_211),
            sound("short", "2026-11-04T090211", 1_100_000_000, 48_211),
        ];
        let wide = render(
            &report(versions.clone()),
            None,
            Style {
                width: 100,
                ..TERMINAL
            },
            &plus_one(),
        );
        assert!(
            wide.stdout[1].starts_with(&long_piece()),
            "{:?}",
            wide.stdout
        );

        let narrow = render(
            &report(versions),
            None,
            Style {
                width: 99,
                ..TERMINAL
            },
            &plus_one(),
        );
        assert!(narrow.stdout[1].contains('…'), "{:?}", narrow.stdout);
        for line in &narrow.stdout {
            assert!(line.width() <= 99, "{line:?}");
        }
        assert!(
            narrow.stdout[1].starts_with("projects/"),
            "{:?}",
            narrow.stdout
        );
        assert!(narrow.stdout[1].contains("xxx"), "{:?}", narrow.stdout);
    }

    #[test]
    fn the_mark_column_takes_room_from_the_piece_in_a_narrow_layout() {
        let plain = vec![sound(&long_piece(), "2026-11-04T090211", 5, 1)];
        let mut damaged = plain.clone();
        damaged[0].state = State::Damaged;
        let style = Style {
            width: 70,
            ..TERMINAL
        };
        let without = render(&report(plain), None, style, &plus_one());
        let with = render(&report(damaged), None, style, &plus_one());
        let cut = |lines: &Lines| {
            lines.stdout[1]
                .split("   ")
                .next()
                .expect("a piece")
                .width()
        };
        assert!(
            cut(&with) < cut(&without),
            "{:?} {:?}",
            with.stdout,
            without.stdout
        );
        for line in without.stdout.iter().chain(&with.stdout) {
            assert!(line.width() <= 70, "{line:?}");
        }
    }

    #[test]
    fn a_very_narrow_terminal_keeps_eight_columns_of_the_piece() {
        let versions = vec![sound(&long_piece(), "2026-11-04T090211", 5, 1)];
        let style = Style {
            width: 20,
            ..TERMINAL
        };
        let lines = render(&report(versions), None, style, &plus_one());
        let piece = lines.stdout[1].split("   ").next().expect("a piece");
        assert_eq!(piece.width(), 8, "{:?}", lines.stdout);
    }

    #[test]
    fn the_utc_offset_is_added_to_versions_that_render_alike() {
        let versions = vec![
            sound("notes", "2026-04-04T143000", 5, 1),
            sound("notes", "2026-04-04T133000", 5, 1),
            sound("other", "2026-04-04T133000", 5, 1),
        ];
        let lines = render(&report(versions), None, TERMINAL, &auckland());
        assert_eq!(
            texts(&lines.stdout[1..]),
            [
                "notes   2026-04-05 02:30:00 +12:00    5 B       1",
                "notes   2026-04-05 02:30:00 +13:00    5 B       1",
                "other   2026-04-05 02:30:00           5 B       1",
            ]
        );
    }

    #[test]
    fn an_incomplete_version_shows_its_stamp_and_adds_no_offset_to_a_sound_one() {
        let versions = vec![
            row("notes", "2026-04-04T143000", State::Incomplete, None),
            sound("notes", "2026-04-04T133000", 5, 1),
        ];
        let lines = render(&report(versions), None, TERMINAL, &auckland());
        assert!(
            lines.stdout[1].contains("2026-04-04T143000"),
            "{:?}",
            lines.stdout
        );
        assert!(
            lines.stdout[2].contains("2026-04-05 02:30:00 "),
            "{:?}",
            lines.stdout
        );
        assert!(!lines.stdout[2].contains('+'), "{:?}", lines.stdout);
    }

    #[test]
    fn thousands_are_grouped_by_a_space() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1 000");
        assert_eq!(thousands(48_211), "48 211");
        assert_eq!(thousands(1_234_567), "1 234 567");
    }

    #[test]
    fn stderr_has_the_settings_notes_the_lock_the_empty_note_and_the_damage_in_that_order() {
        let mut damaged = row("a", "2026-11-04T090211", State::Damaged, Some((10, 20, 3)));
        damaged.damage = vec!["the listing is missing".to_string(), "other".to_string()];
        let mut report = report(vec![damaged]);
        report.warnings = vec![Warning::new("a setting")];
        report.note = Some("another run holds the lock".to_string());
        let lines = render(&report, None, TERMINAL, &plus_one());
        assert_eq!(
            texts(&lines.stderr),
            [
                "note: a setting",
                "note: another run holds the lock",
                "note: a 2026-11-04T090211 is damaged: the listing is missing",
                "note: a 2026-11-04T090211 is damaged: other",
            ]
        );
        let pipe = render(&report, None, PIPE, &plus_one());
        assert_eq!(pipe.stderr, lines.stderr);
    }

    #[test]
    fn nothing_to_list_is_said_on_stderr_in_both_forms() {
        let none = ListReport {
            store_has_versions: false,
            ..report(Vec::new())
        };
        for style in [TERMINAL, PIPE] {
            let lines = render(&none, Some("scan"), style, &plus_one());
            assert!(lines.stdout.is_empty(), "{:?}", lines.stdout);
            assert_eq!(
                texts(&lines.stderr),
                ["note: no versions in the collection"]
            );
        }
    }

    #[test]
    fn a_query_without_a_match_is_said_on_stderr_and_the_lock_note_comes_first() {
        let mut none = report(Vec::new());
        none.note = Some("held".to_string());
        let lines = render(&none, Some("zzz"), TERMINAL, &plus_one());
        assert!(lines.stdout.is_empty(), "{:?}", lines.stdout);
        assert_eq!(
            texts(&lines.stderr),
            ["note: held", "note: no piece matches `zzz`"]
        );
    }
}
