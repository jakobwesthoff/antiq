// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::block::{Block, Para};
use super::env::{Env, utf8_locale};
use super::render::{Part, render_blocks, render_parts};
use super::rich::Rich;
use super::symbols::Symbols;

/// A terminal that shows symbols but no colour.
fn bare(width: usize) -> Env {
    Env::new(true, false, true, width)
}

fn colored() -> Env {
    Env::new(true, true, true, 80)
}

fn ascii() -> Env {
    Env::new(true, false, false, 80)
}

fn rich(text: &str) -> Rich {
    Rich::parse(text)
}

// =========================================================
// Environment
// =========================================================

#[test]
fn the_first_non_empty_locale_variable_decides_utf8() {
    assert!(!utf8_locale(None, None, None));
    assert!(!utf8_locale(
        Some("C"),
        Some("en_US.UTF-8"),
        Some("en_US.UTF-8")
    ));
    assert!(utf8_locale(Some(""), Some("en_US.UTF-8"), Some("C")));
    assert!(utf8_locale(None, None, Some("de_DE.utf8")));
    assert!(!utf8_locale(None, Some("POSIX"), Some("en_US.UTF-8")));
}

#[test]
fn symbols_fall_back_to_ascii_without_utf8() {
    let unicode = Symbols::for_env(&bare(80));
    assert_eq!(
        (unicode.bar, unicode.error, unicode.hint, unicode.matched),
        ("▌", "✖", "→", "▸")
    );
    let plain = Symbols::for_env(&ascii());
    assert_eq!(
        (plain.bar, plain.error, plain.hint, plain.matched),
        ("|", "x", "->", ">")
    );
}

// =========================================================
// Rich text
// =========================================================

#[test]
fn backticks_mark_commands_and_paths_and_vanish_in_plain_text() {
    assert_eq!(
        rich("Run `antiq archive -n` first").to_plain(),
        "Run antiq archive -n first"
    );
}

#[test]
fn the_builder_keeps_a_backtick_in_user_data_literal() {
    assert_eq!(Rich::new().path("a`b").to_plain(), "a`b");
}

// =========================================================
// Error blocks
// =========================================================

fn push_error() -> Block {
    Block::error(rich("No push target is configured"))
        .why(rich("antiq push mirrors the antiq collection to a target."))
        .how(rich("Add the `url`."))
        .doing("push the antiq collection")
}

#[test]
fn an_error_has_a_badge_line_a_bar_and_labelled_sections() {
    assert_eq!(
        push_error().render(&bare(80)),
        [
            " ✖ ERROR",
            "▌ No push target is configured",
            "▌",
            "▌ Why it happened",
            "▌   antiq push mirrors the antiq collection to a target.",
            "▌",
            "▌ How to fix it",
            "▌   Add the url.",
            "▌",
            "▌ What antiq was doing",
            "▌   Attempting to push the antiq collection.",
        ]
    );
}

#[test]
fn a_path_in_a_title_keeps_its_spaces() {
    let title = Rich::new()
        .path(" notes/two  spaces ")
        .text(" is not  an antiq collection");
    assert_eq!(
        Block::error(title).render(&bare(80)),
        [
            " ✖ ERROR",
            "▌  notes/two  spaces  is not an antiq collection"
        ]
    );
}

#[test]
fn a_path_with_spaces_is_not_broken_across_lines() {
    let title = Rich::new().text("Cannot read").path("some dir/with  gaps");
    let lines = Block::error(title).render(&bare(30));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("some dir/with  gaps")),
        "{lines:?}"
    );
}

#[test]
fn an_error_leaves_out_sections_without_content() {
    let block = Block::error(rich("Broken")).doing("do it");
    assert_eq!(
        block.render(&bare(80)),
        [
            " ✖ ERROR",
            "▌ Broken",
            "▌",
            "▌ What antiq was doing",
            "▌   Attempting to do it.",
        ]
    );
    assert_eq!(
        Block::error(rich("Broken")).render(&bare(80)),
        [" ✖ ERROR", "▌ Broken"]
    );
}

#[test]
fn preformatted_lines_and_blank_lines_are_not_wrapped() {
    let block = Block::error(rich("No target"))
        .how(Para::pre("[target]"))
        .how(Para::Blank)
        .how(rich("Then run it."));
    assert_eq!(
        block.render(&bare(80)),
        [
            " ✖ ERROR",
            "▌ No target",
            "▌",
            "▌ How to fix it",
            "▌   [target]",
            "▌",
            "▌   Then run it.",
        ]
    );
}

#[test]
fn an_error_in_ascii_uses_ascii_symbols() {
    assert_eq!(
        Block::error(rich("Broken")).render(&ascii()),
        [" x ERROR", "| Broken"]
    );
}

// =========================================================
// Warning, info and hint
// =========================================================

