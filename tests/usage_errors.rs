// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a command line that does not parse, help and the version print,
//! on a terminal and on a pipe.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use support::cmd::Antiq;
use support::scenario::collection::{Clock, Collection};
use support::scenario::world::{Mode, Setup, World};

fn world() -> World {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    World::new(Setup::new(collection), Mode::Text)
}

#[test]
fn a_bare_antiq_prints_the_help_on_a_terminal_and_exits_2() {
    let world = world();
    let run = world.run_tty_stderr_from_root(&[], &[("NO_COLOR", "1")]);
    run.expect_code(2);
    assert!(run.stderr.contains("Commands:"), "{:?}", run.stderr);
    assert!(!run.stderr.contains("ERROR"), "{:?}", run.stderr);
}

#[test]
fn a_forced_colour_gives_a_pipe_no_escape_sequence_in_a_usage_error() {
    let run = Antiq::new()
        .env("CLICOLOR_FORCE", "1")
        .args(["archive", "--dryrun"])
        .run();
    assert_eq!(run.code(), 2);
    assert!(!run.stderr.contains('\x1b'), "{:?}", run.stderr);
    assert!(run.stderr.contains("--dry-run"), "{:?}", run.stderr);
}

#[test]
fn a_newline_in_an_unexpected_argument_cannot_forge_a_line_of_the_error() {
    let run = Antiq::new().args(["status", "a\nerror: forged"]).run();
    assert_eq!(run.code(), 2);
    let errors = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("error: "))
        .count();
    assert_eq!(errors, 1, "{:?}", run.stderr);
    assert!(run.stderr.contains("a%0Aerror: forged"), "{:?}", run.stderr);
}

#[test]
fn a_bidi_control_and_a_carriage_return_in_an_unexpected_argument_are_escaped_in_the_json_message()
{
    let run = Antiq::new()
        .args(["--json", "status", "a\u{202e}b\rc"])
        .run();
    assert_eq!(run.code(), 2);
    let message = run.error_message();
    assert!(message.contains("a%E2%80%AEb%0Dc"), "{message:?}");
}

#[test]
fn an_unknown_command_with_a_control_character_is_escaped_in_the_message() {
    let run = Antiq::new().args(["--json", "x\ny\n\nz"]).run();
    assert_eq!(run.code(), 2);
    let message = run.error_message();
    assert!(message.contains("x%0Ay%0A%0Az"), "{message:?}");
}

#[test]
fn a_forced_colour_gives_a_pipe_no_escape_sequence_in_help_or_version() {
    for flag in ["--help", "--version"] {
        let run = Antiq::new().env("CLICOLOR_FORCE", "1").arg(flag).run();
        assert_eq!(run.code(), 0);
        assert!(!run.stdout.contains('\x1b'), "{flag}: {:?}", run.stdout);
        assert!(!run.stdout.is_empty(), "{flag}");
    }
}

#[test]
fn a_bare_antiq_on_a_pipe_prints_the_help_without_escape_sequences() {
    let run = Antiq::new().env("CLICOLOR_FORCE", "1").run();
    assert_eq!(run.code(), 2);
    assert!(run.stderr.contains("Commands:"), "{:?}", run.stderr);
    assert!(!run.stderr.contains('\x1b'), "{:?}", run.stderr);
}

#[test]
fn the_help_on_a_terminal_takes_its_colours_from_the_theme() {
    let world = world();
    let run = world.run_tty_env(&["--help"], &[("CLICOLOR_FORCE", "1")]);
    run.expect_code(0);
    assert!(run.stdout.contains("\x1b[1mCommands:"), "{:?}", run.stdout);
    // clap's own style underlines its headings; the theme does not.
    assert!(!run.stdout.contains("\x1b[4m"), "{:?}", run.stdout);
}
