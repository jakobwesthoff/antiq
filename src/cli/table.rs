// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pieces of a console table: padding, cutting, wrapping and colour
//! by display width, and the escaping of names that a terminal would act on.

use crate::cli::output::Style;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// From this width on, tables use their wide layout.
#[allow(dead_code, reason = "used again by C1")]
pub(super) const WIDE_FROM: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "used again by C1")]
pub(super) enum Color {
    Plain,
    Dim,
    Bold,
    Red,
    Green,
    Yellow,
    Magenta,
    Cyan,
}

#[allow(dead_code, reason = "used again by C1")]
pub(super) fn paint(style: Style, color: Color, text: &str) -> String {
    let code = match color {
        Color::Plain => return text.to_string(),
        Color::Dim => "\x1b[2m",
        Color::Bold => "\x1b[1m",
        Color::Red => "\x1b[31m",
        Color::Green => "\x1b[32m",
        Color::Yellow => "\x1b[33m",
        Color::Magenta => "\x1b[35m",
        Color::Cyan => "\x1b[36m",
    };
    if style.color && !text.is_empty() {
        format!("{code}{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

#[allow(dead_code, reason = "used again by C1")]
pub(super) fn spaces(count: usize) -> String {
    " ".repeat(count)
}

#[allow(dead_code, reason = "used again by C1")]
pub(super) fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", spaces(width.saturating_sub(text.width())))
}

#[allow(dead_code, reason = "used again by C1")]
pub(super) fn pad_left(text: &str, width: usize) -> String {
    format!("{}{text}", spaces(width.saturating_sub(text.width())))
}

/// Shows control characters of a name in the escaped form of the spec, so
/// that a name cannot move the cursor or end a line.
#[allow(dead_code, reason = "used again by C1")]
pub(crate) fn sanitize(text: &str) -> String {
    let mut shown = String::new();
    for character in text.chars() {
        if character.is_control() {
            let mut buffer = [0u8; 4];
            for byte in character.encode_utf8(&mut buffer).bytes() {
                shown.push_str(&format!("%{byte:02X}"));
            }
        } else {
            shown.push(character);
        }
    }
    shown
}

/// Cuts `text` to `width` columns by replacing its middle with `…`,
/// keeping the start and the end. A text that fits stays as it is.
#[allow(dead_code, reason = "used again by C1")]
pub(crate) fn mid(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let keep = width - 1;
    let head = (keep / 2 + 2).min(keep);
    let tail = keep - head;
    let mut start = String::new();
    let mut used = 0;
    for character in text.chars() {
        let columns = character.width().unwrap_or(0);
        if used + columns > head {
            break;
        }
        start.push(character);
        used += columns;
    }
    let mut end: Vec<char> = Vec::new();
    used = 0;
    for character in text.chars().rev() {
        let columns = character.width().unwrap_or(0);
        if used + columns > tail {
            break;
        }
        end.push(character);
        used += columns;
    }
    end.reverse();
    format!("{start}…{}", end.into_iter().collect::<String>())
}

/// Breaks `text` into lines of at most `width` columns at spaces; a word
/// longer than a line is cut.
#[allow(dead_code, reason = "used again by C1")]
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ').filter(|word| !word.is_empty()) {
        let mut word = word.to_string();
        while word.width() > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let mut cut = String::new();
            let mut used = 0;
            let mut rest = String::new();
            for character in word.chars() {
                let columns = character.width().unwrap_or(0);
                if rest.is_empty() && used + columns <= width {
                    cut.push(character);
                    used += columns;
                } else {
                    rest.push(character);
                }
            }
            if cut.is_empty() {
                // A character wider than the line still has to go somewhere.
                let mut characters = word.chars();
                cut.push(characters.next().expect("the word is not empty"));
                rest = characters.collect();
            }
            lines.push(cut);
            word = rest;
        }
        if line.is_empty() {
            line = word;
        } else if line.width() + 1 + word.width() <= width {
            line.push(' ');
            line.push_str(&word);
        } else {
            lines.push(std::mem::replace(&mut line, word));
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colored() -> Style {
        Style::for_terminal(Some(80), false, true)
    }

    fn plain() -> Style {
        Style::for_terminal(Some(80), true, true)
    }

    #[test]
    fn mid_keeps_the_start_and_the_end() {
        assert_eq!(mid("abcdefghij", 10), "abcdefghij");
        assert_eq!(mid("abcdefghijklmnop", 10), "abcdef…nop");
        assert_eq!(mid("abcdefghijklmnop", 8).chars().count(), 8);
    }

    #[test]
    fn mid_is_total_for_every_width() {
        assert_eq!(mid("abcdef", 0), "");
        assert_eq!(mid("abcdef", 1), "…");
        for width in 0..6 {
            let cut = mid("abcdefghij", width);
            assert!(cut.width() <= width, "{width}: {cut}");
        }
    }

    #[test]
    fn a_word_longer_than_the_line_is_cut() {
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("ab cd ef", 5), ["ab cd", "ef"]);
        assert_eq!(wrap("ab cdefghijkl", 5), ["ab", "cdefg", "hijkl"]);
        assert_eq!(wrap("", 5), [""]);
        assert_eq!(wrap("日本", 1), ["日", "本"]);
    }

    #[test]
    fn sanitize_escapes_control_characters_by_their_bytes() {
        assert_eq!(sanitize("plain name"), "plain name");
        assert_eq!(sanitize("a\nb\tc"), "a%0Ab%09c");
        assert_eq!(sanitize("\x1b[31m"), "%1B[31m");
        // U+0085 is a control character that takes two bytes in UTF-8.
        assert_eq!(sanitize("x\u{85}y"), "x%C2%85y");
    }

    #[test]
    fn spaces_are_as_many_as_asked_for() {
        assert_eq!(spaces(0), "");
        assert_eq!(spaces(3), "   ");
    }

    #[test]
    fn padding_counts_display_columns() {
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(pad_left("ab", 5), "   ab");
        // Each of these characters takes two columns.
        assert_eq!(pad("日本", 6), "日本  ");
        assert_eq!(pad_left("日本", 6), "  日本");
    }

    #[test]
    fn text_wider_than_the_width_is_not_padded_or_cut() {
        assert_eq!(pad("abcdef", 3), "abcdef");
        assert_eq!(pad_left("abcdef", 3), "abcdef");
    }

    #[test]
    fn paint_wraps_text_in_the_escape_of_its_color() {
        let expected = [
            (Color::Dim, "\x1b[2m"),
            (Color::Bold, "\x1b[1m"),
            (Color::Red, "\x1b[31m"),
            (Color::Green, "\x1b[32m"),
            (Color::Yellow, "\x1b[33m"),
            (Color::Magenta, "\x1b[35m"),
            (Color::Cyan, "\x1b[36m"),
        ];
        for (color, code) in expected {
            assert_eq!(paint(colored(), color, "x"), format!("{code}x\x1b[0m"));
        }
    }

    #[test]
    fn paint_leaves_plain_empty_and_uncolored_text_alone() {
        assert_eq!(paint(colored(), Color::Plain, "x"), "x");
        assert_eq!(paint(colored(), Color::Red, ""), "");
        assert_eq!(paint(plain(), Color::Red, "x"), "x");
    }
}
