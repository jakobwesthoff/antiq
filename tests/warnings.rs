// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Warnings and the findings of `status`, on a terminal and on a pipe.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};

const STAMP: &str = "2026-11-04T090211";

fn world(build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*", "tools/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), Mode::Text)
}

fn with_unknown_key(world: &World) {
    let path = world.collection().path(".antiq/config.toml");
    let config = fs::read_to_string(&path).expect("the config exists");
    let config = config.replacen("[collection]\n", "[collection]\nbogus = 1\n", 1);
    fs::write(&path, config).expect("the config is written");
}

fn stored(builder: &mut CollectionBuilder) {
    builder.version("projects/scanner", STAMP, &[("main.rs", "fn main() {}\n")]);
}

#[test]
fn a_warning_on_a_stderr_terminal_is_a_block_after_an_empty_line() {
    let world = world(stored);
    with_unknown_key(&world);

    let run = world.run_tty_stderr(&["find"], &[("NO_COLOR", "1")]);

    run.expect_code(0);
    assert_eq!(
        run.stderr,
        "\n▌ ⚠ WARNING\n\
         ▌ Unknown setting bogus under [collection], which antiq ignores\n\
         ▌\n\
         ▌ → Correct the name or remove the line in .antiq/config.toml.\n"
    );
}

#[test]
fn a_warning_on_a_stderr_pipe_is_a_warning_line() {
    let world = world(stored);
    with_unknown_key(&world);

    let run = world.run(&["find"]);

    run.expect_code(0);
    assert_eq!(
        run.stderr,
        "warning: unknown setting `bogus` under [collection], which antiq ignores\n"
    );
}

#[test]
fn status_shows_its_findings_as_blocks_and_a_hint_on_a_stdout_terminal() {
    let world = world(|builder| {
        stored(builder);
        builder.piece("tools/old-tool", |piece| {
            piece.age_days(60).file("a", "x");
        });
    });
    world
        .collection()
        .damage_remove_listing("projects/scanner", STAMP);

    let run = world.run_tty(&["status"]);

    run.expect_code(0);
    let shown = anstream::adapter::strip_str(&run.stdout).to_string();
    assert!(
        shown.contains("\n\n ⚠ WARNING \n▌ 1 damaged version: projects/scanner "),
        "{shown:?}"
    );
    // The harness starts every run with `-C`, which the command to copy
    // keeps. The terminal may wrap the long line, so the words are joined.
    let root = world.collection().root().display().to_string();
    let joined = shown.replace("\n▌   ", " ");
    assert!(
        joined.contains(&format!(
            "▌ → antiq -C {root} rebuild projects/scanner 2026-11-04T090211 writes the missing file.\n"
        )),
        "{shown:?}"
    );
    assert!(
        shown.ends_with(
            "\n\n → HINT  Run antiq archive -n to see what the next run would archive.\n"
        ),
        "{shown:?}"
    );
}

#[test]
fn status_keeps_its_finding_lines_and_has_no_hint_on_a_pipe() {
    let world = world(|builder| {
        stored(builder);
        builder.piece("tools/old-tool", |piece| {
            piece.age_days(60).file("a", "x");
        });
    });
    world
        .collection()
        .damage_remove_listing("projects/scanner", STAMP);

    let run = world.run(&["status"]);

    run.expect_code(0);
    assert!(
        run.stdout
            .contains("\n! 1 damaged version: projects/scanner "),
        "{:?}",
        run.stdout
    );
    assert!(!run.stdout.contains("HINT"), "{:?}", run.stdout);
    assert!(!run.stdout.contains("WARNING"), "{:?}", run.stdout);
}
