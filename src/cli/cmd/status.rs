// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `status` command.

use crate::cli::Context;
use crate::cli::console::block::{Block, Para};
use crate::cli::console::rich::Rich;
use crate::cli::notice::{self, Notice};
use crate::cli::output::{Output, Style};
use crate::cli::table::{
    Color, WIDE_FROM, decimal_size, mid, pad, pad_left, paint, sanitize, spaces, wrap,
};
use antiq::clock::{Clock, SystemClock, whole_seconds};
use antiq::collection::Collection;
use antiq::commands::status::{DoomedCause, StatusReport, VersionNote, status};
use antiq::commands::survey::{FutureTime, SurveyedPiece, Verdict};
use antiq::relpath::RelPath;
use antiq::report::lock_note;
use antiq::versions::naming::local_labels;
use antiq::versions::stamp::Stamp;
use anyhow::Context as _;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use unicode_width::UnicodeWidthStr;

pub fn run(ctx: &Context, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let now = SystemClock.now();
    let report = status(&collection, now)
        .map_err(antiq::Error::from)
        .context("read the state of the antiq collection")?;

    if ctx.json {
        out.json(Some("status"), &payload(&report, now))
            .context("print the result")?;
    } else {
        let zone = antiq::versions::naming::system_zone();
        let shown = render(&report, out.stdout_style(), &zone, now);
        // A terminal gets the findings as blocks, a pipe their lines.
        let rich = out.stdout_env().rich;
        let table = if rich {
            &shown.stdout[..shown.findings_at]
        } else {
            &shown.stdout[..]
        };
        for line in table {
            out.line(line).context("print the result")?;
        }
        out.result_blocks(&shown.findings)
            .context("print the result")?;
        if let Some(hint) = due_hint(&report) {
            out.hint_result(&hint).context("print the result")?;
        }
        out.notices(&shown.stderr).context("print the notes")?;
    }
    // Pieces that `archive` refuses are not a failure of `status`.
    Ok(0)
}

/// What a run prints on a console.
#[derive(Debug, Default, PartialEq, Eq)]
struct Rendered {
    /// The header, the table and, for a pipe, the findings.
    stdout: Vec<String>,
    /// Where the findings start in `stdout`.
    findings_at: usize,
    /// The findings as the blocks a terminal gets in their place.
    findings: Vec<Block>,
    stderr: Vec<Notice>,
}

// =========================================================
// The table
// =========================================================

const STATE_WIDTH: usize = 7;
const IDLE_WIDTH: usize = 4;
const GAP: usize = 2;
/// The narrow layout leaves at least this much for the path and for the
/// reason, however narrow the terminal is.
const MIN_PIECE: usize = 24;
const MIN_WHAT: usize = 24;

fn render(report: &StatusReport, style: Style, zone: &TimeZone, now: Timestamp) -> Rendered {
    let mut stdout = vec![header(report)];
    let rows = rows_of(report);
    if !rows.is_empty() {
        stdout.push(String::new());
        stdout.extend(table(&rows, style, now));
    }
    let findings = findings(report, zone);
    let findings_at = stdout.len();
    if !findings.is_empty() {
        stdout.push(String::new());
        stdout.extend(findings.iter().flat_map(|found| found.pipe.clone()));
    }

    let mut stderr = notice::warnings(&report.warnings);
    stderr.extend(report.store_error.iter().cloned().map(Notice::Note));
    stderr.extend(
        report
            .holder
            .iter()
            .map(|holder| Notice::Warning(lock_note(holder))),
    );
    Rendered {
        stdout,
        findings_at,
        findings: findings.into_iter().map(|found| found.block).collect(),
        stderr,
    }
}

/// The hint under the findings: what to run to see the pieces that are
/// due. Only a report with a due piece has one.
fn due_hint(report: &StatusReport) -> Option<Block> {
    report
        .pieces
        .iter()
        .any(|piece| matches!(piece.verdict, Verdict::Due))
        .then(|| {
            Block::hint(Rich::parse(
                "Run `antiq archive -n` to see what the next run would archive.",
            ))
        })
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn header(report: &StatusReport) -> String {
    let mut parts = vec![plural(report.folders, "folder", "folders")];
    if let Some(versions) = report.versions {
        let size = decimal_size(report.archive_bytes.unwrap_or(0));
        parts.push(format!(
            "{} ({size})",
            plural(versions, "version", "versions")
        ));
    }
    parts.push(format!(
        "archive after {} idle",
        plural(report.idle_days as usize, "day", "days")
    ));
    parts.join(" · ")
}

/// A piece that has a state, as the table and the JSON list it.
struct Row<'a> {
    piece: &'a SurveyedPiece,
    state: &'static str,
    color: Color,
    what: String,
}

/// The pieces that have a state; plain ones are only counted in the header.
fn rows_of(report: &StatusReport) -> Vec<Row<'_>> {
    report.pieces.iter().filter_map(row_of).collect()
}

fn row_of(piece: &SurveyedPiece) -> Option<Row<'_>> {
    let (state, color, what) = match &piece.verdict {
        Verdict::Plain => return None,
        Verdict::Due => ("due", Color::Yellow, "archives next run".to_string()),
        Verdict::Soon { days_left } => (
            "soon",
            Color::Cyan,
            format!("archives in {}", plural(*days_left as usize, "day", "days")),
        ),
        Verdict::Pinned => ("pinned", Color::Magenta, "kept by .antiq-pin".to_string()),
        Verdict::Refused(reasons) => {
            let joined = reasons
                .iter()
                .map(|reason| sanitize(&reason.to_string()))
                .collect::<Vec<_>>()
                .join("; ");
            ("refused", Color::Red, joined)
        }
    };
    Some(Row {
        piece,
        state,
        color,
        what,
    })
}

