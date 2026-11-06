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
            let mut block = Block::error(Rich::new().text("Unexpected argument ").path(&argument))
                .why(
                    Rich::new()
                        .path(&argv0)
                        .text(" does not take this argument."),
                );
            if argument.starts_with('-') {
                block = block.how(
                    Rich::new()
                        .text("To pass it as a value, write ")
                        .cmd(&format!("-- {argument}"))
                        .text("."),
                );
            }
            block
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
    use super::block_for;
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
            lines[1].starts_with("▌ a value is required for '--time <TIME>'"),
            "{lines:?}"
        );
    }
}