#[test]
fn a_warning_puts_badge_and_title_on_one_line_above_the_bar() {
    let block = Block::warning(rich("Refused projects/dotfiles"))
        .text(rich("The piece is pinned."))
        .fix(rich("`antiq unpin projects/dotfiles`, then archive again."));
    assert_eq!(
        block.render(&bare(80)),
        [
            " ⚠ WARNING  Refused projects/dotfiles",
            "▌   The piece is pinned.",
            "▌ → antiq unpin projects/dotfiles, then archive again.",
        ]
    );
}

#[test]
fn a_title_only_info_is_one_line() {
    assert_eq!(
        Block::info(rich("Nothing was deleted.")).render(&bare(80)),
        [" ℹ INFO  Nothing was deleted."]
    );
}

#[test]
fn a_block_with_an_empty_title_is_its_badge_alone() {
    assert_eq!(Block::info(Rich::new()).render(&bare(80)), [" ℹ INFO"]);
}

#[test]
fn a_hint_is_inline_and_wrapped_text_hangs_under_the_text() {
    let block = Block::hint(rich("one two three four five six"));
    assert_eq!(
        block.render(&bare(40)),
        [" → HINT  one two three four five six"]
    );
    let long = Block::hint(rich("aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd"));
    assert_eq!(
        long.render(&bare(40)),
        [
            " → HINT  aaaaaaaaaa bbbbbbbbbb",
            "         cccccccccc dddddddddd",
        ]
    );
}

// =========================================================
// Wrapping
// =========================================================

#[test]
fn body_text_wraps_at_the_width_and_not_below_forty_columns() {
    let words = "aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd";
    let block = Block::warning(rich("Title")).text(rich(words));
    assert_eq!(
        block.render(&bare(40)),
        [
            " ⚠ WARNING  Title",
            "▌   aaaaaaaaaa bbbbbbbbbb cccccccccc",
            "▌   dddddddddd",
        ]
    );
    assert_eq!(
        block.render(&bare(39)),
        [" ⚠ WARNING  Title", &format!("▌   {words}")]
    );
}

// =========================================================
// Colour
// =========================================================

#[test]
fn colour_is_named_ansi_and_applies_to_badge_bar_title_and_markup() {
    let block = Block::warning(rich("Refused"))
        .text(rich("Run `x` now."))
        .fix(rich("Do it."));
    assert_eq!(
        block.render(&colored()),
        [
            "\x1b[30m\x1b[43m ⚠ WARNING \x1b[0m \x1b[1mRefused\x1b[0m",
            "\x1b[1m\x1b[33m▌\x1b[0m   Run \x1b[1mx\x1b[0m now.",
            "\x1b[1m\x1b[33m▌\x1b[0m \x1b[1m\x1b[33m→\x1b[0m Do it.",
        ]
    );
}

#[test]
fn words_of_one_style_share_one_run_per_line() {
    let env = Env::new(true, true, true, 42);
    let block = Block::error(rich(
        "alpha beta gamma `delta epsilon` zeta eta theta iota kappa",
    ));
    let bar = "\x1b[1m\x1b[31m▌\x1b[0m";
    assert_eq!(
        block.render(&env),
        [
            "\x1b[30m\x1b[41m ✖ ERROR \x1b[0m".to_string(),
            format!("{bar} \x1b[1mAlpha beta gamma delta epsilon zeta eta\x1b[0m"),
            format!("{bar} \x1b[1mtheta iota kappa\x1b[0m"),
        ]
    );
}

#[test]
fn a_space_between_two_styles_stays_unstyled() {
    let block = Block::warning(rich("Title")).text(rich("Run `a b` now, `c` or `d e`."));
    let lines = block.render(&colored());
    assert_eq!(
        lines[1],
        "\x1b[1m\x1b[33m▌\x1b[0m   Run \x1b[1ma b\x1b[0m now, \x1b[1mc\x1b[0m or \x1b[1md e\x1b[0m."
    );
}

#[test]
fn no_colour_means_no_escape_byte_anywhere() {
    let lines = push_error().render(&bare(80));
    assert!(lines.iter().all(|line| !line.contains('\x1b')));
}

// =========================================================
// Pipes and several blocks
// =========================================================

#[test]
fn a_stderr_pipe_gets_greppable_plain_lines() {
    assert_eq!(
        push_error().render_plain(),
        [
            "error: No push target is configured",
            "  why: antiq push mirrors the antiq collection to a target.",
            "  fix: Add the url.",
            "  while attempting to push the antiq collection",
        ]
    );
    let warning = Block::warning(rich("Refused"))
        .text(rich("Pinned."))
        .fix(rich("Unpin it."));
    assert_eq!(
        warning.render_plain(),
        ["warning: Refused", "  Pinned.", "  fix: Unpin it."]
    );
}