fn table(rows: &[Row<'_>], style: Style, now: Timestamp) -> Vec<String> {
    let names: Vec<String> = rows
        .iter()
        .map(|row| sanitize(&row.piece.piece.to_string()))
        .collect();
    let longest = names.iter().map(|name| name.width()).max().unwrap_or(0);
    let wide = style.width >= WIDE_FROM;
    let fixed = STATE_WIDTH + GAP + GAP + IDLE_WIDTH + GAP;
    let piece_width = if wide {
        longest.max("PIECE".len())
    } else {
        longest
            .min(MIN_PIECE.max(style.width.saturating_sub(fixed + MIN_WHAT)))
            .max("PIECE".len())
    };
    let what_width = if wide {
        usize::MAX
    } else {
        style
            .width
            .saturating_sub(fixed + piece_width)
            .max(MIN_WHAT)
    };
    let continuation = spaces(fixed + piece_width);

    let mut lines = vec![format!(
        "{}{}{}{}{}{}{}",
        pad("STATE", STATE_WIDTH),
        spaces(GAP),
        pad("PIECE", piece_width),
        spaces(GAP),
        pad_left("IDLE", IDLE_WIDTH),
        spaces(GAP),
        "WHAT"
    )];
    for (row, name) in rows.iter().zip(&names) {
        let shown = if wide {
            name.clone()
        } else {
            mid(name, piece_width)
        };
        let prefix = format!(
            "{}{}{}{}{}{}",
            paint(style, row.color, row.state),
            spaces(STATE_WIDTH - row.state.len() + GAP),
            pad(&shown, piece_width),
            spaces(GAP),
            pad_left(&idle_text(row.piece, now), IDLE_WIDTH),
            spaces(GAP),
        );
        let mut wrapped = if wide {
            vec![row.what.clone()]
        } else {
            wrap(&row.what, what_width)
        };
        lines.push(format!("{prefix}{}", wrapped.remove(0)));
        lines.extend(
            wrapped
                .into_iter()
                .map(|rest| format!("{continuation}{rest}")),
        );
    }
    lines
}

/// Whole days since the idle clock, `-` when there is none, and `0d` for a
/// clock in the future.
fn idle_text(row: &SurveyedPiece, now: Timestamp) -> String {
    match row.clock {
        Some(clock) => {
            let days = (now.as_second() - clock.as_second()).div_euclid(86_400);
            format!("{}d", days.max(0))
        }
        None => "-".to_string(),
    }
}

// =========================================================
// The notes below the table
// =========================================================

fn shown(path: &RelPath) -> String {
    sanitize(&path.to_string())
}

/// One finding below the table: the lines a pipe gets and the block a
/// terminal gets.
#[derive(Debug, PartialEq, Eq)]
struct Finding {
    pipe: Vec<String>,
    block: Block,
}

/// What a kind of finding means and what to do about it.
struct Meaning {
    text: Rich,
    fix: Option<Rich>,
}

impl Meaning {
    fn none() -> Meaning {
        Meaning {
            text: Rich::new(),
            fix: None,
        }
    }
}

/// One note: a single item stays on the line of the note, several are
/// indented below it.
fn note(count_of: (&str, &str), items: &[String]) -> Vec<String> {
    match items.len() {
        0 => Vec::new(),
        1 => vec![format!(
            "! {}: {}",
            plural(1, count_of.0, count_of.1),
            items[0]
        )],
        count => {
            let mut lines = vec![format!("! {}:", plural(count, count_of.0, count_of.1))];
            lines.extend(items.iter().map(|item| format!("    {item}")));
            lines
        }
    }
}

/// The finding for `items`, if there are any. The title and the items of
/// the block are the lines of the note without their marks.
fn finding(count_of: (&str, &str), items: Vec<String>, meaning: Meaning) -> Option<Finding> {
    let pipe = note(count_of, &items);
    let (head, rest) = pipe.split_first()?;
    let title = head.trim_start_matches("! ");
    let mut block = Block::warning(Rich::new().text(title));
    for item in rest {
        block = block.text(Para::pre(item.trim_start()));
    }
    if !meaning.text.is_empty() {
        block = block.text(meaning.text);
    }
    if let Some(fix) = meaning.fix {
        block = block.fix(fix);
    }
    Some(Finding { pipe, block })
}

/// A version as a note names it: the piece, the label of the version and
/// the reasons it is damaged, if any.
fn version_text(note: &VersionNote, label: &str) -> String {
    let base = format!("{} {label}", shown(&note.piece));
    if note.reasons.is_empty() {
        base
    } else {
        format!("{base} ({})", sanitize(&note.reasons.join("; ")))
    }
}

fn findings(report: &StatusReport, zone: &TimeZone) -> Vec<Finding> {
    // Versions of one piece that render alike get the UTC offset, so the
    // labels are made for the damaged and newer-format versions of a piece.
    // An incomplete version is named by its stamp only and takes no part.
    let mut stamps: BTreeMap<&RelPath, BTreeSet<Stamp>> = BTreeMap::new();
    for version in report.damaged.iter().chain(&report.newer_format) {
        stamps
            .entry(&version.piece)
            .or_default()
            .insert(version.stamp);
    }
    let mut labels: BTreeMap<(&RelPath, Stamp), String> = BTreeMap::new();
    for (piece, of_piece) in &stamps {
        let of_piece: Vec<Stamp> = of_piece.iter().copied().collect();
        for (stamp, label) in of_piece.iter().zip(local_labels(&of_piece, zone)) {
            labels.insert((piece, *stamp), label);
        }
    }
    let version = |note: &VersionNote| version_text(note, &labels[&(&note.piece, note.stamp)]);
    let incomplete = |note: &VersionNote| version_text(note, &note.stamp.to_string());

    let mut future: Vec<&FutureTime> = report
        .pieces
        .iter()
        .flat_map(|piece| &piece.future)
        .collect();
    future.sort_by(|a, b| a.path.cmp(&b.path));
    let future = future
        .into_iter()
        .map(|found| {
            let date = found.time.to_zoned(zone.clone()).strftime("%Y-%m-%d");
            format!("{} ({date})", shown(&found.path))
        })
        .collect();

    let doomed = report
        .doomed
        .iter()
        .map(|leftover| {
            let tree = leftover.path.to_string();
            let entry = leftover.first_entry.to_string();
            let inside = entry
                .strip_prefix(&format!("{tree}/"))
                .unwrap_or(&entry)
                .to_string();
            let cause = match leftover.cause {
                Some(DoomedCause::Locked) => " (locked)",
                Some(DoomedCause::DirectoryNotWritable) => " (its directory cannot be written)",
                None => "",
            };
            format!(
                "{}, first entry {}{cause}",
                shown(&leftover.path),
                sanitize(&inside)
            )
        })
        .collect();

    let one = |items: &[String]| items.len() == 1;
    let rebuild = |note: &VersionNote| {
        Rich::new()
            .cmd(&format!(
                "antiq rebuild {} {}",
                shown(&note.piece),
                note.stamp
            ))
            .text(" writes the missing file.")
    };
    let forget = |note: &VersionNote| {
        Rich::new()
            .cmd(&format!(
                "antiq forget {} {}",
                shown(&note.piece),
                note.stamp
            ))
            .text(" removes the leftovers, or copy the version back from the push target.")
    };
    let damaged: Vec<String> = report.damaged.iter().map(version).collect();
    let incomplete_items: Vec<String> = report.incomplete.iter().map(incomplete).collect();

    [
        finding(
            (
                "file with a time in the future",
                "files with a time in the future",
            ),
            future,
            Meaning {
                text: Rich::new().text("A file time in the future counts like any other, so a piece that holds such a file cannot become due before that date."),
                fix: Some(Rich::new().text("Correct the file time if it is a mistake.")),
            },
        ),
        finding(
            ("damaged version", "damaged versions"),
            damaged.clone(),
            Meaning {
                text: Rich::new().text("The archive is there, but its listing or checksum file is missing or does not match it, so the version cannot be listed, searched or checked."),
                fix: if one(&damaged) {
                    Some(rebuild(&report.damaged[0]))
                } else {
                    Some(Rich::parse("`antiq rebuild <piece> <version>` writes the missing file of one version."))
                },
            },
        ),
        finding(
            ("incomplete version", "incomplete versions"),
            incomplete_items.clone(),
            Meaning {
                text: Rich::new().text("Only the listing or checksum is left, the archive itself is missing, so the version cannot be restored."),
                fix: if one(&incomplete_items) {
                    Some(forget(&report.incomplete[0]))
                } else {
                    Some(Rich::parse("`antiq forget <piece> <version>` removes the leftovers of one version, or copy the version back from the push target."))
                },
            },
        ),
        finding(
            ("version in a newer format", "versions in a newer format"),
            report.newer_format.iter().map(version).collect(),
            Meaning {
                text: Rich::new().text("The listing of such a version was written in a format this version of antiq cannot read, so unearth, fork and rebuild refuse it."),
                fix: Some(Rich::new().text("Use a version of antiq that reads the newer format.")),
            },
        ),
        finding(
            ("folder in recovered/", "folders in recovered/"),
            report.recovered.iter().map(shown).collect(),
            Meaning {
                text: Rich::parse("A folder that antiq gave back after a failed removal is kept in `.antiq/recovered/`."),
                fix: Some(Rich::new().text("Compare it with the live folder and delete it by hand once you no longer need it.")),
            },
        ),
        finding(
            ("tree left in trash/", "trees left in trash/"),
            report.trash.iter().map(shown).collect(),
            Meaning {
                text: Rich::parse("A folder taken out for archiving could go neither back to its path nor into `.antiq/recovered/`."),
                fix: Some(Rich::new().text("Look at it, then move it back or delete it by hand.")),
            },
        ),
        finding(
            ("leftover in doomed/", "leftovers in doomed/"),
            doomed,
            Meaning {
                text: Rich::new().text("The removal of an archived folder did not finish. antiq tries again at the start of a later run."),
                fix: None,
            },
        ),
        finding(
            (
                "live folder spelled differently from the store",
                "live folders spelled differently from the store",
            ),
            report
                .spelling
                .iter()
                .map(|found| {
                    format!(
                        "{} (stored as {})",
                        shown(&found.live),
                        shown(&found.stored)
                    )
                })
                .collect(),
            Meaning {
                text: Rich::new().text("The volume treats both spellings as one name, so antiq archives into the stored spelling."),
                fix: None,
            },
        ),
        unreadable(&report.unreadable_directories),
        finding(
            (
                "file in a store folder that is not a version file",
                "files in a store folder that is not a version file",
            ),
            report.unparsable.iter().map(shown).collect(),
            Meaning::none(),
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The directories status could not read. A pipe gets one `! unreadable:`
/// line for each.
fn unreadable(directories: &[RelPath]) -> Option<Finding> {
    let mut found = finding(
        (
            "directory antiq could not read",
            "directories antiq could not read",
        ),
        directories.iter().map(shown).collect(),
        Meaning::none(),
    )?;
    found.pipe = directories
        .iter()
        .map(|directory| format!("! unreadable: {}", shown(directory)))
        .collect();
    Some(found)
}

// =========================================================
// The JSON object
// =========================================================

fn timestamp_text(time: Timestamp) -> String {
    whole_seconds(time).to_string()
}

fn piece_json(row: &Row<'_>, now: Timestamp) -> Value {
    let piece = row.piece;
    let idle_for_days = piece.clock.map(|clock| {
        (now.as_second() - clock.as_second())
            .div_euclid(86_400)
            .max(0)
    });
    let reasons: Vec<Value> = match &piece.verdict {
        Verdict::Refused(reasons) => reasons
            .iter()
            .map(|reason| {
                let mut found = json!({
                    "kind": reason.kind.as_str(),
                    "path": reason.path,
                });
                if let Some(line) = reason.line {
                    found["line"] = json!(line);
                }
                found["text"] = json!(reason.to_string());
                found
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut found = json!({
        "piece": piece.piece,
        "state": row.state,
        "idle_for_days": idle_for_days,
    });
    if let Verdict::Soon { days_left } = piece.verdict {
        found["due_in_days"] = json!(days_left);
    }
    found["reasons"] = Value::Array(reasons);
    found
}

fn versions_json(notes: &[VersionNote], with_reasons: bool) -> Vec<Value> {
    notes
        .iter()
        .map(|note| {
            let mut found = json!({"piece": note.piece, "version": note.stamp.to_string()});
            if with_reasons {
                found["reasons"] = json!(note.reasons);
            }
            found
        })
        .collect()
}

/// The JSON object of `status`, without the envelope. Every key is there
/// when its list is empty.
fn payload(report: &StatusReport, now: Timestamp) -> Value {
    let mut future: Vec<&FutureTime> = report
        .pieces
        .iter()
        .flat_map(|piece| &piece.future)
        .collect();
    future.sort_by(|a, b| a.path.cmp(&b.path));

    let mut found = json!({
        "folders": report.folders,
        "versions": report.versions,
        "archive_bytes": report.archive_bytes,
        "idle_days": report.idle_days,
        "soon_days": report.soon_days,
        "pieces": rows_of(report)
            .iter()
            .map(|row| piece_json(row, now))
            .collect::<Vec<_>>(),
        "future_times": future
            .into_iter()
            .map(|found| json!({"path": found.path, "time": timestamp_text(found.time)}))
            .collect::<Vec<_>>(),
        "damaged_versions": versions_json(&report.damaged, true),
        "incomplete_versions": versions_json(&report.incomplete, false),
        "newer_format_versions": versions_json(&report.newer_format, false),
        "recovered": report.recovered,
        "trash": report.trash,
        "doomed": report
            .doomed
            .iter()
            .map(|leftover| {
                let cause = leftover.cause.map(|cause| match cause {
                    DoomedCause::Locked => "locked",
                    DoomedCause::DirectoryNotWritable => "directory_not_writable",
                });
                json!({
                    "path": leftover.path,
                    "first_entry": leftover.first_entry,
                    "cause": cause,
                })
            })
            .collect::<Vec<_>>(),
        "spelling": report
            .spelling
            .iter()
            .map(|found| json!({"live": found.live, "stored": found.stored}))
            .collect::<Vec<_>>(),
        "unreadable_directories": report.unreadable_directories,
        "unparsable_files": report.unparsable,
        "warnings": report.warnings,
        "store_error": report.store_error,
    });
    if let Some(holder) = &report.holder {
        found["note"] = json!(lock_note(holder).message);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::commands::status::{
        Doomed, DoomedCause, SpellingDifference, StatusReport, VersionNote,
    };
    use antiq::commands::survey::{FutureTime, Refusal, RefusalKind, SurveyedPiece, Verdict};
    use antiq::lock::Holder;
    use antiq::relpath::RelPath;
    use antiq::report::Warning;
    use antiq::versions::stamp::Stamp;

    /// 2026-11-04T10:00:00Z.
    const NOW: i64 = 1_793_786_400;
    const DAY: i64 = 86_400;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn stamp(text: &str) -> Stamp {
        text.parse().expect("a valid stamp")
    }

    fn at(offset: i64) -> Timestamp {
        Timestamp::from_second(NOW + offset).expect("in range")
    }

    fn now() -> Timestamp {
        at(0)
    }

    fn wide() -> Style {
        Style {
            width: 120,
            color: false,
            terminal: false,
        }
    }

    fn narrow() -> Style {
        Style {
            width: 80,
            color: false,
            terminal: false,
        }
    }

    fn piece(name: &str, verdict: Verdict, idle_days: Option<i64>) -> SurveyedPiece {
        SurveyedPiece {
            piece: path(name),
            verdict,
            clock: idle_days.map(|days| at(-days * DAY)),
            future: Vec::new(),
        }
    }

    fn refusal(kind: RefusalKind, name: &str) -> Refusal {
        Refusal {
            kind,
            path: path(name),
            line: None,
        }
    }

    /// The state of the example in the spec.
    fn example() -> StatusReport {
        let mut plain = piece("zeta/plain", Verdict::Plain, Some(2));
        plain.future = Vec::new();
        StatusReport {
            folders: 13,
            versions: Some(214),
            archive_bytes: Some(38_200_000_000),
            idle_days: 30,
            soon_days: 3,
            pieces: vec![
                piece("experiments/rust-gpu", Verdict::Due, Some(33)),
                piece("old/client-site", Verdict::Due, Some(141)),
                piece(
                    "media/camera-import",
                    Verdict::Refused(vec![refusal(
                        RefusalKind::MountPoint,
                        "media/camera-import/card",
                    )]),
                    Some(52),
                ),
                piece("projects/antiq", Verdict::Pinned, Some(120)),
                piece("tools/scan-old", Verdict::Soon { days_left: 2 }, Some(28)),
                plain,
            ],
            ..StatusReport::default()
        }
    }

    fn lines(shown: &Rendered) -> Vec<&str> {
        shown.stdout.iter().map(String::as_str).collect()
    }

    #[test]
    fn the_example_of_the_spec_in_the_wide_layout() {
        let mut report = example();
        report.pieces.sort_by(|a, b| a.piece.cmp(&b.piece));
        report.pieces[2].future = vec![FutureTime {
            path: path("notes/plan.md"),
            time: "2027-01-12T12:00:00Z".parse().expect("a timestamp"),
        }];
        report.damaged = vec![VersionNote {
            piece: path("tools/scan-old"),
            stamp: stamp("2024-06-30T081245"),
            reasons: vec!["no listing".to_string()],
        }];

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert_eq!(
            lines(&shown),
            [
                "13 folders · 214 versions (38 GB) · archive after 30 days idle",
                "",
                "STATE    PIECE                 IDLE  WHAT",
                "due      experiments/rust-gpu   33d  archives next run",
                "refused  media/camera-import    52d  mount point: media/camera-import/card",
                "due      old/client-site       141d  archives next run",
                "pinned   projects/antiq        120d  kept by .antiq-pin",
                "soon     tools/scan-old         28d  archives in 2 days",
                "",
                "! 1 file with a time in the future: notes/plan.md (2027-01-12)",
                "! 1 damaged version: tools/scan-old 2024-06-30 08:12:45 (no listing)",
            ]
        );
        assert!(shown.stderr.is_empty());
    }

    // =========================================================
    // The table
    // =========================================================

    fn one(piece: SurveyedPiece) -> StatusReport {
        StatusReport {
            folders: 1,
            versions: Some(1),
            archive_bytes: Some(2_000),
            idle_days: 30,
            soon_days: 3,
            pieces: vec![piece],
            ..StatusReport::default()
        }
    }

    #[test]
    fn a_path_that_does_not_fit_is_cut_in_the_middle_in_the_narrow_layout_only() {
        let long = "projects/some-very-long-collection-name/with/deeply/nested/folder/structure";
        let report = one(piece(long, Verdict::Due, Some(33)));

        let narrow_shown = render(&report, narrow(), &TimeZone::UTC, now());
        let wide_shown = render(&report, wide(), &TimeZone::UTC, now());

        let cut = crate::cli::table::mid(long, 39);
        assert_eq!(
            lines(&narrow_shown)[3],
            format!("due      {cut}   33d  archives next run")
        );
        assert_eq!(
            lines(&wide_shown)[3],
            format!("due      {long}   33d  archives next run")
        );
    }

    #[test]
    fn several_reasons_are_joined_and_wrapped_under_the_what_column() {
        let reasons = vec![
            refusal(
                RefusalKind::UnreadableEntry,
                "media/a-rather-long-name/first",
            ),
            refusal(
                RefusalKind::NestedCollection,
                "media/a-rather-long-name/second",
            ),
        ];
        let report = one(piece("media", Verdict::Refused(reasons), Some(52)));

        let shown = render(&report, narrow(), &TimeZone::UTC, now());

        let rows = &lines(&shown)[3..];
        assert!(rows.len() >= 2, "{rows:?}");
        assert!(rows.iter().all(|row| row.chars().count() <= 80), "{rows:?}");
        let indent = " ".repeat(
            rows[0]
                .find("unreadable")
                .expect("the first reason starts a column"),
        );
        assert!(
            rows[1..].iter().all(|row| row.starts_with(&indent)),
            "{rows:?}"
        );
        let joined = rows
            .iter()
            .map(|row| row.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains(
                "unreadable entry: media/a-rather-long-name/first; nested collection: media/a-rather-long-name/second"
            ) || joined.contains("unreadable entry: media/a-rather-long-name/first;"),
            "{joined}"
        );
    }

    #[test]
    fn the_state_is_coloured_and_the_columns_still_line_up() {
        let colored = Style {
            color: true,
            ..wide()
        };
        let report = one(piece("a", Verdict::Due, Some(33)));

        let shown = render(&report, colored, &TimeZone::UTC, now());

        assert_eq!(
            lines(&shown)[3],
            "\x1b[33mdue\x1b[0m      a       33d  archives next run"
        );
    }

    #[test]
    fn each_state_has_its_own_colour() {
        let colored = Style {
            color: true,
            ..wide()
        };
        let mut report = one(piece("a", Verdict::Due, Some(33)));
        report.pieces = vec![
            piece("a", Verdict::Due, Some(33)),
            piece("b", Verdict::Soon { days_left: 1 }, Some(29)),
            piece("c", Verdict::Pinned, Some(1)),
            piece(
                "d",
                Verdict::Refused(vec![refusal(RefusalKind::Locked, "d/x")]),
                None,
            ),
        ];

        let shown = render(&report, colored, &TimeZone::UTC, now());

        let rows = &lines(&shown)[3..];
        assert!(rows[0].starts_with("\x1b[33mdue\x1b[0m"), "{}", rows[0]);
        assert!(rows[1].starts_with("\x1b[36msoon\x1b[0m"), "{}", rows[1]);
        assert!(rows[2].starts_with("\x1b[35mpinned\x1b[0m"), "{}", rows[2]);
        assert!(rows[3].starts_with("\x1b[31mrefused\x1b[0m"), "{}", rows[3]);
    }

    #[test]
    fn an_idle_time_is_whole_days_and_a_dash_when_unknown() {
        let mut report = one(piece("a", Verdict::Due, None));
        report.pieces = vec![
            piece("a", Verdict::Pinned, None),
            piece("b", Verdict::Pinned, Some(-3)),
            piece("c", Verdict::Soon { days_left: 1 }, Some(29)),
        ];

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert_eq!(
            &lines(&shown)[3..],
            [
                "pinned   a         -  kept by .antiq-pin",
                "pinned   b        0d  kept by .antiq-pin",
                "soon     c       29d  archives in 1 day",
            ]
        );
    }

    #[test]
    fn a_collection_without_a_state_prints_the_header_alone() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.folders = 1;
        report.versions = Some(1);

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert_eq!(
            lines(&shown),
            ["1 folder · 1 version (2.0 kB) · archive after 30 days idle"]
        );
    }

    #[test]
    fn a_store_that_could_not_be_read_leaves_the_versions_out_of_the_header() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.versions = None;
        report.archive_bytes = None;
        report.store_error = Some("cannot read the store: pieces/notes: denied".to_string());

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert_eq!(lines(&shown), ["1 folder · archive after 30 days idle"]);
        assert_eq!(
            crate::cli::notice::pipe_lines(&shown.stderr),
            ["note: cannot read the store: pieces/notes: denied"]
        );
    }

    #[test]
    fn a_warning_and_the_held_lock_are_a_warning_and_a_note_on_stderr() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.warnings = vec![Warning::new(
            "the key `x` under [collection] is unknown and ignored",
        )];
        report.holder = Some(Holder {
            pid: 4711,
            machine: "imperius".to_string(),
            started: "2026-11-04T09:00:00Z".parse().expect("a timestamp"),
        });

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert_eq!(
            crate::cli::notice::pipe_lines(&shown.stderr),
            [
                "warning: the key `x` under [collection] is unknown and ignored",
                "note: another run holds the lock (pid 4711, machine imperius, started 2026-11-04T09:00:00Z)",
            ]
        );
    }

    // =========================================================
    // The notes below the table
    // =========================================================

    #[test]
    fn one_item_of_a_note_is_inline_and_several_are_indented() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.damaged = vec![
            VersionNote {
                piece: path("tools/scan-old"),
                stamp: stamp("2024-06-30T081245"),
                reasons: vec!["no listing".to_string()],
            },
            VersionNote {
                piece: path("tools/scan-old"),
                stamp: stamp("2024-07-01T081245"),
                reasons: vec![
                    "no checksum file".to_string(),
                    "the archive has 1 bytes, its listing records 2".to_string(),
                ],
            },
        ];

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert_eq!(
            &lines(&shown)[1..],
            [
                "",
                "! 2 damaged versions:",
                "    tools/scan-old 2024-06-30 08:12:45 (no listing)",
                "    tools/scan-old 2024-07-01 08:12:45 (no checksum file; the archive has 1 bytes, its listing records 2)",
            ]
        );
    }

    /// The findings of `report` as a terminal 100 columns wide shows them.
    fn finding_lines(report: &StatusReport) -> Vec<String> {
        let shown = render(report, wide(), &TimeZone::UTC, now());
        crate::cli::console::render::render_blocks(
            &shown.findings,
            &crate::cli::console::env::Env::new(true, false, true, 100),
        )
    }

    #[test]
    fn each_finding_is_a_block_that_says_what_it_means_and_how_to_fix_it() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        let version = |piece: &str, text: &str, reasons: &[&str]| VersionNote {
            piece: path(piece),
            stamp: stamp(text),
            reasons: reasons.iter().map(ToString::to_string).collect(),
        };
        report.pieces[0].future = vec![FutureTime {
            path: path("experiments/wasm-fun/plan.md"),
            time: "2027-01-12T12:00:00Z".parse().expect("a timestamp"),
        }];
        report.damaged = vec![version(
            "tools/scan-old",
            "2026-11-05T130211",
            &["no listing"],
        )];
        report.incomplete = vec![version("experiments/synth-toy", "2026-11-05T120211", &[])];
        report.recovered = vec![path("recovered/20261103T081522/experiments")];

        assert_eq!(
            finding_lines(&report),
            [
                " ⚠ WARNING  1 file with a time in the future: experiments/wasm-fun/plan.md (2027-01-12)",
                "▌   A file time in the future counts like any other, so a piece that holds such a file cannot become",
                "▌   due before that date.",
                "▌ → Correct the file time if it is a mistake.",
                "",
                " ⚠ WARNING  1 damaged version: tools/scan-old 2026-11-05 13:02:11 (no listing)",
                "▌   The archive is there, but its listing or checksum file is missing or does not match it, so the",
                "▌   version cannot be listed, searched or checked.",
                "▌ → antiq rebuild tools/scan-old 2026-11-05T130211 writes the missing file.",
                "",
                " ⚠ WARNING  1 incomplete version: experiments/synth-toy 2026-11-05T120211",
                "▌   Only the listing or checksum is left, the archive itself is missing, so the version cannot be",
                "▌   restored.",
                "▌ → antiq forget experiments/synth-toy 2026-11-05T120211 removes the leftovers, or copy the version",
                "▌   back from the push target.",
                "",
                " ⚠ WARNING  1 folder in recovered/: recovered/20261103T081522/experiments",
                "▌   A folder that antiq gave back after a failed removal is kept in .antiq/recovered/.",
                "▌ → Compare it with the live folder and delete it by hand once you no longer need it.",
            ]
        );
    }

    #[test]
    fn a_due_piece_gets_a_hint_how_to_see_what_the_next_run_archives() {
        let report = one(piece("a", Verdict::Due, Some(33)));
        let hint = due_hint(&report).expect("a due piece has a hint");
        assert_eq!(
            crate::cli::console::render::render_blocks(
                &[hint],
                &crate::cli::console::env::Env::new(true, false, true, 100)
            ),
            [" → HINT  Run antiq archive -n to see what the next run would archive."]
        );
        assert_eq!(
            due_hint(&one(piece("a", Verdict::Soon { days_left: 2 }, Some(28)))),
            None
        );
        assert_eq!(due_hint(&one(piece("a", Verdict::Plain, Some(2)))), None);
    }

    #[test]
    fn several_items_of_one_kind_stand_under_the_title() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.trash = vec![path("trash/a/x"), path("trash/a/y")];
        assert_eq!(
            finding_lines(&report),
            [
                " ⚠ WARNING  2 trees left in trash/:",
                "▌   trash/a/x",
                "▌   trash/a/y",
                "▌   A folder taken out for archiving could go neither back to its path nor into .antiq/recovered/.",
                "▌ → Look at it, then move it back or delete it by hand.",
            ]
        );
    }

    #[test]
    fn every_kind_of_note_has_its_wording() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        let version = |piece: &str, text: &str| VersionNote {
            piece: path(piece),
            stamp: stamp(text),
            reasons: Vec::new(),
        };
        report.incomplete = vec![version("notes", "2026-11-04T090211")];
        report.newer_format = vec![version("notes", "2026-11-05T090211")];
        report.recovered = vec![path("recovered/2026-11-04T090211/projects")];
        report.trash = vec![path("trash/2026-11-04T090211/projects/scanner")];
        report.doomed = vec![
            Doomed {
                path: path("doomed/2026-11-04T090211/notes"),
                first_entry: path("doomed/2026-11-04T090211/notes/ro/f"),
                cause: Some(DoomedCause::DirectoryNotWritable),
            },
            Doomed {
                path: path("doomed/2026-11-04T090211/other"),
                first_entry: path("doomed/2026-11-04T090211/other/kept"),
                cause: Some(DoomedCause::Locked),
            },
        ];
        report.spelling = vec![SpellingDifference {
            live: path("projects"),
            stored: path("Projects"),
        }];
        report.unreadable_directories = vec![path("area")];
        report.unparsable = vec![path("pieces/notes/readme.txt")];

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert_eq!(
            &lines(&shown)[1..],
            [
                "",
                "! 1 incomplete version: notes 2026-11-04T090211",
                "! 1 version in a newer format: notes 2026-11-05 09:02:11",
                "! 1 folder in recovered/: recovered/2026-11-04T090211/projects",
                "! 1 tree left in trash/: trash/2026-11-04T090211/projects/scanner",
                "! 2 leftovers in doomed/:",
                "    doomed/2026-11-04T090211/notes, first entry ro/f (its directory cannot be written)",
                "    doomed/2026-11-04T090211/other, first entry kept (locked)",
                "! 1 live folder spelled differently from the store: projects (stored as Projects)",
                "! unreadable: area",
                "! 1 file in a store folder that is not a version file: pieces/notes/readme.txt",
            ]
        );
    }

    #[test]
    fn versions_that_render_alike_in_local_time_carry_the_utc_offset() {
        let berlin = TimeZone::get("Europe/Berlin").expect("the time zone database knows Berlin");
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.newer_format = ["2026-10-25T003000", "2026-10-25T013000"]
            .iter()
            .map(|text| VersionNote {
                piece: path("notes"),
                stamp: stamp(text),
                reasons: Vec::new(),
            })
            .collect();

        let shown = render(&report, wide(), &berlin, now());

        assert_eq!(
            &lines(&shown)[2..],
            [
                "! 2 versions in a newer format:",
                "    notes 2026-10-25 02:30:00 +02:00",
                "    notes 2026-10-25 02:30:00 +01:00",
            ]
        );
    }

    #[test]
    fn an_incomplete_version_is_named_by_its_stamp_and_does_not_tell_others_apart() {
        let berlin = TimeZone::get("Europe/Berlin").expect("the time zone database knows Berlin");
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        let note = |text: &str| VersionNote {
            piece: path("notes"),
            stamp: stamp(text),
            reasons: vec!["no listing".to_string()],
        };
        report.damaged = vec![note("2026-10-25T003000")];
        report.incomplete = vec![VersionNote {
            reasons: Vec::new(),
            ..note("2026-10-25T013000")
        }];

        let shown = render(&report, wide(), &berlin, now());

        assert_eq!(
            &lines(&shown)[2..],
            [
                "! 1 damaged version: notes 2026-10-25 02:30:00 (no listing)",
                "! 1 incomplete version: notes 2026-10-25T013000",
            ]
        );
    }

    #[test]
    fn names_with_control_characters_are_shown_escaped() {
        let report = one(piece("a\u{1b}[31mb", Verdict::Due, Some(1)));

        let shown = render(&report, wide(), &TimeZone::UTC, now());

        assert!(
            lines(&shown)[3].contains("a%1B[31mb"),
            "{}",
            lines(&shown)[3]
        );
        assert!(shown.stdout.iter().all(|line| !line.contains('\u{1b}')));
    }

    #[test]
    fn several_future_files_are_listed_with_their_local_dates() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.pieces[0].future = vec![
            FutureTime {
                path: path("a/.antiq-restored"),
                time: "2027-01-12T23:30:00Z".parse().expect("a timestamp"),
            },
            FutureTime {
                path: path("a/later.txt"),
                time: "2027-02-01T00:00:00Z".parse().expect("a timestamp"),
            },
        ];
        let tokyo = TimeZone::get("Asia/Tokyo").expect("the time zone database knows Tokyo");

        let shown = render(&report, wide(), &tokyo, now());

        assert_eq!(
            &lines(&shown)[2..],
            [
                "! 2 files with a time in the future:",
                "    a/.antiq-restored (2027-01-13)",
                "    a/later.txt (2027-02-01)",
            ]
        );
    }

    // =========================================================
    // The JSON object
    // =========================================================

    #[test]
    fn the_json_object_has_every_key_with_the_documented_shape() {
        let mut report = example();
        report.pieces = vec![
            piece(
                "media/camera-import",
                Verdict::Refused(vec![
                    refusal(RefusalKind::MountPoint, "media/camera-import/card"),
                    Refusal {
                        kind: RefusalKind::RuleFile,
                        path: path("media/camera-import/.antiqignore"),
                        line: Some(2),
                    },
                ]),
                None,
            ),
            piece("old/client-site", Verdict::Due, Some(141)),
            piece("projects/antiq", Verdict::Pinned, Some(120)),
            piece("tools/scan-old", Verdict::Soon { days_left: 2 }, Some(28)),
            piece("zeta/plain", Verdict::Plain, Some(2)),
        ];
        report.pieces[4].future = vec![FutureTime {
            path: path("zeta/plain/plan.md"),
            time: "2027-01-12T12:00:00Z".parse().expect("a timestamp"),
        }];
        report.damaged = vec![VersionNote {
            piece: path("tools/scan-old"),
            stamp: stamp("2024-06-30T081245"),
            reasons: vec!["no listing".to_string()],
        }];
        report.incomplete = vec![VersionNote {
            piece: path("notes"),
            stamp: stamp("2026-11-04T090211"),
            reasons: Vec::new(),
        }];
        report.doomed = vec![
            Doomed {
                path: path("doomed/2026-11-04T090211/notes"),
                first_entry: path("doomed/2026-11-04T090211/notes/f"),
                cause: Some(DoomedCause::Locked),
            },
            Doomed {
                path: path("doomed/2026-11-04T090211/ro"),
                first_entry: path("doomed/2026-11-04T090211/ro/d/f"),
                cause: Some(DoomedCause::DirectoryNotWritable),
            },
        ];
        report.spelling = vec![SpellingDifference {
            live: path("projects"),
            stored: path("Projects"),
        }];
        report.holder = Some(Holder {
            pid: 4711,
            machine: "imperius".to_string(),
            started: "2026-11-04T09:00:00Z".parse().expect("a timestamp"),
        });

        let found = payload(&report, now());

        assert_eq!(
            found,
            json!({
                "folders": 13,
                "versions": 214,
                "archive_bytes": 38_200_000_000u64,
                "idle_days": 30,
                "soon_days": 3,
                "pieces": [
                    {"piece": "media/camera-import", "state": "refused", "idle_for_days": null,
                     "reasons": [
                        {"kind": "mount_point", "path": "media/camera-import/card",
                         "text": "mount point: media/camera-import/card"},
                        {"kind": "rule_file", "path": "media/camera-import/.antiqignore", "line": 2,
                         "text": "rule file: media/camera-import/.antiqignore:2"}]},
                    {"piece": "old/client-site", "state": "due", "idle_for_days": 141, "reasons": []},
                    {"piece": "projects/antiq", "state": "pinned", "idle_for_days": 120, "reasons": []},
                    {"piece": "tools/scan-old", "state": "soon", "idle_for_days": 28,
                     "due_in_days": 2, "reasons": []},
                ],
                "future_times": [{"path": "zeta/plain/plan.md", "time": "2027-01-12T12:00:00Z"}],
                "damaged_versions": [{"piece": "tools/scan-old", "version": "2024-06-30T081245",
                                      "reasons": ["no listing"]}],
                "incomplete_versions": [{"piece": "notes", "version": "2026-11-04T090211"}],
                "newer_format_versions": [],
                "recovered": [],
                "trash": [],
                "doomed": [{"path": "doomed/2026-11-04T090211/notes",
                            "first_entry": "doomed/2026-11-04T090211/notes/f", "cause": "locked"},
                           {"path": "doomed/2026-11-04T090211/ro",
                            "first_entry": "doomed/2026-11-04T090211/ro/d/f",
                            "cause": "directory_not_writable"}],
                "spelling": [{"live": "projects", "stored": "Projects"}],
                "unreadable_directories": [],
                "unparsable_files": [],
                "warnings": [],
                "store_error": null,
                "note": "another run holds the lock (pid 4711, machine imperius, started 2026-11-04T09:00:00Z)",
            })
        );
    }

    #[test]
    fn json_leaves_the_versions_null_without_a_store_and_has_no_note_without_a_holder() {
        let mut report = one(piece("a", Verdict::Plain, Some(2)));
        report.versions = None;
        report.archive_bytes = None;
        report.store_error = Some("cannot read the store: pieces/notes: denied".to_string());
        report.warnings = vec![Warning::new("a warning")];

        let found = payload(&report, now());

        assert_eq!(found["versions"], Value::Null);
        assert_eq!(found["archive_bytes"], Value::Null);
        assert_eq!(
            found["store_error"],
            "cannot read the store: pieces/notes: denied"
        );
        assert_eq!(found["warnings"], json!([{"message": "a warning"}]));
        assert!(found.get("note").is_none());
        assert_eq!(found["pieces"], json!([]));
    }
}
