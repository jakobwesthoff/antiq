// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cards of a long run: what a piece shows while it works and the one
//! line it leaves behind. The same formatter writes both, so the lines on
//! the screen during a run and the lines that remain after it look alike.
//!
//! [`Cards`] is a pure state. The driver feeds it messages and the time and
//! asks it for lines; it touches no terminal.

use crate::cli::console::symbols::Symbols;
use crate::cli::console::theme;
use crate::cli::output::Style;
use crate::cli::table::{decimal_size, mid, pad_left, sanitize, wrap};
use crate::cli::unresolved::pasteable_line;
use anstyle::Style as Look;
use antiq::pin::PIN_FILE;
use antiq::progress::{Counter, Operation, Outcome};
use std::path::PathBuf;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// Lines are laid out for at most this many columns.
const MAX_WIDTH: usize = 100;
/// The columns of the size, of the result and of the time of a line.
const SIZE: usize = 8;
const RESULT: usize = 20;
const TIME: usize = 4;
/// A name never gets less than this.
const MIN_NAME: usize = 6;
/// A name shown with a version is cut to this many columns before the
/// version is dropped.
const MIN_LABELLED_NAME: usize = 8;
/// The lit part of a bar without a total.
const HEAD: usize = 4;
/// The time between two frames of the spinner and of the moving bar.
const FRAME_MILLIS: u128 = 100;

/// How the count of an operation reads under its bar. The formatter draws
/// the bar and places the text; the words belong to the command.
#[derive(Clone, Copy)]
pub(crate) struct Count {
    /// The text for `done` of `total`. The total is `None` while it is not
    /// known or when `done` is past it, and the text may then be empty.
    pub text: fn(done: u64, total: Option<u64>) -> String,
    /// Whether the percentage shows next to the bar. A bar with a percentage
    /// stops at the total; one without it turns into the moving bar when
    /// `done` is past the total.
    pub percent: bool,
}

/// `3/12 files`, and nothing while the total is not known.
pub(crate) fn items(done: u64, total: Option<u64>, noun: &str) -> String {
    total.map_or_else(String::new, |total| format!("{done}/{total} {noun}"))
}

/// The count of an operation that its command does not list.
const FILES: Count = Count {
    text: |done, total| items(done, total, "files"),
    percent: true,
};

pub(crate) struct Config {
    /// The count of the operations that do not read `N/M files`.
    pub counts: &'static [(Operation, Count)],
    pub style: Style,
    /// The `-C` of the run, for the commands a card tells the reader to type.
    pub directory: Option<PathBuf>,
}

/// An event of the library, owned so that it can cross a thread.
pub(crate) enum Msg {
    Planned(Vec<(String, u64)>),
    Started {
        name: String,
        bytes: u64,
        label: Option<String>,
    },
    Operation {
        name: String,
        operation: Operation,
        counter: Counter,
    },
    Paused {
        name: String,
    },
    Finished {
        name: String,
        bytes: u64,
        outcome: Outcome,
    },
    Dropped {
        name: String,
    },
}

/// A piece that has finished and waits for its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ready {
    pub name: String,
    /// `None` for a piece that never sent an event.
    pub bytes: Option<u64>,
    pub label: Option<String>,
    pub outcome: Outcome,
    /// `None` for a piece that never sent an event.
    pub elapsed: Option<Duration>,
}

/// How a part of a summary looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Ok,
    Plain,
    Bad,
    Warn,
}

/// The last line of a run: its mark follows `tone`, its parts are joined
/// by the separator, and the time the run took ends it.
pub(crate) struct Summary {
    pub tone: Tone,
    pub parts: Vec<(String, Tone)>,
    pub took: Duration,
}

/// Where a piece is in its run.
enum Phase {
    /// Expected, not started.
    Waiting,
    /// Has a card. The operation is unknown until the first one is sent.
    Running(Option<(Operation, Counter)>),
    /// Started, off its card until its next operation.
    Paused,
    Done,
}

