// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text with a few styled spans, written once and rendered for any
//! environment.

/// What a span of text is, which decides how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Plain,
    /// A command, a path or another name the reader may copy.
    Strong,
    Dim,
    /// The part of a name a search matched.
    Matched,
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

    /// Reads text where a backtick pair marks a command or path.
    ///
    /// Only literals written in the source go through here. Text from
    /// the user's files can hold backticks and goes through the builder
    /// methods instead.
    pub fn parse(markup: &str) -> Rich {
        let mut rich = Rich::new();
        for (index, piece) in markup.split('`').enumerate() {
            rich = if index % 2 == 1 {
                rich.cmd(piece)
            } else {
                rich.text(piece)
            };
        }
        rich
    }

    fn push(mut self, role: Role, text: &str) -> Rich {
        if !text.is_empty() {
            self.spans.push((role, text.to_string()));
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

    pub fn dim(self, text: &str) -> Rich {
        self.push(Role::Dim, text)
    }

    pub fn matched(self, text: &str) -> Rich {
        self.push(Role::Matched, text)
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn to_plain(&self) -> String {
        self.spans.iter().map(|(_, text)| text.as_str()).collect()
    }

    /// The words of the text, where a word may change its look in the
    /// middle (a command followed by a full stop).
    pub fn words(&self) -> Vec<Word> {
        let mut words: Vec<Word> = Vec::new();
        let mut open = false;
        for (role, text) in &self.spans {
            for (index, part) in text.split(char::is_whitespace).enumerate() {
                if index > 0 {
                    open = false;
                }
                if part.is_empty() {
                    continue;
                }
                if open {
                    words
                        .last_mut()
                        .expect("an open word exists")
                        .push((*role, part.to_string()));
                } else {
                    words.push(vec![(*role, part.to_string())]);
                    open = true;
                }
            }
        }
        words
    }
}
