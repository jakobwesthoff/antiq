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
            self.scrollback.push((text_of(&top), continued));
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
                self.screen()
                    .into_iter()
                    .zip(self.continued.iter().copied()),
            )
            .collect::<Vec<_>>();
        for (text, continued) in rows {
            match lines.last_mut() {
                Some(last) if continued => last.push_str(&text),
                _ => lines.push(text),
            }
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    /// The window changes size. With `rewrap` the visible lines are laid out
    /// again for the new width, as terminals that reflow do; without it
    /// each row is cut or kept as it is.
    pub fn resize(&mut self, cols: usize, rows: usize, rewrap: bool) {
        let mut logical: Vec<String> = Vec::new();
        let mut cursor = (0, 0);
        if rewrap {
            for (index, row) in self.grid.iter().enumerate() {
                if !(self.continued[index] && !logical.is_empty()) {
                    logical.push(String::new());
                }
                if index == self.row {
                    let end = logical.last().expect("a line was pushed").chars().count();
                    cursor = (logical.len() - 1, end + self.col);
                }
                logical
                    .last_mut()
                    .expect("a line was pushed")
                    .push_str(&text_of(row));
            }
            self.cols = cols;
            self.rows = rows;
            self.grid.clear();
            self.continued.clear();
            let mut cursor_row = 0;
            for (index, line) in logical.iter().enumerate() {
                let characters: Vec<char> = line.chars().collect();
                let chunks = characters.chunks(cols.max(1)).count().max(1);
                for chunk in 0..chunks {
                    if index == cursor.0 && cursor.1 / cols.max(1) == chunk {
                        cursor_row = self.grid.len();
                    }
                    let start = chunk * cols;
                    let end = (start + cols).min(characters.len());
                    self.grid
                        .push(characters.get(start..end).unwrap_or(&[]).to_vec());
                    self.continued.push(chunk > 0);
                }
            }
            self.row = cursor_row;
            self.col = cursor.1 % cols.max(1);
        } else {
            self.cols = cols;
            self.rows = rows;
            for row in &mut self.grid {
                row.truncate(cols);
            }
            self.col = self.col.min(cols);
        }
        while self.grid.len() < rows {
            self.grid.push(Vec::new());
            self.continued.push(false);
        }
        while self.grid.len() > rows {
            let top = self.grid.remove(0);
            let continued = self.continued.remove(0);
            self.scrollback.push((text_of(&top), continued));
            self.row = self.row.saturating_sub(1);
        }
        self.row = self.row.min(rows - 1);
    }
}

fn text_of(row: &[char]) -> String {
    let text: String = row.iter().filter(|cell| **cell != '\0').collect();
    text.trim_end().to_string()
}
