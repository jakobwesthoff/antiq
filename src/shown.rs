// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How names from the user's files appear in text: the one rule for every
//! line antiq prints.
//!
//! A name can hold characters that end a line, forge another one, move the
//! cursor or make the name look different from what it is. Text shows each
//! of them as `%` and the upper-case hex digits of each of its UTF-8 bytes.
//! The characters are the Unicode control characters (category Cc), the bidi
//! controls U+202A to U+202E and U+2066 to U+2069, and U+2028 and U+2029.

/// Whether text shows `character` in its escaped form.
pub fn is_escaped(character: char) -> bool {
    character.is_control()
        || matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        || matches!(character, '\u{2028}' | '\u{2029}')
}

/// `text` with every escaped character written as `%` and the hex digits of
/// each of its bytes. Everything else stays as it is, `%` included, so text
/// that is already in the escaped form of [`crate::relpath::escape`] (its
/// `%` is a `%25` already) passes unchanged, and so does the result of this
/// function: applying it twice changes nothing.
pub fn escape_controls(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        if is_escaped(character) {
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

/// The positions `matched` (counted in characters of `raw`) as positions in
/// `escape_controls(raw)`. A character that is escaped takes several
/// positions; one that was matched marks all of them.
pub fn positions(raw: &str, matched: &[usize]) -> Vec<usize> {
    let mut shown = Vec::new();
    let mut start = 0;
    for (position, character) in raw.chars().enumerate() {
        // The width of one character's escape is whatever `escape_controls`
        // makes of it.
        let end = start
            + escape_controls(character.encode_utf8(&mut [0; 4]))
                .chars()
                .count();
        if matched.contains(&position) {
            shown.extend(start..end);
        }
        start = end;
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::{escape_controls, positions};

    #[test]
    fn a_name_without_escaped_characters_stays_as_it_is() {
        assert_eq!(escape_controls("plain name"), "plain name");
        assert_eq!(
            escape_controls("100%25 \u{fc}\u{e4}"),
            "100%25 \u{fc}\u{e4}"
        );
    }

    #[test]
    fn control_characters_are_escaped_by_their_bytes() {
        assert_eq!(escape_controls("a\nb\tc"), "a%0Ab%09c");
        assert_eq!(escape_controls("\x1b[31m"), "%1B[31m");
        assert_eq!(escape_controls("a\x7fb"), "a%7Fb");
        // U+0085 is a control character that takes two bytes in UTF-8.
        assert_eq!(escape_controls("x\u{85}y"), "x%C2%85y");
    }

    #[test]
    fn a_bidi_override_is_escaped() {
        assert_eq!(escape_controls("a\u{202e}b"), "a%E2%80%AEb");
    }

    #[test]
    fn every_bidi_control_is_escaped() {
        for character in ('\u{202a}'..='\u{202e}').chain('\u{2066}'..='\u{2069}') {
            let shown = escape_controls(&character.to_string());
            assert!(shown.starts_with("%E2%"), "{character:?}: {shown}");
            assert_eq!(shown.len(), 9, "{character:?}: {shown}");
        }
    }

    #[test]
    fn the_line_and_paragraph_separators_are_escaped() {
        assert_eq!(escape_controls("a\u{2028}b"), "a%E2%80%A8b");
        assert_eq!(escape_controls("a\u{2029}b"), "a%E2%80%A9b");
    }

    #[test]
    fn the_characters_next_to_the_escaped_ones_stay() {
        for character in ['\u{2027}', '\u{202f}', '\u{2065}', '\u{206a}', '\u{a0}'] {
            let text = character.to_string();
            assert_eq!(escape_controls(&text), text, "{character:?}");
        }
    }

    #[test]
    fn escaping_twice_changes_nothing() {
        for text in ["a\nb", "100%25", "\u{202e}%\u{2028}x\x1b", "%0A"] {
            let once = escape_controls(text);
            assert_eq!(escape_controls(&once), once, "{text:?}");
        }
    }

    #[test]
    fn positions_keep_the_positions_of_a_name_without_escaped_characters() {
        assert_eq!(positions("projects/scanner", &[9, 10]), [9, 10]);
        assert!(positions("a\tb", &[]).is_empty());
    }

    #[test]
    fn positions_move_past_the_escape_of_a_character() {
        // `a%09b/scanner` holds `scan` from the seventh character on.
        assert_eq!(positions("a\tb/scanner", &[4, 5, 6, 7]), [6, 7, 8, 9]);
        // `x%C2%85y`: each of the two bytes of U+0085 is a three-character escape.
        assert_eq!(positions("x\u{85}y", &[2]), [7]);
    }

    #[test]
    fn a_matched_escaped_character_marks_its_whole_escape() {
        assert_eq!(positions("a\tb", &[1]), [1, 2, 3]);
        assert_eq!(positions("x\u{85}y", &[1]), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn positions_cover_exactly_the_characters_escape_controls_writes() {
        for raw in [
            "a\tb",
            "x\u{85}y",
            "e\u{301}",
            "100%25",
            "\u{65e5}\u{672c}",
            "a\u{202e}b",
            "",
        ] {
            let every_raw_position: Vec<usize> = (0..raw.chars().count()).collect();
            let every_shown_position: Vec<usize> =
                (0..escape_controls(raw).chars().count()).collect();
            assert_eq!(
                positions(raw, &every_raw_position),
                every_shown_position,
                "{raw:?}"
            );
        }
    }
}