#[test]
fn a_block_with_a_pipe_line_gives_a_pipe_that_line_and_nothing_else() {
    let warning = Block::warning(rich("Unknown setting"))
        .text(rich("antiq ignores it."))
        .fix(rich("Remove it."))
        .pipe("warning: the key `x` is unknown");
    assert_eq!(warning.render_plain(), ["warning: the key `x` is unknown"]);
    let info = Block::info(rich("Repaired")).pipe("note: repaired: x");
    assert_eq!(render_blocks(&[info], &Env::pipe()), ["note: repaired: x"]);
}

#[test]
fn info_and_hint_have_no_pipe_form() {
    assert!(Block::info(rich("x")).render_plain().is_empty());
    assert!(Block::hint(rich("x")).render_plain().is_empty());
}

#[test]
fn blocks_are_separated_by_an_empty_line_when_rich_and_not_in_a_pipe() {
    let blocks = [Block::warning(rich("One")), Block::warning(rich("Two"))];
    assert_eq!(
        render_blocks(&blocks, &bare(80)),
        [" ⚠ WARNING  One", "", " ⚠ WARNING  Two"]
    );
    assert_eq!(
        render_blocks(&blocks, &Env::new(false, false, true, 80)),
        ["warning: One", "warning: Two"]
    );
}

#[test]
fn a_plain_error_lists_candidates_before_every_step_innermost_first() {
    let block = Block::error(rich("`scan` matches 2 pieces"))
        .candidate("projects/scanner")
        .candidate("tools/scan-old")
        .doing("find the piece")
        .doing("start the run");
    assert_eq!(
        block.render_plain(),
        [
            "error: scan matches 2 pieces",
            "    candidate: projects/scanner",
            "    candidate: tools/scan-old",
            "  while attempting to find the piece",
            "  while attempting to start the run",
        ]
    );
}

#[test]
fn a_rich_error_lists_every_step_under_the_heading() {
    let block = Block::error(rich("Broken"))
        .doing("find the piece")
        .doing("start the run");
    assert_eq!(
        block.render(&bare(80)),
        [
            " ✖ ERROR",
            "▌ Broken",
            "▌",
            "▌ What antiq was doing",
            "▌   Attempting to find the piece.",
            "▌   Attempting to start the run.",
        ]
    );
}

#[test]
fn a_rich_error_leaves_its_candidates_to_the_table_under_it() {
    let block = Block::error(rich("Ambiguous"))
        .candidate("projects/scanner")
        .candidate("tools/scan-old");
    assert_eq!(block.render(&bare(80)), [" ✖ ERROR", "▌ Ambiguous"]);
}

#[test]
fn a_group_is_its_parts_with_an_empty_line_between_them_on_a_terminal() {
    let parts = [
        Part::Block(Block::error(rich("Ambiguous"))),
        Part::Terminal(vec!["The 2 pieces".to_string(), "PIECE".to_string()]),
        Part::Block(Block::info(rich("Nothing was unearthed."))),
    ];
    assert_eq!(
        render_parts(&parts, &bare(80)),
        [
            " ✖ ERROR",
            "▌ Ambiguous",
            "",
            "The 2 pieces",
            "PIECE",
            "",
            " ℹ INFO  Nothing was unearthed.",
        ]
    );
}

#[test]
fn a_group_on_a_pipe_keeps_the_plain_lines_of_its_blocks_only() {
    let parts = [
        Part::Block(Block::error(rich("Ambiguous")).candidate("projects/scanner")),
        Part::Terminal(vec!["PIECE".to_string()]),
        Part::Block(Block::info(rich("Nothing was unearthed."))),
    ];
    assert_eq!(
        render_parts(&parts, &Env::pipe()),
        ["error: Ambiguous", "    candidate: projects/scanner"]
    );
}

// =========================================================
// Code spans and titles
// =========================================================

#[test]
fn a_span_with_a_longer_fence_holds_backticks() {
    let parsed = Rich::parse("piece ``a`b`` is `x`");
    assert_eq!(
        parsed,
        Rich::new().text("piece ").cmd("a`b").text(" is ").cmd("x")
    );
    let padded = Rich::parse("`` `a `` end");
    assert_eq!(padded, Rich::new().cmd("`a").text(" end"));
}

#[test]
fn a_run_of_backticks_without_a_closing_run_is_plain_text() {
    assert_eq!(Rich::parse("a ``b` c").to_plain(), "a ``b` c");
}

#[test]
fn the_title_of_an_error_block_starts_with_a_capital_in_the_rich_form_only() {
    let block = Block::error(Rich::new().text("cannot read ").path("a.toml"));
    assert_eq!(block.render(&bare(80))[1], "▌ Cannot read a.toml");
    assert_eq!(block.render_plain()[0], "error: cannot read a.toml");
    let warning = Block::warning(Rich::new().text("unknown key"));
    assert_eq!(warning.render(&bare(80))[0], " ⚠ WARNING  unknown key");
    assert_eq!(Rich::new().capitalised(), Rich::new());
}
