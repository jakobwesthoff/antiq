// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The interactive fuzzy picker of `find` (ADR 0021, ADR 0057).
//!
//! antiq renders its own picker over the `nucleo` matcher and `crossterm`
//! rather than a picker library, so the selection bar adapts to any terminal
//! theme (it uses reverse video instead of a hardcoded background) and the
//! whole UI stays under our control. It draws on the controlling terminal,
//! keeping stdout free for piped data. The public surface is the
//! single generic [`pick_with_keys`] function; everything else is private, so swapping
//! the engine never touches call sites.
//!
//! All interaction logic lives in [`state::PickerState`] and is unit tested
//! without a TTY. This module is the thin glue that maps `crossterm`
//! key events onto that state and draws it. Each frame is buffered and reaches
//! the terminal as a single write bracketed by a synchronized update (DEC
//! mode 2026), erases per line rather than clearing the screen, and keeps the
//! cursor hidden until it is parked at the prompt, so redraws neither flicker
//! nor leave cursor trails.

mod layout;
mod state;
mod tree;

use std::io::{self, Write};

use anyhow::{Context, Result};
use crossterm::style::Attribute;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue, style, terminal,
};

use unicode_width::UnicodeWidthStr;

pub use layout::align_pieces;
use state::{ChildLine, ListLine, Marker, Page, PickerState, VisibleRow};
pub use state::{Contents, Entries, Members};
pub use tree::Member;

/// One picker row split by role: searched, displayed, and display-only.
///
/// Fuzzy scoring runs over `search` (the full content, which may extend past
/// what the width-sized columns can show); match-highlighting runs over
/// `display`; `detail` is never matched and appears only in the stats line
/// while its row is selected, so a long identifier stays reachable without
/// taking room on every row or polluting the query or the highlight.
pub struct Row {
    /// The displayed, highlightable text.
    pub display: String,
    /// Display-only text about the row, e.g. a piece's ID, shown in the stats
    /// line while the row is selected.
    pub detail: String,
    /// The full content scored against the query, untruncated and unpadded.
    /// It must not depend on the row width: the picker scores it once and
    /// keeps that ranking when the terminal width changes.
    pub search: String,
    /// The display column the row's size column ends at, for the tree lines
    /// of an expanded piece to line their sizes up under it.
    pub size_end: usize,
}

/// A key that acts on the selected item like Enter does, but tells the caller
/// to act on it differently, e.g. Ctrl-K in `find` pins the piece instead of
/// unearthing it. The stats line lists each key with its `hint`.
pub struct AcceptKey<A> {
    /// The letter pressed together with Ctrl.
    pub ctrl: char,
    /// A word or two for the stats line, e.g. `path`.
    pub hint: &'static str,
    /// What the caller receives when the key acts on the item.
    pub action: A,
    /// Whether the key ends the picker. A key that does not end it hands the
    /// item and the action to the `on_stay` callback and keeps the picker
    /// open, showing the callback's answer as a notice.
    pub closes: bool,
}

/// Makes Enter act like a stay [`AcceptKey`]: it hands the selected item and
/// `action` to the `on_stay` callback and the picker stays open. `find --print`
/// uses it to queue a path per Enter.
pub struct EnterStays<A> {
    /// A word for the stats line, e.g. `print`.
    pub hint: &'static str,
    /// What the caller receives when Enter acts on the item.
    pub action: A,
}

/// The Ctrl keys the picker itself handles. An [`AcceptKey`] on one of them
/// is ignored, so a caller cannot take away aborting or moving the selection.
const BUILT_IN_CTRL_KEYS: [char; 7] = ['c', 'd', 'n', 'o', 'p', 'u', 'w'];

/// The accept key a key press matches, if any.
fn accept_key<A>(
    keys: &[AcceptKey<A>],
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<&AcceptKey<A>> {
    let KeyCode::Char(c) = code else {
        return None;
    };
    if !modifiers.contains(KeyModifiers::CONTROL) || BUILT_IN_CTRL_KEYS.contains(&c) {
        return None;
    }
    keys.iter().find(|k| k.ctrl == c)
}

/// The stats line's hint for Enter, if it stays open, and the accept keys,
/// e.g. `⏎ print  ^Y path`, or empty when there are none.
fn key_hints<A>(contents: bool, enter: Option<&EnterStays<A>>, keys: &[AcceptKey<A>]) -> String {
    contents
        .then(|| "^O contents".to_string())
        .into_iter()
        .chain(enter.map(|e| format!("⏎ {}", e.hint)))
        .chain(
            keys.iter()
                .map(|k| format!("^{} {}", k.ctrl.to_ascii_uppercase(), k.hint)),
        )
        .collect::<Vec<_>>()
        .join("  ")
}

/// Present `items` in the interactive picker and return the chosen one.
///
/// `render_all` turns the whole item set into its [`Row`]s in one pass for a
/// given row width in display columns, which lets the renderer align columns
/// across every candidate and size them to the terminal (see [`layout`]). It
/// runs at startup and again whenever the terminal width changes.
///
/// Returns the chosen item together with the action of the accept key that
/// chose it, or `None` for the action when Enter did. Returns `Ok(None)` when
/// there are no items or the user aborts (Esc / Ctrl-C) without selecting.
///
/// With `contents`, a query also matches the member paths of the items, which
/// the picker asks for lazily (see [`PickerState`]).
///
/// An accept key that does not close the picker calls `on_stay` with the
/// selected item and its action; the returned text replaces the stats line
/// until the next key. With `enter_stays`, Enter does the same instead of
/// choosing the item.
pub fn pick_with_keys<'c, T, A: Copy>(
    items: Vec<T>,
    render_all: impl Fn(&[T], usize) -> Vec<Row>,
    enter_stays: Option<&EnterStays<A>>,
    keys: &[AcceptKey<A>],
    contents: Option<Box<dyn Contents<T> + 'c>>,
    mut on_stay: impl FnMut(&T, A) -> String,
) -> Result<Option<(T, Option<A>)>> {
    // Nothing to pick: never touch the terminal so non-interactive callers and
    // empty result sets stay side-effect free.
    if items.is_empty() {
        return Ok(None);
    }

    // The picker draws on the controlling terminal, never on stdout, so stdout
    // stays a pure data channel: `find --print | pbcopy` shows the picker and
    // pipes only the selected path.
    let mut tty =
        crate::cli::tty::open().context("while opening the controlling terminal for the picker")?;
    let mut teardown_tty = tty
        .try_clone()
        .context("while cloning the terminal handle")?;
    terminal::enable_raw_mode().context("while enabling raw mode")?;
    // Arm raw-mode teardown the instant raw mode is on, before any further
    // fallible setup. If entering the alternate screen below fails, this guard
    // still restores the terminal, so the user's shell never stays in raw mode.
    let _raw_guard = TerminalGuard::new(|| {
        let _ = terminal::disable_raw_mode();
    });
    execute!(tty, terminal::EnterAlternateScreen).context("while entering the alternate screen")?;
    // Armed only after the alternate screen is actually entered, so it leaves
    // exactly what was entered. It drops before the raw guard (reverse arming
    // order), leaving the alternate screen before raw mode is disabled.
    // Ending a synchronized update that was never begun is a no-op, so the
    // guard can always close one; without this, an error between the frame's
    // begin/end markers would leave the terminal waiting out its sync timeout.
    let _alt_guard = TerminalGuard::new(move || {
        let _ = execute!(
            teardown_tty,
            terminal::EndSynchronizedUpdate,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
    });

    // The tty file is unbuffered, so without this every `queue!` would be its
    // own syscall — hundreds per frame — and the terminal would render the
    // partially painted states in between, cursor hops included. Buffering
    // delivers each frame as one write; `draw` flushes once per frame.
    let mut frame = io::BufWriter::with_capacity(FRAME_BUFFER_BYTES, tty);

    run_loop(
        &mut frame,
        items,
        render_all,
        enter_stays,
        keys,
        contents,
        &mut on_stay,
    )
}

/// The frame buffer size: comfortably a full redraw of a large terminal, so a
/// frame never flushes partially.
const FRAME_BUFFER_BYTES: usize = 64 * 1024;

/// Restores one piece of terminal state when dropped, on every exit path
/// (`?`, normal return, panic). One guard is armed per setup step so a failure
/// between steps still tears down everything already set up.
///
/// The teardown action is injected rather than hardcoded so the arming-order
/// guarantee is unit-testable without a real terminal.
struct TerminalGuard<F: FnMut()> {
    teardown: F,
}

impl<F: FnMut()> TerminalGuard<F> {
    fn new(teardown: F) -> Self {
        Self { teardown }
    }
}

impl<F: FnMut()> Drop for TerminalGuard<F> {
    fn drop(&mut self) {
        (self.teardown)();
    }
}

/// The read-draw-react loop. Returns the selection with the accept key that
/// chose it (`None` for Enter), or `None` on abort.
fn run_loop<'c, T, A: Copy>(
    tty: &mut impl Write,
    items: Vec<T>,
    render_all: impl Fn(&[T], usize) -> Vec<Row>,
    enter_stays: Option<&EnterStays<A>>,
    keys: &[AcceptKey<A>],
    contents: Option<Box<dyn Contents<T> + 'c>>,
    on_stay: &mut impl FnMut(&T, A) -> String,
) -> Result<Option<(T, Option<A>)>> {
    let (mut cols, rows) = terminal::size().context("while querying the terminal size")?;
    let mut built_row_width = row_width(cols);
    let picker_rows = render_all(&items, built_row_width);
    let has_contents = contents.is_some();
    let mut state = PickerState::new(items, picker_rows, list_height(rows));
    if let Some(contents) = contents {
        state = state.with_contents(contents);
    }
    let hints = key_hints(has_contents, enter_stays, keys);

    // The first frame paints a fresh alternate screen and every later frame
    // erases per line, so a full clear is only needed again after a resize.
    let mut clear_all = true;
    loop {
        draw(tty, &state, cols, clear_all, &hints).context("while drawing the picker")?;
        clear_all = false;

        match event::read().context("while reading a key event")? {
            Event::Key(key) => match react(
                Screen {
                    tty: &mut *tty,
                    cols,
                    hints: &hints,
                },
                &mut state,
                enter_stays,
                keys,
                key,
                on_stay,
            )? {
                Step::Continue => {}
                // With nothing matching there is nothing to return, so the
                // picker ends without a selection, as on abort.
                Step::Accept(action) => {
                    return Ok(state.into_selected().map(|item| (item, action)));
                }
                Step::Abort => return Ok(None),
            },
            Event::Resize(new_cols, new_rows) => {
                // The column layout depends only on the width, so a height-only
                // resize skips the rebuild. On Unix, crossterm reports at most
                // one resize per read however many arrived in between, so a
                // window drag rebuilds once per frame at most.
                if needs_rebuild(built_row_width, new_cols) {
                    built_row_width = row_width(new_cols);
                    let picker_rows = render_all(state.items(), built_row_width);
                    state.set_rows(picker_rows);
                }
                cols = new_cols;
                state.set_height(list_height(new_rows));
                clear_all = true;
            }
            _ => {}
        }
    }
}

