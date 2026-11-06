// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `grep` command: searches the paths of all archived files.

use crate::cli::output::{Output, Style};
use crate::cli::table::{Color, WIDE_FROM, mid, paint, sanitize, spaces};
use crate::cli::{Context, GrepArgs};
use antiq::collection::Collection;
use antiq::commands::grep::{FileHit, GrepReport, PieceHits, compile, grep_collection};
use antiq::report::Warning;
use antiq::versions::naming::{local_labels, system_zone};
use antiq::versions::stamp::Stamp;
use anyhow::Context as _;
use jiff::tz::TimeZone;
use regex::bytes::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use unicode_width::UnicodeWidthStr;

pub fn run(ctx: &Context, args: &GrepArgs, out: &mut Output) -> anyhow::Result<u8> {
    // The pattern is part of the command line, so a bad one is a usage
    // error also where no collection can be found.
    let pattern = compile(&args.pattern)
        .map_err(antiq::Error::from)
        .context("compile the pattern")?;
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let found = grep_collection(&collection, &pattern)
        .map_err(antiq::Error::from)
        .context("search the listings")?;
    print_report(
        out,
        ctx.json,
        &Printed {
            pattern: &args.pattern,
            regex: &pattern,
            found: &found,
            zone: &system_zone(),
        },
    )?;
    Ok(0)
}

/// What a run prints.
struct Printed<'a> {
    pattern: &'a str,
    regex: &'a Regex,
    found: &'a GrepReport,
    /// The zone that dates are shown in on a terminal.
    zone: &'a TimeZone,
}

/// Writes the result: JSON under `--json`, grouped hits on a terminal and
/// pipe lines otherwise, with the notes and warnings on stderr.
fn print_report(out: &mut Output, json: bool, printed: &Printed) -> anyhow::Result<()> {
    let found = printed.found;
    if json {
        out.json(Some("grep"), &json_report(printed.pattern, found))
            .context("print the result")?;
    } else {
        // Notes on the settings, then the lock, as `list` orders them.
        for warning in &found.warnings {
            out.note(&format!("note: {}", warning.message))
                .context("print the notes")?;
        }
        if let Some(note) = &found.note {
            out.note(&format!("note: {note}"))
                .context("print the notes")?;
        }
        for skipped in &found.skipped {
            out.note(&format!(
                "warning: cannot search version {} of {}: {}",
                skipped.stamp,
                sanitize(&skipped.piece.to_string()),
                skipped.reason
            ))
            .context("print the warnings")?;
        }
        let style = out.stdout_style();
        if style.terminal {
            for line in render_terminal(found, printed.regex, style, printed.zone) {
                out.line(&line).context("print the hits")?;
            }
        } else {
            for line in pipe_lines(found) {
                out.line_os(OsStr::from_bytes(&line))
                    .context("print the hits")?;
            }
        }
    }
    if found.pieces.is_empty() {
        out.note(&format!(
            "no archived path matches {}",
            sanitize(printed.pattern)
        ))
        .context("print the notes")?;
    }
    Ok(())
}

// =========================================================
// Pipe lines
// =========================================================

/// One line `piece<TAB>stamp<TAB>path` per hit and version, the piece and
/// the path as the raw bytes they are, and `-` for the path of a piece
/// whose name matches.
fn pipe_lines(found: &GrepReport) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    for hits in &found.pieces {
        let line = |stamp: String, path: &[u8]| {
            let mut line = hits.piece.as_bytes().to_vec();
            line.push(b'\t');
            line.extend_from_slice(stamp.as_bytes());
            line.push(b'\t');
            line.extend_from_slice(path);
            line
        };
        if hits.name_match {
            lines.extend(
                hits.versions
                    .iter()
                    .map(|version| line(version.stamp.to_string(), b"-")),
            );
        }
        for file in &hits.files {
            lines.extend(
                file.versions
                    .iter()
                    .map(|stamp| line(stamp.to_string(), &file.path)),
            );
        }
    }
    lines
}

// =========================================================
// Grouped output on a terminal
// =========================================================

/// The longest a column of paths may be on a narrow terminal, however
/// little room the versions text leaves.
const NARROW_LEFT_MIN: usize = 20;

