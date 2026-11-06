// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The block a command line that does not parse is reported as.

use super::console::block::Block;
use super::console::rich::Rich;
use clap::error::{ContextKind, ContextValue};

/// The error block of a clap error: clap's first line as the title, under
/// "How to fix it" what clap suggests and advises, then the usage.
///
/// Everything is read from the context the error carries, so antiq words
/// nothing per kind of error and a new kind of clap error needs no change
/// here.
pub fn block_for(error: &clap::Error) -> Block {
    let rendered = error.render().to_string();
    let mut block = Block::error(Rich::new().text(&super::first_paragraph(&rendered)));
    for fix in suggestions(error) {
        block = block.how(fix);
    }
    if let Some(ContextValue::StyledStr(usage)) = error.get(ContextKind::Usage) {
        block = block.how(Rich::new().text(&one_line(&usage.to_string())));
    }
    block
}

/// What clap suggests as a fix, in the order clap prints it: the command,
/// option or value it thinks was meant, then its tips. A tip is passed on
/// in clap's words, started with a capital letter and ended by a full stop
/// as the other lines of the section are.
fn suggestions(error: &clap::Error) -> Vec<Rich> {
    let mut fixes = Vec::new();
    for kind in [
        ContextKind::SuggestedSubcommand,
        ContextKind::SuggestedArg,
        ContextKind::SuggestedValue,
    ] {
        if let Some(names) = error.get(kind).and_then(context_text) {
            fixes.push(Rich::new().text("Did you mean ").cmd(&names).text("?"));
        }
    }
    if let Some(ContextValue::StyledStrs(tips)) = error.get(ContextKind::Suggested) {
        for tip in tips {
            let tip = tip.to_string();
            let mut letters = tip.chars();
            let first = letters.next().map(|c| c.to_uppercase().to_string());
            let sentence = format!("{}{}", first.unwrap_or_default(), letters.as_str());
            let ended = sentence.ends_with(['.', '!', '?']);
            fixes.push(Rich::new().text(&if ended {
                sentence
            } else {
                format!("{sentence}.")
            }));
        }
    }
    fixes
}

/// The text of a clap context value, if it has one.
fn context_text(value: &ContextValue) -> Option<String> {
    match value {
        ContextValue::String(text) => Some(text.clone()),
        ContextValue::Strings(texts) => Some(texts.join(", ")),
        ContextValue::StyledStr(text) => Some(text.to_string()),
        _ => None,
    }
}

