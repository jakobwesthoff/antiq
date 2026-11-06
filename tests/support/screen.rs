// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A small terminal screen for tests: it applies the bytes a program wrote
//! and says what a person would see, on the screen and in the scrollback.
//!
//! It knows what antiq's output uses: printable characters, carriage
//! return, line feed (which also returns the carriage, as a terminal with
//! `ONLCR` does), the colour sequences, which it drops, and cursor up,
//! erase to the end of the screen and erase line. Any other escape sequence
//! is recorded in `unsupported`, so a test sees a program that switches a
//! terminal mode or scrolls a region.

use unicode_width::UnicodeWidthChar;

pub struct Screen {
    cols: usize,
    rows: usize,
    /// The visible rows. A wide character takes its cell and a `'\0'`.
    grid: Vec<Vec<char>>,
    /// Whether a visible row continues the one above it because the line
    /// was too long for the screen.
    continued: Vec<bool>,
    /// The rows that scrolled off the top, with the same flag.
    scrollback: Vec<(String, bool)>,
    row: usize,
    col: usize,
    /// The escape sequences the screen does not know, as written.
    pub unsupported: Vec<String>,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Screen {
        Screen {
            cols,
            rows,
            grid: vec![Vec::new(); rows],
            continued: vec![false; rows],
            scrollback: Vec::new(),
            row: 0,
            col: 0,
            unsupported: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            match character {
                '\r' => self.col = 0,
                '\n' => {
                    self.line_feed();
                    self.col = 0;
                }
                '\x1b' => {
                    let mut sequence = String::from("\x1b");
                    if chars.peek() == Some(&'[') {
                        sequence.push(chars.next().expect("peeked"));
                        for next in chars.by_ref() {
                            sequence.push(next);
                            if ('\x40'..='\x7e').contains(&next) {
                                break;
                            }
                        }
                        self.control(&sequence);
                    } else {
                        sequence.extend(chars.next());
                        self.unsupported.push(sequence);
                    }
                }
                other if other.is_control() => self.unsupported.push(format!("{other:?}")),
                other => self.put(other),
            }
        }
    }

