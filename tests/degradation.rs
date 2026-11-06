// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a terminal that cannot show colour or Unicode gets from the
//! commands that print tables: no escape sequences, and ASCII stand-ins
//! for `…` and `·`.

mod support;

use support::scenario::collection::{Clock, Collection};
use support::scenario::world::{Mode, Setup, World};

const NEW: &str = "2026-11-04T090211";
const LONG_PIECE: &str =
    "archive/of/a/deeply/nested/folder/with/a/name/that/does/not/fit/a/line/of/eighty";

fn world() -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder
        .version(
            "projects/scanner",
            NEW,
            &[("src/main.rs", "fn main() {}\n")],
        )
        .version(LONG_PIECE, NEW, &[("main.rs", "fn main() {}\n")]);
    World::new(Setup::new(builder.finish()), Mode::Text)
}

/// Every table command, as the arguments to run.
const TABLES: [&[&str]; 3] = [&["list"], &["status"], &["grep", "main"]];

// =========================================================
// Colour
// =========================================================

#[test]
fn a_terminal_that_cannot_show_colour_gets_no_escape_sequence_in_tables() {
    let world = world();
    for (variable, value) in [("TERM", "dumb"), ("CLICOLOR", "0"), ("NO_COLOR", "1")] {
        for args in TABLES {
            let run = world.run_tty_env(args, &[(variable, value)]);
            run.expect_code(0);
            assert!(
                !run.stdout.contains('\x1b'),
                "{args:?} with {variable}={value}: {:?}",
                run.stdout
            );
        }
    }
}

// =========================================================
// Unicode
// =========================================================

#[test]
fn a_terminal_without_utf8_gets_ascii_in_tables_and_summaries() {
    let world = world();
    for args in TABLES {
        let run = world.run_tty_env(args, &[("LC_ALL", "C")]);
        run.expect_code(0);
        assert!(run.stdout.is_ascii(), "{args:?}: {:?}", run.stdout);
    }
}

#[test]
fn a_cut_piece_name_ends_in_three_dots_without_utf8() {
    let world = world();
    let run = world.run_tty_env(&["list"], &[("LC_ALL", "C")]);
    run.expect_code(0);
    assert!(run.stdout.contains("..."), "{:?}", run.stdout);
}

#[test]
fn the_status_header_joins_its_parts_with_a_hyphen_without_utf8() {
    let world = world();
    let run = world.run_tty_env(&["status"], &[("LC_ALL", "C")]);
    run.expect_code(0);
    assert!(run.stdout.contains(" - archive after "), "{:?}", run.stdout);
}
