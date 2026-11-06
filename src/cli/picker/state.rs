// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The picker's pure interaction state, free of any terminal I/O.
//!
//! Everything that decides *what* the picker shows lives here: fuzzy filtering
//! and ranking, the query string, the selection cursor, and the scroll
//! viewport. The terminal loop in the parent module only reads this state to
//! draw and feeds key events back into it, so the entire behaviour is unit
//! testable without a TTY. Snapshot tests drive [`PickerState`]
//! directly via [`PickerState::debug_render`].

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str, Utf32String};

use super::Row;

/// The two pre-converted nucleo haystacks for one row, in their distinct
/// coordinate spaces.
///
/// Scoring and filtering run over `search` (the full content); the highlight
/// pass re-runs the query over `display` so the positions it returns are already
/// char indices into the drawn text. Keeping both pre-converted avoids
/// reallocating a [`Utf32String`] on every keystroke.
struct Haystacks {
    /// Full content; what a query is scored and filtered against.
    search: Utf32String,
    /// The displayed text; what the highlight pass matches against.
    display: Utf32String,
}

/// One ranked result: the item it points at and the highlight positions within
/// that item's displayed text.
struct Scored {
    /// Index into [`PickerState::display`] / `items`.
    item: usize,
    /// Char positions in the displayed text to highlight, ascending.
    highlights: Vec<u32>,
}

/// What the picker knows about the members of one item.
enum Slot {
    /// Not asked for yet, or asked for and not retained.
    Unloaded,
    /// The item has no members on this machine, or they cannot be read.
    Absent,
    /// Retained; the picker does not ask again.
    Loaded(Members),
}

/// What the last pass found out about one item, for a query that extends it.
enum Scan {
    /// Nothing a narrowing can rely on.
    Unknown,
    /// The row text matched, so no member was scored.
    Row,
    /// Neither the row nor any member matched; an extended query cannot
    /// match either.
    Miss,
    /// Only these members (indices into a retained [`Members`]) matched.
    Hits(Vec<u32>),
}

/// The members the picker keeps in memory at most, summed over all items.
const MEMBER_BUDGET: usize = 2_000_000;

/// The characters that give a nucleo atom a special meaning. A query holding
/// one may match differently from a prefix of itself, so it is always scored
/// in full.
const ATOM_SYNTAX: [char; 5] = ['!', '^', '\'', '$', '\\'];

/// Whether every match of `previous` is known to include the matches of
/// `query`, so that scoring the earlier hits is enough.
fn extends(previous: &str, query: &str) -> bool {
    query.starts_with(previous) && !previous.contains(ATOM_SYNTAX) && !query.contains(ATOM_SYNTAX)
}

/// A row currently inside the viewport, handed to the renderer (or a test).
pub struct VisibleRow<'a> {
    /// The displayed, highlightable text of the row.
    pub display: &'a str,
    /// Char positions in `display` to highlight, ascending.
    pub highlights: &'a [u32],
    /// Whether this row is the current selection.
    pub selected: bool,
}

/// How far a page move goes: half the viewport height or all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Half,
    Full,
}

/// The member paths of one piece, as the picker matches them.
pub struct Members {
    /// The text in front of each member: the piece's path and a slash. A
    /// query is scored against `prefix` and a path together, so it can name
    /// both.
    pub prefix: String,
    /// The members' paths in display form, in listing order.
    pub paths: Vec<String>,
}

/// Where the picker reads the member paths of its items from.
pub trait Contents<T> {
    /// The members of `item`, or `None` when it has none on this machine or
    /// they cannot be read. The picker asks at most once per item and
    /// retains the answer within its budget.
    fn members(&mut self, item: &T) -> Option<Members>;
}

/// The picker's interaction state over an owned set of items of type `T`.
pub struct PickerState<'c, T> {
    /// The candidate items, index-aligned with `display`/`detail`/`haystacks`.
    items: Vec<T>,
    /// The displayed, highlightable text per item.
    display: Vec<String>,
    /// The display-only detail per item, shown while it is selected and never
    /// matched.
    detail: Vec<String>,
    /// Pre-converted search and display haystacks per item.
    haystacks: Vec<Haystacks>,
    /// Reused fuzzy matcher; allocates a large scratch buffer, so it is kept.
    matcher: Matcher,
    /// The current query text.
    query: String,
    /// Current filtered, ranked results (best first).
    scored: Vec<Scored>,
    /// Selection cursor as an index into `scored`.
    selected: usize,
    /// Index into `scored` of the first visible row (scroll offset).
    offset: usize,
    /// Number of list rows the viewport can show (always at least 1).
    height: usize,
    /// A message that replaces the counter in the stats line until the next
    /// key press.
    notice: Option<String>,
    /// What the picker knows about the members of each item.
    slots: Vec<Slot>,
    /// How many members the `Loaded` slots hold together.
    retained: usize,
    /// The most members the picker retains.
    budget: usize,
    /// What the last pass found out about each item.
    scans: Vec<Scan>,
    /// The query the last pass scored, `None` after an empty one.
    scanned_query: Option<String>,
    /// Whether any item's members were asked for yet.
    read_any: bool,
    /// Members scored over the picker's life.
    #[cfg(test)]
    scored_members: usize,
    /// Where the member paths of a piece come from, when the caller has any.
    contents: Option<Box<dyn Contents<T> + 'c>>,
}

impl<'c, T> PickerState<'c, T> {
    /// Build the state from `items` and their pre-rendered `rows`.
    ///
    /// `rows` is index-aligned with `items` (one [`Row`] per item); the renderer
    /// produces them in one batch so it can align columns across every item.
    /// `height` is the number of list rows the viewport can show; it is clamped
    /// to at least one so movement and scrolling always have room.
    pub fn new(items: Vec<T>, rows: Vec<Row>, height: usize) -> Self {
        debug_assert_eq!(items.len(), rows.len(), "one row per item");
        let mut display: Vec<String> = Vec::with_capacity(rows.len());
        let mut detail: Vec<String> = Vec::with_capacity(rows.len());
        let mut haystacks: Vec<Haystacks> = Vec::with_capacity(rows.len());
        for row in rows {
            haystacks.push(Haystacks {
                search: Utf32String::from(row.search.as_str()),
                display: Utf32String::from(row.display.as_str()),
            });
            display.push(row.display);
            detail.push(row.detail);
        }
        let n = items.len();
        let mut state = Self {
            items,
            display,
            detail,
            haystacks,
            matcher: Matcher::new(Config::DEFAULT),
            query: String::new(),
            scored: Vec::new(),
            selected: 0,
            offset: 0,
            height: height.max(1),
            notice: None,
            slots: std::iter::repeat_with(|| Slot::Unloaded).take(n).collect(),
            retained: 0,
            budget: MEMBER_BUDGET,
            scans: std::iter::repeat_with(|| Scan::Unknown).take(n).collect(),
            scanned_query: None,
            read_any: false,
            #[cfg(test)]
            scored_members: 0,
            contents: None,
        };
        state.recompute();
        state
    }