/// What a key press means for the picker's loop.
#[derive(Debug, PartialEq, Eq)]
enum Flow<A> {
    /// Keep reading keys; the state may have changed.
    Continue,
    /// End with the selected item, chosen by Enter (`None`) or by the accept
    /// key with this action.
    Accept(Option<A>),
    /// Act on the selected item without ending the picker.
    Stay(A),
    /// End without a selection.
    Abort,
}

/// Where and how a frame is drawn.
struct Screen<'a, W: Write> {
    tty: &'a mut W,
    cols: u16,
    hints: &'a str,
}

/// Apply `key` like [`step`], after drawing the notice that contents are being
/// read when the key starts the first scan of them.
fn react<T, A: Copy>(
    screen: Screen<'_, impl Write>,
    state: &mut PickerState<T>,
    enter_stays: Option<&EnterStays<A>>,
    keys: &[AcceptKey<A>],
    key: KeyEvent,
    on_stay: &mut impl FnMut(&T, A) -> String,
) -> Result<Step<A>> {
    // The first scan reads every listing that is not matched by its row, which
    // takes long on a large collection. The frame goes out before the scan so
    // the wait has an explanation.
    if key.kind == KeyEventKind::Press && edits_query(&key) && state.reading_pending() {
        state.set_notice("reading contents...".to_string());
        draw(screen.tty, state, screen.cols, false, screen.hints)
            .context("while drawing the picker")?;
    }
    Ok(step(state, enter_stays, keys, key, on_stay))
}

/// Whether `key` changes the query: typing, Backspace and Ctrl-W.
fn edits_query(key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Backspace => true,
        KeyCode::Char('w') => ctrl,
        KeyCode::Char(_) => !ctrl && !key.modifiers.contains(KeyModifiers::ALT),
        _ => false,
    }
}

/// What the loop does after a key press.
#[derive(Debug, PartialEq, Eq)]
enum Step<A> {
    /// Draw again and read the next event.
    Continue,
    /// End with the selected item, chosen by Enter (`None`) or by the accept
    /// key with this action.
    Accept(Option<A>),
    /// End without a selection.
    Abort,
}

/// Handle one key press of the loop: apply it to the state, run `on_stay` for
/// an accept key (or Enter, when `enter_stays` is set) that keeps the picker open, and keep the notice only until
/// the next press.
///
/// Releases and repeats leave the notice alone: a terminal that reports them
/// sends one with every press, and the notice would vanish the moment it
/// appeared. With nothing selected a stay key does nothing.
fn step<T, A: Copy>(
    state: &mut PickerState<T>,
    enter_stays: Option<&EnterStays<A>>,
    keys: &[AcceptKey<A>],
    key: KeyEvent,
    on_stay: &mut impl FnMut(&T, A) -> String,
) -> Step<A> {
    if key.kind == KeyEventKind::Press {
        state.clear_notice();
    }
    let flow = match enter_stays {
        Some(enter) if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter => {
            Flow::Stay(enter.action)
        }
        _ => handle_key(state, keys, key),
    };
    match flow {
        Flow::Continue => Step::Continue,
        Flow::Accept(action) => Step::Accept(action),
        Flow::Abort => Step::Abort,
        Flow::Stay(action) => {
            if let Some(item) = state.selected() {
                let notice = on_stay(item, action);
                state.set_notice(notice);
            }
            Step::Continue
        }
    }
}

/// Apply one key event to the picker state and say whether the picker goes on.
///
/// Only presses count. Where crossterm also reports repeats and releases
/// (Windows, or Unix with keyboard enhancement enabled), acting on them would
/// apply one key press more than once.
fn handle_key<T, A: Copy>(
    state: &mut PickerState<T>,
    keys: &[AcceptKey<A>],
    key: KeyEvent,
) -> Flow<A> {
    if key.kind != KeyEventKind::Press {
        return Flow::Continue;
    }
    if let Some(accept) = accept_key(keys, key.code, key.modifiers) {
        return if accept.closes {
            Flow::Accept(Some(accept.action))
        } else {
            Flow::Stay(accept.action)
        };
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => return Flow::Accept(None),
        KeyCode::Esc => return Flow::Abort,
        KeyCode::Char('c') if ctrl => return Flow::Abort,
        // The list is bottom-anchored with the best match at the bottom, so Up
        // moves toward worse matches (up the screen) and Down toward the best
        // (down toward the prompt). The page keys follow the same directions,
        // Ctrl-U and Ctrl-D as in vim.
        KeyCode::Up => state.select_worse(),
        KeyCode::Down => state.select_better(),
        KeyCode::Char('p') if ctrl => state.select_worse(),
        KeyCode::Char('n') if ctrl => state.select_better(),
        KeyCode::Char('u') if ctrl => state.page_worse(Page::Half),
        KeyCode::Char('d') if ctrl => state.page_better(Page::Half),
        KeyCode::PageUp => state.page_worse(Page::Full),
        KeyCode::PageDown => state.page_better(Page::Full),
        KeyCode::Char('o') if ctrl => state.toggle_contents(),
        KeyCode::Char('w') if ctrl => state.delete_word(),
        KeyCode::Backspace => state.backspace(),
        // Plain typing (no Ctrl/Alt) edits the query. TODO: no intra-query
        // cursor movement (Left/Right) in v1.
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            state.push_char(c)
        }
        _ => {}
    }
    Flow::Continue
}

