// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The message blocks: error, warning, info and hint.

use super::rich::Rich;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Error,
    Warning,
    Info,
    Hint,
}

/// One paragraph of a block section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Para {
    /// Prose, wrapped to the width.
    Text(Rich),
    /// A line kept as written, such as a line of a config file.
    Pre(String),
    Blank,
}

impl Para {
    pub fn pre(line: &str) -> Para {
        Para::Pre(line.to_string())
    }
}

impl From<Rich> for Para {
    fn from(text: Rich) -> Para {
        Para::Text(text)
    }
}

/// A message to the reader. Rendering is in `render`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub(super) kind: Kind,
    pub(super) title: Rich,
    pub(super) text: Vec<Para>,
    pub(super) fix: Option<Rich>,
    pub(super) how: Vec<Para>,
    pub(super) said: Vec<Para>,
    pub(super) doing: Vec<String>,
    pub(super) candidates: Vec<String>,
    pub(super) pipe: Option<String>,
    pub(super) message: Option<String>,
}

impl Block {
    fn new(kind: Kind, title: Rich) -> Block {
        Block {
            kind,
            title,
            text: Vec::new(),
            fix: None,
            how: Vec::new(),
            said: Vec::new(),
            doing: Vec::new(),
            candidates: Vec::new(),
            pipe: None,
            message: None,
        }
    }

    pub fn error(title: Rich) -> Block {
        Block::new(Kind::Error, title)
    }

    pub fn warning(title: Rich) -> Block {
        Block::new(Kind::Warning, title)
    }

    pub fn info(title: Rich) -> Block {
        Block::new(Kind::Info, title)
    }

    pub fn hint(text: Rich) -> Block {
        Block::new(Kind::Hint, text)
    }

    /// The one line a pipe gets for this block, whatever the kind. Scripts
    /// read it, so it keeps the wording they match on.
    pub fn pipe(mut self, line: &str) -> Block {
        self.pipe = Some(line.to_string());
        self
    }

    /// The text the title was parsed from. A pipe has no styles to show, so
    /// it prints this text as written, backticks included, which is the
    /// message that `--json` carries.
    pub fn message(mut self, message: &str) -> Block {
        self.message = Some(message.to_string());
        self
    }

    /// A paragraph below the title of a warning or info.
    pub fn text(mut self, paragraph: impl Into<Para>) -> Block {
        self.text.push(paragraph.into());
        self
    }

    /// The `→` line of a warning or info: what repairs the situation.
    pub fn fix(mut self, fix: Rich) -> Block {
        self.fix = Some(fix);
        self
    }

    /// A paragraph of the "How to fix it" section of an error.
    pub fn how(mut self, paragraph: impl Into<Para>) -> Block {
        self.how.push(paragraph.into());
        self
    }

    /// A paragraph of the "What rsync said" section of an error, which
    /// follows "How to fix it".
    pub fn rsync_said(mut self, paragraph: impl Into<Para>) -> Block {
        self.said.push(paragraph.into());
        self
    }

    /// A step the run was in when an error stopped it, as the rest of the
    /// sentence "Attempting to ...". Steps are given innermost first.
    pub fn doing(mut self, phrase: &str) -> Block {
        self.doing.push(phrase.to_string());
        self
    }

    /// A match of an ambiguous reference. A pipe lists these as
    /// `candidate:` lines. A terminal shows none: the caller prints a
    /// table of the matches under the block.
    pub fn candidate(mut self, path: &str) -> Block {
        self.candidates.push(path.to_string());
        self
    }
}