    /// Recompute the ranked result set for the current query.
    ///
    /// An empty query keeps every item in its original (newest-first) order with
    /// no highlights. A non-empty query is matched in two passes per row: scoring
    /// runs over the full `search` content (so a match in truncated-away text
    /// still ranks the row), and a second pass over the `display` text yields the
    /// highlight positions in display coordinates. A row whose only match lives
    /// in hidden content therefore ranks but shows no highlight.
    ///
    /// An item whose row does not match is tried through its members, each
    /// scored as `prefix + path`. Row matches rank first, by score; member-only
    /// matches follow, by their best member's score and show no highlight.
    /// Ties keep the original index, so equal scores stay newest-first. The
    /// cursor and scroll reset to the top because the result set has changed
    /// underneath them.
    fn recompute(&mut self) {
        self.selected = 0;
        self.offset = 0;

        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.scored = (0..self.items.len())
                .map(|item| Scored {
                    item,
                    highlights: Vec::new(),
                })
                .collect();
            self.scans.iter_mut().for_each(|scan| *scan = Scan::Unknown);
            self.scanned_query = None;
            return;
        }

        // A query that extends the last one can only lose matches, so what
        // the last pass ruled out stays out.
        let narrowing = self
            .scanned_query
            .as_deref()
            .is_some_and(|previous| extends(previous, &query));

        let pattern = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
        let mut by_row: Vec<(u32, usize, Vec<u32>)> = Vec::new();
        let mut by_member: Vec<(u32, usize)> = Vec::new();
        let mut buf: Vec<u32> = Vec::new();
        let mut text = String::new();
        let mut chars: Vec<char> = Vec::new();
        for (item, haystack) in self.haystacks.iter().enumerate() {
            // The score over the full content decides whether the row matches and
            // how it ranks.
            if let Some(score) = pattern.score(haystack.search.slice(..), &mut self.matcher) {
                let marks = highlights(&pattern, &haystack.display, &mut self.matcher, &mut buf);
                by_row.push((score, item, marks));
                self.scans[item] = Scan::Row;
                continue;
            }

            let before = std::mem::replace(&mut self.scans[item], Scan::Unknown);
            let candidates = match before {
                Scan::Miss if narrowing => {
                    self.scans[item] = Scan::Miss;
                    continue;
                }
                Scan::Hits(hits) if narrowing => Some(hits),
                _ => None,
            };
            let Some(contents) = self.contents.as_deref_mut() else {
                continue;
            };

            // Members are read the first time a scan needs them. One that
            // does not fit the budget is scanned and let go, so a later scan
            // reads it again.
            let slot = &mut self.slots[item];
            let mut unretained = None;
            if matches!(slot, Slot::Unloaded) {
                self.read_any = true;
                match contents.members(&self.items[item]) {
                    None => *slot = Slot::Absent,
                    Some(members) if self.retained + members.paths.len() <= self.budget => {
                        self.retained += members.paths.len();
                        *slot = Slot::Loaded(members);
                    }
                    Some(members) => unretained = Some(members),
                }
            }
            let retained = matches!(slot, Slot::Loaded(_));
            let members = match slot {
                Slot::Loaded(members) => members,
                Slot::Absent => {
                    self.scans[item] = Scan::Miss;
                    continue;
                }
                Slot::Unloaded => unretained.as_ref().expect("an unretained read is kept"),
            };

            let mut best = None;
            let mut hits: Vec<u32> = Vec::new();
            let mut score_member = |index: usize| {
                text.clear();
                text.push_str(&members.prefix);
                text.push_str(&members.paths[index]);
                let haystack = Utf32Str::new(&text, &mut chars);
                if let Some(score) = pattern.score(haystack, &mut self.matcher) {
                    best = best.max(Some(score));
                    hits.push(index as u32);
                }
            };
            match &candidates {
                Some(candidates) => {
                    candidates
                        .iter()
                        .for_each(|&index| score_member(index as usize));
                    #[cfg(test)]
                    {
                        self.scored_members += candidates.len();
                    }
                }
                None => {
                    (0..members.paths.len()).for_each(&mut score_member);
                    #[cfg(test)]
                    {
                        self.scored_members += members.paths.len();
                    }
                }
            }

            self.scans[item] = match (best, retained) {
                (None, _) => Scan::Miss,
                (Some(_), true) => Scan::Hits(hits),
                (Some(_), false) => Scan::Unknown,
            };
            if let Some(score) = best {
                by_member.push((score, item));
            }
        }
        self.scanned_query = Some(query);

