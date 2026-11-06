// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The block a command line that does not parse is reported as.

use super::console::block::Block;
use super::console::rich::Rich;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use std::ffi::OsString;

/// The error block of a clap error. The kinds antiq can say better than
/// clap get their own title and sections; every other kind keeps clap's
/// first paragraph as the title.
pub fn block_for(error: &clap::Error, args: &[OsString]) -> Block {
    let rendered = error.render().to_string();
    let command = super::guess_command(args);
    let argv0 = match &command {
        Some(command) => format!("antiq {command}"),
        None => "antiq".to_string(),
    };
    let context = |kind| error.get(kind).and_then(context_text);

    let block = match (error.kind(), context(ContextKind::InvalidArg)) {
        (ErrorKind::UnknownArgument, Some(argument)) => {
            Block::error(Rich::new().text("Unexpected argument ").path(&argument)).why(
                Rich::new()
                    .path(&argv0)
                    .text(" does not take this argument."),
            )
        }
        (ErrorKind::MissingRequiredArgument, Some(missing)) => {
            let noun = if missing.contains(", ") {
                "Missing arguments "
            } else {
                "Missing argument "
            };
            Block::error(Rich::new().text(noun).path(&missing))
        }
        (ErrorKind::ArgumentConflict, Some(argument)) => {
            let title = Rich::new().path(&argument);
            match context(ContextKind::PriorArg) {
                Some(prior) => Block::error(title.text(" cannot be used with ").path(&prior)),
                None => return fallback(&rendered),
            }
        }
        _ => match (error.kind(), context(ContextKind::InvalidSubcommand)) {
            (ErrorKind::InvalidSubcommand, Some(name)) => {
                Block::error(Rich::new().text("Unknown command ").path(&name))
                    .why(Rich::new().text("antiq has no command of that name."))
            }
            _ => return fallback(&rendered),
        },
    };
    let block = suggestions(error, &argument_of(error))
        .into_iter()
        .fold(block, Block::how);
    let block = match usage_line(&rendered) {
        Some(usage) => block.how(Rich::new().text(&usage)),
        None => block,
    };
    let help = if error.kind() == ErrorKind::InvalidSubcommand {
        Rich::parse("Run `antiq --help` for the list of commands.")
    } else {
        Rich::new()
            .text("Run ")
            .cmd(&format!("{argv0} --help"))
            .text(" to see what it takes.")
    };
    block.how(help)
}

/// The argument clap could not place, if the error names one.
fn argument_of(error: &clap::Error) -> String {
    error
        .get(ContextKind::InvalidArg)
        .and_then(context_text)
        .unwrap_or_default()
}