/// The color of the divider lines that frame the prompt.
const DIVIDER_COLOR: style::Color = style::Color::Blue;

/// The prompt prefix; its width is also the indent the query text and the stats
/// line share, so the stats sit directly under the query.
const PROMPT_PREFIX: &str = "❯ ";

/// The selected-row marker. A left bar reads as a selection gutter and stays
/// distinct from the prompt's `❯`. Two columns wide, like the unselected `  `.
const SELECTION_POINTER: &str = "▌ ";

/// The columns the triangle in front of a row takes, with its space.
const MARKER_WIDTH: usize = 2;

/// The columns in front of a row's content: the selection pointer (or the
/// matching two-space gutter) and the triangle. The lines of a tree start
/// behind the same columns.
const GUTTER: usize = 2 + MARKER_WIDTH;

/// The display columns a row's content gets on a terminal `cols` wide: what is
/// left after the gutter.
fn row_width(cols: u16) -> usize {
    (cols as usize).saturating_sub(GUTTER)
}

/// Whether a terminal now `new_cols` wide needs its rows laid out again, given
/// the row width they were built for.
fn needs_rebuild(built_row_width: usize, new_cols: u16) -> bool {
    row_width(new_cols) != built_row_width
}

/// The number of list rows that fit above the divider/prompt/divider/stats chrome.
fn list_height(terminal_rows: u16) -> usize {
    (terminal_rows as usize).saturating_sub(4).max(1)
}

/// Draw the whole picker, bottom-anchored: the list region (best match at the
/// bottom), a divider, the prompt, a second divider, then the dimmed `m/n` stats
/// with the accept-key `hints` at their right.
///
/// `clear_all` wipes the screen before painting; it is needed after a resize,
/// where rows the frame does not touch may hold reflowed leftovers.
fn draw<T>(
    tty: &mut impl Write,
    state: &PickerState<T>,
    cols: u16,
    clear_all: bool,
    hints: &str,
) -> Result<()> {
    // The frame is bracketed by a synchronized update (DEC mode 2026): a
    // supporting terminal applies everything in between as one atomic screen
    // swap, so a display refresh can never catch the frame half painted.
    // Others ignore the markers. The cursor is hidden while the frame paints
    // and shown again only once it is parked at the prompt, so the terminal
    // never sees it anywhere else — cursor-trail animations (kitty, ghostty)
    // would otherwise chase every intermediate `MoveTo`.
    queue!(tty, terminal::BeginSynchronizedUpdate, cursor::Hide)?;
    if clear_all {
        queue!(
            tty,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::All),
        )?;
    }

    // The list region is exactly `height` lines; blanks (`None`) at the top
    // keep the rows hugging the divider and prompt below them. Every row is
    // erased only from the end of its new content, never blanked up front: a
    // full-screen clear followed by a repaint flickers on terminals without
    // synchronized updates when a refresh lands between the two.
    let lines = state.list_lines();
    let list_rows = lines.len() as u16;
    for (i, line) in lines.iter().enumerate() {
        queue!(tty, cursor::MoveTo(0, i as u16))?;
        match line {
            ListLine::Blank => {}
            ListLine::Piece(row) => draw_row(tty, row, cols)?,
            ListLine::Child(child) => draw_child(tty, child, cols, state.size_end())?,
            ListLine::Cut(text) => draw_cut(tty, text, cols)?,
        }
        queue!(tty, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }

    // Divider directly above the prompt (full width, nothing left to erase).
    queue!(tty, cursor::MoveTo(0, list_rows))?;
    draw_divider(tty, cols)?;

    // Prompt, framed by a second divider below it. The clear erases what a
    // longer previous query left behind.
    let prompt_row = list_rows + 1;
    let (prompt, prompt_col) = prompt_line(state.query(), cols);
    queue!(
        tty,
        cursor::MoveTo(0, prompt_row),
        style::Print(prompt),
        terminal::Clear(terminal::ClearType::UntilNewLine),
        cursor::MoveTo(0, prompt_row + 1),
    )?;
    draw_divider(tty, cols)?;

    // Dimmed stats under the second divider, aligned under the query text.
    let (matching, total) = state.counter();
    let rank = state.selected_rank();
    let contents = state.contents_stat();
    let detail = stats_detail(contents.as_deref(), state.selected_detail());
    queue!(
        tty,
        cursor::MoveTo(0, prompt_row + 2),
        style::SetAttribute(Attribute::Dim),
        style::Print(stats_line(
            cols as usize,
            rank,
            matching,
            total,
            detail.as_deref(),
            state.notice(),
            hints,
        )),
        style::SetAttribute(Attribute::Reset),
        terminal::Clear(terminal::ClearType::UntilNewLine),
    )?;

    // Park the cursor at the end of the query so typing reads naturally, and
    // only now make it visible again.
    queue!(
        tty,
        cursor::MoveTo(prompt_col, prompt_row),
        cursor::Show,
        terminal::EndSynchronizedUpdate,
    )?;

    tty.flush().context("while flushing the picker frame")?;
    Ok(())
}

/// The prompt line for a terminal `cols` wide, and the column the cursor parks
/// at after the query.
///
/// A query too wide for the terminal shows its end, where typing happens, as
/// shells do. One column stays free after the visible query so the cursor sits
/// behind its last character, inside the terminal. The line never exceeds
/// `cols`, so it cannot wrap onto the divider below and break the frame.
fn prompt_line(query: &str, cols: u16) -> (String, u16) {
    let cols = cols as usize;
    let prefix = PROMPT_PREFIX.width();
    let shown = layout::fit_suffix(query, cols.saturating_sub(prefix + 1));
    // On a terminal narrower than the prefix even the prefix is cut.
    let line = layout::fit_prefix(&format!("{PROMPT_PREFIX}{shown}"), cols).to_string();
    let cursor = (prefix + shown.width()).min(cols.saturating_sub(1));
    (line, cursor as u16)
}

/// Draw a full-width colored divider line.
fn draw_divider(tty: &mut impl Write, cols: u16) -> Result<()> {
    queue!(
        tty,
        style::SetForegroundColor(DIVIDER_COLOR),
        style::Print(divider_line(cols as usize, '─')),
        style::ResetColor,
    )?;
    Ok(())
}

/// A run of `fill` exactly `width` columns wide.
fn divider_line(width: usize, fill: char) -> String {
    std::iter::repeat_n(fill, width).collect()
}

/// The text after the counter in the stats line: what the expanded piece's
/// contents come to, then the selected row's detail.
fn stats_detail(contents: Option<&str>, detail: Option<&str>) -> Option<String> {
    let detail = detail.filter(|detail| !detail.is_empty());
    match (contents, detail) {
        (Some(contents), Some(detail)) => Some(format!("{contents} · {detail}")),
        (Some(text), None) | (None, Some(text)) => Some(text.to_string()),
        (None, None) => None,
    }
}

/// The smallest gap between the stats and the right-aligned key hints.
const HINTS_GAP: usize = 2;

