// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which glyph set the emblem is drawn in.
//!
//! No escape sequence tells whether a terminal can show a glyph, and where
//! neither the terminal nor its font draws sextants they come out as boxes
//! of a missing glyph. So the choice goes by the terminal's identity: the
//! terminals that draw sextants themselves, from the first version that does.
//! A terminal that is not on the list gets block elements, even if its font
//! has sextants.

use super::Glyphs;

/// The glyphs for the terminal named by the environment, where `var` reads
/// a variable (an empty one counts as unset). Inside tmux the environment
/// is that of the terminal tmux was started from, which may not be the one
/// a client attached from later, so tmux's answer about its client decides:
/// `client_termtype`, the client terminal's answer to XTVERSION. Without
/// that answer a tmux pane gets blocks.
pub fn for_terminal(
    var: impl Fn(&str) -> Option<String>,
    tmux_client_termtype: Option<&str>,
) -> Glyphs {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    if set("TMUX").is_some() {
        return tmux_client_termtype.map_or(Glyphs::Block, from_xtversion);
    }
    let term = set("TERM").unwrap_or_default();
    let version = || set("TERM_PROGRAM_VERSION").unwrap_or_default();
    // GNU screen and Zellij draw their own screen in a terminal that cannot
    // be asked about, and pass on the variables of whichever terminal they
    // were started from.
    if term.starts_with("screen") || set("ZELLIJ").is_some() {
        return Glyphs::Block;
    }
    // A terminal sets `TERM_PROGRAM` itself, while a `KITTY_WINDOW_ID` or an
    // `ALACRITTY_WINDOW_ID` may be left from the terminal another one was
    // started from. So a `TERM_PROGRAM` decides first, also one that is not
    // on the list.
    if let Some(program) = set("TERM_PROGRAM") {
        return match program.as_str() {
            "ghostty" => Glyphs::Sextant,
            "WezTerm" => from_version(&version(), &[20_210_814]),
            "iTerm.app" => from_version(&version(), &[3, 6, 7]),
            "rio" => from_version(&version(), &[0, 4, 6]),
            _ => Glyphs::Block,
        };
    }
    // iTerm2's variables come along over ssh, where `TERM_PROGRAM` stays
    // behind.
    if set("LC_TERMINAL").as_deref() == Some("iTerm2") {
        let version = set("LC_TERMINAL_VERSION").unwrap_or_default();
        return from_version(&version, &[3, 6, 7]);
    }
    if let Some(vte) = set("VTE_VERSION") {
        return from_version(&vte, &[6000]);
    }
    if let Some(konsole) = set("KONSOLE_VERSION") {
        return from_version(&konsole, &[220_400]);
    }
    // Ghostty's and kitty's `TERM` come along over ssh too. kitty and
    // Alacritty name no version, and Contour is on the list without one.
    let known = term == "xterm-ghostty"
        || term == "xterm-kitty"
        || term.starts_with("foot")
        || term == "alacritty"
        || set("TERMINAL_NAME").as_deref() == Some("contour")
        || set("KITTY_WINDOW_ID").is_some()
        || set("ALACRITTY_WINDOW_ID").is_some();
    if known {
        Glyphs::Sextant
    } else {
        Glyphs::Block
    }
}

/// The glyphs for the terminal that answered XTVERSION with `text`, such as
/// `kitty(0.35.2)` or `iTerm2 3.6.7`. Alacritty does not answer XTVERSION,
/// so it gets blocks here, and so does a tmux inside tmux.
pub fn from_xtversion(text: &str) -> Glyphs {
    let after = |prefix: &str| text.strip_prefix(prefix);
    if after("kitty(")
        .or(after("foot("))
        .or(after("ghostty "))
        .or(after("contour "))
        .is_some()
    {
        return Glyphs::Sextant;
    }
    let floors: [(&str, &[u32]); 5] = [
        ("WezTerm ", &[20_210_814]),
        ("iTerm2 ", &[3, 6, 7]),
        ("VTE(", &[6000]),
        ("Konsole ", &[220_400]),
        ("Rio ", &[0, 4, 6]),
    ];
    floors
        .iter()
        .find_map(|(prefix, floor)| after(prefix).map(|version| from_version(version, floor)))
        .unwrap_or(Glyphs::Block)
}

/// Sextants when `version` is `floor` or later, otherwise blocks, also when
/// `version` cannot be read.
fn from_version(version: &str, floor: &[u32]) -> Glyphs {
    match version_numbers(version) {
        Some(numbers) if !below(&numbers, floor) => Glyphs::Sextant,
        _ => Glyphs::Block,
    }
}

/// The numbers at the start of `version`, such as `[3, 6, 7]` for `3.6.7beta2`
/// and `[20240203]` for `20240203-110809-5046fc22`: the digits of each part
/// between dots, up to the first part that goes on with something else.
/// `None` when `version` does not start with a digit.
fn version_numbers(version: &str) -> Option<Vec<u32>> {
    let mut numbers = Vec::new();
    for part in version.split('.') {
        let digits = part.len() - part.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let Ok(number) = part[..digits].parse() else {
            break;
        };
        numbers.push(number);
        if digits < part.len() {
            break;
        }
    }
    (!numbers.is_empty()).then_some(numbers)
}

