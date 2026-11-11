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
//! A `%` is shown as `%25` and a byte that is not UTF-8 as `%XX`.
//!
//! Two kinds of text meet this rule. A raw name, such as a `Path`, goes
//! through [`Shown`], which adds the `%25` and the `%XX`. Text that is
//! already in the escaped form of [`crate::relpath::escape`], such as the
//! `Display` of a `RelPath`, has those escapes and only needs
//! [`escape_controls`].

use crate::relpath::{Name, RelPath, escape};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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

/// `text` without its SGR sequences, `ESC [`, digits and semicolons, `m`:
/// the styling antiq writes itself. Any other escape sequence stays, so a
/// cursor movement or a screen erase is still found in it.
pub fn strip_sgr(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("\x1b[") {
        plain.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let digits = after
            .bytes()
            .take_while(|byte| byte.is_ascii_digit() || *byte == b';')
            .count();
        if after.as_bytes().get(digits) == Some(&b'm') {
            rest = &after[digits + 1..];
        } else {
            plain.push_str("\x1b[");
            rest = after;
        }
    }
    plain.push_str(rest);
    plain
}

/// The first character of `line` that no text line of antiq holds: one of
/// the escaped class after the SGR sequences are taken out, other than the
/// tab of a pipe table. Lines are written without their line end.
pub fn first_raw_character(line: &str) -> Option<char> {
    strip_sgr(line)
        .chars()
        .find(|character| is_escaped(*character) && *character != '\t')
}

/// What [`Shown`] can show: a raw name as the bytes the filesystem holds.
pub trait Named {
    fn name_bytes(&self) -> &[u8];
}

impl Named for str {
    fn name_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Named for [u8] {
    fn name_bytes(&self) -> &[u8] {
        self
    }
}

impl Named for String {
    fn name_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Named for OsStr {
    fn name_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Named for OsString {
    fn name_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Named for Path {
    fn name_bytes(&self) -> &[u8] {
        self.as_os_str().as_bytes()
    }
}

impl Named for PathBuf {
    fn name_bytes(&self) -> &[u8] {
        self.as_os_str().as_bytes()
    }
}

impl Named for RelPath {
    fn name_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Named for Name {
    fn name_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<T: Named + ?Sized> Named for &T {
    fn name_bytes(&self) -> &[u8] {
        (**self).name_bytes()
    }
}

/// A name from the user's files, shown by the rule of this module, in the
/// manner of `Path::display`: the value stays raw and only its display is
/// escaped.
pub struct Shown<T>(pub T);

impl<T: Named> fmt::Display for Shown<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `escape` adds the `%25` and the `%XX` of the bytes that are not
        // UTF-8; the control characters are in the valid text it leaves.
        formatter.write_str(&escape_controls(&escape(self.0.name_bytes())))
    }
}

/// `word` as one word of a command line that a shell reads back as the
/// same bytes, and that holds no character of the escaped class.
///
/// A word of plain characters stays as it is. A word with a character of the
/// escaped class is written in the ANSI-C quoting of bash, zsh and ksh,
/// `$'...'`, which a `%0A` in single quotes could not be. Any other word is
/// in single quotes. A `%` is a plain character to a shell, so the word names
/// the path itself and not its shown form.
pub fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        word.to_string()
    } else if word.chars().any(is_escaped) {
        ansi_c(word.as_bytes())
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// `bytes` in the `$'...'` quoting of a shell: printable ASCII and valid
/// characters outside the escaped class as they are, `\`, `'`, newline and tab
/// by their backslash escapes, everything else as `\xHH` per byte.
pub fn ansi_c(bytes: &[u8]) -> String {
    let mut quoted = String::from("$'");
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '\\' => quoted.push_str("\\\\"),
                '\'' => quoted.push_str("\\'"),
                '\n' => quoted.push_str("\\n"),
                '\t' => quoted.push_str("\\t"),
                other if is_escaped(other) => {
                    let mut buffer = [0u8; 4];
                    for byte in other.encode_utf8(&mut buffer).bytes() {
                        quoted.push_str(&format!("\\x{byte:02x}"));
                    }
                }
                other => quoted.push(other),
            }
        }
        for byte in chunk.invalid() {
            quoted.push_str(&format!("\\x{byte:02x}"));
        }
    }
    quoted.push('\'');
    quoted
}

/// Writes `path` as the lossy text `Path::display` would show, for the JSON
/// fields that hold a path as it is. The console shows the same field
/// through [`Shown`].
pub fn serialize_lossy<P: AsRef<Path>, S: serde::Serializer>(
    path: &P,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.as_ref().to_string_lossy())
}

