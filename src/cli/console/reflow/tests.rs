// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use std::cell::RefCell;
use std::collections::BTreeSet;

// =========================================================
// How many rows a line takes
// =========================================================

#[test]
fn a_line_that_fits_takes_one_row() {
    assert_eq!(rows_taken("abcd", 10), 1);
}

#[test]
fn an_empty_line_takes_one_row() {
    assert_eq!(rows_taken("", 10), 1);
}

#[test]
fn a_line_as_wide_as_the_window_takes_one_row() {
    assert_eq!(rows_taken("abcd", 4), 1);
}

#[test]
fn one_column_more_than_the_window_takes_a_second_row() {
    assert_eq!(rows_taken("abcde", 4), 2);
}

#[test]
fn a_wide_character_that_does_not_fit_moves_to_the_next_row() {
    // "ab" fills two of three columns, so the wide character goes down and
    // "c" joins it. "d" no longer fits there.
    assert_eq!(rows_taken("ab世cd", 3), 3);
}

#[test]
fn colour_sequences_take_no_columns() {
    assert_eq!(rows_taken("\x1b[32mabcd\x1b[0m", 4), 1);
}

#[test]
fn spaces_at_the_end_do_not_count() {
    // A terminal that drops blank cells when it lays the line out again
    // needs two rows here; one that keeps them needs three.
    assert_eq!(rows_taken("abc    ", 5), 1);
    assert_eq!(rows_taken("abcde   \x1b[0m", 5), 1);
}

#[test]
fn spaces_inside_the_line_count() {
    assert_eq!(rows_taken("ab   cd", 5), 2);
}

#[test]
fn no_columns_count_as_one_column() {
    assert_eq!(rows_taken("abc", 0), 3);
}

#[test]
fn a_wide_character_alone_on_a_row_takes_that_row_even_when_it_is_too_wide_for_it() {
    assert_eq!(rows_taken("中", 1), 1);
    assert!(rows_taken("中中", 1) <= 2);
}

#[test]
fn an_emoji_sequence_counts_by_the_width_of_the_whole_sequence() {
    // A family emoji is four emoji joined by zero width joiners. Per
    // character it is 8 columns, as a whole it is 2.
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    assert_eq!(rows_taken(family, 2), 1);
}

#[test]
fn a_presentation_selector_keeps_the_smaller_per_character_count() {
    // A heart with VS16 is one column per character and two as a whole. The
    // line is counted as one column wide, the smaller of the two.
    let heart = "\u{2764}\u{FE0F}";
    assert_eq!(rows_taken(heart, 1), 1);
}

#[test]
fn a_text_presentation_selector_keeps_the_smaller_count_of_the_whole() {
    // A wide emoji with VS15 is two columns per character and one as a
    // whole. The line is counted as one column wide.
    let text = "\u{231A}\u{FE0E}";
    assert_eq!(rows_taken(text, 1), 1);
}

// =========================================================
// How far the cursor climbs
// =========================================================

fn lines(widths: &[usize]) -> Vec<String> {
    widths.iter().map(|width| "x".repeat(*width)).collect()
}

#[test]
fn a_terminal_that_cuts_keeps_one_row_per_line() {
    assert_eq!(rows_to_climb(&lines(&[100, 100, 40]), 60, Reflow::Cuts), 3);
}

#[test]
fn a_terminal_that_rewraps_adds_the_rows_of_the_wide_lines() {
    assert_eq!(
        rows_to_climb(&lines(&[100, 100, 40]), 60, Reflow::Rewraps),
        2 + 2 + 1
    );
}

#[test]
fn without_a_change_of_width_both_kinds_climb_one_row_per_line() {
    let shown = lines(&[60, 59, 10]);
    assert_eq!(rows_to_climb(&shown, 60, Reflow::Rewraps), 3);
    assert_eq!(rows_to_climb(&shown, 60, Reflow::Cuts), 3);
}

#[test]
fn a_wider_window_keeps_one_row_per_line() {
    assert_eq!(rows_to_climb(&lines(&[60, 60]), 100, Reflow::Rewraps), 2);
}

// =========================================================
// Which terminal this is
// =========================================================