/// Whether `numbers` come before `floor`, a missing number counting as 0.
fn below(numbers: &[u32], floor: &[u32]) -> bool {
    let length = numbers.len().max(floor.len());
    let padded = |of: &[u32]| {
        let mut padded = of.to_vec();
        padded.resize(length, 0);
        padded
    };
    padded(numbers) < padded(floor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Glyphs::{Block, Sextant};

    fn glyphs(vars: &[(&str, &str)], tmux: Option<&str>) -> Glyphs {
        for_terminal(
            |name| {
                vars.iter()
                    .find(|(set, _)| *set == name)
                    .map(|(_, value)| (*value).to_owned())
            },
            tmux,
        )
    }

    #[test]
    fn terminals_that_draw_sextants_themselves_get_them() {
        for vars in [
            &[("TERM_PROGRAM", "ghostty")][..],
            &[("TERM", "xterm-ghostty")],
            &[("TERM", "xterm-kitty")],
            &[("KITTY_WINDOW_ID", "1")],
            &[
                ("TERM_PROGRAM", "WezTerm"),
                ("TERM_PROGRAM_VERSION", "20240203-110809-5046fc22"),
            ],
            &[
                ("TERM_PROGRAM", "iTerm.app"),
                ("TERM_PROGRAM_VERSION", "3.6.9"),
            ],
            &[("LC_TERMINAL", "iTerm2"), ("LC_TERMINAL_VERSION", "3.6.9")],
            &[("TERM", "foot-extra")],
            &[("VTE_VERSION", "7802")],
            &[("KONSOLE_VERSION", "240802")],
            &[("TERM", "alacritty")],
            &[("ALACRITTY_WINDOW_ID", "4294967299")],
            &[("TERM_PROGRAM", "rio"), ("TERM_PROGRAM_VERSION", "0.4.7")],
            &[("TERMINAL_NAME", "contour")],
        ] {
            assert_eq!(glyphs(vars, None), Sextant, "{vars:?}");
        }
    }

    #[test]
    fn older_versions_and_unknown_terminals_get_blocks() {
        for vars in [
            &[][..],
            &[("TERM", "xterm-256color")],
            &[("TERM_PROGRAM", "Apple_Terminal")],
            &[
                ("TERM_PROGRAM", "iTerm.app"),
                ("TERM_PROGRAM_VERSION", "3.5.0"),
            ],
            &[("TERM_PROGRAM", "iTerm.app")],
            &[
                ("TERM_PROGRAM", "WezTerm"),
                ("TERM_PROGRAM_VERSION", "20200101-000000-abc"),
            ],
            &[("VTE_VERSION", "5000")],
            &[("KONSOLE_VERSION", "210000")],
            &[("TERM_PROGRAM", "rio"), ("TERM_PROGRAM_VERSION", "0.4.5")],
        ] {
            assert_eq!(glyphs(vars, None), Block, "{vars:?}");
        }
    }

    #[test]
    fn screen_and_zellij_get_blocks_whatever_terminal_they_started_in() {
        assert_eq!(
            glyphs(
                &[("TERM", "screen-256color"), ("TERM_PROGRAM", "ghostty")],
                None
            ),
            Block
        );
        assert_eq!(
            glyphs(&[("ZELLIJ", "0"), ("TERM_PROGRAM", "ghostty")], None),
            Block
        );
    }

    #[test]
    fn a_terminal_program_decides_before_a_leftover_window_id() {
        assert_eq!(
            glyphs(
                &[("TERM_PROGRAM", "Apple_Terminal"), ("KITTY_WINDOW_ID", "1")],
                None
            ),
            Block
        );
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(
            glyphs(&[("TERM_PROGRAM", ""), ("KITTY_WINDOW_ID", "1")], None),
            Sextant
        );
    }

    #[test]
    fn inside_tmux_the_clients_answer_decides_not_the_environment() {
        let env = [
            ("TMUX", "/tmp/tmux-501/default,1,0"),
            ("TERM_PROGRAM", "Apple_Terminal"),
        ];
        assert_eq!(glyphs(&env, Some("ghostty 1.3.2")), Sextant);
        let env = [
            ("TMUX", "/tmp/tmux-501/default,1,0"),
            ("TERM_PROGRAM", "ghostty"),
        ];
        assert_eq!(glyphs(&env, Some("")), Block);
        assert_eq!(glyphs(&env, None), Block);
    }

    #[test]
    fn an_xtversion_answer_names_the_terminal() {
        for sextant in [
            "kitty(0.35.2)",
            "foot(1.20.0)",
            "ghostty 1.3.2-mouse-fix",
            "contour 0.6.0",
            "WezTerm 20240203-110809-5046fc22",
            "iTerm2 3.6.7",
            "VTE(7802)",
            "Konsole 240802",
            "Rio 0.4.6",
        ] {
            assert_eq!(from_xtversion(sextant), Sextant, "{sextant}");
        }
        for block in [
            "tmux 3.5a",
            "XTerm(388)",
            "iTerm2 3.5.0",
            "VTE(5000)",
            "Rio 0.4.5",
            "",
        ] {
            assert_eq!(from_xtversion(block), Block, "{block}");
        }
    }
}
