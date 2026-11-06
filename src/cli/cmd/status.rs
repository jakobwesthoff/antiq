// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `status` command.
//!
//! The report comes from the library. This file lays it out for a console:
//! a table of what needs the user or changes at the next run, wide where the
//! terminal has room and narrow at 80 columns. Every word the user reads
//! comes from [`Kind`], so changing one is one edit.

use crate::cli::Context;
use crate::cli::output::{Output, Style};
use antiq::clock::{Clock, SystemClock};
use antiq::collection::Collection;
use antiq::ops::status::{Flag, Piece, PieceKind, StatusReport, status};
use anyhow::Context as _;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use std::path::PathBuf;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn run(ctx: &Context, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let clock = SystemClock;
    let report = status(&collection, &clock)
        .map_err(antiq::Error::from)
        .context("read the state of the pieces")?;

    // Under `--json` the warnings are part of the object and nothing goes
    // to stderr.
    if ctx.json {
        out.json(Some("status"), &report)
            .context("print the result")?;
        return Ok(0);
    }
    let lines = render(&report, clock.now(), out.stdout_style());
    for line in &lines.stdout {
        out.line(line).context("print the result")?;
    }
    for line in &lines.stderr {
        out.note(line).context("print the notes")?;
    }
    Ok(0)
}

/// What a run prints on a console.
#[derive(Debug, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

/// From this width on, tables use their wide layout.
pub(super) const WIDE_FROM: usize = 100;

// =========================================================
// The words and colours of the console
// =========================================================

/// The kinds of rows, in the order they are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Error,
    Decide,
    Diverged,
    Superseded,
    Stale,
    Missing,
    Recovered,
    Unreadable,
    Mount,
    Nested,
    Excluded,
    Future,
    Gone,
    Due,
    Upload,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Color {
    Plain,
    Dim,
    Bold,
    Red,
    Green,
    Yellow,
    Magenta,
    Cyan,
}

/// What the words of a row depend on besides its kind.
struct Words<'a> {
    piece: Option<&'a Piece>,
    /// A message for the kinds that carry one.
    message: &'a str,
    now: Timestamp,
    wide: bool,
}

impl Kind {
    /// The word in the STATE column.
    fn state(self) -> &'static str {
        match self {
            Kind::Error => "error",
            Kind::Decide => "decide",
            Kind::Diverged => "diverged",
            Kind::Superseded => "superseded",
            Kind::Stale => "stale",
            Kind::Missing => "missing",
            Kind::Recovered => "recovered",
            Kind::Unreadable => "unreadable",
            Kind::Mount | Kind::Nested => "mount",
            Kind::Excluded => "excluded",
            Kind::Future => "future",
            Kind::Gone => "gone",
            Kind::Due => "due",
            Kind::Upload => "upload",
            Kind::New => "new",
        }
    }

    fn color(self) -> Color {
        match self {
            Kind::Error => Color::Red,
            Kind::Due => Color::Magenta,
            Kind::Upload => Color::Cyan,
            Kind::New => Color::Plain,
            _ => Color::Yellow,
        }
    }

    /// The colour of the WHAT text.
    fn what_color(self) -> Color {
        match self {
            Kind::New => Color::Dim,
            other => other.color(),
        }
    }

    /// The command that deals with the row, shown on wide terminals.
    fn fix(self, path: &str) -> Option<String> {
        match self {
            Kind::Decide | Kind::Diverged | Kind::Superseded => {
                Some(format!("antiq resolve {}", shell_word(path)))
            }
            Kind::Stale | Kind::Missing | Kind::Upload => Some("antiq sync".to_string()),
            Kind::Due => Some("antiq archive".to_string()),
            _ => None,
        }
    }

    /// The text of the WHAT column.
    fn what(self, words: &Words<'_>) -> String {
        let wide = words.wide;
        let piece = words.piece;
        match self {
            Kind::Error => sanitize(words.message),
            Kind::Decide => piece
                .map(decision_words)
                .unwrap_or_else(|| "decision pending".to_string()),
            Kind::Diverged => "folder changed since unearthing".to_string(),
            Kind::Superseded if wide => "superseded archive kept".to_string(),
            Kind::Superseded => "old archive kept".to_string(),
            Kind::Stale => {
                let since = piece
                    .and_then(|piece| piece.detail.stale_since)
                    .map(|since| date(since, words.now, wide))
                    .unwrap_or_default();
                if wide {
                    format!("backup failed since {since}")
                } else {
                    format!("backup failing since {since}")
                }
            }
            Kind::Missing if wide => "archive is gone from the remote".to_string(),
            Kind::Missing => "gone from the remote".to_string(),
            Kind::Recovered => "kept in .antiq/recovered".to_string(),
            Kind::Unreadable => {
                let first = piece
                    .and_then(|piece| piece.detail.unreadable_first.as_ref())
                    .map(|first| sanitize(&first.to_string()))
                    .unwrap_or_default();
                format!("cannot read {first}")
            }
            Kind::Mount if wide => {
                let at = piece
                    .and_then(|piece| piece.detail.mount_points.first())
                    .map(|at| sanitize(&at.to_string()))
                    .unwrap_or_default();
                format!("holds a mount point at {at}")
            }
            Kind::Mount => "holds a mount point".to_string(),
            Kind::Nested => "holds another collection".to_string(),
            Kind::Excluded if wide => {
                let at = piece
                    .and_then(|piece| piece.detail.excluded_paths.first())
                    .map(|at| sanitize(&at.to_string()))
                    .unwrap_or_default();
                format!("holds the excluded path {at}")
            }
            Kind::Excluded => "holds an excluded path".to_string(),
            Kind::Future if wide => {
                let count = piece.map_or(0, |piece| piece.detail.future_files.len());
                format!("{count} files with times in the future")
            }
            Kind::Future => "file times in the future".to_string(),
            Kind::Gone => "folder is missing".to_string(),
            Kind::Due if wide => "archives at the next run".to_string(),
            Kind::Due => "archives next run".to_string(),
            Kind::Upload if wide => "archive waits for sync".to_string(),
            Kind::Upload => "waits for sync".to_string(),
            Kind::New if wide => "gets an ID at the next sync".to_string(),
            Kind::New => "ID at next sync".to_string(),
        }
    }
}

/// The answers of the pending decisions, as `replace or fork`.
fn decision_words(piece: &Piece) -> String {
    use antiq::tracking::DecisionKind;
    let words: Vec<&str> = piece
        .detail
        .decisions
        .iter()
        .map(|decision| match decision.kind {
            DecisionKind::VersionConflict => "mine, theirs or fork",
            DecisionKind::ReplacingWithLoss => "replace or fork",
            DecisionKind::DivergedFolder => "theirs, mine or fork",
            DecisionKind::SupersededArchive => "drop superseded",
        })
        .collect();
    if words.is_empty() {
        "decision pending".to_string()
    } else {
        words.join(", ")
    }
}

