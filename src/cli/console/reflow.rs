// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a terminal does to the lines already on its screen when its width
//! changes, and how many rows the region of cards occupies as a result.
//!
//! The driver moves the cursor up to the first row of the region before it
//! draws the region again. Too few rows leave a fragment of an old card on
//! the screen. Too many erase a finished line, which must never happen. So
//! every guess in this module rounds down, and a terminal that is not
//! known to lay lines out again is treated as one that does not.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// What a terminal does to old lines when its width changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reflow {
    /// A line wider than the new width is laid out again over more rows.
    Rewraps,
    /// Every line keeps its row and loses what no longer fits.
    Cuts,
}

/// The `TERM_PROGRAM` values of terminals that lay lines out again. The
/// reflow of iTerm2, Terminal.app and WezTerm rests on the reference
/// https://terminfo-dev.pages.dev/extensions/text-reflow ("Supported by
/// iTerm2, WezTerm, foot, and Terminal.app") and that of ghostty on its
/// git history ("When a terminal is resized with text reflow ..."). Both
/// are third-party; the other terminals below have the source in their own
/// repositories.
const REFLOWING_PROGRAMS: [&str; 4] = ["iTerm.app", "Apple_Terminal", "WezTerm", "ghostty"];

impl Reflow {
    /// Every variable [`Reflow::detect`] reads. A test that needs a known
    /// answer removes these from the environment it hands to a program.
    #[cfg(test)]
    pub(crate) const VARIABLES: [&str; 10] = [
        "XTERM_VERSION",
        "STY",
        "INSIDE_EMACS",
        "VIM_TERMINAL",
        "TMUX",
        "TERM",
        "VTE_VERSION",
        "KITTY_WINDOW_ID",
        "ALACRITTY_LOG",
        "TERM_PROGRAM",
    ];

    /// Names the terminal from the environment `get` reads.
    ///
    /// A terminal passes its variables on to every program started from it,
    /// including another terminal. The rules therefore lean towards `Cuts`:
    ///
    /// 1. `XTERM_VERSION` (xterm exports it, and xterm cuts), `STY` (GNU
    ///    screen), `INSIDE_EMACS` (Emacs `M-x shell` and `term`) and
    ///    `VIM_TERMINAL` (Vim's `:terminal`) win over everything else: a
    ///    terminal emulated inside a program keeps its width.
    /// 2. In a tmux pane, `TMUX` is set and `TERM` starts with `tmux` or
    ///    `screen`. tmux lays lines out again (its CHANGES: "Support wide
    ///    characters properly when reflowing"), and `TMUX` comes from
    ///    `environ_for_session` in its `environ.c`. Whatever the outer
    ///    terminal exported does not matter inside it.
    /// 3. Otherwise exactly one terminal family must be named, and `TERM`
    ///    must be one the family exports. A terminal that sets no variable
    ///    of its own (st, for one) exports its own `TERM` and fails this
    ///    test whichever terminal it was started from. The families:
    ///    - `VTE_VERSION` (set by VTE in `spawn.cc`; VTE reflows since
    ///      0.35), with `TERM` `xterm*`, as VTE terminals export;
    ///    - `KITTY_WINDOW_ID` (`kitty/tabs.py`; `rewrap` in
    ///      `kitty/resize.c`), with `TERM=xterm-kitty`, the default of its
    ///      `term` option in `kitty/options/definition.py`;
    ///    - `ALACRITTY_LOG` (set at start in `alacritty/src/logging.rs`;
    ///      CHANGELOG 0.3.0: "Text will reflow instead of truncating when
    ///      resizing Alacritty"), with `TERM=alacritty` or `xterm*`, which
    ///      it falls back to without its terminfo entry;
    ///    - a `TERM_PROGRAM` of [`REFLOWING_PROGRAMS`], with `TERM` `xterm*`
    ///      (iTerm2, Terminal.app, WezTerm), `wezterm` (WezTerm's `term`
    ///      setting) or `xterm-ghostty` (the default of `term` in ghostty's
    ///      `Config.zig`).
    ///
    ///    Two families mean a terminal started from another one, and which
    ///    of them it is stays unknown.
    pub(crate) fn detect(get: impl Fn(&str) -> Option<String>) -> Reflow {
        let set = |name: &str| get(name).is_some_and(|value| !value.is_empty());
        if set("XTERM_VERSION") || set("STY") || set("INSIDE_EMACS") || set("VIM_TERMINAL") {
            return Reflow::Cuts;
        }
        let term = get("TERM").unwrap_or_default();
        if set("TMUX") && (term.starts_with("tmux") || term.starts_with("screen")) {
            return Reflow::Rewraps;
        }
        let xterm = term.starts_with("xterm");
        let program = get("TERM_PROGRAM").unwrap_or_default();
        // Each family that is named, with whether `TERM` is one it exports.
        let named = [
            (set("VTE_VERSION"), xterm),
            (set("KITTY_WINDOW_ID"), term == "xterm-kitty"),
            (set("ALACRITTY_LOG"), term == "alacritty" || xterm),
            (
                REFLOWING_PROGRAMS.contains(&program.as_str()),
                match program.as_str() {
                    "WezTerm" => xterm || term == "wezterm",
                    "ghostty" => xterm || term == "xterm-ghostty",
                    _ => xterm,
                },
            ),
        ];
        let fits: Vec<bool> = named
            .into_iter()
            .filter(|(named, _)| *named)
            .map(|(_, term_fits)| term_fits)
            .collect();
        if fits == [true] {
            Reflow::Rewraps
        } else {
            Reflow::Cuts
        }
    }