        // Best score first; equal scores keep their original newest-first order.
        by_row.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        by_member.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.scored = by_row
            .into_iter()
            .map(|(_, item, highlights)| Scored { item, highlights })
            .chain(by_member.into_iter().map(|(_, item)| Scored {
                item,
                highlights: Vec::new(),
            }))
            .collect();
    }

    /// Replace the displayed rows, e.g. after the terminal width changed and the
    /// renderer laid the columns out anew.
    ///
    /// Only the displayed text changes: `display`, `detail` and the display
    /// haystack. `Row.search` is ignored, so the renderer's search text must
    /// not depend on the width. The ranking, the selection and the scroll
    /// offset all stay; only the highlight positions are recomputed, since
    /// they index into the display text that just changed.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        debug_assert_eq!(self.items.len(), rows.len(), "one row per item");
        self.display.clear();
        self.detail.clear();
        for (haystack, row) in self.haystacks.iter_mut().zip(rows) {
            haystack.display = Utf32String::from(row.display.as_str());
            self.display.push(row.display);
            self.detail.push(row.detail);
        }

        let query = self.query.trim();
        if query.is_empty() {
            return;
        }
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut buf: Vec<u32> = Vec::new();
        for scored in &mut self.scored {
            scored.highlights = highlights(
                &pattern,
                &self.haystacks[scored.item].display,
                &mut self.matcher,
                &mut buf,
            );
        }
    }

    /// Let a query match the member paths `contents` gives, too.
    pub fn with_contents(mut self, contents: Box<dyn Contents<T> + 'c>) -> Self {
        self.contents = Some(contents);
        self
    }

    /// Whether the next query edit may have to read members for the first
    /// time, which can take a while.
    pub fn reading_pending(&self) -> bool {
        self.contents.is_some() && !self.read_any
    }

    /// The members scored so far.
    #[cfg(test)]
    pub fn members_scored(&self) -> usize {
        self.scored_members
    }

    /// Lower the number of members the picker retains.
    #[cfg(test)]
    pub fn set_member_budget(&mut self, budget: usize) {
        self.budget = budget;
    }

    /// The candidate items, in their original order.
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Set the viewport height (e.g. after a terminal resize), keeping the
    /// selection visible.
    ///
    /// A taller viewport could show matches above an offset chosen for the
    /// old height while blank lines pad its top, so the offset comes down to
    /// the last one that still fills the viewport.
    pub fn set_height(&mut self, height: usize) {
        self.height = height.max(1);
        self.offset = self
            .offset
            .min(self.scored.len().saturating_sub(self.height));
        self.scroll_to_selection();
    }

    /// Append a typed character to the query.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.recompute();
    }

    /// Delete the character before the cursor (the query's last char).
    pub fn backspace(&mut self) {
        self.query.pop();
        self.recompute();
    }

    /// Delete the word before the cursor (readline `unix-word-rubout`): drop
    /// trailing whitespace, then the run of non-whitespace before it.
    pub fn delete_word(&mut self) {
        let trimmed = self.query.trim_end_matches(char::is_whitespace);
        // `rfind` yields the whitespace char's *byte* offset; cut after the whole
        // char, not after one byte, or a multi-byte separator (NBSP, the
        // `U+2000`–`U+200A` spaces) would split mid-char and panic `truncate`.
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|i| {
                i + trimmed[i..]
                    .chars()
                    .next()
                    .expect("rfind landed on a char boundary")
                    .len_utf8()
            })
            .unwrap_or(0);
        self.query.truncate(cut);
        self.recompute();
    }

    /// Move the selection toward the better-ranked end (index 0).
    ///
    /// In the bottom-anchored layout the best match is drawn at the bottom, so
    /// this is what the Down key / Ctrl-N maps to.
    pub fn select_better(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.scroll_to_selection();
        }
    }

    /// Move the selection toward the worse-ranked end (higher index).
    ///
    /// Worse matches are drawn higher up the screen, so this is what the Up key
    /// / Ctrl-P maps to.
    pub fn select_worse(&mut self) {
        if self.selected + 1 < self.scored.len() {
            self.selected += 1;
            self.scroll_to_selection();
        }
    }

    /// Move the selection a page toward the worse-ranked end (up the screen),
    /// what Ctrl-U and PageUp map to.
    ///
    /// The viewport scrolls by the same number of rows as the selection, so
    /// the selected row keeps its place on the screen and every press shows a
    /// new page; only at the end of the list do the two stop, each at its own
    /// limit. The offset limit `len - height` is the one every other writer of
    /// `offset` keeps too (`set_height` included), so the arithmetic never
    /// meets an offset above it.
    pub fn page_worse(&mut self, page: Page) {
        if self.scored.is_empty() {
            return;
        }
        let rows = self.page_rows(page);
        self.selected = (self.selected + rows).min(self.scored.len() - 1);
        self.offset = (self.offset + rows).min(self.scored.len().saturating_sub(self.height));
        self.scroll_to_selection();
    }

    /// Move the selection a page toward the best-ranked end (down the screen),
    /// what Ctrl-D and PageDown map to. The viewport scrolls with it as in
    /// [`Self::page_worse`].
    pub fn page_better(&mut self, page: Page) {
        let rows = self.page_rows(page);
        self.selected = self.selected.saturating_sub(rows);
        self.offset = self.offset.saturating_sub(rows);
        self.scroll_to_selection();
    }

    /// The rows one page move covers; half a page is at least one row.
    fn page_rows(&self, page: Page) -> usize {
        match page {
            Page::Half => (self.height / 2).max(1),
            Page::Full => self.height,
        }
    }

    /// Adjust the scroll offset so the selected row sits within the viewport.
    fn scroll_to_selection(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + self.height {
            self.offset = self.selected + 1 - self.height;
        }
    }

    /// The query text as currently typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// `(matching, total)` counts for the `m/n` indicator.
    pub fn counter(&self) -> (usize, usize) {
        (self.scored.len(), self.items.len())
    }

    /// The 1-based rank of the selection within the matches, or `None` when
    /// nothing matches. Drives the cursor-position part of the stats line.
    pub fn selected_rank(&self) -> Option<usize> {
        if self.scored.is_empty() {
            None
        } else {
            Some(self.selected + 1)
        }
    }

    /// The selected row's detail (e.g. a piece's ID) for the stats line, or
    /// `None` when nothing matches.
    pub fn selected_detail(&self) -> Option<&str> {
        let item = self.scored.get(self.selected)?.item;
        Some(&self.detail[item])
    }

    /// The selected item, or `None` when nothing matches.
    pub fn selected(&self) -> Option<&T> {
        let item = self.scored.get(self.selected)?.item;
        Some(&self.items[item])
    }

    /// The notice to show instead of the counter, if one is set.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Replace the counter in the stats line with `notice`.
    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// Show the counter again.
    pub fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// The rows currently inside the viewport, top to bottom.
    pub fn visible(&self) -> Vec<VisibleRow<'_>> {
        let end = (self.offset + self.height).min(self.scored.len());
        (self.offset..end)
            .map(|i| {
                let s = &self.scored[i];
                VisibleRow {
                    display: &self.display[s.item],
                    highlights: &s.highlights,
                    selected: i == self.selected,
                }
            })
            .collect()
    }

    /// The list region as exactly `height` screen lines, top to bottom.
    ///
    /// The picker is bottom-anchored: the prompt sits at the bottom and the list
    /// grows upward with the best match nearest the prompt. So this returns the
    /// viewport rows in *screen* order (worst-ranked first/top, best-ranked
    /// last/bottom) and pads the top with `None` blanks when fewer rows match
    /// than the viewport can show, keeping the list flush above the prompt.
    pub fn list_lines(&self) -> Vec<Option<VisibleRow<'_>>> {
        let visible = self.visible();
        let blanks = self.height.saturating_sub(visible.len());
        let mut lines: Vec<Option<VisibleRow<'_>>> = Vec::with_capacity(self.height);
        lines.extend(std::iter::repeat_with(|| None).take(blanks));
        // `visible` is best-first; reverse it so the best match lands at the
        // bottom, nearest the prompt.
        lines.extend(visible.into_iter().rev().map(Some));
        lines
    }

    /// Consume the state and return the currently selected item, if any.
    pub fn into_selected(mut self) -> Option<T> {
        let item = self.scored.get(self.selected)?.item;
        // `swap_remove` is O(1) and order no longer matters once we are done.
        Some(self.items.swap_remove(item))
    }

    /// Render the visible state to a plain, deterministic string for snapshot
    /// tests, in screen order (top to bottom): each visible row prefixed with
    /// `> ` (selected) or two spaces and matched characters wrapped in `[ ]`,
    /// then the `m/n` counter line with the selected row's detail, then the
    /// prompt at the bottom.
    ///
    /// Rows come from [`Self::list_lines`], so they are bottom-anchored (best
    /// match nearest the prompt). The top blank-fill lines are omitted here to
    /// keep snapshots tight; blank-fill is covered directly in `list_lines`
    /// tests.
    #[cfg(test)]
    pub fn debug_render(&self) -> String {
        use std::collections::HashSet;
        use std::fmt::Write as _;

        // Trailing whitespace (e.g. the empty prompt's `> `) is trimmed per line
        // so inline snapshots stay free of fragile trailing spaces.
        let mut out = String::new();
        for row in self.list_lines().into_iter().flatten() {
            let pointer = if row.selected { "> " } else { "  " };
            let marked: HashSet<u32> = row.highlights.iter().copied().collect();
            let mut line = String::new();
            for (i, c) in row.display.chars().enumerate() {
                if marked.contains(&(i as u32)) {
                    let _ = write!(line, "[{c}]");
                } else {
                    line.push(c);
                }
            }
            let _ = writeln!(out, "{}", format!("{pointer}{line}").trim_end());
        }
        let (m, n) = self.counter();
        match self.selected_detail().filter(|d| !d.is_empty()) {
            Some(detail) => {
                let _ = writeln!(out, "{m}/{n} · {detail}");
            }
            None => {
                let _ = writeln!(out, "{m}/{n}");
            }
        }
        let _ = writeln!(out, "{}", format!("> {}", self.query).trim_end());
        out
    }
}