/// The kinds of row a piece gets, most important first. The remote's
/// upload only counts when a remote is attached.
fn kinds_of(piece: &Piece, remote_attached: bool) -> Vec<Kind> {
    let has = |flag: Flag| piece.flags.contains(&flag);
    let mut kinds = Vec::new();
    if piece.state == PieceKind::Error || has(Flag::Error) {
        kinds.push(Kind::Error);
    }
    if has(Flag::NeedsDecision) {
        kinds.push(Kind::Decide);
    } else if has(Flag::Diverged) {
        kinds.push(Kind::Diverged);
    }
    if has(Flag::Superseded) {
        kinds.push(Kind::Superseded);
    }
    if has(Flag::StaleBackup) {
        kinds.push(Kind::Stale);
    }
    if has(Flag::MissingOnRemote) {
        kinds.push(Kind::Missing);
    }
    if has(Flag::Unreadable) {
        kinds.push(Kind::Unreadable);
    }
    if has(Flag::MountPoint) {
        kinds.push(Kind::Mount);
    }
    if has(Flag::NestedCollection) {
        kinds.push(Kind::Nested);
    }
    if has(Flag::Excluded) {
        kinds.push(Kind::Excluded);
    }
    if has(Flag::FutureMtime) {
        kinds.push(Kind::Future);
    }
    if piece.state == PieceKind::Gone {
        kinds.push(Kind::Gone);
    }
    if has(Flag::Idle) {
        kinds.push(Kind::Due);
    }
    if has(Flag::PendingUpload) && remote_attached {
        kinds.push(Kind::Upload);
    }
    // A new piece that is due gets its ID when it is archived. Pinned and
    // empty pieces are counted, not listed.
    if piece.state == PieceKind::New && !has(Flag::Idle) && !has(Flag::Pinned) && !has(Flag::Empty)
    {
        kinds.push(Kind::New);
    }
    kinds
}

// =========================================================
// Rows
// =========================================================

struct Row {
    kind: Kind,
    path: String,
    age: Option<i64>,
    /// The kinds the WHAT text is made of.
    texts: Vec<(Kind, String)>,
    fix: Option<String>,
}

impl Row {
    fn what(&self) -> String {
        let texts: Vec<&str> = self.texts.iter().map(|(_, text)| text.as_str()).collect();
        texts.join(", ")
    }
}

fn rows(report: &StatusReport, now: Timestamp, wide: bool) -> Vec<Row> {
    let remote_attached = report.remote.url.is_some();
    let mut rows = Vec::new();
    for piece in &report.pieces {
        if piece.detail.below_unreadable.is_some() {
            continue;
        }
        let kinds = kinds_of(piece, remote_attached);
        let Some(&first) = kinds.first() else {
            continue;
        };
        let path = sanitize(&piece.path.to_string());
        let texts = kinds
            .iter()
            .map(|kind| {
                let message = piece.detail.error.as_deref().unwrap_or_default();
                let words = Words {
                    piece: Some(piece),
                    message,
                    now,
                    wide,
                };
                (*kind, kind.what(&words))
            })
            .collect();
        rows.push(Row {
            kind: first,
            fix: first.fix(&path),
            path,
            age: piece.age_days,
            texts,
        });
    }
    for name in &report.recovered {
        let path = format!(".antiq/recovered/{}", sanitize(name));
        let words = Words {
            piece: None,
            message: "",
            now,
            wide,
        };
        rows.push(Row {
            kind: Kind::Recovered,
            fix: None,
            texts: vec![(Kind::Recovered, Kind::Recovered.what(&words))],
            path,
            age: None,
        });
    }
    // A damaged tracking file shows as the error piece of its entry; an
    // object that cannot be read has no piece of its own.
    for damaged in &report.damaged {
        let shown = report
            .pieces
            .iter()
            .any(|piece| piece.id == Some(damaged.id) && piece.state == PieceKind::Error);
        if !shown {
            let words = Words {
                piece: None,
                message: &damaged.error,
                now,
                wide,
            };
            rows.push(Row {
                kind: Kind::Error,
                fix: None,
                texts: vec![(Kind::Error, Kind::Error.what(&words))],
                path: sanitize(&damaged.path),
                age: None,
            });
        }
    }
    // Due rows by age, the others by path; the sort is stable, so equal
    // rows keep the order of the report.
    rows.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| match a.kind {
                Kind::Due => a.age.cmp(&b.age),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.path.cmp(&b.path))
    });
    rows
}

// =========================================================
// Rendering
// =========================================================

fn render(report: &StatusReport, now: Timestamp, style: Style) -> Lines {
    let wide = style.width >= WIDE_FROM;
    let mut stdout = vec![header(report, now, style, wide), String::new()];
    let rows = rows(report, now, wide);
    if rows.is_empty() {
        stdout.push(format!(
            "{}{}",
            paint(style, Color::Green, "nothing to do"),
            paint(
                style,
                Color::Dim,
                ": no decisions, backups current, nothing due"
            )
        ));
        if let Some(next) = next_change(report, style.width) {
            stdout.push(paint(style, Color::Dim, &next));
        }
        if let Some(counts) = count_line(report) {
            stdout.push(paint(style, Color::Dim, &counts));
        }
    } else {
        stdout.extend(table(&rows, style, wide));
        stdout.push(String::new());
        if let Some(counts) = count_line(report) {
            stdout.push(paint(style, Color::Dim, &counts));
        }
        let decisions = rows.iter().any(|row| row.kind == Kind::Decide);
        let hint = if wide {
            "everything else is up to date · every piece: antiq find"
        } else if decisions {
            "answer decisions: antiq resolve · every piece: antiq find"
        } else {
            "every piece: antiq find"
        };
        stdout.push(paint(style, Color::Dim, hint));
    }
    let stderr = report
        .warnings
        .iter()
        .map(|warning| format!("note: {}", sanitize(&warning.message)))
        .collect();
    Lines { stdout, stderr }
}

