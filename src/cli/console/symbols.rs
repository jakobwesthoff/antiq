// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The glyphs of the console and their ASCII stand-ins.

use super::env::Env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbols {
    /// The bar down the left side of a block.
    pub bar: &'static str,
    pub error: &'static str,
    pub warning: &'static str,
    pub info: &'static str,
    pub hint: &'static str,
    pub done: &'static str,
    pub dry_run: &'static str,
    /// Marks the matching rows of an ambiguity table.
    pub matched: &'static str,
    pub rule: &'static str,
    /// The line down the left side of text that is quoted, such as a file
    /// a dry run would write.
    pub frame: &'static str,
}

const UNICODE: Symbols = Symbols {
    bar: "▌",
    error: "✖",
    warning: "⚠",
    info: "ℹ",
    hint: "→",
    done: "✔",
    dry_run: "○",
    matched: "▸",
    rule: "─",
    frame: "│",
};

const ASCII: Symbols = Symbols {
    bar: "|",
    error: "x",
    warning: "!",
    info: "i",
    hint: "->",
    done: "ok",
    dry_run: "o",
    matched: ">",
    rule: "-",
    frame: "|",
};

impl Symbols {
    pub fn for_env(env: &Env) -> Symbols {
        if env.utf8 { UNICODE } else { ASCII }
    }
}