/// The dimmed stats string, indented to sit directly under the query text (past
/// the prompt prefix). Shows the cursor's rank within the matches plus the total
/// candidate count, or an empty-state hint, followed by the selected row's
/// detail (the piece's ID) when it has one. `rank` is the 1-based position of
/// the selection among the matches, or `None` when nothing matches. A `notice`
/// replaces the counter and the detail.
///
/// The accept-key `hints` sit right-aligned at the end of the line and win
/// over the detail and the notice: those are clipped with an ellipsis to leave
/// the hints and [`HINTS_GAP`] columns between them. Only when the hints do not
/// fit next to the counter either do they go, and the line is clipped to
/// `width`.
fn stats_line(
    width: usize,
    rank: Option<usize>,
    matching: usize,
    total: usize,
    detail: Option<&str>,
    notice: Option<&str>,
    hints: &str,
) -> String {
    let indent = " ".repeat(PROMPT_PREFIX.width());
    // `fixed` stays whole while the hints show; `flexible` gives way to them.
    let (fixed, flexible) = match notice {
        Some(notice) => (indent, notice.to_string()),
        None => {
            let counter = match rank {
                None => format!("no matches · {total} total"),
                Some(rank) => format!("{rank}/{matching} · {total} total"),
            };
            let detail = detail
                .filter(|d| !d.is_empty())
                .map(|d| format!(" · {d}"))
                .unwrap_or_default();
            (format!("{indent}{counter}"), detail)
        }
    };

    let hints_room = hints.width() + HINTS_GAP;
    if !hints.is_empty() && fixed.width() + hints_room <= width {
        let flexible = layout::truncate(&flexible, width - fixed.width() - hints_room);
        let used = fixed.width() + flexible.width() + hints.width();
        return format!("{fixed}{flexible}{}{hints}", " ".repeat(width - used));
    }
    layout::truncate(&format!("{fixed}{flexible}"), width)
}

/// Draw a single list row. The selected row is drawn in cyan with a `▌ ` bar;
/// matched characters are yellow on either row. All colors are the terminal's
/// own ANSI palette, so the picker adapts to its theme.
fn draw_row(tty: &mut impl Write, row: &VisibleRow<'_>, cols: u16) -> Result<()> {
    let width = cols as usize;
    let selected = row.selected;
    let pointer = if selected { SELECTION_POINTER } else { "  " };

    // The selected row's body is cyan; the pointer shares that accent.
    if selected {
        queue!(tty, style::SetForegroundColor(style::Color::Cyan))?;
    }
    queue!(tty, style::Print(pointer))?;
    let marker = match row.marker {
        Marker::None => ' ',
        Marker::Closed => '▸',
        Marker::Open => '▾',
    };
    queue!(tty, style::Print(marker), style::Print(' '))?;

    // Clip to the terminal width (in display columns) so a long row never wraps
    // and breaks the layout. The `▌ ` bar is two display columns; a wide
    // character is dropped whole rather than allowed to straddle the right
    // edge. The clip measures like the layout's padding does (see
    // `layout::fit_prefix`), so a row laid out to fit is drawn whole. The
    // visible part is a character prefix, so the highlight indices still
    // address its characters.
    let shown = layout::fit_prefix(row.display, width.saturating_sub(GUTTER));
    let highlights = row.highlights;
    for (i, c) in shown.chars().enumerate() {
        let matched = highlights.binary_search(&(i as u32)).is_ok();
        if matched {
            queue!(tty, style::SetForegroundColor(style::Color::Yellow))?;
        }
        queue!(tty, style::Print(c))?;
        if matched {
            // Restore the row's base color after a highlighted character.
            if selected {
                queue!(tty, style::SetForegroundColor(style::Color::Cyan))?;
            } else {
                queue!(tty, style::ResetColor)?;
            }
        }
    }

    // Reset styling so it never bleeds into the next line or a blank area.
    queue!(
        tty,
        style::SetAttribute(Attribute::Reset),
        style::ResetColor
    )?;
    Ok(())
}

/// Draw one line of the tree under the expanded piece: the name, bold for a
/// directory with the matched characters yellow, and the size dimmed at the
/// right.
fn draw_child(
    tty: &mut impl Write,
    child: &ChildLine<'_>,
    cols: u16,
    size_end: usize,
) -> Result<()> {
    let cells = layout::child_cells(child, cols as usize, size_end, GUTTER);
    queue!(tty, style::Print(" ".repeat(cells.indent)))?;
    if child.dir {
        queue!(tty, style::SetAttribute(Attribute::Bold))?;
    }
    for (i, c) in cells.text.chars().enumerate() {
        let matched = child.highlights.binary_search(&(i as u32)).is_ok();
        if matched {
            queue!(tty, style::SetForegroundColor(style::Color::Yellow))?;
        }
        queue!(tty, style::Print(c))?;
        if matched {
            queue!(tty, style::ResetColor)?;
        }
    }
    queue!(tty, style::SetAttribute(Attribute::Reset))?;
    if !cells.size.is_empty() {
        queue!(
            tty,
            style::Print(" ".repeat(cells.gap)),
            style::SetAttribute(Attribute::Dim),
            style::Print(&cells.size),
            style::SetAttribute(Attribute::Reset),
        )?;
    }
    Ok(())
}