/// The characters of `display` that `pattern` matches, as sorted, deduplicated
/// char indices for highlighting.
///
/// `buf` is scratch space reused across rows; it is cleared first, as
/// `Pattern::indices` only appends. A row is scored on its full search text,
/// so its display text may hold only some of the query's atoms. `indices`
/// then reports no match but keeps the positions the earlier atoms pushed,
/// and those stay highlighted.
fn highlights(
    pattern: &Pattern,
    display: &Utf32String,
    matcher: &mut Matcher,
    buf: &mut Vec<u32>,
) -> Vec<u32> {
    buf.clear();
    let _ = pattern.indices(display.slice(..), matcher, buf);
    buf.sort_unstable();
    buf.dedup();
    buf.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed candidate set rendered by its own string identity (no detail).
    ///
    /// `search` mirrors `display`, so these fixtures exercise the common case
    /// where the searched and displayed text coincide.
    fn state(rows: &[&str], height: usize) -> PickerState<'static, String> {
        let items: Vec<String> = rows.iter().map(|s| s.to_string()).collect();
        let picker_rows = items
            .iter()
            .map(|s| Row {
                display: s.clone(),
                detail: String::new(),
                search: s.clone(),
            })
            .collect();
        PickerState::new(items, picker_rows, height)
    }

    /// Build a [`PickerState`] over rows with explicit `display`/`search` text,
    /// so tests can drive the case where the searched content extends past what
    /// is displayed.
    fn state_with_search(rows: &[(&str, &str)], height: usize) -> PickerState<'static, String> {
        let items: Vec<String> = rows
            .iter()
            .map(|(display, _)| display.to_string())
            .collect();
        let picker_rows = rows
            .iter()
            .map(|(display, search)| Row {
                display: display.to_string(),
                detail: String::new(),
                search: search.to_string(),
            })
            .collect();
        PickerState::new(items, picker_rows, height)
    }

    #[test]
    fn empty_query_lists_all_in_original_order() {
        let s = state(&["alpha", "beta", "gamma"], 10);
        // Bottom-anchored: best/selected row is nearest the prompt at the bottom.
        insta::assert_snapshot!(s.debug_render(), @r"
          gamma
          beta
        > alpha
        3/3
        >
        ");
    }

    #[test]
    fn query_filters_and_highlights_matches() {
        let mut s = state(&["alpha", "beta", "gamma"], 10);
        s.push_char('a');
        // Every row contains an 'a'; each keeps at least one highlighted match.
        insta::assert_snapshot!(s.debug_render());
    }

    #[test]
    fn non_matching_query_empties_the_list() {
        let mut s = state(&["alpha", "beta"], 10);
        for c in "zzz".chars() {
            s.push_char(c);
        }
        insta::assert_snapshot!(s.debug_render(), @r"
        0/2
        > zzz
        ");
    }

    #[test]
    fn selection_moves_and_clamps_at_both_ends() {
        let mut s = state(&["a", "b", "c"], 10);
        // Cannot move past the best-ranked end (index 0).
        s.select_better();
        assert_eq!(s.selected, 0);
        s.select_worse();
        s.select_worse();
        assert_eq!(s.selected, 2);
        // Cannot move past the worst-ranked end.
        s.select_worse();
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn viewport_scrolls_to_follow_the_selection() {
        let mut s = state(&["r0", "r1", "r2", "r3", "r4"], 2);
        // Only two rows fit; moving toward worse matches past the window scrolls
        // it. The selected (worse) row sits at the top, the better row below it.
        s.select_worse();
        s.select_worse();
        insta::assert_snapshot!(s.debug_render(), @r"
        > r2
          r1
        5/5
        >
        ");
        // Moving back toward the best match brings earlier rows into view again.
        s.select_better();
        s.select_better();
        insta::assert_snapshot!(s.debug_render(), @r"
          r1
        > r0
        5/5
        >
        ");
    }

    /// Twenty rows named `r0` to `r19`, best first.
    fn twenty(height: usize) -> PickerState<'static, String> {
        let names: Vec<String> = (0..20).map(|i| format!("r{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        state(&refs, height)
    }

    #[test]
    fn growing_the_viewport_fills_it_with_matches() {
        let mut s = twenty(5);
        for _ in 0..19 {
            s.select_worse();
        }
        assert_eq!((s.selected, s.offset), (19, 15));
        s.set_height(10);
        assert_eq!(s.offset, 10);
        assert!(s.offset <= s.selected && s.selected < s.offset + 10);
        assert!(s.list_lines().iter().all(Option::is_some));
    }

    #[test]
    fn shrinking_the_viewport_keeps_the_selection_visible() {
        let mut s = twenty(10);
        for _ in 0..19 {
            s.select_worse();
        }
        assert_eq!((s.selected, s.offset), (19, 10));
        s.set_height(5);
        assert_eq!(s.offset, 15);
    }

    // -------------------------------------------------------------------------
    // Paging
    // -------------------------------------------------------------------------

    #[test]
    fn half_page_worse_moves_half_the_height_and_keeps_the_screen_row() {
        let mut s = twenty(10);
        s.page_worse(Page::Half);
        assert_eq!((s.selected_rank(), s.offset), (Some(6), 5));
    }

    #[test]
    fn full_page_worse_moves_the_height() {
        let mut s = twenty(10);
        s.page_worse(Page::Full);
        assert_eq!((s.selected_rank(), s.offset), (Some(11), 10));
    }

    #[test]
    fn page_better_moves_back_toward_the_best() {
        let mut s = twenty(10);
        for _ in 0..19 {
            s.select_worse();
        }
        assert_eq!((s.selected, s.offset), (19, 10));
        s.page_better(Page::Half);
        assert_eq!((s.selected_rank(), s.offset), (Some(15), 5));
    }

    #[test]
    fn paging_worse_stops_at_the_last_match() {
        let names: Vec<String> = (0..12).map(|i| format!("r{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut s = state(&refs, 10);
        s.page_worse(Page::Full);
        s.page_worse(Page::Full);
        assert_eq!((s.selected_rank(), s.offset), (Some(12), 2));
    }

    #[test]
    fn paging_better_stops_at_the_best_match() {
        let mut s = twenty(10);
        for _ in 0..3 {
            s.select_worse();
        }
        s.page_better(Page::Half);
        assert_eq!((s.selected_rank(), s.offset), (Some(1), 0));
    }

    #[test]
    fn a_half_page_of_a_one_row_viewport_is_one_row() {
        let mut s = state(&["a", "b", "c"], 1);
        s.page_worse(Page::Half);
        assert_eq!((s.selected_rank(), s.offset), (Some(2), 1));
    }

    #[test]
    fn paging_with_fewer_matches_than_rows_selects_the_last() {
        let mut s = state(&["a", "b", "c"], 10);
        s.page_worse(Page::Full);
        assert_eq!((s.selected_rank(), s.offset), (Some(3), 0));
    }

    #[test]
    fn paging_after_the_viewport_grew_moves_by_the_page() {
        let mut s = twenty(5);
        for _ in 0..12 {
            s.select_worse();
        }
        s.set_height(10);
        assert_eq!((s.selected, s.offset), (12, 8));
        s.page_worse(Page::Half);
        assert_eq!((s.selected, s.offset), (17, 10));
        s.page_worse(Page::Half);
        assert_eq!((s.selected, s.offset), (19, 10));
    }

    #[test]
    fn a_page_that_reaches_no_end_keeps_the_screen_row() {
        let mut s = twenty(10);
        for _ in 0..3 {
            s.select_worse();
        }
        s.page_worse(Page::Half);
        assert_eq!((s.selected, s.offset), (8, 5));
    }

    #[test]
    fn paging_without_matches_is_a_no_op() {
        let mut s = state(&["alpha"], 10);
        s.push_char('z');
        s.page_worse(Page::Full);
        s.page_better(Page::Full);
        assert_eq!(s.selected_rank(), None);
    }

    #[test]
    fn typing_resets_selection_to_the_top() {
        let mut s = state(&["alpha", "alps", "also"], 10);
        s.select_worse();
        s.select_worse();
        assert_eq!(s.selected, 2);
        s.push_char('a');
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn backspace_widens_the_result_set() {
        let mut s = state(&["alpha", "beta"], 10);
        s.push_char('b');
        assert_eq!(s.counter().0, 1);
        s.backspace();
        assert_eq!(s.counter().0, 2);
    }

    #[test]
    fn delete_word_drops_the_last_whitespace_run() {
        let mut s = state(&["x"], 10);
        for c in "foo bar".chars() {
            s.push_char(c);
        }
        s.delete_word();
        assert_eq!(s.query(), "foo ");
        s.delete_word();
        assert_eq!(s.query(), "");
    }

    #[test]
    fn delete_word_handles_a_multibyte_whitespace_separator() {
        // NBSP (`U+00A0`, 2 bytes) between the words: `rfind` + 1 would land
        // inside it and `truncate` would panic on a non-char-boundary.
        let mut s = state(&["x"], 10);
        for c in "alpha\u{00A0}beta".chars() {
            s.push_char(c);
        }
        s.delete_word();
        assert_eq!(s.query(), "alpha\u{00A0}");
        s.delete_word();
        assert_eq!(s.query(), "");
    }

    #[test]
    fn delete_word_handles_a_three_byte_whitespace_separator() {
        // `U+2003` EM SPACE encodes to 3 bytes.
        let mut s = state(&["x"], 10);
        for c in "one\u{2003}two".chars() {
            s.push_char(c);
        }
        s.delete_word();
        assert_eq!(s.query(), "one\u{2003}");
    }

    #[test]
    fn delete_word_on_all_whitespace_empties_the_query() {
        let mut s = state(&["x"], 10);
        for c in "  \u{00A0} ".chars() {
            s.push_char(c);
        }
        s.delete_word();
        assert_eq!(s.query(), "");
    }

    #[test]
    fn delete_word_without_whitespace_empties_the_query() {
        let mut s = state(&["x"], 10);
        for c in "single".chars() {
            s.push_char(c);
        }
        s.delete_word();
        assert_eq!(s.query(), "");
    }

    #[test]
    fn selected_borrows_the_cursor_row() {
        let mut s = state(&["alpha", "beta", "gamma"], 10);
        assert_eq!(s.selected().map(String::as_str), Some("alpha"));
        s.select_worse();
        assert_eq!(s.selected().map(String::as_str), Some("beta"));
    }

    #[test]
    fn selected_is_none_when_nothing_matches() {
        let mut s = state(&["alpha"], 10);
        s.push_char('z');
        assert_eq!(s.selected(), None);
    }

    #[test]
    fn a_notice_is_kept_until_cleared() {
        let mut s = state(&["alpha"], 10);
        assert_eq!(s.notice(), None);
        s.set_notice("pinned alpha".to_string());
        assert_eq!(s.notice(), Some("pinned alpha"));
        s.clear_notice();
        assert_eq!(s.notice(), None);
    }

    #[test]
    fn into_selected_returns_the_cursor_row() {
        let mut s = state(&["alpha", "beta", "gamma"], 10);
        s.select_worse();
        assert_eq!(s.into_selected().as_deref(), Some("beta"));
    }

    #[test]
    fn into_selected_is_none_when_nothing_matches() {
        let mut s = state(&["alpha"], 10);
        s.push_char('z');
        assert_eq!(s.into_selected(), None);
    }

    #[test]
    fn detail_is_shown_with_the_counter_but_not_matched() {
        let items = vec!["alpha".to_string()];
        let rows = vec![Row {
            display: "alpha".to_string(),
            detail: "ZID".into(),
            search: "alpha".to_string(),
        }];
        let mut s = PickerState::new(items, rows, 10);
        // The selected row's detail sits next to the counter, not in the row...
        insta::assert_snapshot!(s.debug_render(), @r"
        > alpha
        1/1 · ZID
        >
        ");
        // ...and a character that occurs only in the detail finds no match.
        s.push_char('z');
        assert_eq!(s.counter().0, 0);
    }

    #[test]
    fn selected_detail_follows_the_selection() {
        let items: Vec<String> = ["alpha", "beta"].iter().map(|s| s.to_string()).collect();
        let rows = vec![
            Row {
                display: "alpha".to_string(),
                detail: "ID-A".into(),
                search: "alpha".to_string(),
            },
            Row {
                display: "beta".to_string(),
                detail: "ID-B".into(),
                search: "beta".to_string(),
            },
        ];
        let mut s = PickerState::new(items, rows, 10);
        assert_eq!(s.selected_detail(), Some("ID-A"));
        s.select_worse();
        assert_eq!(s.selected_detail(), Some("ID-B"));
    }

    #[test]
    fn selected_detail_is_none_when_nothing_matches() {
        let mut s = state(&["alpha"], 10);
        s.push_char('z');
        assert_eq!(s.selected_detail(), None);
    }

    #[test]
    fn an_empty_detail_adds_nothing_to_the_counter_line() {
        let s = state(&["alpha"], 10);
        assert_eq!(s.selected_detail(), Some(""));
        assert!(s.debug_render().contains("\n1/1\n"));
    }

    #[test]
    fn empty_item_set_selects_nothing() {
        let s: PickerState<String> = PickerState::new(Vec::new(), Vec::new(), 10);
        assert_eq!(s.counter(), (0, 0));
        assert_eq!(s.into_selected(), None);
    }

    /// Collect `(display, selected)` for the non-blank lines, top to bottom.
    fn line_rows(s: &PickerState<String>) -> Vec<(String, bool)> {
        s.list_lines()
            .into_iter()
            .flatten()
            .map(|r| (r.display.to_string(), r.selected))
            .collect()
    }

    #[test]
    fn list_lines_pads_the_top_with_blanks() {
        let s = state(&["a", "b"], 5);
        let lines = s.list_lines();
        // Exactly `height` lines; the top is blank so the list hugs the prompt.
        assert_eq!(lines.len(), 5);
        assert!(lines[0].is_none());
        assert!(lines[1].is_none());
        assert!(lines[2].is_none());
        assert!(lines[3].is_some());
        assert!(lines[4].is_some());
    }

    #[test]
    fn list_lines_put_the_best_match_last_and_selected() {
        // Empty query keeps original order; index 0 ("a") is best and selected.
        let rows = line_rows(&state(&["a", "b", "c"], 10));
        assert_eq!(
            rows,
            vec![
                ("c".to_string(), false),
                ("b".to_string(), false),
                ("a".to_string(), true),
            ]
        );
    }

    #[test]
    fn list_lines_have_no_blanks_when_the_viewport_is_full() {
        let s = state(&["r0", "r1", "r2", "r3", "r4"], 2);
        let lines = s.list_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(Option::is_some));
    }

    #[test]
    fn selected_rank_is_one_based_and_tracks_movement() {
        let mut s = state(&["a", "b", "c"], 10);
        assert_eq!(s.selected_rank(), Some(1));
        s.select_worse();
        assert_eq!(s.selected_rank(), Some(2));
    }

    #[test]
    fn selected_rank_is_none_when_nothing_matches() {
        let mut s = state(&["alpha", "beta"], 10);
        s.push_char('z');
        assert_eq!(s.selected_rank(), None);
    }

    #[test]
    fn list_lines_track_the_selection_after_moving() {
        let mut s = state(&["a", "b", "c"], 10);
        s.select_worse();
        // The selected (now "b") row moves up one toward the worse end.
        let rows = line_rows(&s);
        assert_eq!(
            rows,
            vec![
                ("c".to_string(), false),
                ("b".to_string(), true),
                ("a".to_string(), false),
            ]
        );
    }

    // -------------------------------------------------------------------------
    // Dual-match: full-content scoring, display-coordinate highlighting
    // -------------------------------------------------------------------------

    /// Type each character of `query` into the picker in order.
    fn type_query(s: &mut PickerState<String>, query: &str) {
        for c in query.chars() {
            s.push_char(c);
        }
    }

    #[test]
    fn match_only_in_hidden_content_ranks_without_highlighting() {
        // `hiddenword` exists only in `search`, never in the displayed text, and
        // shares no 'h' with it, so the display pass finds nothing to mark.
        let mut s = state_with_search(&[("visible title", "visible title hiddenword")], 10);
        type_query(&mut s, "hiddenword");
        assert_eq!(s.counter().0, 1);
        assert!(s.visible()[0].highlights.is_empty());
    }

    #[test]
    fn match_in_visible_content_highlights_display_chars() {
        // The query hits the displayed text, so highlights are non-empty and stay
        // within the displayed row's char range.
        let mut s = state_with_search(&[("alpha", "alpha extra")], 10);
        s.push_char('a');
        let visible = s.visible();
        let highlights = visible[0].highlights;
        assert!(!highlights.is_empty());
        let display_chars = visible[0].display.chars().count() as u32;
        assert!(highlights.iter().all(|&i| i < display_chars));
    }

    #[test]
    fn date_like_content_matches_via_search_only() {
        // A date lives in `search` but not in the `display` text, so the row
        // matches yet shows no highlight.
        let mut s = state_with_search(&[("note", "note 2026-06-25")], 10);
        type_query(&mut s, "2026");
        assert_eq!(s.counter().0, 1);
        assert!(s.visible()[0].highlights.is_empty());
    }

    #[test]
    fn no_match_in_either_corpus_drops_the_row() {
        let mut s = state_with_search(&[("alpha", "alpha beta")], 10);
        type_query(&mut s, "zzz");
        assert_eq!(s.counter().0, 0);
    }

    // -------------------------------------------------------------------------
    // Replacing the rows after a width change
    // -------------------------------------------------------------------------

    /// Rows with the given display text, a fixed detail, and `search` text.
    fn rows(pairs: &[(&str, &str)]) -> Vec<Row> {
        pairs
            .iter()
            .map(|(display, search)| Row {
                display: display.to_string(),
                detail: "ID".to_string(),
                search: search.to_string(),
            })
            .collect()
    }

    /// The display text of the visible rows, best match first.
    fn shown(s: &PickerState<String>) -> Vec<String> {
        s.visible().iter().map(|r| r.display.to_string()).collect()
    }

    #[test]
    fn items_are_the_candidates_in_their_original_order() {
        let s = state(&["alpha", "beta"], 10);
        assert_eq!(s.items(), ["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn set_rows_replaces_display_and_detail() {
        let mut s = state(&["alpha", "beta"], 10);
        s.set_rows(rows(&[("ALPHA", "alpha"), ("BETA", "beta")]));
        let visible = s.visible();
        assert_eq!(visible[0].display, "ALPHA");
        assert_eq!(s.selected_detail(), Some("ID"));
        assert_eq!(visible[1].display, "BETA");
    }

    #[test]
    fn set_rows_keeps_the_selection_and_the_scroll_offset() {
        let names = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let mut s = state(&names, 2);
        for _ in 0..3 {
            s.select_worse();
        }
        let rank = s.selected_rank();
        let before = shown(&s);
        let marked: Vec<(String, String)> = names
            .iter()
            .map(|n| (format!("{n}!"), n.to_string()))
            .collect();
        let pairs: Vec<(&str, &str)> = marked
            .iter()
            .map(|(d, n)| (d.as_str(), n.as_str()))
            .collect();
        s.set_rows(rows(&pairs));
        assert_eq!(s.selected_rank(), rank);
        let after: Vec<String> = before.iter().map(|d| format!("{d}!")).collect();
        assert_eq!(shown(&s), after);
    }

    #[test]
    fn set_rows_keeps_the_ranking_and_ignores_the_new_search_text() {
        let mut s = state_with_search(&[("zzz", "zzz"), ("abc", "abc"), ("abx", "abx")], 10);
        type_query(&mut s, "abc");
        let counter = s.counter();
        let before = shown(&s);
        // New search text would match every row and rank them differently;
        // the ranking still comes from the text the state was built with.
        s.set_rows(rows(&[("zzz", "abc"), ("abc", "zzz"), ("abx", "abc")]));
        assert_eq!(s.counter(), counter);
        assert_eq!(shown(&s), before);
    }

    #[test]
    fn later_queries_still_score_the_original_search_text() {
        let mut s = state_with_search(&[("alpha", "alpha"), ("beta", "beta")], 10);
        s.set_rows(rows(&[("alpha", "zzz"), ("beta", "zzz")]));
        type_query(&mut s, "beta");
        assert_eq!(shown(&s), ["beta"]);
    }

    #[test]
    fn set_rows_moves_the_highlights_onto_the_new_display() {
        let mut s = state_with_search(&[("first", "first"), ("xx…", "xx tailword")], 10);
        type_query(&mut s, "tailword");
        let position = shown(&s).iter().position(|d| d == "xx…");
        assert!(s.visible()[0].highlights.is_empty());

        s.set_rows(rows(&[("first", "first"), ("xx tailword", "xx tailword")]));
        let visible = s.visible();
        let row = visible
            .iter()
            .position(|r| r.display == "xx tailword")
            .expect("row still matches");
        assert_eq!(Some(row), position);
        assert!(!visible[row].highlights.is_empty());
    }

    #[test]
    fn set_rows_with_an_empty_query_leaves_no_highlights() {
        let mut s = state(&["alpha", "beta"], 10);
        s.set_rows(rows(&[("ALPHA", "alpha"), ("BETA", "beta")]));
        assert!(s.visible().iter().all(|r| r.highlights.is_empty()));
    }

    // -------------------------------------------------------------------------
    // Highlight pass
    // -------------------------------------------------------------------------

    fn pattern(query: &str) -> Pattern {
        Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
    }

    #[test]
    fn highlights_discard_what_the_buffer_held_before() {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = vec![99];
        let hay = Utf32String::from("abc");
        assert_eq!(highlights(&pattern("b"), &hay, &mut matcher, &mut buf), [1]);
    }

    #[test]
    fn highlights_keep_the_atoms_that_matched_before_one_failed() {
        // The display text holds only the first atom; the second matched in
        // the full search text. The first atom's characters stay highlighted.
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let hay = Utf32String::from("abc");
        assert_eq!(
            highlights(&pattern("ab zz"), &hay, &mut matcher, &mut buf),
            [0, 1]
        );
    }

    #[test]
    fn highlights_are_sorted_and_free_of_duplicates() {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let hay = Utf32String::from("abc");
        assert_eq!(
            highlights(&pattern("c a"), &hay, &mut matcher, &mut buf),
            [0, 2]
        );
        assert_eq!(
            highlights(&pattern("a a"), &hay, &mut matcher, &mut buf),
            [0]
        );
    }

    // -------------------------------------------------------------------------
    // Matching member paths
    // -------------------------------------------------------------------------

    use std::cell::RefCell;
    use std::rc::Rc;

    /// The names of the pieces whose members were asked for, in order.
    type Asked = Rc<RefCell<Vec<String>>>;

    /// Members by piece name; a piece without an entry has none to give.
    struct FakeContents {
        members: Vec<(String, Vec<String>)>,
        asked: Asked,
    }

    impl Contents<String> for FakeContents {
        fn members(&mut self, item: &String) -> Option<Members> {
            self.asked.borrow_mut().push(item.clone());
            let (_, paths) = self.members.iter().find(|(name, _)| name == item)?;
            Some(Members {
                prefix: format!("{item}/"),
                paths: paths.clone(),
            })
        }
    }

    /// A picker over `pieces`, each with the member paths listed for it. A
    /// piece listed with `None` has no members to read.
    fn with_members(pieces: &[(&str, Option<&[&str]>)]) -> (PickerState<'static, String>, Asked) {
        let names: Vec<&str> = pieces.iter().map(|(name, _)| *name).collect();
        let asked = Asked::default();
        let contents = FakeContents {
            members: pieces
                .iter()
                .filter_map(|(name, paths)| {
                    paths.map(|paths| {
                        (
                            name.to_string(),
                            paths.iter().map(|p| p.to_string()).collect(),
                        )
                    })
                })
                .collect(),
            asked: Rc::clone(&asked),
        };
        (state(&names, 10).with_contents(Box::new(contents)), asked)
    }

    /// How often the members of `piece` were asked for.
    fn asked_for(asked: &Asked, piece: &str) -> usize {
        asked.borrow().iter().filter(|name| *name == piece).count()
    }

    #[test]
    fn a_piece_matches_through_a_member_path() {
        let (mut s, _) = with_members(&[
            ("old-app", Some(&["src/main.txt"])),
            ("other", Some(&["notes.txt"])),
        ]);
        type_query(&mut s, "main");
        assert_eq!(shown(&s), ["old-app"]);
    }

    #[test]
    fn a_member_only_match_ranks_below_a_row_match() {
        // The member `main` scores far above the scattered letters of the
        // row, and still ranks below it.
        let (mut s, _) = with_members(&[("zzz", Some(&["main"])), ("m_a_i_n-notes", Some(&["x"]))]);
        type_query(&mut s, "main");
        assert_eq!(shown(&s), ["m_a_i_n-notes", "zzz"]);
    }

    #[test]
    fn member_only_matches_rank_by_their_best_member() {
        let (mut s, _) =
            with_members(&[("first", Some(&["m-a-i-n"])), ("second", Some(&["main"]))]);
        type_query(&mut s, "main");
        assert_eq!(shown(&s), ["second", "first"]);
    }

    #[test]
    fn a_member_only_match_has_no_highlight_in_its_row() {
        let (mut s, _) = with_members(&[("zzz", Some(&["main"]))]);
        type_query(&mut s, "main");
        assert!(s.visible()[0].highlights.is_empty());
    }

    #[test]
    fn a_piece_is_matched_through_its_path_and_a_member_together() {
        // `scan.rs` alone lacks `canu`, and the row alone lacks `scan`.
        let (mut s, _) =
            with_members(&[("canu", Some(&["scan.rs"])), ("other", Some(&["scan.rs"]))]);
        type_query(&mut s, "canu scan");
        assert_eq!(shown(&s), ["canu"]);
    }

    #[test]
    fn members_load_only_for_the_pieces_a_scan_needs() {
        let (mut s, asked) = with_members(&[("main-app", Some(&["a"])), ("other", Some(&["b"]))]);
        assert!(asked.borrow().is_empty(), "nothing is read at the start");
        s.push_char('m');
        assert_eq!(*asked.borrow(), ["other"], "a row match reads nothing");
        s.push_char('x');
        assert_eq!(
            *asked.borrow(),
            ["other", "main-app"],
            "a piece that stops matching its row is read then, others stay read"
        );
    }

    #[test]
    fn members_beyond_the_budget_are_scanned_and_dropped() {
        let (mut s, asked) = with_members(&[
            ("first", Some(&["q1", "q2", "q3"])),
            ("second", Some(&["q4", "q5", "q6"])),
        ]);
        s.set_member_budget(4);
        s.push_char('q');
        s.push_char('q');
        assert_eq!(asked_for(&asked, "first"), 1, "retained within the budget");
        assert_eq!(
            asked_for(&asked, "second"),
            2,
            "read again for the narrowing"
        );
        s.backspace();
        assert_eq!(asked_for(&asked, "first"), 1);
        assert_eq!(asked_for(&asked, "second"), 3);
    }

    #[test]
    fn a_query_that_extends_the_last_scores_only_the_previous_hits() {
        let (mut s, _) = with_members(&[
            ("one", Some(&["ma", "mb", "mc", "xx"])),
            ("two", Some(&["xy", "xz"])),
        ]);
        s.push_char('m');
        assert_eq!(s.members_scored(), 6, "the first character scores all");
        s.push_char('a');
        assert_eq!(s.members_scored(), 6 + 3, "only the three hits of `m`");
        assert_eq!(shown(&s), ["one"]);
    }

    #[test]
    fn a_piece_that_leaves_row_matching_is_scanned_in_full() {
        let (mut s, _) = with_members(&[("main", Some(&["a", "b", "c"]))]);
        s.push_char('m');
        assert_eq!(s.members_scored(), 0, "a row match scores no member");
        s.push_char('x');
        assert_eq!(s.members_scored(), 3);
    }

    /// The members scored by the second character when the first was `m`
    /// and the second is `second`, over a piece whose `m` hits are two of
    /// four members.
    fn scored_by_second_character(second: char) -> usize {
        let (mut s, _) = with_members(&[("zzz", Some(&["ma", "mb", "xx", "yy"]))]);
        s.push_char('m');
        s.push_char(second);
        s.members_scored() - 4
    }

    #[test]
    fn a_query_with_a_bang_scores_in_full() {
        assert_eq!(scored_by_second_character('!'), 4);
    }

    #[test]
    fn a_query_with_a_caret_scores_in_full() {
        assert_eq!(scored_by_second_character('^'), 4);
    }

    #[test]
    fn a_query_with_a_quote_scores_in_full() {
        assert_eq!(scored_by_second_character('\''), 4);
    }

    #[test]
    fn a_query_with_a_dollar_scores_in_full() {
        assert_eq!(scored_by_second_character('$'), 4);
    }

    #[test]
    fn a_query_with_a_backslash_scores_in_full() {
        assert_eq!(scored_by_second_character('\\'), 4);
    }

    #[test]
    fn an_ordinary_second_character_scores_the_hits_only() {
        assert_eq!(scored_by_second_character('a'), 2);
    }

    #[test]
    fn backspace_scores_in_full_and_widens_the_set() {
        let (mut s, _) = with_members(&[("one", Some(&["ma", "mb"])), ("two", Some(&["mc"]))]);
        type_query(&mut s, "ma");
        assert_eq!(shown(&s), ["one"]);
        let before = s.members_scored();
        s.backspace();
        assert_eq!(s.members_scored() - before, 3);
        assert_eq!(shown(&s), ["one", "two"]);
    }

    #[test]
    fn a_row_match_scans_no_members() {
        let (mut s, asked) = with_members(&[("alpha", Some(&["x"])), ("alpine", Some(&["y"]))]);
        s.push_char('a');
        assert_eq!(s.members_scored(), 0);
        assert!(asked.borrow().is_empty());
    }

    #[test]
    fn a_piece_without_members_is_left_out_and_asked_once() {
        let (mut s, asked) = with_members(&[("gone", None), ("zzz", Some(&["main"]))]);
        s.push_char('m');
        s.push_char('a');
        assert_eq!(shown(&s), ["zzz"]);
        assert_eq!(asked_for(&asked, "gone"), 1);
    }

    #[test]
    fn clearing_the_query_forgets_what_the_last_one_found() {
        let (mut s, _) = with_members(&[("one", Some(&["ma", "xx"]))]);
        s.push_char('m');
        s.backspace();
        let before = s.members_scored();
        s.push_char('m');
        assert_eq!(s.members_scored() - before, 2);
    }
}