/// `text` of several lines with the escaped characters written out, its
/// line breaks kept. For a help text, whose paragraphs a break separates:
/// a name in it has to be escaped where the text is made, since a break in
/// a name cannot be told from one between paragraphs here.
pub fn escape_lines(text: &str) -> String {
    text.split('\n')
        .map(escape_controls)
        .collect::<Vec<_>>()
        .join("\n")
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
    use super::{
        Shown, ansi_c, escape_controls, escape_lines, first_raw_character, positions, shell_word,
        strip_sgr,
    };
    use crate::relpath::RelPath;
    use std::ffi::OsStr;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    #[test]
    fn sgr_sequences_are_stripped() {
        assert_eq!(strip_sgr("\x1b[1;38;2;1;2;3mab\x1b[0m c"), "ab c");
        assert_eq!(strip_sgr("\x1b[mx"), "x");
    }

    #[test]
    fn a_sequence_that_is_no_sgr_stays() {
        assert_eq!(strip_sgr("\x1b[2Jxm"), "\x1b[2Jxm");
        assert_eq!(strip_sgr("\x1b[31"), "\x1b[31");
        assert_eq!(strip_sgr("\x1b[1Am"), "\x1b[1Am");
    }

    #[test]
    fn styled_text_and_tabs_hold_no_raw_character() {
        assert_eq!(first_raw_character("\x1b[1mname\x1b[0m\tx"), None);
    }

    #[test]
    fn a_raw_control_or_bidi_character_is_found() {
        assert_eq!(first_raw_character("a\nb"), Some('\n'));
        assert_eq!(first_raw_character("\x1b[2Jm"), Some('\x1b'));
        assert_eq!(first_raw_character("x\u{202e}"), Some('\u{202e}'));
        assert_eq!(first_raw_character("\x1b[1m\x1b[31"), Some('\x1b'));
    }

    #[test]
    fn a_plain_word_stays_a_plain_word() {
        assert_eq!(shell_word("projects/a-b_c.d"), "projects/a-b_c.d");
        assert_eq!(shell_word("--exclude=/a"), "--exclude=/a");
        assert_eq!(shell_word("100%"), "100%");
    }

    #[test]
    fn a_word_a_shell_would_split_is_in_single_quotes() {
        assert_eq!(shell_word("my docs"), "'my docs'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word(""), "''");
    }

    #[test]
    fn a_word_with_an_escaped_character_is_in_ansi_c_quotes() {
        assert_eq!(shell_word("a\nb"), "$'a\\nb'");
        assert_eq!(shell_word("a\tb"), "$'a\\tb'");
        assert_eq!(shell_word("a\x1bb"), "$'a\\x1bb'");
        assert_eq!(shell_word("a\u{202e}b"), "$'a\\xe2\\x80\\xaeb'");
        assert_eq!(shell_word("it's 100% \\ \n"), "$'it\\'s 100% \\\\ \\n'");
    }

    #[test]
    fn ansi_c_writes_a_byte_that_is_not_utf_8_as_hex() {
        assert_eq!(ansi_c(b"projects/d\xff"), "$'projects/d\\xff'");
        assert_eq!(ansi_c(b"\xc3\xa4\xff"), "$'\u{e4}\\xff'");
    }

    /// What bash reads back from `word`: its bytes, through `printf %s`.
    fn bash_reads(word: &str) -> Vec<u8> {
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("printf %s {word}"))
            .output()
            .expect("bash runs");
        assert!(output.status.success(), "{word}");
        output.stdout
    }

    #[test]
    fn bash_reads_an_ansi_c_word_back_as_the_exact_bytes() {
        for text in [
            "a\nb",
            "a\tb\x1b[2J",
            "a\u{202e}b\u{2028}",
            "it's \\ 100%\n",
        ] {
            assert_eq!(bash_reads(&shell_word(text)), text.as_bytes(), "{text:?}");
        }
        assert_eq!(bash_reads(&ansi_c(b"d\xff\n")), b"d\xff\n");
    }

    #[test]
    fn bash_reads_a_single_quoted_or_plain_word_back_as_the_exact_bytes() {
        for text in ["plain/path", "my docs", "it's", "a*b?", "100%"] {
            assert_eq!(bash_reads(&shell_word(text)), text.as_bytes(), "{text:?}");
        }
    }

    #[test]
    fn a_percent_sign_in_a_name_is_shown_as_percent_25() {
        assert_eq!(Shown("100%").to_string(), "100%25");
        assert_eq!(
            Shown(Path::new("/tmp/100% done")).to_string(),
            "/tmp/100%25 done"
        );
    }

    #[test]
    fn a_control_character_in_a_path_is_shown_by_its_bytes() {
        assert_eq!(Shown(Path::new("/tmp/a\nb")).to_string(), "/tmp/a%0Ab");
        assert_eq!(Shown("\u{202e}gpj").to_string(), "%E2%80%AEgpj");
    }

    #[test]
    fn a_byte_that_is_not_utf_8_is_shown_as_percent_and_its_hex_digits() {
        assert_eq!(
            Shown(OsStr::from_bytes(b"/tmp/a\xffb")).to_string(),
            "/tmp/a%FFb"
        );
        assert_eq!(
            Shown(Path::new(OsStr::from_bytes(b"a\xc3"))).to_string(),
            "a%C3"
        );
    }

    #[test]
    fn a_relative_path_is_shown_without_a_second_percent_escape() {
        let path = RelPath::from_bytes(b"100%\n".to_vec()).expect("a valid path");
        assert_eq!(Shown(&path).to_string(), "100%25%0A");
        assert_eq!(escape_controls(&path.to_string()), "100%25%0A");
    }

    #[test]
    fn an_owned_os_string_and_a_name_are_shown_by_the_rule() {
        let os_string = std::ffi::OsString::from_vec(b"a\xff%".to_vec());
        assert_eq!(Shown(&os_string).to_string(), "a%FF%25");
        let name = crate::relpath::Name::from_bytes(b"a\nb".to_vec()).expect("a valid name");
        assert_eq!(Shown(&name).to_string(), "a%0Ab");
    }

    #[test]
    fn bytes_are_shown_like_the_name_they_are() {
        assert_eq!(Shown(&b"a%\xff\n"[..]).to_string(), "a%25%FF%0A");
    }

    #[test]
    fn an_owned_value_is_shown_like_a_borrowed_one() {
        assert_eq!(Shown(String::from("a%")).to_string(), "a%25");
        assert_eq!(Shown(std::path::PathBuf::from("a\tb")).to_string(), "a%09b");
    }

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
    fn escape_lines_keeps_the_line_breaks_and_escapes_the_rest() {
        assert_eq!(
            escape_lines("a\n\n  b\x1b\tc\r\nd"),
            "a\n\n  b%1B%09c%0D\nd"
        );
        assert_eq!(escape_lines(""), "");
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