struct Item {
    name: String,
    bytes: u64,
    label: Option<String>,
    phase: Phase,
    /// The time of the segments that have closed.
    elapsed: Duration,
    /// When the open segment began, while the piece is running.
    since: Option<Duration>,
    /// The place in the row of cards: pieces keep the order they started in.
    order: u64,
}

pub(crate) struct Cards {
    config: Config,
    items: Vec<Item>,
    next_order: u64,
    ready: Vec<Ready>,
}

/// The columns of a line at a terminal width.
struct Layout {
    name: usize,
    size: bool,
    bar: usize,
    detail: bool,
}

impl Layout {
    fn of(cols: usize) -> Layout {
        let width = cols.min(MAX_WIDTH);
        let size = width >= 50;
        let fixed = 4 + 2 + RESULT + 2 + TIME + if size { 2 + SIZE } else { 0 };
        Layout {
            name: width.saturating_sub(fixed).max(MIN_NAME),
            size,
            bar: match width {
                70.. => 40,
                40.. => 12,
                _ => 10,
            },
            detail: width >= 60,
        }
    }
}

/// `line` cut to `cols` columns. A cut that falls inside a coloured stretch
/// ends it, so the colour does not run into the line below.
fn cut_to_width(line: &str, cols: usize) -> String {
    if visible_width(line) <= cols {
        return line.to_string();
    }
    let mut cut = String::new();
    let mut used = 0;
    let mut coloured = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            cut.push(c);
            for end in chars.by_ref() {
                cut.push(end);
                if end == 'm' {
                    break;
                }
            }
            coloured = !cut.ends_with("\x1b[0m");
            continue;
        }
        let room = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + room > cols {
            break;
        }
        used += room;
        cut.push(c);
    }
    if coloured {
        cut.push_str("\x1b[0m");
    }
    cut
}