/// What clap suggests as a fix, in the order clap prints it: the command
/// or option it thinks was meant, then its tips. Clap's tip for passing a
/// dash-led word as a value (`-- <word>`) is the one tip antiq words
/// itself; it is recognised by the text clap starts it with. Any other tip
/// is passed on in clap's words.
fn suggestions(error: &clap::Error, argument: &str) -> Vec<Rich> {
    let mut fixes = Vec::new();
    for kind in [ContextKind::SuggestedSubcommand, ContextKind::SuggestedArg] {
        if let Some(name) = error.get(kind).and_then(context_text) {
            fixes.push(Rich::new().text("Did you mean ").cmd(&name).text("?"));
        }
    }
    if let Some(ContextValue::StyledStrs(tips)) = error.get(ContextKind::Suggested) {
        for tip in tips {
            let tip = tip.to_string();
            if tip.starts_with("to pass '") {
                fixes.push(
                    Rich::new()
                        .text("To pass it as a value, write ")
                        .cmd(&format!("-- {argument}"))
                        .text("."),
                );
            } else {
                let mut letters = tip.chars();
                let first = letters.next().map(|c| c.to_uppercase().to_string());
                fixes.push(Rich::new().text(&format!(
                    "{}{}.",
                    first.unwrap_or_default(),
                    letters.as_str()
                )));
            }
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

/// The `Usage:` paragraph of a rendered clap error as one line.
fn usage_line(rendered: &str) -> Option<String> {
    rendered
        .split("\n\n")
        .map(str::trim)
        .find(|paragraph| paragraph.starts_with("Usage:"))
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn fallback(rendered: &str) -> Block {
    let mut block = Block::error(Rich::new().text(&super::first_paragraph(rendered)));
    for paragraph in rendered.split("\n\n").skip(1) {
        let paragraph = paragraph.trim();
        if paragraph.starts_with("tip:") || paragraph.starts_with("Usage:") {
            block = block
                .how(Rich::new().text(&paragraph.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
    }
    block
}

#[cfg(test)]
mod tests {
    use super::super::console::env::Env;
    use super::super::{Cli, os_args};
    use super::{ContextKind, ContextValue, ErrorKind, block_for, context_text, fallback};
    use clap::Parser;

    /// The rich lines for the command line `args` on a terminal 100
    /// columns wide.
    fn rich(args: &[&str]) -> Vec<String> {
        let args = os_args(args);
        let error = Cli::try_parse_from(&args).expect_err("the command line does not parse");
        block_for(&error, &args).render(&Env::new(true, false, true, 100))
    }

    #[test]
    fn an_unknown_option_names_the_command_and_how_to_pass_it_as_a_value() {
        assert_eq!(
            rich(&["antiq", "archive", "--everything"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument --everything",
                "▌",
                "▌ Why it happened",
                "▌   antiq archive does not take this argument.",
                "▌",
                "▌ How to fix it",
                "▌   To pass it as a value, write -- --everything.",
                "▌   Usage: antiq archive [OPTIONS] [PIECE]",
                "▌   Run antiq archive --help to see what it takes.",
            ]
        );
    }

    #[test]
    fn a_misspelled_option_gets_clap_s_suggestion_first() {
        assert_eq!(
            rich(&["antiq", "archive", "--dryrun"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument --dryrun",
                "▌",
                "▌ Why it happened",
                "▌   antiq archive does not take this argument.",
                "▌",
                "▌ How to fix it",
                "▌   Did you mean --dry-run?",
                "▌   Usage: antiq archive --dry-run [PIECE]",
                "▌   Run antiq archive --help to see what it takes.",
            ]
        );
    }

    #[test]
    fn a_misspelled_command_gets_clap_s_suggestion_first() {
        assert_eq!(
            rich(&["antiq", "unerth"]),
            [
                " ✖ ERROR",
                "▌ Unknown command unerth",
                "▌",
                "▌ Why it happened",
                "▌   antiq has no command of that name.",
                "▌",
                "▌ How to fix it",
                "▌   Did you mean unearth?",
                "▌   Usage: antiq [OPTIONS] <COMMAND>",
                "▌   Run antiq --help for the list of commands.",
            ]
        );
    }

    #[test]
    fn the_value_tip_is_given_only_where_clap_gives_it() {
        // `status` takes no value, so there is nothing to pass `--x` as.
        assert_eq!(
            rich(&["antiq", "status", "--x"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument --x",
                "▌",
                "▌ Why it happened",
                "▌   antiq status does not take this argument.",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq status [OPTIONS]",
                "▌   Run antiq status --help to see what it takes.",
            ]
        );
    }

    #[test]
    fn an_extra_word_is_an_unexpected_argument_without_the_value_tip() {
        assert_eq!(
            rich(&["antiq", "list", "a", "b"]),
            [
                " ✖ ERROR",
                "▌ Unexpected argument b",
                "▌",
                "▌ Why it happened",
                "▌   antiq list does not take this argument.",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq list [OPTIONS] [QUERY]",
                "▌   Run antiq list --help to see what it takes.",
            ]
        );
    }

    #[test]
    fn an_unknown_command_points_at_the_list_of_commands() {
        assert_eq!(
            rich(&["antiq", "frobnicate"]),
            [
                " ✖ ERROR",
                "▌ Unknown command frobnicate",
                "▌",
                "▌ Why it happened",
                "▌   antiq has no command of that name.",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq [OPTIONS] <COMMAND>",
                "▌   Run antiq --help for the list of commands.",
            ]
        );
    }

    #[test]
    fn a_missing_argument_is_named_with_the_usage() {
        assert_eq!(
            rich(&["antiq", "unearth"]),
            [
                " ✖ ERROR",
                "▌ Missing argument <PIECE>",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq unearth <PIECE>",
                "▌   Run antiq unearth --help to see what it takes.",
            ]
        );
    }

    #[test]
    fn two_arguments_that_exclude_each_other_are_named_together() {
        assert_eq!(
            rich(&["antiq", "push", "--init", "--if-reachable"]),
            [
                " ✖ ERROR",
                "▌ --init cannot be used with --if-reachable",
                "▌",
                "▌ How to fix it",
                "▌   Usage: antiq push --init",
                "▌   Run antiq push --help to see what it takes.",
            ]
        );
    }

    #[test]
    fn any_other_usage_error_keeps_clap_s_first_paragraph() {
        let lines = rich(&["antiq", "import", "dir", "--time"]);
        assert_eq!(lines[0], " ✖ ERROR");
        assert!(
            lines[1].starts_with("▌ A value is required for '--time <TIME>'"),
            "{lines:?}"
        );
    }

    #[test]
    fn several_missing_arguments_are_named_in_the_plural() {
        let lines = rich(&["antiq", "rebuild"]);
        assert_eq!(lines[1], "▌ Missing arguments <PIECE>, <VERSION>");
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
    fn a_conflict_without_the_other_argument_keeps_clap_s_first_paragraph() {
        let error = raw(
            ErrorKind::ArgumentConflict,
            "the argument '--init' cannot be used with another",
            &[(
                ContextKind::InvalidArg,
                ContextValue::String("--init".into()),
            )],
        );
        let lines = block_for(&error, &[]).render(&Env::new(true, false, true, 100));
        assert_eq!(
            lines[1],
            "▌ The argument '--init' cannot be used with another"
        );
    }

    #[test]
    fn an_error_without_a_usage_paragraph_gets_only_the_help_pointer() {
        let error = raw(
            ErrorKind::InvalidSubcommand,
            "unrecognized subcommand 'x'",
            &[(
                ContextKind::InvalidSubcommand,
                ContextValue::String("x".into()),
            )],
        );
        let lines = block_for(&error, &[]).render(&Env::new(true, false, true, 100));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("▌   Run antiq --help for the list of commands.")
        );
        assert!(
            !lines.iter().any(|line| line.contains("Usage:")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_tip_of_clap_other_than_the_value_tip_is_passed_on_in_its_words() {
        let error = raw(
            ErrorKind::UnknownArgument,
            "unexpected argument '--init' found",
            &[
                (
                    ContextKind::InvalidArg,
                    ContextValue::String("--init".into()),
                ),
                (
                    ContextKind::Suggested,
                    ContextValue::StyledStrs(vec!["'push --init' exists".to_string().into()]),
                ),
            ],
        );
        let lines = block_for(&error, &[]).render(&Env::new(true, false, true, 100));
        assert!(
            lines.contains(&"▌   'push --init' exists.".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn the_paragraphs_of_a_fallback_keep_the_tip_and_the_usage_on_one_line_each() {
        let rendered = "error: bad value\n\n  tip: try\n  this\n\nUsage: antiq\n  list\n\nFor more information, try '--help'.";
        assert_eq!(
            fallback(rendered).render(&Env::new(true, false, true, 100)),
            [
                " ✖ ERROR",
                "▌ Bad value",
                "▌",
                "▌ How to fix it",
                "▌   tip: try this",
                "▌   Usage: antiq list",
            ]
        );
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