/// The space between the two columns.
const GAP: usize = 3;

/// One line of the grouped output: a piece or an indented path on the left,
/// the versions that hold it on the right.
struct Row {
    /// The text of the left column without colour, for its width.
    plain: String,
    /// The left column as it is shown when it fits, highlight included.
    shown: String,
    right: String,
}

/// The hits grouped by piece, each path once with the versions that hold
/// it, and a line that counts them.
fn render_terminal(
    found: &GrepReport,
    regex: &Regex,
    style: Style,
    zone: &TimeZone,
) -> Vec<String> {
    let mut rows = Vec::new();
    for hits in &found.pieces {
        let labels = version_labels(hits, zone);
        rows.push(piece_row(hits, &labels, style));
        for file in &hits.files {
            rows.push(file_row(hits, file, &labels, regex, style));
        }
    }
    if rows.is_empty() {
        return Vec::new();
    }

    let widest = rows.iter().map(|row| row.plain.width()).max().unwrap_or(0);
    let left = if style.width >= WIDE_FROM {
        widest
    } else {
        let right = rows.iter().map(|row| row.right.width()).max().unwrap_or(0);
        widest.min(style.width.saturating_sub(right + GAP).max(NARROW_LEFT_MIN))
    };
    let mut lines: Vec<String> = rows
        .iter()
        .map(|row| {
            // A column that is cut loses its highlight: the match may be
            // what was cut away.
            let (text, width) = if row.plain.width() > left {
                let cut = mid(&row.plain, left);
                let width = cut.width();
                (cut, width)
            } else {
                (row.shown.clone(), row.plain.width())
            };
            format!(
                "{text}{}{}",
                spaces(left.saturating_sub(width) + GAP),
                row.right
            )
        })
        .collect();
    lines.push(String::new());
    lines.push(summary(found));
    lines
}

/// How each version of the piece is named: by its date, or by its time
/// where two versions of the piece share a date. The comparison covers
/// every version that has an archive, searched or not, because a version
/// argument is resolved against all of them.
fn version_labels(hits: &PieceHits, zone: &TimeZone) -> HashMap<Stamp, String> {
    let dates: Vec<String> = hits
        .all_stamps
        .iter()
        .map(|stamp| {
            stamp
                .time()
                .to_zoned(zone.clone())
                .strftime("%Y-%m-%d")
                .to_string()
        })
        .collect();
    let shared = dates
        .iter()
        .enumerate()
        .any(|(index, date)| dates[..index].contains(date));
    let labels = if shared {
        local_labels(&hits.all_stamps, zone)
    } else {
        dates
    };
    hits.all_stamps.iter().copied().zip(labels).collect()
}

fn piece_row(hits: &PieceHits, labels: &HashMap<Stamp, String>, style: Style) -> Row {
    let name = display(hits.piece.as_bytes()).text();
    let newest = hits
        .versions
        .first()
        .expect("a hit piece has a searched version");
    let count = hits.versions.len();
    let mut right = format!(
        "{count} {}, {}{}",
        if count == 1 { "version" } else { "versions" },
        if count == 1 { "" } else { "newest " },
        labels[&newest.stamp],
    );
    if hits.name_match {
        right = format!("piece name matches, {right}");
        if let Some(files) = newest.files {
            right = format!("{right}, {} files", grouped(files));
        }
    }
    Row {
        shown: paint(style, Color::Bold, &name),
        plain: name,
        right,
    }
}

