// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The colours of the console. Only the named ANSI colours are used, so
//! the terminal's own theme decides the shades.

use super::block::Kind;
use anstyle::{AnsiColor, Style};

pub const BOLD: Style = Style::new().bold();
pub const DIM: Style = Style::new().dimmed();
pub const RED: Style = AnsiColor::Red.on_default();
pub const YELLOW: Style = AnsiColor::Yellow.on_default();
pub const GREEN: Style = AnsiColor::Green.on_default();
pub const MAGENTA: Style = AnsiColor::Magenta.on_default();
pub const CYAN: Style = AnsiColor::Cyan.on_default();
/// A piece that failed or was refused.
pub const FAILED: Style = AnsiColor::Red.on_default().bold();
/// The part of a name a search matched.
pub const MATCH: Style = AnsiColor::Red.on_default().bold();

const fn accent(kind: Kind) -> AnsiColor {
    match kind {
        Kind::Error => AnsiColor::Red,
        Kind::Warning => AnsiColor::Yellow,
        Kind::Info | Kind::Hint => AnsiColor::Cyan,
    }
}

/// Black text on the colour of the kind.
pub fn badge(kind: Kind) -> Style {
    Style::new()
        .fg_color(Some(AnsiColor::Black.into()))
        .bg_color(Some(accent(kind).into()))
}

/// The bar and the arrow of a block.
pub const fn bar(kind: Kind) -> Style {
    accent(kind).on_default().bold()
}

/// The headings of an error block.
pub const fn label(kind: Kind) -> Style {
    accent(kind).on_default()
}

/// The colours of the help and the usage errors that clap prints: the
/// headings and the usage line in bold, options and commands as the code
/// spans of the blocks, placeholders dim, and the error, valid and invalid
/// words in the colours of an error, an info and a warning.
pub const fn clap_styles() -> clap::builder::Styles {
    clap::builder::Styles::styled()
        .header(BOLD)
        .usage(BOLD)
        .literal(BOLD)
        .placeholder(DIM)
        .error(bar(Kind::Error))
        .valid(label(Kind::Info))
        .invalid(label(Kind::Warning))
}