pub(super) fn paint(style: Style, color: Color, text: &str) -> String {
    let code = match color {
        Color::Plain => return text.to_string(),
        Color::Dim => "\x1b[2m",
        Color::Bold => "\x1b[1m",
        Color::Red => "\x1b[31m",
        Color::Green => "\x1b[32m",
        Color::Yellow => "\x1b[33m",
        Color::Magenta => "\x1b[35m",
        Color::Cyan => "\x1b[36m",
    };
    if style.color && !text.is_empty() {
        format!("{code}{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The line above the table: the remote, the last sync and the counts.
fn header(report: &StatusReport, now: Timestamp, style: Style, wide: bool) -> String {
    let counts = &report.counts;
    let number = |value: usize| {
        paint(
            style,
            if wide { Color::Bold } else { Color::Plain },
            &value.to_string(),
        )
    };
    let dim = |text: &str| paint(style, Color::Dim, text);
    let mut line = String::new();
    match &report.remote.url {
        Some(url) => {
            let host = mid(&remote_name(url), if wide { 40 } else { 24 });
            let state = match report.remote.state {
                antiq::remote::RemoteState::Reachable => paint(style, Color::Green, "reachable"),
                antiq::remote::RemoteState::Away => paint(style, Color::Yellow, "away"),
                antiq::remote::RemoteState::Broken => paint(style, Color::Red, "broken"),
                antiq::remote::RemoteState::None => paint(style, Color::Dim, "unknown"),
            };
            let synced = match report.last_sync {
                Some(time) => format!("synced {}", ago(now, time)),
                None => "never synced".to_string(),
            };
            if wide {
                line.push_str(&dim(&format!("remote {host}:")));
            } else {
                line.push_str(&dim(&host));
            }
            line.push(' ');
            line.push_str(&state);
            line.push_str(&dim(&format!(", {synced} · ")));
        }
        None => line.push_str(&dim("no remote · ")),
    }
    line.push_str(&number(counts.live));
    if wide {
        line.push_str(&dim(" live, "));
    } else {
        line.push_str(&dim(" live · "));
    }
    line.push_str(&number(counts.archived));
    let remote_only = if wide && counts.remote_only > 0 {
        format!(" ({} remote-only)", counts.remote_only)
    } else {
        String::new()
    };
    line.push_str(&dim(&format!(" archived{remote_only}")));
    line
}

/// The host of an ssh URL, the path of a path remote.
fn remote_name(url: &str) -> String {
    let Some(rest) = url.strip_prefix("ssh://") else {
        return sanitize(url);
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    sanitize(host)
}

/// How long ago `time` was, in the largest unit that keeps two digits.
fn ago(now: Timestamp, time: Timestamp) -> String {
    let seconds = (now.as_second() - time.as_second()).max(0);
    match seconds {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..172_800 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// `YYYY-MM-DD`, in the narrow layout `MM-DD` within the current year.
fn date(time: Timestamp, now: Timestamp, wide: bool) -> String {
    let zoned = time.to_zoned(TimeZone::UTC);
    if !wide && zoned.year() == now.to_zoned(TimeZone::UTC).year() {
        zoned.strftime("%m-%d").to_string()
    } else {
        zoned.strftime("%Y-%m-%d").to_string()
    }
}

/// The pinned and empty pieces and the skipped entries, which the table
/// does not list.
fn count_line(report: &StatusReport) -> Option<String> {
    let skipped = &report.skipped;
    let plural = |count: usize, one: &str, many: &str| {
        (count > 0).then(|| format!("{count} {}", if count == 1 { one } else { many }))
    };
    let parts: Vec<String> = [
        plural(report.counts.pinned, "pinned", "pinned"),
        plural(report.counts.empty, "empty", "empty"),
        plural(skipped.symlinks, "symlink skipped", "symlinks skipped"),
        plural(
            skipped.dot_entries,
            "dot entry skipped",
            "dot entries skipped",
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The next piece that archives or becomes remote-only.
fn next_change(report: &StatusReport, width: usize) -> Option<String> {
    let mut candidates: Vec<(i64, String, &str)> = Vec::new();
    for piece in &report.pieces {
        if let Some(days) = piece.archives_in_days {
            candidates.push((days, piece.path.to_string(), "archives"));
        }
        if let Some(days) = piece.remote_only_in_days {
            candidates.push((days, piece.path.to_string(), "becomes remote-only"));
        }
    }
    candidates.sort();
    let (days, path, verb) = candidates.into_iter().next()?;
    let tail = format!(
        " {verb} in {days} {}",
        if days == 1 { "day" } else { "days" }
    );
    let room = width.saturating_sub("next: ".width() + tail.width());
    Some(format!("next: {}{tail}", mid(&sanitize(&path), room)))
}

struct Columns {
    state: usize,
    piece: usize,
    age: usize,
    what: usize,
    fix: bool,
}

fn table(rows: &[Row], style: Style, wide: bool) -> Vec<String> {
    let state_w = rows
        .iter()
        .map(|row| row.kind.state().width())
        .max()
        .unwrap_or(0)
        .max(if wide { 5 } else { 6 });
    let ages: Vec<String> = rows.iter().map(|row| age_text(row.age, wide)).collect();
    let age_w = ages
        .iter()
        .map(|age| age.width())
        .max()
        .unwrap_or(0)
        .max(if wide { 6 } else { 3 });
    // Everything left of the PIECE column's content and right of it.
    let other = state_w + age_w + 6;
    // The column is at least as wide as its header.
    let piece_longest = rows
        .iter()
        .map(|row| row.path.width())
        .max()
        .unwrap_or(0)
        .max("PIECE".len());
    let what_longest = rows.iter().map(|row| row.what().width()).max().unwrap_or(0);
    let fix_longest = rows
        .iter()
        .filter_map(|row| row.fix.as_ref())
        .map(|fix| fix.width())
        .max()
        .unwrap_or(0);

    let shrink = |piece: usize, what: usize| {
        let over = (other + piece + what).saturating_sub(style.width);
        piece.saturating_sub(over).max(piece.min(20))
    };
    let columns = if wide {
        let with_fix = other + piece_longest + what_longest + 2 + fix_longest;
        let squeezed = style
            .width
            .saturating_sub(other + what_longest + 2 + fix_longest);
        if fix_longest > 0 && with_fix <= style.width {
            Columns {
                state: state_w,
                piece: piece_longest,
                age: age_w,
                what: what_longest,
                fix: true,
            }
        } else if fix_longest > 0 && squeezed >= piece_longest.min(30) {
            Columns {
                state: state_w,
                piece: piece_longest.min(squeezed),
                age: age_w,
                what: what_longest,
                fix: true,
            }
        } else {
            let piece = shrink(piece_longest, what_longest);
            Columns {
                state: state_w,
                piece,
                age: age_w,
                what: what_longest.min(style.width.saturating_sub(other + piece).max(1)),
                fix: false,
            }
        }
    } else {
        let piece = shrink(piece_longest.min(34), what_longest);
        Columns {
            state: state_w,
            piece,
            age: age_w,
            what: what_longest.min(style.width.saturating_sub(other + piece).max(1)),
            fix: false,
        }
    };

    let mut lines = Vec::new();
    let age_header = if wide {
        pad("ACTIVE", columns.age)
    } else {
        pad_left("AGE", columns.age)
    };
    let header = format!(
        "{}  {}  {}  WHAT",
        pad("STATE", columns.state),
        pad("PIECE", columns.piece),
        age_header
    );
    lines.push(paint(style, Color::Dim, &header));
    for (row, age) in rows.iter().zip(&ages) {
        lines.extend(row_lines(row, age, &columns, style, wide));
    }
    lines
}

fn age_text(age: Option<i64>, wide: bool) -> String {
    match age {
        Some(days) if wide => format!("{days} d"),
        Some(days) => format!("{days}d"),
        None => "-".to_string(),
    }
}

fn row_lines(row: &Row, age: &str, columns: &Columns, style: Style, wide: bool) -> Vec<String> {
    let state = format!(
        "{}{}",
        paint(style, row.kind.color(), row.kind.state()),
        spaces(columns.state - row.kind.state().width())
    );
    let age_cell = if wide {
        pad(age, columns.age)
    } else if row.age.is_none() {
        // A dash has nothing to align to, so it sits in the middle.
        let left = (columns.age - 1) / 2;
        format!("{}-{}", spaces(left), spaces(columns.age - 1 - left))
    } else {
        pad_left(age, columns.age)
    };
    let head = format!(
        "{state}  {}  {age_cell}  ",
        pad(&mid(&row.path, columns.piece), columns.piece)
    );
    let indent = spaces(columns.state + columns.piece + columns.age + 6);
    let what_lines = wrap(&row.what(), columns.what);

    let mut lines = Vec::new();
    for (index, text) in what_lines.iter().enumerate() {
        let lead = if index == 0 {
            head.clone()
        } else {
            indent.clone()
        };
        let mut line = format!("{lead}{}", paint(style, row.kind.what_color(), text));
        if index == 0
            && columns.fix
            && let Some(fix) = &row.fix
        {
            line.push_str(&spaces(columns.what - text.width()));
            line.push_str("  ");
            line.push_str(&paint(style, Color::Dim, fix));
        }
        lines.push(line);
    }
    lines
}

// =========================================================
// Text
// =========================================================

pub(super) fn spaces(count: usize) -> String {
    " ".repeat(count)
}

pub(super) fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", spaces(width.saturating_sub(text.width())))
}

pub(super) fn pad_left(text: &str, width: usize) -> String {
    format!("{}{text}", spaces(width.saturating_sub(text.width())))
}

/// Shows control characters of a name in the escaped form of the spec, so
/// that a name cannot move the cursor or end a line.
pub(super) fn sanitize(text: &str) -> String {
    let mut shown = String::new();
    for character in text.chars() {
        if character.is_control() {
            let mut buffer = [0u8; 4];
            for byte in character.encode_utf8(&mut buffer).bytes() {
                shown.push_str(&format!("%{byte:02X}"));
            }
        } else {
            shown.push(character);
        }
    }
    shown
}

/// Cuts `text` to `width` columns by replacing its middle with `…`,
/// keeping the start and the end. A text that fits stays as it is.
pub(super) fn mid(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let keep = width - 1;
    let head = (keep / 2 + 2).min(keep);
    let tail = keep - head;
    let mut start = String::new();
    let mut used = 0;
    for character in text.chars() {
        let columns = character.width().unwrap_or(0);
        if used + columns > head {
            break;
        }
        start.push(character);
        used += columns;
    }
    let mut end: Vec<char> = Vec::new();
    used = 0;
    for character in text.chars().rev() {
        let columns = character.width().unwrap_or(0);
        if used + columns > tail {
            break;
        }
        end.push(character);
        used += columns;
    }
    end.reverse();
    format!("{start}…{}", end.into_iter().collect::<String>())
}

/// Breaks `text` into lines of at most `width` columns at spaces; a word
/// longer than a line is cut.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ').filter(|word| !word.is_empty()) {
        let mut word = word.to_string();
        while word.width() > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let mut cut = String::new();
            let mut used = 0;
            let mut rest = String::new();
            for character in word.chars() {
                let columns = character.width().unwrap_or(0);
                if rest.is_empty() && used + columns <= width {
                    cut.push(character);
                    used += columns;
                } else {
                    rest.push(character);
                }
            }
            if cut.is_empty() {
                // A character wider than the line still has to go somewhere.
                let mut characters = word.chars();
                cut.push(characters.next().expect("the word is not empty"));
                rest = characters.collect();
            }
            lines.push(cut);
            word = rest;
        }
        if line.is_empty() {
            line = word;
        } else if line.width() + 1 + word.width() <= width {
            line.push(' ');
            line.push_str(&word);
        } else {
            lines.push(std::mem::replace(&mut line, word));
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// A word for a shell: left as it is when it holds only plain characters,
/// else in single quotes.
fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::id::ObjectId;
    use antiq::ops::status::{
        CollectionInfo, Counts, Damaged, Decision, Flag, Piece, PieceKind, RemoteInfo, Skipped,
    };
    use antiq::relpath::{Name, RelPath};
    use antiq::remote::RemoteState;
    use antiq::report::Warning;
    use antiq::tracking::DecisionKind;

    const NOW: &str = "2026-11-02T12:00:00Z";

    fn now() -> Timestamp {
        NOW.parse().expect("a time")
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("a time")
    }

    fn style(width: usize, color: bool) -> Style {
        Style { width, color }
    }

    fn id(number: u64) -> ObjectId {
        ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_000 + number, 1))
    }

    fn piece(number: Option<u64>, path: &str, state: PieceKind) -> Piece {
        let rel = RelPath::parse(path).expect("a valid path");
        let name = Name::from_bytes(rel.components().last().expect("a name").to_vec())
            .expect("a valid name");
        Piece::new(number.map(id), rel, name, state)
    }

    /// A report shaped like the console mockups: a piece that waits for a
    /// decision, one with a failing backup, two that are due, one that waits
    /// for the upload and a new one.
    fn mock_report() -> StatusReport {
        let mut decide = piece(Some(1), "2026_09_03_data-extraction", PieceKind::Live);
        decide.flags = vec![Flag::NeedsDecision];
        decide.detail.decisions = vec![Decision {
            kind: DecisionKind::ReplacingWithLoss,
            options: DecisionKind::ReplacingWithLoss.options().to_vec(),
        }];
        let mut stale = piece(
            Some(2),
            "2026_10_22_upstream-issues",
            PieceKind::Live,
        );
        stale.flags = vec![Flag::StaleBackup];
        stale.detail.stale_since = Some(at("2026-10-30T08:00:00Z"));
        stale.age_days = Some(9);
        let mut work = piece(Some(3), "2026_10_01_debug-session", PieceKind::Live);
        work.flags = vec![Flag::Idle];
        work.age_days = Some(33);
        let mut zero = piece(
            Some(4),
            "2026_09_02_print-server-ideas",
            PieceKind::Live,
        );
        zero.flags = vec![Flag::Idle];
        zero.age_days = Some(62);
        let upload = {
            let mut upload = piece(
                Some(5),
                "2026_10_30_todo-run",
                PieceKind::Archived,
            );
            upload.flags = vec![Flag::PendingUpload];
            upload
        };
        let mut scratch = piece(None, "scratch", PieceKind::New);
        scratch.age_days = Some(1);
        StatusReport {
            collection: CollectionInfo {
                id: id(0),
                path: "/home/me/playground".into(),
            },
            remote: RemoteInfo {
                state: RemoteState::Reachable,
                url: Some("ssh://me@nas.local:2222/volume1/antiq".into()),
            },
            last_sync: Some(at("2026-11-02T10:00:00Z")),
            pieces: vec![decide, stale, work, zero, upload, scratch],
            recovered: Vec::new(),
            damaged: Vec::new(),
            unreadable_directories: Vec::new(),
            skipped: Skipped {
                symlinks: 0,
                dot_entries: 0,
            },
            counts: Counts {
                live: 13,
                archived: 281,
                remote_only: 202,
                pinned: 0,
                empty: 0,
            },
            warnings: Vec::new(),
        }
    }

    fn stdout(report: &StatusReport, style: Style) -> String {
        render(report, now(), style).stdout.join("\n")
    }

    #[test]
    fn at_80_columns_the_table_is_narrow_with_paths_shortened_in_the_middle() {
        insta::assert_snapshot!(stdout(&mock_report(), style(80, false)), @r"
        nas.local reachable, synced 2 h ago · 13 live · 281 archived

        STATE   PIECE                               AGE  WHAT
        decide  2026_09_03_data-extraction           -   replace or fork
        stale   2026_10_22_upstream-issues    9d  backup failing since 10-30
        due     2026_10_01_debug-session        33d  archives next run
        due     2026_09_02_print-server-ideas…nt_server_ideas  62d  archives next run
        upload  2026_10_30_todo-run…d-off-by-agents   -   waits for sync
        new     scratch                              1d  ID at next sync

        answer decisions: antiq resolve · every piece: antiq find
        ");
    }

    #[test]
    fn at_120_columns_the_table_is_wide_with_the_fixing_command() {
        insta::assert_snapshot!(stdout(&mock_report(), style(120, false)), @r"
        remote nas.local: reachable, synced 2 h ago · 13 live, 281 archived (202 remote-only)

        STATE   PIECE                           ACTIVE  WHAT
        decide  2026_09_03_data-extraction      -       replace or fork                 antiq resolve 2026_09_03_data-extraction
        stale   2026_10_22_upstream-issues…stream_issues  9 d     backup failed since 2026-10-30  antiq sync
        due     2026_10_01_debug-session    33 d    archives at the next run        antiq archive
        due     2026_09_02_print-server-ideas…_server_ideas  62 d    archives at the next run        antiq archive
        upload  2026_10_30_todo-run…off-by-agents  -       archive waits for sync          antiq sync
        new     scratch                         1 d     gets an ID at the next sync

        everything else is up to date · every piece: antiq find
        ");
    }

    // =========================================================
    // Colour
    // =========================================================

    /// Escape sequences made visible, so that a snapshot shows what is dim
    /// and what is coloured.
    fn visible(text: &str) -> String {
        text.replace('\x1b', "<ESC>")
    }

    #[test]
    fn colour_is_dim_for_the_frame_and_coloured_by_kind() {
        insta::assert_snapshot!(visible(&stdout(&mock_report(), style(80, true))), @r"
        <ESC>[2mnas.local<ESC>[0m <ESC>[32mreachable<ESC>[0m<ESC>[2m, synced 2 h ago · <ESC>[0m13<ESC>[2m live · <ESC>[0m281<ESC>[2m archived<ESC>[0m

        <ESC>[2mSTATE   PIECE                               AGE  WHAT<ESC>[0m
        <ESC>[33mdecide<ESC>[0m  2026_09_03_data-extraction           -   <ESC>[33mreplace or fork<ESC>[0m
        <ESC>[33mstale<ESC>[0m   2026_10_22_upstream-issues    9d  <ESC>[33mbackup failing since 10-30<ESC>[0m
        <ESC>[35mdue<ESC>[0m     2026_10_01_debug-session        33d  <ESC>[35marchives next run<ESC>[0m
        <ESC>[35mdue<ESC>[0m     2026_09_02_print-server-ideas…nt_server_ideas  62d  <ESC>[35marchives next run<ESC>[0m
        <ESC>[36mupload<ESC>[0m  2026_10_30_todo-run…d-off-by-agents   -   <ESC>[36mwaits for sync<ESC>[0m
        new     scratch                              1d  <ESC>[2mID at next sync<ESC>[0m

        <ESC>[2manswer decisions: antiq resolve · every piece: antiq find<ESC>[0m
        ");
    }

    #[test]
    fn wide_counts_are_bold_and_the_fixing_command_is_dim() {
        let text = visible(&stdout(&mock_report(), style(120, true)));
        let first = text.lines().next().expect("a header");
        assert_eq!(
            first,
            "<ESC>[2mremote nas.local:<ESC>[0m <ESC>[32mreachable<ESC>[0m<ESC>[2m, synced 2 h ago · <ESC>[0m<ESC>[1m13<ESC>[0m<ESC>[2m live, <ESC>[0m<ESC>[1m281<ESC>[0m<ESC>[2m archived (202 remote-only)<ESC>[0m"
        );
        assert!(
            text.contains("<ESC>[2mantiq resolve 2026_09_03_data-extraction<ESC>[0m"),
            "{text}"
        );
    }

    // =========================================================
    // The other states of the screen
    // =========================================================

    fn clean_report() -> StatusReport {
        let mut report = mock_report();
        let mut soon = piece(Some(7), "2026_10_28_todo-run-b", PieceKind::Live);
        soon.age_days = Some(5);
        soon.archives_in_days = Some(9);
        report.pieces = vec![soon];
        report.counts = Counts {
            live: 13,
            archived: 284,
            remote_only: 202,
            pinned: 0,
            empty: 0,
        };
        report.last_sync = Some(at("2026-11-02T11:48:00Z"));
        report
    }

    #[test]
    fn without_rows_nothing_is_to_do_and_the_next_change_is_named() {
        insta::assert_snapshot!(stdout(&clean_report(), style(80, false)), @r"
        nas.local reachable, synced 12 min ago · 13 live · 284 archived

        nothing to do: no decisions, backups current, nothing due
        next: 2026_10_28_todo-run-b archives in 9 days
        ");
        insta::assert_snapshot!(stdout(&clean_report(), style(120, false)), @r"
        remote nas.local: reachable, synced 12 min ago · 13 live, 284 archived (202 remote-only)

        nothing to do: no decisions, backups current, nothing due
        next: 2026_10_28_todo-run-b archives in 9 days
        ");
    }

    #[test]
    fn the_next_change_may_be_a_piece_becoming_remote_only() {
        let mut report = clean_report();
        report.pieces[0].archives_in_days = Some(30);
        let mut old = piece(Some(8), "old", PieceKind::Archived);
        old.remote_only_in_days = Some(1);
        report.pieces.push(old);
        let text = stdout(&report, style(80, false));
        assert!(
            text.ends_with("next: old becomes remote-only in 1 day"),
            "{text}"
        );
    }

    #[test]
    fn without_anything_due_ever_there_is_no_next_line() {
        let mut report = clean_report();
        report.pieces.clear();
        let text = stdout(&report, style(80, false));
        assert!(text.ends_with("nothing due"), "{text}");
    }

    #[test]
    fn without_a_remote_the_header_says_so_and_upload_rows_are_not_listed() {
        let mut report = mock_report();
        report.remote.url = None;
        report.remote.state = RemoteState::None;
        report.last_sync = None;
        let narrow = stdout(&report, style(80, false));
        assert!(
            narrow.starts_with("no remote · 13 live · 281 archived\n"),
            "{narrow}"
        );
        assert!(!narrow.contains("upload"), "{narrow}");
        let wide = stdout(&report, style(120, false));
        assert!(
            wide.starts_with("no remote · 13 live, 281 archived (202 remote-only)\n"),
            "{wide}"
        );
    }

    #[test]
    fn a_remote_that_never_synced_says_so() {
        let mut report = mock_report();
        report.last_sync = None;
        report.remote.state = RemoteState::Away;
        let text = stdout(&report, style(80, false));
        assert!(
            text.starts_with("nas.local away, never synced · 13 live · 281 archived\n"),
            "{text}"
        );
    }

    #[test]
    fn counts_of_pieces_the_table_does_not_list_sit_above_the_hint() {
        let mut report = mock_report();
        report.counts.pinned = 3;
        report.counts.empty = 1;
        report.skipped.symlinks = 1;
        report.skipped.dot_entries = 4;
        let text = stdout(&report, style(80, false));
        assert!(
            text.contains(
                "\n3 pinned · 1 empty · 1 symlink skipped · 4 dot entries skipped\nanswer decisions"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_count_line_closes_the_clean_screen() {
        let mut report = clean_report();
        report.counts.pinned = 2;
        let text = stdout(&report, style(80, false));
        assert!(text.ends_with("archives in 9 days\n2 pinned"), "{text}");
    }

    #[test]
    fn the_resolve_hint_needs_a_pending_decision() {
        let mut report = mock_report();
        report.pieces.remove(0);
        let text = stdout(&report, style(80, false));
        assert!(text.ends_with("\n\nevery piece: antiq find"), "{text}");
    }

    #[test]
    fn rows_are_listed_by_priority_and_due_rows_by_age() {
        let mut report = mock_report();
        report.pieces.reverse();
        let text = stdout(&report, style(80, false));
        let states: Vec<&str> = text
            .lines()
            .skip(3)
            .take(6)
            .map(|line| line.split_whitespace().next().expect("a state"))
            .collect();
        assert_eq!(states, ["decide", "stale", "due", "due", "upload", "new"]);
        let due: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("due"))
            .collect();
        assert!(due[0].contains("33d") && due[1].contains("62d"), "{due:?}");
    }

    #[test]
    fn a_piece_with_several_conditions_gets_one_row_with_all_texts() {
        let mut report = mock_report();
        report.pieces.truncate(1);
        report.pieces[0].flags = vec![Flag::NeedsDecision, Flag::StaleBackup];
        report.pieces[0].detail.stale_since = Some(at("2025-12-31T00:00:00Z"));
        let text = stdout(&report, style(120, false));
        assert!(
            text.contains(
                "replace or fork, backup failed since 2025-12-31  antiq resolve 2026_09_03_data-extraction"
            ) || text.contains("replace or fork, backup failed since 2025-12-31"),
            "{text}"
        );
        assert_eq!(text.lines().filter(|l| l.starts_with("decide")).count(), 1);
    }

    #[test]
    fn a_stale_backup_of_an_earlier_year_names_the_year_in_the_narrow_layout() {
        let mut report = mock_report();
        report.pieces[1].detail.stale_since = Some(at("2025-12-31T00:00:00Z"));
        let text = stdout(&report, style(80, false));
        assert!(text.contains("backup failing since 2025-12-31"), "{text}");
    }

    #[test]
    fn errors_damaged_files_and_recovered_trees_get_rows() {
        let mut report = mock_report();
        report.pieces.clear();
        let mut broken = piece(Some(9), "broken", PieceKind::Error);
        broken.flags = vec![Flag::Error];
        broken.detail.error = Some("the tracking entry has no metadata".into());
        report.pieces.push(broken);
        report.damaged = vec![
            Damaged {
                id: id(9),
                path: ".antiq/tracking/X.json".into(),
                error: "shown as the piece".into(),
            },
            Damaged {
                id: id(10),
                path: ".antiq/objects/Y".into(),
                error: "nothing readable".into(),
            },
        ];
        report.recovered = vec!["01ARZ-2026-11-01".into()];
        let text = stdout(&report, style(120, false));
        let rows: Vec<&str> = text.lines().skip(3).take(3).collect();
        // Rows of one kind are listed by path.
        assert!(
            rows[0].contains(".antiq/objects/Y") && rows[0].contains("nothing readable"),
            "{text}"
        );
        assert!(
            rows[1].contains("broken") && rows[1].contains("no metadata"),
            "{text}"
        );
        assert!(
            rows[2].contains(".antiq/recovered/01ARZ-2026-11-01"),
            "{text}"
        );
        assert!(!text.contains("shown as the piece"));
    }

    #[test]
    fn walk_findings_get_their_words() {
        let mut report = mock_report();
        report.pieces.clear();
        let mut unreadable = piece(Some(1), "u", PieceKind::Live);
        unreadable.flags = vec![Flag::Unreadable];
        unreadable.detail.unreadable_first = RelPath::parse("u/sub").ok();
        let mut mount = piece(Some(2), "m", PieceKind::Live);
        mount.flags = vec![Flag::MountPoint];
        mount.detail.mount_points = vec![RelPath::parse("m/disk").expect("a path")];
        let mut excluded = piece(Some(3), "x", PieceKind::Live);
        excluded.flags = vec![Flag::Excluded];
        excluded.detail.excluded_paths = vec![RelPath::parse("x/skip").expect("a path")];
        let mut future = piece(Some(4), "f", PieceKind::Live);
        future.flags = vec![Flag::FutureMtime];
        future.detail.future_files = vec![
            RelPath::parse("f/a").expect("a path"),
            RelPath::parse("f/b").expect("a path"),
        ];
        let gone = piece(Some(5), "g", PieceKind::Gone);
        let mut diverged = piece(Some(6), "d", PieceKind::Live);
        diverged.flags = vec![Flag::Diverged];
        let mut superseded = piece(Some(7), "s", PieceKind::Archived);
        superseded.flags = vec![Flag::Superseded];
        let mut missing = piece(Some(8), "n", PieceKind::Archived);
        missing.flags = vec![Flag::MissingOnRemote];
        report.pieces = vec![
            unreadable, mount, excluded, future, gone, diverged, superseded, missing,
        ];
        insta::assert_snapshot!(stdout(&report, style(120, false)), @r"
        remote nas.local: reachable, synced 2 h ago · 13 live, 281 archived (202 remote-only)

        STATE       PIECE  ACTIVE  WHAT
        diverged    d      -       folder changed since unearthing   antiq resolve d
        superseded  s      -       superseded archive kept           antiq resolve s
        missing     n      -       archive is gone from the remote   antiq sync
        unreadable  u      -       cannot read u/sub
        mount       m      -       holds a mount point at m/disk
        excluded    x      -       holds the excluded path x/skip
        future      f      -       2 files with times in the future
        gone        g      -       folder is missing

        everything else is up to date · every piece: antiq find
        ");
    }

    #[test]
    fn a_collection_inside_a_piece_shows_in_the_mount_state() {
        let mut report = mock_report();
        report.pieces.clear();
        let mut nested = piece(Some(1), "n", PieceKind::Live);
        nested.flags = vec![Flag::NestedCollection];
        report.pieces = vec![nested];
        let narrow = stdout(&report, style(80, false));
        assert!(
            narrow.contains("mount   n       -   holds another collection"),
            "{narrow}"
        );
        let wide = stdout(&report, style(120, false));
        assert!(wide.contains("holds another collection"), "{wide}");
    }

    #[test]
    fn walk_findings_have_shorter_words_in_the_narrow_layout() {
        let mut report = mock_report();
        report.pieces.clear();
        let mut mount = piece(Some(2), "m", PieceKind::Live);
        mount.flags = vec![Flag::MountPoint];
        let mut excluded = piece(Some(3), "x", PieceKind::Live);
        excluded.flags = vec![Flag::Excluded];
        let mut future = piece(Some(4), "f", PieceKind::Live);
        future.flags = vec![Flag::FutureMtime];
        let mut superseded = piece(Some(7), "s", PieceKind::Archived);
        superseded.flags = vec![Flag::Superseded];
        let mut missing = piece(Some(8), "n", PieceKind::Archived);
        missing.flags = vec![Flag::MissingOnRemote];
        report.pieces = vec![mount, excluded, future, superseded, missing];
        insta::assert_snapshot!(stdout(&report, style(80, false)), @r"
        nas.local reachable, synced 2 h ago · 13 live · 281 archived

        STATE       PIECE  AGE  WHAT
        superseded  s       -   old archive kept
        missing     n       -   gone from the remote
        mount       m       -   holds a mount point
        excluded    x       -   holds an excluded path
        future      f       -   file times in the future

        every piece: antiq find
        ");
    }

    #[test]
    fn every_kind_of_decision_names_its_answers() {
        let words = |kind: DecisionKind| {
            let mut pending = piece(Some(1), "p", PieceKind::Live);
            pending.flags = vec![Flag::NeedsDecision];
            pending.detail.decisions = vec![Decision {
                kind,
                options: kind.options().to_vec(),
            }];
            decision_words(&pending)
        };
        assert_eq!(words(DecisionKind::VersionConflict), "mine, theirs or fork");
        assert_eq!(words(DecisionKind::ReplacingWithLoss), "replace or fork");
        assert_eq!(words(DecisionKind::DivergedFolder), "theirs, mine or fork");
        assert_eq!(words(DecisionKind::SupersededArchive), "drop superseded");
    }

    #[test]
    fn a_decision_without_details_is_still_pending() {
        let mut pending = piece(Some(1), "p", PieceKind::Live);
        pending.flags = vec![Flag::NeedsDecision];
        assert_eq!(decision_words(&pending), "decision pending");
    }

    #[test]
    fn a_broken_remote_and_an_unprobed_one_have_their_words() {
        let mut report = mock_report();
        report.remote.state = RemoteState::Broken;
        assert!(stdout(&report, style(80, false)).starts_with("nas.local broken, synced"));
        report.remote.state = RemoteState::None;
        assert!(stdout(&report, style(80, false)).starts_with("nas.local unknown, synced"));
    }

    #[test]
    fn new_pieces_that_are_pinned_or_empty_are_counted_and_not_listed() {
        let mut report = mock_report();
        report.pieces.clear();
        let mut pinned = piece(None, "pinned", PieceKind::New);
        pinned.flags = vec![Flag::Pinned];
        let mut empty = piece(None, "empty", PieceKind::New);
        empty.flags = vec![Flag::Empty];
        report.pieces = vec![pinned, empty];
        let text = stdout(&report, style(80, false));
        assert!(text.contains("nothing to do"), "{text}");
    }

    #[test]
    fn pieces_below_an_unreadable_directory_and_ordinary_pieces_have_no_row() {
        let mut report = mock_report();
        report.pieces.clear();
        let mut below = piece(Some(1), "group/work", PieceKind::Live);
        below.flags = vec![Flag::Idle];
        below.detail.below_unreadable = RelPath::parse("group").ok();
        let mut pinned = piece(Some(2), "pinned", PieceKind::Live);
        pinned.flags = vec![Flag::Pinned];
        let archived = piece(Some(3), "archived", PieceKind::Archived);
        report.pieces = vec![below, pinned, archived];
        let text = stdout(&report, style(80, false));
        assert!(text.contains("nothing to do"), "{text}");
    }

    #[test]
    fn warnings_go_to_stderr_as_notes() {
        let mut report = mock_report();
        report.warnings = vec![Warning::new("the collection is on an SMB mount")];
        let lines = render(&report, now(), style(80, false));
        assert_eq!(lines.stderr, ["note: the collection is on an SMB mount"]);
        assert!(!lines.stdout.join("\n").contains("SMB"));
    }

    // =========================================================
    // Width
    // =========================================================

    fn widest(text: &str) -> usize {
        text.lines().map(UnicodeWidthStr::width).max().unwrap_or(0)
    }

    #[test]
    fn no_narrow_line_is_wider_than_80_columns() {
        let mut report = mock_report();
        let mut long = piece(
            Some(11),
            &"very_long_directory_name_".repeat(6),
            PieceKind::Live,
        );
        long.flags = vec![
            Flag::NeedsDecision,
            Flag::StaleBackup,
            Flag::Superseded,
            Flag::MissingOnRemote,
            Flag::Unreadable,
            Flag::FutureMtime,
        ];
        long.detail.decisions = vec![Decision {
            kind: DecisionKind::VersionConflict,
            options: DecisionKind::VersionConflict.options().to_vec(),
        }];
        long.detail.stale_since = Some(at("2025-01-01T00:00:00Z"));
        long.detail.unreadable_first = RelPath::parse("a/b/c").ok();
        long.age_days = Some(1200);
        let double = piece(
            Some(12),
            "日本語のフォルダ名日本語のフォルダ名日本語のフォルダ名日本語のフォルダ名",
            PieceKind::Gone,
        );
        let combining = piece(
            Some(13),
            "e\u{301}e\u{301}e\u{301}_name_that_is_long_enough_to_be_cut_somewhere",
            PieceKind::Gone,
        );
        let mut escape = piece(Some(14), "a\u{1b}[2Jb", PieceKind::Live);
        escape.flags = vec![Flag::Idle];
        escape.age_days = Some(40);
        report.pieces.extend([long, double, combining, escape]);
        let text = stdout(&report, style(80, false));
        assert!(widest(&text) <= 80, "{text}");
        assert!(text.contains("1200d"), "{text}");
        assert!(!text.contains('\x1b'));
        assert!(text.contains("a%1B[2Jb"), "{text}");
    }

    #[test]
    fn a_long_what_wraps_under_its_column() {
        let mut report = mock_report();
        report.pieces.truncate(1);
        report.pieces[0].flags = vec![
            Flag::NeedsDecision,
            Flag::StaleBackup,
            Flag::MissingOnRemote,
        ];
        report.pieces[0].detail.decisions = vec![Decision {
            kind: DecisionKind::VersionConflict,
            options: DecisionKind::VersionConflict.options().to_vec(),
        }];
        report.pieces[0].detail.stale_since = Some(at("2026-10-30T00:00:00Z"));
        let text = stdout(&report, style(80, false));
        assert!(widest(&text) <= 80, "{text}");
        let rows: Vec<&str> = text.lines().skip(3).take(3).collect();
        assert!(rows[0].starts_with("decide"));
        assert!(rows[1].starts_with(&" ".repeat(35)), "{text}");
        assert!(!rows[1].starts_with(&" ".repeat(36)), "{text}");
        assert!(rows[1].trim_start().len() < rows[1].len());
    }

    #[test]
    fn at_100_columns_the_layout_is_wide_but_without_the_fixing_command() {
        let text = stdout(&mock_report(), style(100, false));
        assert!(widest(&text) <= 100, "{text}");
        assert!(text.contains("ACTIVE"));
        assert!(!text.contains("antiq resolve 2026"), "{text}");
        assert!(!text.contains("antiq sync"), "{text}");
    }

    #[test]
    fn at_99_columns_the_layout_is_still_narrow() {
        let text = stdout(&mock_report(), style(99, false));
        assert!(text.contains(" AGE "), "{text}");
        assert!(widest(&text) <= 99, "{text}");
    }

    #[test]
    fn a_long_path_remote_is_cut_in_the_header() {
        let mut report = mock_report();
        report.remote.url = Some(format!("/mnt/{}", "x".repeat(100)));
        let text = stdout(&report, style(80, false));
        assert!(widest(&text) <= 80, "{text}");
    }

    // =========================================================
    // Text helpers
    // =========================================================

    #[test]
    fn mid_keeps_the_start_and_the_end() {
        assert_eq!(mid("abcdefghij", 10), "abcdefghij");
        assert_eq!(mid("abcdefghijklmnop", 10), "abcdef…nop");
        assert_eq!(mid("abcdefghijklmnop", 8).chars().count(), 8);
    }

    #[test]
    fn mid_is_total_for_every_width() {
        assert_eq!(mid("abcdef", 0), "");
        assert_eq!(mid("abcdef", 1), "…");
        for width in 0..6 {
            let cut = mid("abcdefghij", width);
            assert!(cut.width() <= width, "{width}: {cut}");
        }
    }

    #[test]
    fn times_are_told_in_the_largest_fitting_unit() {
        let base = at("2026-11-02T12:00:00Z");
        let before =
            |seconds: i64| Timestamp::from_second(base.as_second() - seconds).expect("a time");
        assert_eq!(ago(base, before(0)), "just now");
        assert_eq!(ago(base, before(59)), "just now");
        assert_eq!(ago(base, before(60)), "1 min ago");
        assert_eq!(ago(base, before(3_599)), "59 min ago");
        assert_eq!(ago(base, before(3_600)), "1 h ago");
        assert_eq!(ago(base, before(172_799)), "47 h ago");
        assert_eq!(ago(base, before(172_800)), "2 d ago");
        assert_eq!(ago(base, before(-30)), "just now");
    }

    #[test]
    fn a_word_for_a_shell_is_quoted_when_it_needs_to_be() {
        assert_eq!(shell_word("a/b-c_d.e"), "a/b-c_d.e");
        assert_eq!(shell_word("my folder"), "'my folder'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word(""), "''");
    }

    #[test]
    fn a_word_longer_than_the_line_is_cut() {
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("ab cd ef", 5), ["ab cd", "ef"]);
        assert_eq!(wrap("ab cdefghijkl", 5), ["ab", "cdefg", "hijkl"]);
        assert_eq!(wrap("", 5), [""]);
        assert_eq!(wrap("日本", 1), ["日", "本"]);
    }
}
