// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a stream can show: symbols, colour and how wide a line may be.

use anstream::AutoStream;
use anstream::ColorChoice;
use anstream::stream::RawStream;
use std::os::fd::AsFd;

/// The width a console is laid out for when nothing says otherwise.
const DEFAULT_WIDTH: usize = 80;

/// How one output stream is rendered. Stdout and stderr each get their
/// own, because either of them can be a pipe while the other is a
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    /// The stream is a terminal. Only then are blocks, badges, symbols,
    /// info, hints and the dry-run tag shown; a pipe keeps the line form
    /// that scripts read.
    pub rich: bool,
    /// SGR codes may be written. A terminal with `NO_COLOR` stays rich
    /// but has no colour.
    pub color: bool,
    /// Unicode symbols may be written.
    pub utf8: bool,
    /// Columns of the terminal, or 80.
    pub width: usize,
    /// The terminal moves its cursor on the escape sequences of an ANSI
    /// terminal, so that a region of lines can be redrawn in place. `TERM`
    /// unset or `dumb` clears it; `NO_COLOR` does not.
    pub cursor: bool,
}

impl Env {
    pub fn new(rich: bool, color: bool, utf8: bool, width: usize) -> Env {
        Env {
            rich,
            color,
            utf8,
            width,
            cursor: rich,
        }
    }

    /// The same environment with the cursor switch set.
    #[cfg(test)]
    pub fn with_cursor(mut self, cursor: bool) -> Env {
        self.cursor = cursor;
        self
    }

    /// A pipe: nothing but plain lines.
    pub fn pipe() -> Env {
        Env::new(false, false, true, DEFAULT_WIDTH)
    }

    /// The environment of `stream` as the process sees it.
    ///
    /// `anstream` only decides the colour here: it knows `NO_COLOR`,
    /// `CLICOLOR` and a `TERM` that cannot show colour. Its forcing
    /// switch (`CLICOLOR_FORCE`) is not honoured for layout, which is why
    /// colour requires a terminal too.
    pub fn detect<S: RawStream + AsFd>(stream: &S) -> Env {
        let rich = rustix::termios::isatty(stream);
        if !rich {
            return Env::pipe();
        }
        let var = |name: &str| std::env::var(name).ok();
        Env {
            rich,
            color: AutoStream::choice(stream) != ColorChoice::Never,
            utf8: utf8_locale(
                var("LC_ALL").as_deref(),
                var("LC_CTYPE").as_deref(),
                var("LANG").as_deref(),
            ),
            width: crate::cli::output::terminal_columns(stream).map_or(DEFAULT_WIDTH, usize::from),
            cursor: cursor_term(var("TERM").as_deref()),
        }
    }
}

/// Whether a terminal named by `TERM` can redraw in place: any terminal
/// but one that is unnamed or `dumb`.
pub fn cursor_term(term: Option<&str>) -> bool {
    term.is_some_and(|term| !term.is_empty() && term != "dumb")
}

/// Whether the locale names UTF-8. The first of the three variables that
/// is set and not empty decides, as in POSIX.
pub fn utf8_locale(lc_all: Option<&str>, lc_ctype: Option<&str>, lang: Option<&str>) -> bool {
    [lc_all, lc_ctype, lang]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        })
}