fn file_row(
    hits: &PieceHits,
    file: &FileHit,
    labels: &HashMap<Stamp, String>,
    regex: &Regex,
    style: Style,
) -> Row {
    let right = if hits.versions.len() == 1 {
        labels[&file.versions[0]].clone()
    } else if file.versions.len() == hits.versions.len() {
        match file.versions.len() {
            2 => "both versions".to_string(),
            count => format!("all {count} versions"),
        }
    } else {
        file.versions
            .iter()
            .map(|stamp| labels[stamp].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    // The match is found in the text the pattern was run against, the piece
    // and the path; only the part in the path is shown.
    let mut haystack = hits.piece.as_bytes().to_vec();
    haystack.push(b'/');
    let prefix = haystack.len();
    haystack.extend_from_slice(&file.path);
    let spans: Vec<Range<usize>> = regex
        .find_iter(&haystack)
        .filter(|found| found.end() > prefix)
        .map(|found| found.start().saturating_sub(prefix)..found.end() - prefix)
        .collect();

    let shown = display(&file.path);
    Row {
        plain: format!("  {}", shown.text()),
        shown: format!("  {}", shown.highlighted(&spans, style)),
        right,
    }
}

/// The line under the groups: how much was found, and how to bring it
/// back.
fn summary(found: &GrepReport) -> String {
    let files: usize = found.pieces.iter().map(|hits| hits.files.len()).sum();
    let in_pieces = found.pieces.iter().filter(|hits| !hits.name_match).count();
    let names = found.pieces.len() - in_pieces;
    let mut counts = Vec::new();
    if files > 0 {
        counts.push(format!(
            "{files} {} in {in_pieces} {}",
            if files == 1 { "file" } else { "files" },
            if in_pieces == 1 { "piece" } else { "pieces" },
        ));
    }
    if names > 0 {
        counts.push(format!(
            "{names} piece {}",
            if names == 1 { "name" } else { "names" }
        ));
    }
    format!(
        "{} · antiq unearth <piece> [--version <version>]",
        counts.join(", ")
    )
}

/// `48211` as `48 211`.
fn grouped(number: u64) -> String {
    let digits = number.to_string();
    let mut text = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(' ');
        }
        text.push(digit);
    }
    text
}

/// A path as a terminal shows it: the escapes of a comment line of a
/// listing, any other control character as `%XX` of its bytes, and a byte
/// that is not UTF-8 as `%XX`. Each unit remembers the bytes of the path it
/// stands for, so a match can be marked in it.
struct Displayed {
    units: Vec<(String, Range<usize>)>,
}

fn display(bytes: &[u8]) -> Displayed {
    let mut units = Vec::new();
    let mut offset = 0;
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            let end = offset + character.len_utf8();
            let shown = match character {
                '\\' => "\\\\".to_string(),
                '\n' => "\\n".to_string(),
                '\t' => "\\t".to_string(),
                '\r' => "\\r".to_string(),
                other => sanitize(&other.to_string()),
            };
            units.push((shown, offset..end));
            offset = end;
        }
        for byte in chunk.invalid() {
            units.push((format!("%{byte:02X}"), offset..offset + 1));
            offset += 1;
        }
    }
    Displayed { units }
}

impl Displayed {
    fn text(&self) -> String {
        self.units.iter().map(|(shown, _)| shown.as_str()).collect()
    }

    /// The text with every unit that a span touches painted, a run of such
    /// units as one.
    fn highlighted(&self, spans: &[Range<usize>], style: Style) -> String {
        let touched = |unit: &Range<usize>| {
            spans
                .iter()
                .any(|span| span.start < unit.end && unit.start < span.end)
        };
        let mut text = String::new();
        let mut run = String::new();
        for (shown, bytes) in &self.units {
            if touched(bytes) {
                run.push_str(shown);
            } else {
                text.push_str(&paint(style, Color::Red, &std::mem::take(&mut run)));
                text.push_str(shown);
            }
        }
        text.push_str(&paint(style, Color::Red, &run));
        text
    }
}

// =========================================================
// JSON
// =========================================================

#[derive(Serialize)]
struct JsonReport<'a> {
    pattern: &'a str,
    pieces: Vec<JsonPiece>,
    skipped: Vec<JsonSkipped<'a>>,
    warnings: &'a [Warning],
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Serialize)]
struct JsonPiece {
    piece: String,
    name_match: bool,
    versions: Vec<JsonVersion>,
    files: Vec<JsonFile>,
}

#[derive(Serialize)]
struct JsonVersion {
    stamp: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<u64>,
}

#[derive(Serialize)]
struct JsonFile {
    path: String,
    versions: Vec<String>,
}

#[derive(Serialize)]
struct JsonSkipped<'a> {
    piece: String,
    stamp: String,
    reason: &'a str,
}