/// The columns `line` takes on a terminal. The colour sequences of the
/// cards (`ESC [ ... m`) take none.
fn visible_width(line: &str) -> usize {
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for end in chars.by_ref() {
                if end == 'm' {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain.width()
}

impl Cards {
    pub(crate) fn new(config: Config) -> Cards {
        Cards {
            config,
            items: Vec::new(),
            next_order: 0,
            ready: Vec::new(),
        }
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.items.iter().position(|item| item.name == name)
    }

    /// The index of the piece, which a message for an unknown one adds.
    fn find_or_add(&mut self, name: &str) -> usize {
        self.position(name).unwrap_or_else(|| {
            self.items.push(Item {
                name: name.to_string(),
                bytes: 0,
                label: None,
                phase: Phase::Waiting,
                elapsed: Duration::ZERO,
                since: None,
                order: 0,
            });
            self.items.len() - 1
        })
    }

    /// Puts the piece on a card, opening a segment of its time.
    fn open(&mut self, index: usize, now: Duration) {
        let order = self.next_order;
        let item = &mut self.items[index];
        if matches!(item.phase, Phase::Running(_) | Phase::Done) {
            return;
        }
        item.phase = Phase::Running(None);
        item.since = Some(now);
        item.order = order;
        self.next_order += 1;
    }

    fn close_segment(item: &mut Item, now: Duration) {
        if let Some(since) = item.since.take() {
            item.elapsed += now.saturating_sub(since);
        }
    }

    pub(crate) fn apply(&mut self, msg: Msg, now: Duration) {
        match msg {
            Msg::Planned(planned) => {
                for (name, bytes) in planned {
                    if self.position(&name).is_none() {
                        let index = self.find_or_add(&name);
                        self.items[index].bytes = bytes;
                    }
                }
            }
            Msg::Started { name, bytes, label } => {
                let index = self.find_or_add(&name);
                self.open(index, now);
                let item = &mut self.items[index];
                item.bytes = bytes;
                item.label = label;
            }
            Msg::Operation {
                name,
                operation,
                counter,
            } => {
                let index = self.find_or_add(&name);
                self.open(index, now);
                if let Phase::Running(current) = &mut self.items[index].phase {
                    *current = Some((operation, counter));
                }
            }
            Msg::Paused { name } => {
                if let Some(index) = self.position(&name)
                    && matches!(self.items[index].phase, Phase::Running(_))
                {
                    let item = &mut self.items[index];
                    Self::close_segment(item, now);
                    item.phase = Phase::Paused;
                }
            }
            Msg::Finished {
                name,
                bytes,
                outcome,
            } => {
                let index = self.find_or_add(&name);
                let item = &mut self.items[index];
                if matches!(item.phase, Phase::Done) {
                    return;
                }
                Self::close_segment(item, now);
                item.phase = Phase::Done;
                self.ready.push(Ready {
                    name,
                    bytes: Some(bytes),
                    label: item.label.clone(),
                    outcome,
                    elapsed: Some(item.elapsed),
                });
            }
            Msg::Dropped { name } => {
                if let Some(index) = self.position(&name)
                    && matches!(self.items[index].phase, Phase::Waiting)
                {
                    self.items.remove(index);
                }
            }
        }
    }

    /// The pieces that finished since the last call, in the order they did.
    pub(crate) fn take_ready(&mut self) -> Vec<Ready> {
        std::mem::take(&mut self.ready)
    }

    /// Whether a finished piece waits for its line.
    pub(crate) fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Whether the piece has finished, whether or not its line was taken.
    pub(crate) fn has_finished(&self, name: &str) -> bool {
        self.items
            .iter()
            .any(|item| item.name == name && matches!(item.phase, Phase::Done))
    }

    // =====================================================
    // Looks
    // =====================================================

    fn symbols(&self) -> Symbols {
        self.config.style.symbols()
    }

    fn paint(&self, look: Look, text: &str) -> String {
        if self.config.style.color && !text.is_empty() {
            format!("{look}{text}{look:#}")
        } else {
            text.to_string()
        }
    }

    fn tone_look(tone: Tone) -> Look {
        match tone {
            Tone::Ok => theme::GREEN,
            Tone::Plain => Look::new(),
            Tone::Bad => theme::FAILED,
            Tone::Warn => theme::YELLOW,
        }
    }

    fn tone_symbol(&self, tone: Tone) -> String {
        let symbols = self.symbols();
        let text = match tone {
            Tone::Ok | Tone::Plain => symbols.done,
            Tone::Bad => symbols.error,
            Tone::Warn => symbols.warning,
        };
        self.paint(Self::tone_look(tone), text)
    }

    /// The name of a piece and, after an unearth, its version, cut to
    /// `width` columns: the cell is returned painted with its display width.
    fn name_cell(&self, name: &str, label: Option<&str>, width: usize) -> (String, usize) {
        let ellipsis = self.symbols().ellipsis;
        let name = sanitize(name);
        let label = label.map(|label| format!("version {}", sanitize(label)));
        if let Some(label) = label {
            let room = width.saturating_sub(1 + label.width());
            if room >= MIN_LABELLED_NAME {
                let shown = mid(&name, room, ellipsis);
                let cell_width = shown.width() + 1 + label.width();
                return (
                    format!(
                        "{} {}",
                        self.paint(theme::BOLD, &shown),
                        self.paint(theme::DIM, &label)
                    ),
                    cell_width,
                );
            }
        }
        let shown = mid(&name, width, ellipsis);
        let cell_width = shown.width();
        (self.paint(theme::BOLD, &shown), cell_width)
    }

    fn size_cell(&self, bytes: Option<u64>, layout: &Layout) -> String {
        if !layout.size {
            return String::new();
        }
        let size = bytes.map(decimal_size).unwrap_or_default();
        format!("  {}", pad_left(&size, SIZE))
    }

    fn time_text(elapsed: Duration) -> String {
        let seconds = elapsed.as_secs();
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }

    // =====================================================
    // The line of a finished piece
    // =====================================================

    fn outcome_tone(outcome: &Outcome) -> Tone {
        match outcome {
            Outcome::Files(_) | Outcome::Changes { .. } => Tone::Ok,
            Outcome::Refused(_) | Outcome::Unfinished(_) => Tone::Bad,
            Outcome::Skipped(_) | Outcome::NoRoom { .. } => Tone::Warn,
        }
    }

    fn result_text(outcome: &Outcome) -> String {
        match outcome {
            Outcome::Files(1) => "1 file".to_string(),
            Outcome::Files(count) => format!("{count} files"),
            Outcome::Changes {
                created,
                updated,
                deleted,
            } => {
                let parts: Vec<String> = [
                    (created, "created"),
                    (updated, "updated"),
                    (deleted, "deleted"),
                ]
                .into_iter()
                .filter(|(count, _)| **count > 0)
                .map(|(count, word)| format!("{count} {word}"))
                .collect();
                if parts.is_empty() {
                    "no changes".to_string()
                } else {
                    parts.join(", ")
                }
            }
            Outcome::Refused(_) => "refused".to_string(),
            Outcome::Skipped(_) | Outcome::NoRoom { .. } => "skipped".to_string(),
            Outcome::Unfinished(_) => "unfinished".to_string(),
        }
    }

    /// The paragraphs under a piece that did not finish cleanly, each with
    /// the command in it that is shown in bold.
    fn reasons(&self, ready: &Ready) -> Vec<(String, Option<String>)> {
        match &ready.outcome {
            Outcome::Refused(reasons) => reasons
                .iter()
                .map(|reason| self.reason_text(&ready.name, reason))
                .collect(),
            Outcome::Skipped(message) | Outcome::Unfinished(message) => {
                vec![(sanitize(message), None)]
            }
            Outcome::NoRoom { needed, available } => vec![(
                format!(
                    "{} needed, {} free",
                    decimal_size(*needed),
                    decimal_size(*available)
                ),
                None,
            )],
            Outcome::Files(_) | Outcome::Changes { .. } => Vec::new(),
        }
    }

    /// A named piece that carries a pin is told how to release it; every
    /// other reason is shown as the library words it.
    fn reason_text(&self, name: &str, reason: &str) -> (String, Option<String>) {
        if reason == format!("pinned: {name}/{PIN_FILE}") {
            let command = sanitize(&pasteable_line(
                self.config.directory.as_deref(),
                &["unpin".to_string(), name.to_string()],
            ));
            let text = format!("pinned, so it stays as it is. {command} releases it");
            return (text, Some(command));
        }
        (sanitize(reason), None)
    }

    /// The one line of a finished piece and, under it, the reasons it did
    /// not finish cleanly, wrapped to the width and never cut.
    pub(crate) fn finished_lines(&self, ready: &Ready, cols: usize) -> Vec<String> {
        let layout = Layout::of(cols);
        let tone = Self::outcome_tone(&ready.outcome);
        let (cell, cell_width) = self.name_cell(&ready.name, ready.label.as_deref(), layout.name);
        let result = Self::result_text(&ready.outcome);
        let result = format!(
            "{}{}",
            " ".repeat(RESULT.saturating_sub(result.width())),
            self.paint(
                if tone == Tone::Ok {
                    Look::new()
                } else {
                    Self::tone_look(tone)
                },
                &result
            )
        );
        let time = ready
            .elapsed
            .map(|elapsed| self.paint(theme::DIM, &Self::time_text(elapsed)))
            .unwrap_or_default();
        let line = format!(
            "  {} {}{}{}  {}  {}",
            self.tone_symbol(tone),
            cell,
            " ".repeat(layout.name.saturating_sub(cell_width)),
            self.size_cell(ready.bytes, &layout),
            result,
            time
        );
        let mut lines = vec![line.trim_end().to_string()];
        let room = cols.min(MAX_WIDTH).saturating_sub(4).max(10);
        for (reason, command) in self.reasons(ready) {
            for wrapped in wrap(&reason, room) {
                let shown = match &command {
                    Some(command) if wrapped.contains(command.as_str()) => {
                        wrapped.replacen(command, &self.paint(theme::BOLD, command), 1)
                    }
                    _ => wrapped,
                };
                lines.push(format!("    {shown}"));
            }
        }
        lines
    }

    // =====================================================
    // The summary
    // =====================================================

    /// The last line of a run.
    pub(crate) fn summary_line(&self, summary: &Summary) -> String {
        let separator = format!(" {} ", self.symbols().separator);
        let parts: Vec<String> = summary
            .parts
            .iter()
            .map(|(text, tone)| self.paint(Self::tone_look(*tone), text))
            .collect();
        format!(
            "{} {}   {}",
            self.tone_symbol(summary.tone),
            parts.join(&separator),
            self.paint(
                theme::DIM,
                &format!("took {}", Self::time_text(summary.took))
            )
        )
    }

    // =====================================================
    // The live region
    // =====================================================

    fn operation_word(operation: Operation) -> &'static str {
        match operation {
            Operation::Packing => "packing",
            Operation::Verifying => "verifying",
            Operation::Storing => "storing",
            Operation::Removing => "removing",
            Operation::Extracting => "extracting",
        }
    }

    /// How the count of an operation reads.
    fn count(&self, operation: Operation) -> Count {
        self.config
            .counts
            .iter()
            .find(|(listed, _)| *listed == operation)
            .map_or(FILES, |(_, count)| *count)
    }

    fn frame(now: Duration) -> usize {
        usize::try_from(now.as_millis() / FRAME_MILLIS).unwrap_or(0)
    }

    /// The two lines of a running piece.
    ///
    /// The detail of the second line goes first when it does not fit in
    /// `cols`: the bar and the percentage are what the line is for.
    fn card(&self, item: &Item, now: Duration, layout: &Layout, cols: usize) -> [String; 2] {
        let symbols = self.symbols();
        let spinner = symbols.spinner[Self::frame(now) % symbols.spinner.len()];
        let (cell, cell_width) = self.name_cell(&item.name, item.label.as_deref(), layout.name);
        let current = match &item.phase {
            Phase::Running(Some((operation, counter))) => Some((*operation, counter)),
            _ => None,
        };
        let word = current.map_or("", |(operation, _)| Self::operation_word(operation));
        let first = format!(
            "  {} {}{}{}  {}",
            self.paint(theme::CYAN, spinner),
            cell,
            " ".repeat(layout.name.saturating_sub(cell_width)),
            self.size_cell(Some(item.bytes), layout),
            self.paint(theme::CYAN, word)
        );

        let second = match current {
            Some((operation, counter)) => self.count_line(operation, counter, now, layout, cols),
            None => format!("    {}", self.moving_bar(now, layout.bar)),
        };
        [first.trim_end().to_string(), second]
    }

    /// The second line of a card whose operation has a counter.
    fn count_line(
        &self,
        operation: Operation,
        counter: &Counter,
        now: Duration,
        layout: &Layout,
        cols: usize,
    ) -> String {
        let count = self.count(operation);
        let (done, total) = (counter.done(), counter.total());
        let proportional = total > 0 && (count.percent || done <= total);
        let mut line;
        let text;
        if proportional {
            let done = done.min(total);
            // A card whose counter is at its total is still working: the
            // operation after it, or the finished line, replaces it. So the
            // bar and the percentage stop one step short of full.
            line = format!("    {}", self.bar(done, total, layout.bar));
            if count.percent {
                let percent =
                    ((u128::from(done) * 100 + u128::from(total) / 2) / u128::from(total)).min(99);
                line.push_str(&format!(
                    "  {}",
                    self.paint(theme::BOLD, &format!("{percent:>3}%"))
                ));
            }
            text = (count.text)(done, Some(total));
        } else {
            line = format!("    {}", self.moving_bar(now, layout.bar));
            text = (count.text)(done, None);
        }
        if layout.detail && !text.is_empty() {
            let detail = format!("  {}", self.paint(theme::DIM, &text));
            if visible_width(&line) + visible_width(&detail) <= cols {
                line.push_str(&detail);
            }
        }
        line
    }

    /// A bar `width` cells wide that is `done` of `total` full, never
    /// quite: its last cell stays unlit.
    fn bar(&self, done: u64, total: u64, width: usize) -> String {
        let symbols = self.symbols();
        let fill = usize::try_from(u128::from(done) * width as u128 / u128::from(total))
            .unwrap_or(width)
            .min(width - 1);
        let lit = format!("{}{}", symbols.bar_fill.repeat(fill), symbols.bar_head);
        let rest = symbols.bar_rest.repeat(width - fill - 1);
        format!(
            "{}{}",
            self.paint(theme::CYAN, &lit),
            self.paint(theme::DIM, &rest)
        )
    }

    /// The bar of an operation whose total is not known: a short lit
    /// stretch that runs along it and wraps around.
    fn moving_bar(&self, now: Duration, width: usize) -> String {
        let symbols = self.symbols();
        let start = Self::frame(now) % width;
        let mut lit = String::new();
        let mut rest = String::new();
        let mut line = String::new();
        let flush = |cards: &Cards, lit: &mut String, rest: &mut String, line: &mut String| {
            line.push_str(&cards.paint(theme::CYAN, lit));
            line.push_str(&cards.paint(theme::DIM, rest));
            lit.clear();
            rest.clear();
        };
        let mut was_lit = false;
        for cell in 0..width {
            let is_lit = (cell + width - start) % width < HEAD;
            if is_lit != was_lit {
                flush(self, &mut lit, &mut rest, &mut line);
            }
            if is_lit {
                lit.push_str(symbols.bar_fill);
            } else {
                rest.push_str(symbols.bar_rest);
            }
            was_lit = is_lit;
        }
        flush(self, &mut lit, &mut rest, &mut line);
        line
    }

    /// The lines the screen shows while the run goes on: the cards of the
    /// pieces that are working, then one line for the others. The region
    /// never has more than `rows - 1` lines, so the line above it stays on
    /// the screen, and no line is wider than `cols`, so each is one row and
    /// the redraw moves up by exactly the number of lines.
    pub(crate) fn region(&self, now: Duration, cols: usize, rows: usize) -> Vec<String> {
        let layout = Layout::of(cols);
        let mut running: Vec<&Item> = self
            .items
            .iter()
            .filter(|item| matches!(item.phase, Phase::Running(_)))
            .collect();
        running.sort_by_key(|item| item.order);
        let waiting = self
            .items
            .iter()
            .filter(|item| matches!(item.phase, Phase::Waiting | Phase::Paused))
            .count();

        let cap = rows.saturating_sub(1);
        let all_fit = running.len() * 2 + usize::from(waiting > 0) <= cap;
        let shown = if all_fit {
            running.len()
        } else {
            cap.saturating_sub(1) / 2
        };
        let hidden = running.len() - shown;

        let mut lines = Vec::new();
        for item in &running[..shown] {
            lines.extend(self.card(item, now, &layout, cols));
        }
        let mut counts = Vec::new();
        if waiting > 0 {
            counts.push(format!("{waiting} waiting"));
        }
        if hidden > 0 {
            counts.push(format!("{hidden} more running"));
        }
        if !counts.is_empty() {
            lines.push(format!(
                "  {} {}",
                self.paint(theme::DIM, self.symbols().waiting),
                self.paint(theme::DIM, &counts.join(", "))
            ));
        }
        lines.iter().map(|line| cut_to_width(line, cols)).collect()
    }
}

#[cfg(test)]
mod tests;