    fn control(&mut self, sequence: &str) {
        let body = &sequence[2..sequence.len() - 1];
        let number = || body.parse::<usize>().ok();
        match sequence
            .chars()
            .last()
            .expect("a sequence has a final byte")
        {
            'm' => {}
            // A parameter of 0 or none moves one line, as in a terminal.
            'A' => self.row = self.row.saturating_sub(number().unwrap_or(1).max(1)),
            'J' if body.is_empty() || body == "0" => {
                self.grid[self.row].truncate(self.col);
                for row in self.row + 1..self.rows {
                    self.grid[row].clear();
                    self.continued[row] = false;
                }
            }
            'K' if body == "2" => self.grid[self.row].clear(),
            _ => self.unsupported.push(sequence.to_string()),
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 == self.rows {
            let top = self.grid.remove(0);
            let continued = self.continued.remove(0);
            self.scrollback.push((raw_text_of(&top), continued));
            self.grid.push(Vec::new());
            self.continued.push(false);
        } else {
            self.row += 1;
        }
    }

    fn put(&mut self, character: char) {
        let width = character.width().unwrap_or(0);
        if width == 0 {
            return;
        }
        if self.col + width > self.cols {
            self.line_feed();
            self.continued[self.row] = true;
            self.col = 0;
        }
        let row = &mut self.grid[self.row];
        // Writing over a cell replaces that cell and leaves the rest of the
        // row, as a terminal does.
        if row.len() < self.col + width {
            row.resize(self.col + width, ' ');
        }
        row[self.col] = character;
        if width == 2 {
            row[self.col + 1] = '\0';
        }
        self.col += width;
    }

    /// The visible rows, without trailing spaces.
    pub fn screen(&self) -> Vec<String> {
        self.grid.iter().map(|row| text_of(row)).collect()
    }

    /// The visible rows up to the last one with text.
    pub fn visible(&self) -> Vec<String> {
        let mut rows = self.screen();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    /// What a person could scroll back to, then the screen, with the rows of
    /// a long line joined: the lines in order.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let rows = self
            .scrollback
            .iter()
            .cloned()
            .chain(
                self.grid
                    .iter()
                    .map(|row| raw_text_of(row))
                    .zip(self.continued.iter().copied()),
            )
            .collect::<Vec<_>>();
        for (text, continued) in rows {
            match lines.last_mut() {
                Some(last) if continued => last.push_str(&text),
                _ => lines.push(text),
            }
        }
        let mut lines: Vec<String> = lines
            .into_iter()
            .map(|line| line.trim_end().to_string())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    /// The window changes size. With `rewrap` the visible lines are laid out
    /// again for the new width, as terminals that reflow do: a character goes
    /// on the row it fits on, a wide one that does not fit on the next. Without
    /// it each row is cut or kept as it is.
    pub fn resize(&mut self, cols: usize, rows: usize, rewrap: bool) {
        let cols = cols.max(1);
        if rewrap {
            self.rewrap(cols);
        } else {
            for row in &mut self.grid {
                row.truncate(cols);
                // A wide character cut in half leaves nothing of it.
                if row.last().is_some_and(|cell| cell.width() == Some(2)) {
                    *row.last_mut().expect("checked non-empty") = ' ';
                }
            }
            self.col = self.col.min(cols);
        }
        self.cols = cols;
        self.rows = rows;
        while self.grid.len() < rows {
            self.grid.push(Vec::new());
            self.continued.push(false);
        }
        while self.grid.len() > rows {
            let top = self.grid.remove(0);
            let continued = self.continued.remove(0);
            self.scrollback.push((raw_text_of(&top), continued));
            self.row = self.row.saturating_sub(1);
        }
        self.row = self.row.min(rows - 1);
    }

    /// Lays the visible lines out again for `cols` columns and moves the
    /// cursor with the character it stands on.
    fn rewrap(&mut self, cols: usize) {
        // Each logical line is the characters of its rows, written spaces
        // included. The cursor is remembered as a position in that line.
        let mut logical: Vec<Vec<char>> = Vec::new();
        let mut cursor = (0, 0);
        for (index, row) in self.grid.iter().enumerate() {
            if !(self.continued[index] && !logical.is_empty()) {
                logical.push(Vec::new());
            }
            let number = logical.len() - 1;
            let line = logical.last_mut().expect("a line was pushed");
            if index == self.row {
                let before: Vec<char> = row.iter().copied().take(self.col).collect();
                let before = before.iter().filter(|cell| **cell != '\0').count();
                cursor = (number, line.len() + before);
            }
            line.extend(row.iter().copied().filter(|cell| *cell != '\0'));
        }
        // Blank rows below the cursor and the last text are not lines of
        // their own, so they do not grow with the layout.
        let last = logical
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(cursor.0, |last| last.max(cursor.0));
        logical.truncate(last + 1);
        self.grid.clear();
        self.continued.clear();
        let (mut cursor_row, mut cursor_col) = (0, 0);
        for (number, line) in logical.iter().enumerate() {
            let mut row: Vec<char> = Vec::new();
            let mut used = 0;
            let mut first = true;
            for (offset, character) in line.iter().enumerate() {
                let width = character.width().unwrap_or(0);
                if used + width > cols {
                    self.grid.push(std::mem::take(&mut row));
                    self.continued.push(!first);
                    first = false;
                    used = 0;
                }
                if (number, offset) == cursor {
                    (cursor_row, cursor_col) = (self.grid.len(), used);
                }
                row.push(*character);
                if width == 2 {
                    row.push('\0');
                }
                used += width;
            }
            if (number, line.len()) == cursor {
                (cursor_row, cursor_col) = (self.grid.len(), used);
            }
            self.grid.push(row);
            self.continued.push(!first);
        }
        self.row = cursor_row;
        self.col = cursor_col;
    }
}

fn raw_text_of(row: &[char]) -> String {
    row.iter().filter(|cell| **cell != '\0').collect()
}

fn text_of(row: &[char]) -> String {
    raw_text_of(row).trim_end().to_string()
}
