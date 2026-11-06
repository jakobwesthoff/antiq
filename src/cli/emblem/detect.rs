// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reads the facts about the terminal behind stdout that the choice of a
//! rendering needs.

use super::{CellShape, TerminalFacts, glyphs, tmux};
use crate::cli::console::env::Env;

/// The facts about stdout, whose console environment is `env`.
///
/// Stdout that is no terminal, takes no colour or no 24-bit colour gets no
/// emblem whatever else is true, so tmux is not asked then. Nothing here
/// writes an escape sequence to the terminal to ask it.
pub fn facts_of_stdout(env: &Env) -> TerminalFacts {
    let var = |name: &str| std::env::var(name).ok();
    let truecolor = truecolor_term(var("COLORTERM").as_deref());
    let window = rustix::termios::tcgetwinsize(std::io::stdout()).ok();
    let columns = window.map_or(env.width, |size| usize::from(size.ws_col));
    let rows = window.map_or(0, |size| usize::from(size.ws_row));

    let mut cell = window.and_then(|size| {
        CellShape::of_window(size.ws_xpixel, size.ws_ypixel, size.ws_col, size.ws_row)
    });
    let in_tmux = var("TMUX").is_some_and(|value| !value.is_empty());
    let wanted = env.rich && env.color && truecolor;
    let client = if wanted && in_tmux { tmux::ask() } else { None };
    if cell.is_none() {
        cell = client
            .as_ref()
            .and_then(|client| client.cell_pixels)
            .and_then(|(width, height)| CellShape::of_cell(width, height));
    }
    let termtype = client
        .as_ref()
        .and_then(|client| client.termtype.as_deref());
    TerminalFacts {
        terminal: env.rich,
        color: env.color,
        truecolor,
        columns,
        rows,
        cell,
        glyphs: glyphs::for_terminal(var, termtype),
    }
}

/// Whether `COLORTERM` says the terminal takes 24-bit colour.
pub fn truecolor_term(colorterm: Option<&str>) -> bool {
    colorterm.is_some_and(|value| value == "truecolor" || value == "24bit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_truecolor_and_24bit_name_24_bit_colour() {
        assert!(truecolor_term(Some("truecolor")));
        assert!(truecolor_term(Some("24bit")));
        assert!(!truecolor_term(Some("yes")));
        assert!(!truecolor_term(Some("")));
        assert!(!truecolor_term(None));
    }
}