/// `text` with its lines and indents joined into one line, as clap breaks
/// a usage over several lines when it is long.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::super::console::env::Env;
    use super::super::{Cli, os_args};
    use super::{ContextKind, ContextValue, block_for, context_text};
    use clap::error::ErrorKind;
    use clap::{CommandFactory, Parser};

    /// The rich lines for the command line `args` on a terminal 100
    /// columns wide.
    fn rich(args: &[&str]) -> Vec<String> {
        let args = os_args(args);
        let error = Cli::try_parse_from(&args).expect_err("the command line does not parse");
        block_for(&error).render(&Env::new(true, false, true, 100))
    }

    #[test]
    fn an_unknown_option_shows_clap_s_first_line_its_tip_and_the_usage() {
        assert_eq!(
            rich(&["antiq", "archive", "--everything"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument '--everything' found",
                "▌",
                "▌ How to fix it",
                "▌   To pass '--everything' as a value, use '-- --everything'.",
                "▌   Usage: antiq archive [OPTIONS] [PIECE]",
            ]
        );
    }

    #[test]
    fn a_misspelled_option_gets_clap_s_suggestion_first() {
        assert_eq!(
            rich(&["antiq", "archive", "--dryrun"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument '--dryrun' found",
                "▌",
                "▌ How to fix it",
                "▌   Did you mean --dry-run?",
                "▌   Usage: antiq archive --dry-run [PIECE]",
            ]
        );
    }

    #[test]
    fn a_misspelled_command_gets_clap_s_suggestion_first() {
        assert_eq!(
            rich(&["antiq", "unerth"]),
            [
                " ✖ ERROR",
                "▌ Unrecognized subcommand 'unerth'",
                "▌",
                "▌ How to fix it",
                "▌   Did you mean unearth?",
                "▌   Usage: antiq [OPTIONS] <COMMAND>",
            ]
        );
    }

    #[test]
    fn several_suggestions_are_listed_in_one_line() {
        let error = raw(
            ErrorKind::InvalidSubcommand,
            "unrecognized subcommand 'pi'",
            &[(
                ContextKind::SuggestedSubcommand,
                ContextValue::Strings(vec!["pin".into(), "init".into()]),
            )],
        );
        let lines = block_for(&error).render(&Env::new(true, false, true, 100));
        assert!(
            lines.contains(&"▌   Did you mean pin, init?".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn a_suggestion_is_read_from_the_context_not_from_the_text() {
        let error = raw(
            ErrorKind::InvalidValue,
            "a value that says nothing of a suggestion",
            &[(
                ContextKind::SuggestedValue,
                ContextValue::String("fast".into()),
            )],
        );
        let lines = block_for(&error).render(&Env::new(true, false, true, 100));
        assert!(
            lines.contains(&"▌   Did you mean fast?".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn the_value_tip_is_given_only_where_clap_gives_it() {
        // `status` takes no value, so there is nothing to pass `--x` as.
        assert_eq!(
            rich(&["antiq", "status", "--x"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument '--x' found",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq status [OPTIONS]",
            ]
        );
    }

    #[test]
    fn an_extra_word_shows_clap_s_first_line_and_the_usage() {
        assert_eq!(
            rich(&["antiq", "list", "a", "b"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument 'b' found",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq list [OPTIONS] [QUERY]",
            ]
        );
    }

    #[test]
    fn an_unknown_command_shows_clap_s_first_line_and_the_usage() {
        assert_eq!(
            rich(&["antiq", "frobnicate"]),
            [
                " ✖ ERROR",
                "▌ Unrecognized subcommand 'frobnicate'",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq [OPTIONS] <COMMAND>",
            ]
        );
    }

    #[test]
    fn a_missing_argument_joins_clap_s_first_paragraph_into_the_title() {
        assert_eq!(
            rich(&["antiq", "unearth"]),
            [
                " ✖ ERROR",
                "▌ The following required arguments were not provided: <PIECE>",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq unearth <PIECE>",
            ]
        );
    }

    #[test]
    fn several_missing_arguments_stay_in_the_title() {
        let lines = rich(&["antiq", "rebuild"]);
        assert_eq!(
            lines[1],
            "▌ The following required arguments were not provided: <PIECE> <VERSION>"
        );
    }

    #[test]
    fn two_arguments_that_exclude_each_other_show_clap_s_first_line() {
        assert_eq!(
            rich(&["antiq", "push", "--init", "--if-reachable"]),
            [
                " ✖ ERROR",
                "▌ The argument '--init' cannot be used with '--if-reachable'",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq push --init",
            ]
        );
    }

    #[test]
    fn an_error_that_carries_no_usage_has_a_title_alone() {
        assert_eq!(
            rich(&["antiq", "import", "dir", "--time"]),
            [
                " ✖ ERROR",
                "▌ A value is required for '--time <TIME>' but none was supplied"
            ]
        );
    }

    /// The error clap would raise for `kind`, built from the context values
    /// given, so a test can reach combinations that no command line of
    /// antiq produces.
    fn raw(kind: ErrorKind, message: &str, context: &[(ContextKind, ContextValue)]) -> clap::Error {
        let mut error = clap::Error::raw(kind, message);
        for (key, value) in context {
            error.insert(*key, value.clone());
        }
        error
    }

    #[test]
    fn an_error_without_suggestions_tips_or_usage_has_no_fix_section() {
        let error = raw(
            ErrorKind::InvalidSubcommand,
            "unrecognized subcommand 'x'",
            &[],
        );
        assert_eq!(
            block_for(&error).render(&Env::new(true, false, true, 100)),
            [" ✖ ERROR", "▌ Unrecognized subcommand 'x'"]
        );
    }

    #[test]
    fn a_tip_of_clap_is_passed_on_in_its_words() {
        let error = raw(
            ErrorKind::UnknownArgument,
            "unexpected argument '--init' found",
            &[(
                ContextKind::Suggested,
                ContextValue::StyledStrs(vec!["'push --init' exists".to_string().into()]),
            )],
        );
        let lines = block_for(&error).render(&Env::new(true, false, true, 100));
        assert!(
            lines.contains(&"▌   'push --init' exists.".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn a_tip_that_ends_a_sentence_already_gets_no_second_full_stop() {
        let error = raw(
            ErrorKind::UnknownArgument,
            "unexpected argument '--init' found",
            &[(
                ContextKind::Suggested,
                ContextValue::StyledStrs(vec!["use the other one.".to_string().into()]),
            )],
        );
        let lines = block_for(&error).render(&Env::new(true, false, true, 100));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("▌   Use the other one.")
        );
    }

    #[test]
    fn the_usage_comes_from_the_context_of_the_error() {
        let error = raw(
            ErrorKind::UnknownArgument,
            "unexpected argument '--x' found",
            &[(
                ContextKind::Usage,
                ContextValue::StyledStr("Usage: antiq\n  list".to_string().into()),
            )],
        );
        let lines = block_for(&error).render(&Env::new(true, false, true, 100));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("▌   Usage: antiq list")
        );
    }

    #[test]
    fn the_help_colours_are_the_theme_of_the_console() {
        let command = Cli::command();
        let styles = command.get_styles();
        assert_eq!(styles.get_header(), &super::super::console::theme::BOLD);
        assert_eq!(styles.get_usage(), &super::super::console::theme::BOLD);
        assert_eq!(styles.get_literal(), &super::super::console::theme::BOLD);
        assert_eq!(styles.get_placeholder(), &super::super::console::theme::DIM);
    }

    #[test]
    fn context_values_without_text_are_ignored() {
        assert_eq!(
            context_text(&ContextValue::StyledStr("styled".to_string().into())),
            Some("styled".to_string())
        );
        assert_eq!(context_text(&ContextValue::Number(3)), None);
    }
}