/// Draw the dimmed line that says how many entries the tree leaves out,
/// indented like the deepest lines of a shallow tree.
fn draw_cut(tty: &mut impl Write, text: &str, cols: u16) -> Result<()> {
    let indent = GUTTER + 2;
    let shown = layout::fit_prefix(text, (cols as usize).saturating_sub(indent));
    queue!(
        tty,
        style::Print(" ".repeat(indent)),
        style::SetAttribute(Attribute::Dim),
        style::Print(shown),
        style::SetAttribute(Attribute::Reset),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn guards_tear_down_in_reverse_arming_order() {
        let log = Rc::new(RefCell::new(Vec::new()));
        {
            let raw_log = Rc::clone(&log);
            let _raw = TerminalGuard::new(move || raw_log.borrow_mut().push("raw"));
            let alt_log = Rc::clone(&log);
            let _alt = TerminalGuard::new(move || alt_log.borrow_mut().push("alt"));
        }
        // The alternate screen is left before raw mode is disabled.
        assert_eq!(*log.borrow(), vec!["alt", "raw"]);
    }

    #[test]
    fn raw_guard_restores_when_the_alt_screen_step_is_skipped() {
        // Models `EnterAlternateScreen` failing: the alt guard is never armed,
        // yet raw mode must still be restored.
        let log = Rc::new(RefCell::new(Vec::new()));
        {
            let raw_log = Rc::clone(&log);
            let _raw = TerminalGuard::new(move || raw_log.borrow_mut().push("raw"));
        }
        assert_eq!(*log.borrow(), vec!["raw"]);
    }

    #[test]
    fn divider_fills_the_full_width_with_the_glyph() {
        let line = divider_line(30, '─');
        assert_eq!(line.chars().count(), 30);
        assert!(line.chars().all(|c| c == '─'));
    }

    #[test]
    fn divider_of_zero_width_is_empty() {
        assert_eq!(divider_line(0, '─'), "");
    }

    #[test]
    fn stats_align_under_the_query_text() {
        let line = stats_line(40, Some(3), 12, 40, None, None, "");
        // Indented past the prompt prefix so it sits under the query text.
        assert_eq!(line, "  3/12 · 40 total");
        assert_eq!(line.trim_start(), "3/12 · 40 total");
    }

    #[test]
    fn stats_show_an_empty_state_when_nothing_matches() {
        assert_eq!(
            stats_line(40, None, 0, 40, None, None, ""),
            "  no matches · 40 total"
        );
    }

    #[test]
    fn stats_end_with_the_selected_rows_detail() {
        assert_eq!(
            stats_line(
                80,
                Some(3),
                12,
                40,
                Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                None,
                ""
            ),
            "  3/12 · 40 total · 01ARZ3NDEKTSV4RRFFQ69G5FAV"
        );
    }

    #[test]
    fn an_empty_detail_adds_no_separator() {
        assert_eq!(
            stats_line(80, Some(1), 1, 1, Some(""), None, ""),
            "  1/1 · 1 total"
        );
    }

    /// The counts and ULID part used by the key-hint tests: 46 columns.
    const LEFT: &str = "  3/12 · 40 total · 01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn stats_with_hint(width: usize) -> String {
        stats_line(
            width,
            Some(3),
            12,
            40,
            Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            None,
            "^Y path",
        )
    }

    #[test]
    fn key_hints_are_right_aligned() {
        let line = stats_with_hint(60);
        assert!(line.starts_with(LEFT));
        assert!(line.ends_with("^Y path"));
        assert_eq!(line.width(), 60);
    }

    #[test]
    fn key_hints_keep_a_two_column_gap_and_clip_the_detail_for_it() {
        // 46 + 2 + 7 = 55 fits whole. One column less clips the detail by one
        // column, marking the cut.
        assert!(stats_with_hint(55).ends_with(&format!("{LEFT}  ^Y path")));
        let clipped = stats_with_hint(54);
        assert!(clipped.starts_with("  3/12 · 40 total · 01ARZ3NDEKTSV4RRFFQ69G5F…"));
        assert!(clipped.ends_with("  ^Y path"));
        assert_eq!(clipped.width(), 54);
    }

    #[test]
    fn the_hints_win_over_the_detail_at_80_columns() {
        let hints = "^F fork  ^K pin  ^X unpin";
        let line = stats_line(
            80,
            Some(12345),
            12345,
            12345,
            Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            None,
            hints,
        );
        assert_eq!(
            line,
            "  12345/12345 · 12345 total · 01ARZ3NDEKTSV4RRFFQ69G…  ^F fork  ^K pin  ^X unpin"
        );
        assert_eq!(line.width(), 80);
    }

    #[test]
    fn a_notice_replaces_the_counter_and_the_detail() {
        let line = stats_line(
            40,
            Some(3),
            12,
            40,
            Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            Some("pinned sources/old-app"),
            "",
        );
        assert_eq!(line, "  pinned sources/old-app");
    }

    #[test]
    fn a_long_notice_is_clipped_for_the_hints() {
        let line = stats_line(
            30,
            Some(1),
            1,
            1,
            None,
            Some("error: the collection is locked by another run"),
            "^K pin",
        );
        assert_eq!(line, "  error: the collecti…  ^K pin");
        assert_eq!(line.width(), 30);
    }

    #[test]
    fn a_narrow_terminal_cuts_the_line_when_even_the_hints_do_not_fit() {
        let line = stats_line(
            20,
            Some(3),
            12,
            40,
            Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            None,
            "^Y path",
        );
        assert_eq!(line, "  3/12 · 40 total ·…");
    }

    // -------------------------------------------------------------------------
    // Accept keys
    // -------------------------------------------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Action {
        First,
        Second,
    }

    const KEYS: [AcceptKey<Action>; 2] = [
        AcceptKey {
            ctrl: 'y',
            hint: "path",
            action: Action::First,
            closes: true,
        },
        AcceptKey {
            ctrl: 't',
            hint: "other",
            action: Action::Second,
            closes: true,
        },
    ];

    #[test]
    fn a_configured_ctrl_key_yields_its_action() {
        let ctrl = KeyModifiers::CONTROL;
        let action = |c| accept_key(&KEYS, KeyCode::Char(c), ctrl).map(|k| k.action);
        assert_eq!(action('y'), Some(Action::First));
        assert_eq!(action('t'), Some(Action::Second));
    }

    #[test]
    fn the_letter_without_ctrl_is_no_accept_key() {
        let none = KeyModifiers::NONE;
        assert!(accept_key(&KEYS, KeyCode::Char('y'), none).is_none());
    }

    #[test]
    fn an_unconfigured_ctrl_key_is_no_accept_key() {
        let ctrl = KeyModifiers::CONTROL;
        assert!(accept_key(&KEYS, KeyCode::Char('z'), ctrl).is_none());
        assert!(accept_key::<Action>(&[], KeyCode::Char('y'), ctrl).is_none());
    }

    #[test]
    fn an_accept_key_cannot_shadow_a_built_in_key() {
        let shadowing = [AcceptKey {
            ctrl: 'c',
            hint: "nope",
            action: Action::First,
            closes: true,
        }];
        let ctrl = KeyModifiers::CONTROL;
        assert!(accept_key(&shadowing, KeyCode::Char('c'), ctrl).is_none());
    }

    #[test]
    fn an_accept_key_cannot_shadow_ctrl_d() {
        let shadowing = [AcceptKey {
            ctrl: 'd',
            hint: "nope",
            action: Action::First,
            closes: true,
        }];
        let ctrl = KeyModifiers::CONTROL;
        assert!(accept_key(&shadowing, KeyCode::Char('d'), ctrl).is_none());
    }

    #[test]
    fn key_hints_name_each_key_and_its_purpose() {
        assert_eq!(key_hints(false, None, &KEYS), "^Y path  ^T other");
        assert_eq!(key_hints::<Action>(false, None, &[]), "");
    }

    #[test]
    fn the_pointer_and_the_gutter_have_the_same_width() {
        assert_eq!(SELECTION_POINTER.width(), 2);
        assert_eq!(GUTTER, SELECTION_POINTER.width() + MARKER_WIDTH);
    }

    #[test]
    fn row_width_saturates_on_a_terminal_narrower_than_the_pointer() {
        assert_eq!(row_width(0), 0);
        assert_eq!(row_width(1), 0);
        assert_eq!(row_width(4), 0);
    }

    #[test]
    fn rows_are_rebuilt_only_when_the_row_width_changes() {
        assert!(!needs_rebuild(row_width(80), 80));
        assert!(needs_rebuild(row_width(80), 81));
        assert!(needs_rebuild(row_width(80), 79));
    }

    #[test]
    fn terminals_too_narrow_for_any_row_share_one_layout() {
        // Every width up to the pointer leaves a zero-width row.
        assert!(!needs_rebuild(row_width(0), 2));
    }

    // -------------------------------------------------------------------------
    // Prompt line
    // -------------------------------------------------------------------------

    const LONG_QUERY: &str = "abcdefghijklmnopqrstuvwxyz0123";

    #[test]
    fn a_long_query_shows_its_end() {
        let (line, _) = prompt_line(LONG_QUERY, 20);
        assert!(line.width() <= 20, "{line:?} is {} columns", line.width());
        assert!(line.ends_with(&LONG_QUERY[13..]), "{line:?}");
    }

    #[test]
    fn the_cursor_stays_inside_the_terminal() {
        let (_, cursor) = prompt_line(LONG_QUERY, 20);
        assert_eq!(cursor, 19);
    }

    #[test]
    fn a_wide_character_is_not_split_at_the_cut() {
        // Five columns are left for the query after the prefix and the
        // cursor's column; `本語` fills four, `語本語` would need six.
        assert_eq!(prompt_line("日本語日本語", 8), ("❯ 本語".to_string(), 6));
    }

    #[test]
    fn a_terminal_narrower_than_the_prefix_keeps_the_cursor_on_screen() {
        let (line, cursor) = prompt_line("abc", 1);
        assert!(line.width() <= 1, "{line:?} is {} columns", line.width());
        assert_eq!(cursor, 0);
    }

    #[test]
    fn a_zero_width_terminal_parks_the_cursor_at_zero() {
        assert_eq!(prompt_line("abc", 0).1, 0);
    }

    #[test]
    fn a_short_query_is_shown_whole() {
        assert_eq!(prompt_line("alpha", 40), ("❯ alpha".to_string(), 7));
    }

    #[test]
    fn stats_degrade_to_a_truncation_when_too_narrow() {
        let line = stats_line(4, Some(1), 100, 200, None, None, "^Y path");
        assert_eq!(line.chars().count(), 4);
    }

    // -------------------------------------------------------------------------
    // Key handling
    // -------------------------------------------------------------------------

    /// A picker over three rows that all contain an `a`, so a typed `a` keeps
    /// every row, with a list `height` rows tall.
    fn picker(height: usize) -> PickerState<'static, String> {
        let items: Vec<String> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rows = items
            .iter()
            .map(|s| Row {
                display: s.clone(),
                detail: String::new(),
                search: s.clone(),
                size_end: 0,
            })
            .collect();
        PickerState::new(items, rows, height)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Feed `events` to the picker one after the other, expecting it to go on.
    fn feed(state: &mut PickerState<String>, events: &[KeyEvent]) {
        for &event in events {
            assert_eq!(handle_key::<_, Action>(state, &KEYS, event), Flow::Continue);
        }
    }

    #[test]
    fn enter_accepts_without_an_action() {
        let mut s = picker(10);
        assert_eq!(
            handle_key(&mut s, &KEYS, press(KeyCode::Enter)),
            Flow::Accept(None)
        );
    }

    #[test]
    fn an_accept_key_accepts_with_its_action() {
        let mut s = picker(10);
        assert_eq!(
            handle_key(&mut s, &KEYS, ctrl('y')),
            Flow::Accept(Some(Action::First))
        );
    }

    #[test]
    fn esc_and_ctrl_c_abort() {
        let mut s = picker(10);
        assert_eq!(handle_key(&mut s, &KEYS, press(KeyCode::Esc)), Flow::Abort);
        assert_eq!(handle_key(&mut s, &KEYS, ctrl('c')), Flow::Abort);
    }

    #[test]
    fn up_and_ctrl_p_move_toward_worse_matches() {
        let mut s = picker(10);
        feed(&mut s, &[press(KeyCode::Up)]);
        assert_eq!(s.selected_rank(), Some(2));
        feed(&mut s, &[ctrl('p')]);
        assert_eq!(s.selected_rank(), Some(3));
    }

    #[test]
    fn down_and_ctrl_n_move_toward_the_best() {
        let mut s = picker(10);
        feed(&mut s, &[press(KeyCode::Up), press(KeyCode::Up)]);
        feed(&mut s, &[press(KeyCode::Down)]);
        assert_eq!(s.selected_rank(), Some(2));
        feed(&mut s, &[ctrl('n')]);
        assert_eq!(s.selected_rank(), Some(1));
    }

    #[test]
    fn typing_edits_the_query() {
        let mut s = picker(10);
        feed(
            &mut s,
            &[press(KeyCode::Char('a')), press(KeyCode::Char('B'))],
        );
        assert_eq!(s.query(), "aB");
    }

    #[test]
    fn alt_letters_and_unbound_ctrl_letters_do_nothing() {
        let mut s = picker(10);
        feed(
            &mut s,
            &[
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
                ctrl('x'),
            ],
        );
        assert_eq!(s.query(), "");
        assert_eq!(s.selected_rank(), Some(1));
    }

    #[test]
    fn backspace_deletes_a_character_and_ctrl_w_a_word() {
        let mut s = picker(10);
        for c in "al ph".chars() {
            feed(&mut s, &[press(KeyCode::Char(c))]);
        }
        feed(&mut s, &[press(KeyCode::Backspace)]);
        assert_eq!(s.query(), "al p");
        feed(&mut s, &[ctrl('w')]);
        assert_eq!(s.query(), "al ");
    }

    // A list two rows tall pages one row per half page and two per full page,
    // so a swapped page size or direction lands on a different rank.

    #[test]
    fn ctrl_u_pages_half_toward_worse_matches_and_keeps_the_query() {
        let mut s = picker(2);
        feed(&mut s, &[press(KeyCode::Char('a')), ctrl('u')]);
        assert_eq!(s.query(), "a");
        assert_eq!(s.selected_rank(), Some(2));
    }

    #[test]
    fn page_up_pages_a_full_page_toward_worse_matches() {
        let mut s = picker(2);
        feed(&mut s, &[press(KeyCode::PageUp)]);
        assert_eq!(s.selected_rank(), Some(3));
    }

    #[test]
    fn ctrl_d_pages_half_toward_the_best() {
        let mut s = picker(2);
        feed(&mut s, &[press(KeyCode::Up), press(KeyCode::Up), ctrl('d')]);
        assert_eq!(s.selected_rank(), Some(2));
    }

    #[test]
    fn page_down_pages_a_full_page_toward_the_best() {
        let mut s = picker(2);
        feed(
            &mut s,
            &[
                press(KeyCode::Up),
                press(KeyCode::Up),
                press(KeyCode::PageDown),
            ],
        );
        assert_eq!(s.selected_rank(), Some(1));
    }

    #[test]
    fn a_key_release_is_ignored() {
        let mut s = picker(10);
        let release =
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(handle_key(&mut s, &KEYS, release), Flow::Continue);
    }

    // -------------------------------------------------------------------------
    // Keys that keep the picker open
    // -------------------------------------------------------------------------

    const STAY_KEYS: [AcceptKey<Action>; 2] = [
        AcceptKey {
            ctrl: 'y',
            hint: "stay",
            action: Action::First,
            closes: false,
        },
        AcceptKey {
            ctrl: 't',
            hint: "close",
            action: Action::Second,
            closes: true,
        },
    ];

    #[test]
    fn a_key_that_does_not_close_yields_stay() {
        let mut s = picker(10);
        assert_eq!(
            handle_key(&mut s, &STAY_KEYS, ctrl('y')),
            Flow::Stay(Action::First)
        );
    }

    #[test]
    fn a_key_that_closes_still_accepts() {
        let mut s = picker(10);
        assert_eq!(
            handle_key(&mut s, &STAY_KEYS, ctrl('t')),
            Flow::Accept(Some(Action::Second))
        );
    }

    #[test]
    fn a_stay_key_cannot_shadow_a_built_in_key() {
        let shadowing = [AcceptKey {
            ctrl: 'n',
            hint: "nope",
            action: Action::First,
            closes: false,
        }];
        let mut s = picker(10);
        feed_with(&mut s, &shadowing, &[ctrl('n')]);
        assert_eq!(s.notice(), None);
    }

    /// Feed `events` through [`step`] with an `on_stay` that names the item.
    fn feed_with(state: &mut PickerState<String>, keys: &[AcceptKey<Action>], events: &[KeyEvent]) {
        let mut on_stay = |item: &String, action: Action| format!("{action:?} {item}");
        for &event in events {
            assert_eq!(step(state, None, keys, event, &mut on_stay), Step::Continue);
        }
    }

    #[test]
    fn a_stay_key_calls_back_with_the_selected_item_and_shows_the_notice() {
        let mut s = picker(10);
        feed_with(&mut s, &STAY_KEYS, &[press(KeyCode::Up), ctrl('y')]);
        assert_eq!(s.notice(), Some("First beta"));
    }

    #[test]
    fn a_stay_key_keeps_the_query_and_the_selection() {
        let mut s = picker(10);
        feed_with(
            &mut s,
            &STAY_KEYS,
            &[press(KeyCode::Char('a')), press(KeyCode::Up), ctrl('y')],
        );
        assert_eq!(s.query(), "a");
        assert_eq!(s.selected_rank(), Some(2));
    }

    #[test]
    fn the_next_key_press_clears_the_notice() {
        let mut s = picker(10);
        feed_with(&mut s, &STAY_KEYS, &[ctrl('y'), press(KeyCode::Up)]);
        assert_eq!(s.notice(), None);
    }

    #[test]
    fn a_key_release_keeps_the_notice() {
        let mut s = picker(10);
        feed_with(&mut s, &STAY_KEYS, &[ctrl('y')]);
        let release =
            KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Release);
        feed_with(&mut s, &STAY_KEYS, &[release]);
        assert_eq!(s.notice(), Some("First alpha"));
    }

    #[test]
    fn a_stay_key_with_no_match_does_not_call_back() {
        let mut s = picker(10);
        let mut on_stay = |_: &String, _: Action| -> String { unreachable!("nothing is selected") };
        for c in "zzzz".chars() {
            step(
                &mut s,
                None,
                &STAY_KEYS,
                press(KeyCode::Char(c)),
                &mut on_stay,
            );
        }
        assert_eq!(
            step(&mut s, None, &STAY_KEYS, ctrl('y'), &mut on_stay),
            Step::Continue
        );
        assert_eq!(s.notice(), None);
    }

    #[test]
    fn step_passes_accepting_and_aborting_keys_on() {
        let mut s = picker(10);
        let mut on_stay = |_: &String, _: Action| String::new();
        assert_eq!(
            step(
                &mut s,
                None,
                &STAY_KEYS,
                press(KeyCode::Enter),
                &mut on_stay
            ),
            Step::Accept(None)
        );
        assert_eq!(
            step(&mut s, None, &STAY_KEYS, ctrl('t'), &mut on_stay),
            Step::Accept(Some(Action::Second))
        );
        assert_eq!(
            step(&mut s, None, &STAY_KEYS, press(KeyCode::Esc), &mut on_stay),
            Step::Abort
        );
    }

    const ENTER_PRINTS: EnterStays<Action> = EnterStays {
        hint: "print",
        action: Action::Second,
    };

    #[test]
    fn enter_that_stays_calls_back_and_keeps_the_picker_open() {
        let mut s = picker(10);
        let mut on_stay = |item: &String, action: Action| format!("{action:?} {item}");
        let enter = press(KeyCode::Enter);
        let stepped = step(&mut s, Some(&ENTER_PRINTS), &STAY_KEYS, enter, &mut on_stay);
        assert_eq!(stepped, Step::Continue);
        assert_eq!(s.notice(), Some("Second alpha"));
    }

    #[test]
    fn enter_that_stays_does_nothing_without_a_match() {
        let mut s = picker(10);
        let mut on_stay = |_: &String, _: Action| -> String { unreachable!("nothing is selected") };
        for c in "zzzz".chars() {
            step(
                &mut s,
                None,
                &STAY_KEYS,
                press(KeyCode::Char(c)),
                &mut on_stay,
            );
        }
        let enter = press(KeyCode::Enter);
        let stepped = step(&mut s, Some(&ENTER_PRINTS), &STAY_KEYS, enter, &mut on_stay);
        assert_eq!(stepped, Step::Continue);
    }

    #[test]
    fn the_hint_of_an_enter_that_stays_comes_first() {
        assert_eq!(
            key_hints(false, Some(&ENTER_PRINTS), &KEYS),
            "⏎ print  ^Y path  ^T other"
        );
        assert_eq!(key_hints(false, Some(&ENTER_PRINTS), &[]), "⏎ print");
    }

    // -------------------------------------------------------------------------
    // The notice before the first scan of contents
    // -------------------------------------------------------------------------

    /// A frame buffer the test can read while the state holds the contents.
    #[derive(Clone, Default)]
    struct Frames(Rc<RefCell<Vec<u8>>>);

    impl Write for Frames {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Frames {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.borrow()).into_owned()
        }
    }

    /// Gives the same one member to every piece and records what the frames
    /// held when it was asked.
    struct Recording {
        frames: Frames,
        seen: Rc<RefCell<Vec<String>>>,
    }

    impl Contents<String> for Recording {
        fn has(&self, _: &String) -> bool {
            false
        }

        fn entries(&mut self, _: &String) -> Result<Option<Entries>, String> {
            unimplemented!("no test expands")
        }

        fn members(&mut self, item: &String) -> Option<Members> {
            self.seen.borrow_mut().push(self.frames.text());
            Some(Members {
                prefix: format!("{item}/"),
                paths: vec!["file".to_string()],
            })
        }
    }

    fn react_to(
        state: &mut PickerState<String>,
        frames: &mut Frames,
        key: KeyEvent,
    ) -> Step<Action> {
        let mut on_stay = |_: &String, _: Action| -> String { unreachable!("no stay key") };
        react(
            Screen {
                tty: frames,
                cols: 80,
                hints: "",
            },
            state,
            None,
            &KEYS,
            key,
            &mut on_stay,
        )
        .expect("drawing succeeds")
    }

    /// A picker whose contents were asked `seen` times so far.
    fn recording_picker() -> (
        PickerState<'static, String>,
        Frames,
        Rc<RefCell<Vec<String>>>,
    ) {
        let frames = Frames::default();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let contents = Recording {
            frames: frames.clone(),
            seen: Rc::clone(&seen),
        };
        let state = picker(10).with_contents(Box::new(contents));
        (state, frames, seen)
    }

    #[test]
    fn the_notice_frame_is_drawn_before_the_first_scan() {
        let (mut s, mut frames, seen) = recording_picker();
        react_to(&mut s, &mut frames, press(KeyCode::Char('z')));
        let seen = seen.borrow();
        assert!(!seen.is_empty(), "the scan asked for members");
        assert!(
            seen[0].contains("reading contents..."),
            "frames before the scan: {:?}",
            seen[0]
        );
        assert_eq!(s.notice(), None, "the notice ends with the key");
    }

    #[test]
    fn a_key_that_does_not_edit_the_query_draws_no_notice() {
        let (mut s, mut frames, _) = recording_picker();
        react_to(&mut s, &mut frames, press(KeyCode::Down));
        assert_eq!(frames.text(), "");
    }

    #[test]
    fn the_notice_is_drawn_once() {
        let (mut s, mut frames, _) = recording_picker();
        react_to(&mut s, &mut frames, press(KeyCode::Char('z')));
        let drawn = frames.text().len();
        react_to(&mut s, &mut frames, press(KeyCode::Char('y')));
        assert_eq!(frames.text().len(), drawn);
    }

    #[test]
    fn typing_backspace_and_ctrl_w_edit_the_query() {
        assert!(edits_query(&press(KeyCode::Char('a'))));
        assert!(edits_query(&press(KeyCode::Backspace)));
        assert!(edits_query(&ctrl('w')));
    }

    #[test]
    fn other_keys_leave_the_query_alone() {
        assert!(!edits_query(&ctrl('n')));
        assert!(!edits_query(&press(KeyCode::Enter)));
        let alt = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert!(!edits_query(&alt));
    }

    #[test]
    fn a_picker_without_contents_draws_no_notice() {
        let mut s = picker(10);
        let mut frames = Frames::default();
        react_to(&mut s, &mut frames, press(KeyCode::Char('z')));
        assert_eq!(frames.text(), "");
    }

    // -------------------------------------------------------------------------
    // Contents under a piece
    // -------------------------------------------------------------------------

    /// Every item has one five-byte file.
    struct OneFile;

    impl Contents<String> for OneFile {
        fn has(&self, _: &String) -> bool {
            true
        }

        fn entries(&mut self, item: &String) -> Result<Option<Entries>, String> {
            Ok(Some(Entries {
                prefix: format!("{item}/"),
                members: vec![tree::Member {
                    path: "a.txt".to_string(),
                    dir: false,
                    size: 5,
                }],
            }))
        }

        fn members(&mut self, _: &String) -> Option<Members> {
            None
        }
    }

    /// A picker over `alpha`, `beta` and `gamma` whose rows end their size
    /// column at 30.
    fn with_files(height: usize) -> PickerState<'static, String> {
        let mut state = picker(height).with_contents(Box::new(OneFile));
        let rows = ["alpha", "beta", "gamma"]
            .iter()
            .map(|name| Row {
                display: name.to_string(),
                detail: String::new(),
                search: name.to_string(),
                size_end: 30,
            })
            .collect();
        state.set_rows(rows);
        state
    }

    fn tree_lines(state: &PickerState<String>) -> usize {
        state
            .list_lines()
            .iter()
            .filter(|line| matches!(line, ListLine::Child(_)))
            .count()
    }

    #[test]
    fn ctrl_o_expands_the_selected_piece_and_again_collapses_it() {
        let mut s = with_files(10);
        feed(&mut s, &[ctrl('o')]);
        assert_eq!(tree_lines(&s), 1);
        feed(&mut s, &[ctrl('o')]);
        assert_eq!(tree_lines(&s), 0);
    }

    #[test]
    fn an_accept_key_cannot_take_ctrl_o() {
        let taking = [AcceptKey {
            ctrl: 'o',
            hint: "nope",
            action: Action::First,
            closes: true,
        }];
        let mut s = with_files(10);
        assert_eq!(handle_key(&mut s, &taking, ctrl('o')), Flow::Continue);
        assert_eq!(tree_lines(&s), 1);
    }

    #[test]
    fn the_hint_for_the_contents_comes_before_the_hints_of_the_caller() {
        assert_eq!(
            key_hints(true, Some(&ENTER_PRINTS), &KEYS),
            "^O contents  ⏎ print  ^Y path  ^T other"
        );
        assert_eq!(key_hints::<Action>(true, None, &[]), "^O contents");
    }

    #[test]
    fn the_hints_of_find_with_print_take_47_columns() {
        let hints = "^O contents  ⏎ print  ^F fork  ^K pin  ^X unpin";
        assert_eq!(hints.width(), 47);
    }

    #[test]
    fn the_stats_of_the_contents_come_before_the_detail() {
        assert_eq!(
            stats_detail(Some("4 701 in contents"), Some("01ARZ")).as_deref(),
            Some("4 701 in contents · 01ARZ")
        );
        assert_eq!(
            stats_detail(Some("4 701 in contents"), None).as_deref(),
            Some("4 701 in contents")
        );
        assert_eq!(
            stats_detail(Some("4 701 in contents"), Some("")).as_deref(),
            Some("4 701 in contents")
        );
        assert_eq!(stats_detail(None, Some("01ARZ")).as_deref(), Some("01ARZ"));
        assert_eq!(stats_detail(None, None), None);
    }

    #[test]
    fn the_hints_win_over_the_counts_of_the_contents_at_80_columns() {
        let hints = "^O contents  ⏎ print  ^F fork  ^K pin  ^X unpin";
        let line = stats_line(
            80,
            Some(1),
            9,
            9_999_999,
            Some("1 234 567 in contents · 01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            None,
            hints,
        );
        assert!(line.ends_with(hints), "{line:?}");
        assert!(line.starts_with("  1/9 · 9999999 total"), "{line:?}");
        assert_eq!(line.width(), 80);
    }

    #[test]
    fn the_rows_leave_room_for_the_pointer_and_the_triangle() {
        assert_eq!(row_width(80), 76);
    }

    /// What `draw` writes for the picker at `cols` columns, as one string.
    fn drawn(state: &PickerState<String>, cols: u16) -> String {
        let mut frames = Frames::default();
        draw(&mut frames, state, cols, false, "").expect("drawing succeeds");
        frames.text()
    }

    #[test]
    fn the_frame_shows_the_triangle_and_the_tree_with_its_size() {
        let mut s = with_files(10);
        let closed = drawn(&s, 80);
        assert!(closed.contains("▸ alpha"), "{closed:?}");
        feed(&mut s, &[ctrl('o')]);
        let open = drawn(&s, 80);
        assert!(open.contains("▾ alpha"), "{open:?}");
        assert!(open.contains("a.txt"), "{open:?}");
        assert!(open.contains("5 B"), "{open:?}");
    }

    #[test]
    fn the_frame_shows_the_cut_line_dimmed() {
        struct Many;
        impl Contents<String> for Many {
            fn has(&self, _: &String) -> bool {
                true
            }
            fn entries(&mut self, item: &String) -> Result<Option<Entries>, String> {
                Ok(Some(Entries {
                    prefix: format!("{item}/"),
                    members: (0..30)
                        .map(|n| tree::Member {
                            path: format!("f{n}"),
                            dir: false,
                            size: 1,
                        })
                        .collect(),
                }))
            }
            fn members(&mut self, _: &String) -> Option<Members> {
                None
            }
        }
        let mut s = picker(10).with_contents(Box::new(Many));
        feed(&mut s, &[ctrl('o')]);
        let frame = drawn(&s, 80);
        assert!(frame.contains("… 26 more members"), "{frame:?}");
    }

    // -------------------------------------------------------------------------
    // The expanded tree on the screen
    // -------------------------------------------------------------------------

    use antiq::id::ObjectId;
    use antiq::ops::find::{FoundPiece, FoundState};
    use antiq::relpath::{Name, RelPath};
    use antiq::tracking::PieceState;
    use std::path::PathBuf;

    fn found(number: u64, path: &str, state: PieceState, size: Option<u64>) -> FoundPiece {
        let rel = RelPath::parse(path).expect("a valid path");
        let name = Name::from_bytes(path.as_bytes().to_vec()).expect("a valid name");
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_000 + number, 1));
        FoundPiece {
            id,
            abs_path: format!("/work/{path}"),
            folder: PathBuf::from(format!("/work/{path}")),
            short_id: id.to_string()[..10].to_string(),
            path: rel,
            name,
            state: FoundState::Piece(state),
            created: "2026-09-03T08:00:00Z".parse().expect("a time"),
            size,
        }
    }

    /// The contents of the third piece: a source directory with a scanner
    /// below it and 4 700 scans.
    struct Scans;

    impl Scans {
        const PIECE: &'static str = "2026_09_03_data-extraction";

        fn members() -> Vec<tree::Member> {
            let dir = |path: &str| tree::Member {
                path: path.to_string(),
                dir: true,
                size: 0,
            };
            let file = |path: String, size: u64| tree::Member {
                path,
                dir: false,
                size,
            };
            let mut members = vec![
                dir("src"),
                dir("src/scanner"),
                file("src/scanner/lib.rs".to_string(), 61 * 1024),
                file("src/util.rs".to_string(), 27 * 1024),
                dir("data"),
                dir("data/raw"),
            ];
            members.extend((1..=4700).map(|n| {
                let size = if n <= 2 { 48 * 1024 * 1024 } else { 1024 };
                file(format!("data/raw/scan-{n:04}.tif"), size)
            }));
            members
        }
    }

    impl Contents<FoundPiece> for Scans {
        fn has(&self, piece: &FoundPiece) -> bool {
            piece.path.to_string() == Self::PIECE
        }

        fn entries(&mut self, _: &FoundPiece) -> Result<Option<Entries>, String> {
            Ok(Some(Entries {
                prefix: format!("{}/", Self::PIECE),
                members: Self::members(),
            }))
        }

        fn members(&mut self, piece: &FoundPiece) -> Option<Members> {
            self.has(piece).then(|| Members {
                prefix: format!("{}/", Self::PIECE),
                paths: Self::members().into_iter().map(|m| m.path).collect(),
            })
        }
    }

    /// The picker over three pieces with the query `scan`, the third piece
    /// selected and expanded, as the screen shows it at `cols` columns:
    /// every list line, the prompt and the stats line, without colors.
    fn expanded_tree_at(cols: u16) -> String {
        let pieces = vec![
            found(1, "2026_07_01_scan-tools", PieceState::Live, Some(3 * 1024)),
            found(
                2,
                "2026_08_02_scanner-old",
                PieceState::RemoteOnly,
                Some(5 * 1024),
            ),
            found(
                3,
                Scans::PIECE,
                PieceState::Archived,
                Some(2 * 1024 * 1024 * 1024),
            ),
        ];
        let rows = align_pieces(&pieces, row_width(cols));
        let mut state = PickerState::new(pieces, rows, 12).with_contents(Box::new(Scans));
        type_query(&mut state, "scan");
        state.select_worse();
        state.select_worse();
        state.toggle_contents();

        let mut frame = String::new();
        for line in state.list_lines() {
            let text = match line {
                ListLine::Blank => continue,
                ListLine::Piece(row) => {
                    let mark = match row.marker {
                        Marker::None => ' ',
                        Marker::Closed => '▸',
                        Marker::Open => '▾',
                    };
                    let bar = if row.selected { '▌' } else { ' ' };
                    let shown = layout::fit_prefix(row.display, row_width(cols));
                    format!("{bar} {mark} {shown}")
                }
                ListLine::Child(child) => {
                    let cells = layout::child_cells(&child, cols.into(), state.size_end(), GUTTER);
                    format!(
                        "{}{}{}{}",
                        " ".repeat(cells.indent),
                        cells.text,
                        " ".repeat(cells.gap),
                        cells.size
                    )
                }
                ListLine::Cut(text) => format!("{}{text}", " ".repeat(GUTTER + 2)),
            };
            frame.push_str(text.trim_end());
            frame.push('\n');
        }
        frame.push_str(&format!("❯ {}\n", state.query()));
        let contents = state.contents_stat();
        let detail = stats_detail(contents.as_deref(), None);
        let (matching, total) = state.counter();
        frame.push_str(
            stats_line(
                cols.into(),
                state.selected_rank(),
                matching,
                total,
                detail.as_deref(),
                None,
                "^O contents  ^F fork  ^K pin  ^X unpin",
            )
            .trim_end(),
        );
        frame.push('\n');
        frame
    }

    fn type_query(state: &mut PickerState<'_, FoundPiece>, query: &str) {
        for c in query.chars() {
            state.push_char(c);
        }
    }

    #[test]
    fn the_expanded_tree_at_80_columns() {
        insta::assert_snapshot!(expanded_tree_at(80));
    }

    #[test]
    fn the_expanded_tree_at_120_columns() {
        insta::assert_snapshot!(expanded_tree_at(120));
    }
}