    /// The terminal the process runs in, by its environment.
    pub(crate) fn from_process() -> Reflow {
        Reflow::detect(|name| std::env::var(name).ok())
    }
}

/// How many rows `line` takes on a terminal of `columns` columns that
/// lays it out again, at least one.
///
/// A character goes on the row it fits on, a wide one that does not fit on
/// the next. Trailing spaces are left out, since a terminal may drop them
/// when it lays the line out. Terminals that cluster characters (an emoji
/// sequence) and terminals that add up the width of each character disagree
/// about some text; the smaller count is taken.
pub(crate) fn rows_taken(line: &str, columns: usize) -> usize {
    let columns = columns.max(1);
    let plain = without_colour(line);
    let plain = plain.trim_end_matches(' ');
    let widths = |character: char| character.width().unwrap_or(0);
    let by_character: usize = plain.chars().map(widths).sum();
    let as_text = plain.width();
    if as_text < by_character {
        return as_text.div_ceil(columns).max(1);
    }
    let mut rows = 1;
    let mut used = 0;
    for character in plain.chars() {
        let width = widths(character);
        // A character wider than the whole window stays on its row, since
        // moving it down would only give it an empty row again.
        if used > 0 && used + width > columns {
            rows += 1;
            used = 0;
        }
        used += width;
    }
    rows
}

/// `line` without its colour sequences (`ESC [ ... m`).
fn without_colour(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            for end in characters.by_ref() {
                if end == 'm' {
                    break;
                }
            }
        } else {
            plain.push(character);
        }
    }
    plain
}

/// How many rows the cursor climbs from the empty row below `shown` to the
/// first row of it, on a terminal that has `columns` columns now.
///
/// The lines were cut to the width they were drawn for. If the window is
/// narrower now and the terminal lays lines out again, a line takes
/// several rows. A climb past the top of the screen stops there, which is
/// right when the region began above it.
pub(crate) fn rows_to_climb(shown: &[String], columns: usize, reflow: Reflow) -> usize {
    match reflow {
        Reflow::Cuts => shown.len(),
        Reflow::Rewraps => shown.iter().map(|line| rows_taken(line, columns)).sum(),
    }
}

#[cfg(test)]
mod tests;