fn detect(pairs: &[(&str, &str)]) -> Reflow {
    Reflow::detect(|name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
}

#[test]
fn an_empty_environment_is_a_terminal_that_cuts() {
    assert_eq!(detect(&[]), Reflow::Cuts);
}

#[test]
fn a_terminal_that_names_itself_rewraps() {
    for pairs in [
        vec![("VTE_VERSION", "7600"), ("TERM", "xterm-256color")],
        vec![("KITTY_WINDOW_ID", "1"), ("TERM", "xterm-kitty")],
        vec![
            ("ALACRITTY_LOG", "/tmp/Alacritty-1.log"),
            ("TERM", "alacritty"),
        ],
        vec![
            ("ALACRITTY_LOG", "/tmp/Alacritty-1.log"),
            ("TERM", "xterm-256color"),
        ],
        vec![("TERM_PROGRAM", "iTerm.app"), ("TERM", "xterm-256color")],
        vec![
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("TERM", "xterm-256color"),
        ],
        vec![("TERM_PROGRAM", "WezTerm"), ("TERM", "xterm-256color")],
        vec![("TERM_PROGRAM", "WezTerm"), ("TERM", "wezterm")],
        vec![("TERM_PROGRAM", "ghostty"), ("TERM", "xterm-ghostty")],
        vec![("TERM_PROGRAM", "ghostty"), ("TERM", "xterm-256color")],
    ] {
        assert_eq!(detect(&pairs), Reflow::Rewraps, "{pairs:?}");
    }
}

#[test]
fn a_terminal_that_inherited_the_variable_of_another_cuts() {
    // st sets none of the variables and exports its own TERM, so it cuts
    // whichever terminal it was started from.
    for pairs in [
        vec![("VTE_VERSION", "7600"), ("TERM", "st-256color")],
        vec![("KITTY_WINDOW_ID", "1"), ("TERM", "st-256color")],
        vec![("ALACRITTY_LOG", "/tmp/a.log"), ("TERM", "st-256color")],
        vec![("TERM_PROGRAM", "iTerm.app"), ("TERM", "st-256color")],
        vec![("TERM_PROGRAM", "ghostty"), ("TERM", "st-256color")],
        vec![("KITTY_WINDOW_ID", "1"), ("TERM", "xterm-256color")],
        vec![("TERM_PROGRAM", "WezTerm")],
        vec![("VTE_VERSION", "7600")],
    ] {
        assert_eq!(detect(&pairs), Reflow::Cuts, "{pairs:?}");
    }
}

#[test]
fn a_terminal_inside_an_editor_cuts() {
    for marker in ["INSIDE_EMACS", "VIM_TERMINAL"] {
        let pairs = [
            ("VTE_VERSION", "7600"),
            ("TERM", "xterm-256color"),
            (marker, "1"),
        ];
        assert_eq!(detect(&pairs), Reflow::Cuts, "{marker}");
    }
}

#[test]
fn a_terminal_that_is_not_listed_cuts() {
    for pairs in [
        vec![("TERM_PROGRAM", "xterm")],
        vec![("TERM_PROGRAM", "vscode")],
        vec![("WT_SESSION", "9d1c")],
        vec![("VTE_VERSION", "")],
        vec![("TERM", "xterm-256color")],
    ] {
        assert_eq!(detect(&pairs), Reflow::Cuts, "{pairs:?}");
    }
}

#[test]
fn a_tmux_pane_rewraps_whatever_the_outer_terminal_exported() {
    for outer in [
        vec![],
        vec![("VTE_VERSION", "7600")],
        vec![("KITTY_WINDOW_ID", "1"), ("VTE_VERSION", "7600")],
        vec![("TERM_PROGRAM", "tmux")],
    ] {
        for term in ["tmux-256color", "screen-256color"] {
            let mut pairs = vec![("TMUX", "/tmp/tmux-501/default,1,0"), ("TERM", term)];
            pairs.extend(outer.clone());
            assert_eq!(detect(&pairs), Reflow::Rewraps, "{pairs:?}");
        }
    }
}

#[test]
fn a_leftover_tmux_variable_in_another_terminal_is_not_a_tmux_pane() {
    let pairs = [("TMUX", "/tmp/tmux-501/default,1,0"), ("TERM", "xterm")];
    assert_eq!(detect(&pairs), Reflow::Cuts);
}

#[test]
fn xterm_and_screen_cut_even_when_started_from_a_terminal_that_rewraps() {
    for marker in [("XTERM_VERSION", "XTerm(390)"), ("STY", "123.pts-0.host")] {
        let pairs = [("VTE_VERSION", "7600"), marker];
        assert_eq!(detect(&pairs), Reflow::Cuts, "{pairs:?}");
    }
}

#[test]
fn screen_inside_tmux_cuts() {
    let pairs = [
        ("TMUX", "/tmp/tmux-501/default,1,0"),
        ("TERM", "screen"),
        ("STY", "123.pts-0.host"),
    ];
    assert_eq!(detect(&pairs), Reflow::Cuts);
}

#[test]
fn two_terminals_named_at_once_cut() {
    for pairs in [
        vec![("VTE_VERSION", "7600"), ("KITTY_WINDOW_ID", "1")],
        vec![("TERM_PROGRAM", "iTerm.app"), ("VTE_VERSION", "7600")],
        vec![("ALACRITTY_LOG", "/tmp/a.log"), ("TERM_PROGRAM", "ghostty")],
    ] {
        assert_eq!(detect(&pairs), Reflow::Cuts, "{pairs:?}");
    }
}

#[test]
fn tmux_as_program_is_no_terminal_of_its_own() {
    let pairs = [
        ("TERM_PROGRAM", "tmux"),
        ("VTE_VERSION", "7600"),
        ("TERM", "xterm-256color"),
    ];
    assert_eq!(detect(&pairs), Reflow::Rewraps);
}

#[test]
fn detect_reads_only_the_variables_that_are_listed() {
    let asked = RefCell::new(BTreeSet::new());
    let _ = Reflow::detect(|name| {
        asked.borrow_mut().insert(name.to_string());
        None
    });
    let _ = Reflow::detect(|name| {
        asked.borrow_mut().insert(name.to_string());
        Some("1".to_string())
    });
    let listed: BTreeSet<String> = Reflow::VARIABLES.iter().map(|v| v.to_string()).collect();
    assert!(asked.into_inner().is_subset(&listed));
}

#[test]
fn the_tests_that_run_antiq_clear_every_listed_variable_from_its_environment() {
    // `make_hermetic` removes them and sets `TERM` itself.
    let hermetic = include_str!("../../../../tests/support/cmd.rs");
    for name in Reflow::VARIABLES {
        let handled = hermetic.contains(&format!("\"{name}\""))
            || hermetic.contains(&format!("command.env(\"{name}\""));
        assert!(handled, "{name}");
    }
}
