// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text with a few styled spans, written once and rendered for any
//! environment.

use antiq::shown::escape_controls;

/// What a span of text is, which decides how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Plain,
    /// A command, a path or another name the reader may copy.
    Strong,
}

/// A run of text without whitespace, made of spans.
pub type Word = Vec<(Role, String)>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rich {
    spans: Vec<(Role, String)>,
}

impl Rich {
    pub fn new() -> Rich {
        Rich::default()
    }

    /// Reads text where a code span marks a command or path.
    ///
    /// A span opens with a run of backticks and ends at the next run of
    /// exactly as many, as in CommonMark, and a single space inside each
    /// edge of a span that has one on both sides is dropped. Text from
    /// the user's files can hold backticks, so the library fences it with
    /// [`antiq::code_span::CodeSpan`]; an opening run without a closing
    /// one is plain text.
    pub fn parse(markup: &str) -> Rich {
        let mut rich = Rich::new();
        let mut rest = markup;
        while let Some(open) = rest.find('`') {
            let run = rest[open..].chars().take_while(|c| *c == '`').count();
            let after = &rest[open + run..];
            let mut search = 0;
            let close = loop {
                let Some(found) = after[search..].find('`') else {
                    break None;
                };
                let start = search + found;
                let len = after[start..].chars().take_while(|c| *c == '`').count();
                if len == run {
                    break Some(start);
                }
                search = start + len;
            };
            match close {
                Some(close) => {
                    let code = &after[..close];
                    let code = match (code.strip_prefix(' '), code.strip_suffix(' ')) {
                        (Some(_), Some(_)) if code.trim() != "" => &code[1..code.len() - 1],
                        _ => code,
                    };
                    rich = rich.text(&rest[..open]).cmd(code);
                    rest = &after[close + run..];
                }
                None => {
                    rich = rich.text(&rest[..open + run]);
                    rest = after;
                }
            }
        }
        rich.text(rest)
    }

    /// Adds a span. Its text can hold a name from the user's files, so a
    /// character that a terminal acts on is escaped here, before any width
    /// is counted and before the look of the span is added around it.
    fn push(mut self, role: Role, text: &str) -> Rich {
        if !text.is_empty() {
            self.spans.push((role, escape_controls(text)));
        }
        self
    }

    pub fn text(self, text: &str) -> Rich {
        self.push(Role::Plain, text)
    }

    pub fn cmd(self, text: &str) -> Rich {
        self.push(Role::Strong, text)
    }

    pub fn path(self, text: &str) -> Rich {
        self.push(Role::Strong, text)
    }

    /// The text with its first letter in upper case, for the title of a
    /// block whose words come from a message that starts in lower case.
    /// A text that starts with a code span stays as written, as a command
    /// or path has no capital.
    pub fn capitalised(mut self) -> Rich {
        if let Some((Role::Plain, first)) = self.spans.first_mut() {
            let mut letters = first.chars();
            if let Some(letter) = letters.next() {
                *first = letter.to_uppercase().chain(letters).collect();
            }
        }
        self
    }

    pub fn to_plain(&self) -> String {
        self.spans.iter().map(|(_, text)| text.as_str()).collect()
    }

    /// The words of the text, where a word may change its look in the
    /// middle (a command followed by a full stop). Plain text breaks at
    /// whitespace. A command or path is one word whatever it contains, as
    /// a name with a space in it is not two names and its spaces are its
    /// own.
    pub fn words(&self) -> Vec<Word> {
        let mut words: Vec<Word> = Vec::new();
        let mut open = false;
        let add = |words: &mut Vec<Word>, open: &mut bool, role: Role, part: &str| {
            if *open {
                words
                    .last_mut()
                    .expect("an open word exists")
                    .push((role, part.to_string()));
            } else {
                words.push(vec![(role, part.to_string())]);
                *open = true;
            }
        };
        for (role, text) in &self.spans {
            if *role == Role::Strong {
                add(&mut words, &mut open, *role, text);
                continue;
            }
            for (index, part) in text.split(char::is_whitespace).enumerate() {
                if index > 0 {
                    open = false;
                }
                if !part.is_empty() {
                    add(&mut words, &mut open, *role, part);
                }
            }
        }
        words
    }
}