/// The report as the object of `--json`: pieces and paths in the escaped
/// form, versions by stamp and by their time.
fn json_report<'a>(pattern: &'a str, found: &'a GrepReport) -> JsonReport<'a> {
    let piece = |hits: &PieceHits| JsonPiece {
        piece: hits.piece.to_string(),
        name_match: hits.name_match,
        versions: hits
            .versions
            .iter()
            .map(|version| JsonVersion {
                stamp: version.stamp.to_string(),
                version: version.stamp.time().to_string(),
                files: version.files,
            })
            .collect(),
        files: hits
            .files
            .iter()
            .map(|file| JsonFile {
                path: antiq::relpath::escape(&file.path),
                versions: file.versions.iter().map(ToString::to_string).collect(),
            })
            .collect(),
    };
    JsonReport {
        pattern,
        pieces: found.pieces.iter().map(piece).collect(),
        skipped: found
            .skipped
            .iter()
            .map(|skipped| JsonSkipped {
                piece: skipped.piece.to_string(),
                stamp: skipped.stamp.to_string(),
                reason: &skipped.reason,
            })
            .collect(),
        warnings: &found.warnings,
        note: found.note.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::output::testing::Capture;
    use antiq::commands::grep::SearchedVersion;
    use antiq::relpath::RelPath;

    const NEW: &str = "2026-11-04T100211";
    const OLD: &str = "2026-09-03T121500";
    const ANCIENT: &str = "2025-02-11T194007";
    const RED: &str = "\x1b[31m";
    const END: &str = "\x1b[0m";

    fn stamp(text: &str) -> Stamp {
        text.parse().expect("a valid stamp")
    }

    /// A piece with a hit on files, every version searched.
    fn hits(piece: &str, versions: &[&str], files: &[(&str, &[&str])]) -> PieceHits {
        hits_of(piece, versions, files, false)
    }

    fn hits_of(
        piece: &str,
        versions: &[&str],
        files: &[(&str, &[&str])],
        name_match: bool,
    ) -> PieceHits {
        let stamps: Vec<Stamp> = versions.iter().map(|text| stamp(text)).collect();
        PieceHits {
            piece: RelPath::parse(piece).expect("a valid piece"),
            name_match,
            versions: stamps
                .iter()
                .map(|stamp| SearchedVersion {
                    stamp: *stamp,
                    files: name_match.then_some(48211),
                })
                .collect(),
            all_stamps: stamps,
            files: files
                .iter()
                .map(|(path, held)| FileHit {
                    path: path.as_bytes().to_vec(),
                    versions: held.iter().map(|text| stamp(text)).collect(),
                })
                .collect(),
        }
    }

    fn report(pieces: Vec<PieceHits>) -> GrepReport {
        GrepReport {
            pieces,
            ..GrepReport::default()
        }
    }

    fn style(width: usize, color: bool) -> Style {
        Style {
            width,
            color,
            terminal: true,
        }
    }

    fn render(found: &GrepReport, pattern: &str, style: Style) -> Vec<String> {
        let regex = compile(pattern).expect("the pattern compiles");
        render_terminal(found, &regex, style, &TimeZone::UTC)
    }

    fn scanner_and_scan_old() -> GrepReport {
        report(vec![
            hits(
                "projects/scanner",
                &[NEW, OLD],
                &[
                    ("src/main.rs", &[NEW, OLD]),
                    ("tools/bench/main.rs", &[OLD]),
                ],
            ),
            hits(
                "tools/scan-old",
                &[ANCIENT],
                &[("cli/src/main.rs", &[ANCIENT])],
            ),
        ])
    }

    // =========================================================
    // Layout
    // =========================================================

    #[test]
    fn hits_are_grouped_by_piece_with_the_versions_that_hold_each_path() {
        let lines = render(&scanner_and_scan_old(), r"main\.rs", style(120, false));

        assert_eq!(
            lines,
            [
                "projects/scanner        2 versions, newest 2026-11-04",
                "  src/main.rs           both versions",
                "  tools/bench/main.rs   2026-09-03",
                "tools/scan-old          1 version, 2025-02-11",
                "  cli/src/main.rs       2025-02-11",
                "",
                "3 files in 2 pieces · antiq unearth <piece> [--version <version>]",
            ]
        );
    }

    #[test]
    fn more_than_two_versions_are_all_n_versions_and_a_subset_lists_dates() {
        let found = report(vec![hits(
            "notes",
            &[NEW, OLD, ANCIENT],
            &[("a", &[NEW, OLD, ANCIENT]), ("b", &[NEW, ANCIENT])],
        )]);

        let lines = render(&found, "a|b", style(120, false));

        assert_eq!(lines[0], "notes   3 versions, newest 2026-11-04");
        assert_eq!(lines[1], "  a     all 3 versions");
        assert_eq!(lines[2], "  b     2026-11-04, 2025-02-11");
    }

    #[test]
    fn versions_that_share_a_date_with_any_version_of_the_piece_show_their_time() {
        // The 2026-09-03 version could not be searched, but `--version
        // 2026-09-03` would still be ambiguous.
        let mut piece = hits("notes", &[NEW], &[("a", &[NEW])]);
        piece.all_stamps = vec![stamp(NEW), stamp("2026-11-04T080000"), stamp(OLD)];

        let lines = render(&report(vec![piece]), "a", style(120, false));

        assert_eq!(lines[0], "notes   1 version, 2026-11-04 10:02:11");
        assert_eq!(lines[1], "  a     2026-11-04 10:02:11");
    }

    #[test]
    fn a_piece_name_match_is_one_row_with_the_counts() {
        let found = report(vec![hits_of("projects/scanner", &[NEW, OLD], &[], true)]);

        let lines = render(&found, "scanner", style(120, false));

        assert_eq!(
            lines,
            [
                "projects/scanner   piece name matches, 2 versions, newest 2026-11-04, 48 211 files",
                "",
                "1 piece name · antiq unearth <piece> [--version <version>]",
            ]
        );
    }

    #[test]
    fn the_summary_counts_files_and_pieces_and_names() {
        let found = report(vec![
            hits("a", &[NEW], &[("x", &[NEW])]),
            hits_of("b", &[NEW], &[], true),
            hits_of("c", &[NEW], &[], true),
        ]);

        let lines = render(&found, "x", style(120, false));

        assert_eq!(
            lines.last().expect("a summary"),
            "1 file in 1 piece, 2 piece names · antiq unearth <piece> [--version <version>]"
        );
    }

    #[test]
    fn nothing_found_renders_nothing() {
        assert!(render(&report(Vec::new()), "x", style(120, false)).is_empty());
    }

    // =========================================================
    // Names a terminal must not act on
    // =========================================================

    #[test]
    fn tab_backslash_newline_and_other_control_characters_are_shown_escaped() {
        let found = report(vec![hits(
            "notes",
            &[NEW],
            &[("a\tb\\c\nd\re\x1bf", &[NEW])],
        )]);

        let lines = render(&found, "a", style(120, false));

        assert_eq!(lines[1], "  a\\tb\\\\c\\nd\\re%1Bf   2026-11-04");
    }

    #[test]
    fn a_byte_that_is_not_utf8_is_shown_as_its_hex() {
        let mut piece = hits("notes", &[NEW], &[("x", &[NEW])]);
        piece.files[0].path = b"a\xffb".to_vec();

        let lines = render(&report(vec![piece]), "(?-u)\\xff", style(120, false));

        assert_eq!(lines[1], "  a%FFb   2026-11-04");
    }

    // =========================================================
    // Highlight
    // =========================================================

    #[test]
    fn the_match_in_the_path_is_painted_and_an_escape_before_it_shifts_nothing() {
        let found = report(vec![hits("notes", &[NEW], &[("a\tbXY", &[NEW])])]);

        let lines = render(&found, "XY", style(120, true));

        assert_eq!(lines[1], format!("  a\\tb{RED}XY{END}   2026-11-04"),);
    }

    #[test]
    fn a_match_that_straddles_the_slash_is_painted_in_the_path_part_only() {
        let found = report(vec![hits("notes", &[NEW], &[("xyz", &[NEW])])]);

        let straddling = render(&found, "es/xy", style(120, true));
        let in_the_piece = render(&found, "es/", style(120, true));

        assert_eq!(straddling[1], format!("  {RED}xy{END}z   2026-11-04"));
        assert_eq!(in_the_piece[1], "  xyz   2026-11-04");
    }

    #[test]
    fn a_match_on_an_escaped_character_paints_its_whole_escape() {
        let found = report(vec![hits("notes", &[NEW], &[("a\tb", &[NEW])])]);

        let lines = render(&found, r"\t", style(120, true));

        assert_eq!(lines[1], format!("  a{RED}\\t{END}b   2026-11-04"));
    }

    #[test]
    fn without_colour_nothing_is_painted() {
        let found = report(vec![hits("notes", &[NEW], &[("abc", &[NEW])])]);
        let lines = render(&found, "b", style(120, false));
        assert_eq!(lines[1], "  abc   2026-11-04");
    }

    // =========================================================
    // Width
    // =========================================================

    fn long_path() -> GrepReport {
        report(vec![hits(
            "notes",
            &[NEW, OLD],
            &[(
                "deeply/nested/directory/with/a/long/file-name.txt",
                &[NEW, OLD],
            )],
        )])
    }

    #[test]
    fn a_wide_terminal_never_cuts_a_path() {
        let lines = render(&long_path(), "file", style(WIDE_FROM, false));
        assert!(lines[1].starts_with("  deeply/nested/directory/with/a/long/file-name.txt"));
    }

    #[test]
    fn a_narrow_terminal_cuts_the_left_column_in_the_middle_and_paints_nothing() {
        let lines = render(&long_path(), "file", style(WIDE_FROM - 1, false));
        // 99 columns minus the longest right text (29) and the gap leave 67,
        // which holds this path; 45 columns cut it.
        assert!(lines[1].starts_with("  deeply/nested/directory/with/a/long/file-name.txt"));

        let narrow = render(&long_path(), "file", style(45, true));
        assert!(narrow[1].contains('…'), "{}", narrow[1]);
        assert!(!narrow[1].contains(RED), "{}", narrow[1]);
        assert!(narrow[1].ends_with("   both versions"), "{}", narrow[1]);
        let left = narrow[1].trim_end_matches("   both versions");
        assert_eq!(left.width(), 20);
    }

    // =========================================================
    // What is printed
    // =========================================================

    fn printed_by(json: bool, style: Style) -> (Capture, GrepReport) {
        let capture = Capture::default();
        let found = scanner_and_scan_old();
        let regex = compile(r"main\.rs").expect("compiles");
        let mut out = capture.output().with_stdout_style(style);
        print_report(
            &mut out,
            json,
            &Printed {
                pattern: r"main\.rs",
                regex: &regex,
                found: &found,
                zone: &TimeZone::UTC,
            },
        )
        .expect("the report prints");
        (capture, found)
    }

    #[test]
    fn a_terminal_gets_the_groups() {
        let (capture, _) = printed_by(false, style(120, false));
        assert!(
            capture.stdout().starts_with("projects/scanner "),
            "{}",
            capture.stdout()
        );
        assert!(capture.stdout().contains("3 files in 2 pieces"));
    }

    #[test]
    fn a_pipe_gets_the_lines() {
        let (capture, _) = printed_by(
            false,
            Style {
                terminal: false,
                ..style(80, false)
            },
        );
        assert!(
            capture
                .stdout()
                .starts_with("projects/scanner\t2026-11-04T100211\tsrc/main.rs\n"),
            "{}",
            capture.stdout()
        );
    }

    #[test]
    fn json_wins_over_the_terminal() {
        let (capture, _) = printed_by(true, style(120, false));
        assert!(
            capture
                .stdout()
                .starts_with("{\"format\":1,\"command\":\"grep\"")
        );
    }

    #[test]
    fn the_grouped_numbers_have_a_space_every_three_digits() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(812), "812");
        assert_eq!(grouped(1000), "1 000");
        assert_eq!(grouped(1234567), "1 234 567");
    }
}
