// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Warnings and notes come after the result of a command. On a terminal
//! both streams show on one screen, so the order is visible.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs;
use support::scenario::collection::{Clock, Collection};
use support::scenario::target::Target;
use support::scenario::world::{Mode, Setup, World};

const STAMP: &str = "2026-11-04T090211";

#[test]
fn grep_prints_its_hits_before_the_versions_it_could_not_search() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .version("notes", "2026-09-03T121500", &[("needle", "x")])
        .version("notes", STAMP, &[("needle", "x")])
        .finish();
    collection.damage_remove_listing("notes", "2026-09-03T121500");
    let world = World::new(Setup::new(collection), Mode::Text);

    let run = world.run_tty_merged(&["grep", "needle"], &[("NO_COLOR", "1")]);

    run.expect_code(0);
    let hits = run.stdout.find("needle").expect("the hits are printed");
    let warning = run
        .stdout
        .find("WARNING")
        .expect("the skipped version is a warning");
    assert!(hits < warning, "{:?}", run.stdout);
}

#[test]
fn push_prints_its_result_before_the_repairs_it_made() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["*/*"])
        .piece("notes/daily", |piece| {
            piece.age_days(5).file("today.md", "fresh\n");
        })
        .finish();
    fs::write(collection.path(".antiq/tmp/leftover"), "x").expect("the leftover");
    let world = World::new(
        Setup::new(collection)
            .with_target(Target::new())
            .with_real_rsync(),
        Mode::Text,
    );

    let run = world.run_tty_merged(&["push", "--init"], &[("NO_COLOR", "1")]);

    run.expect_code(0);
    let result = run.stdout.find("Claimed").expect("the result is printed");
    let repair = run.stdout.find("leftover").expect("the repair is reported");
    assert!(result < repair, "{:?}", run.stdout);
}

#[test]
fn status_prints_the_hint_after_the_notices() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["tools/*"])
        .piece("tools/old", |piece| {
            piece.age_days(60).file("a.sh", "#!/bin/sh\n");
        })
        .finish();
    let path = collection.path(".antiq/config.toml");
    let config = fs::read_to_string(&path).expect("the config exists");
    fs::write(
        &path,
        config.replacen("[collection]\n", "[collection]\nbogus = 1\n", 1),
    )
    .expect("the config is written");
    let world = World::new(Setup::new(collection), Mode::Text);

    let run = world.run_tty_merged(&["status"], &[("NO_COLOR", "1")]);

    run.expect_code(0);
    let notice = run.stdout.find("nknown setting").expect("the notice");
    let hint = run.stdout.find("HINT").expect("the hint");
    assert!(notice < hint, "{:?}", run.stdout);
}

#[test]
fn a_dry_run_ends_with_its_badge_after_the_notices() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["tools/*"])
        .piece("tools/old", |piece| {
            piece.age_days(60).file("a.sh", "#!/bin/sh\n");
        })
        .finish();
    let path = collection.path(".antiq/config.toml");
    let config = fs::read_to_string(&path).expect("the config exists");
    fs::write(
        &path,
        config.replacen("[collection]\n", "[collection]\nbogus = 1\n", 1),
    )
    .expect("the config is written");
    let world = World::new(Setup::new(collection), Mode::Text);

    let run = world.run_tty_merged(&["archive", "-n"], &[("NO_COLOR", "1")]);

    run.expect_code(0);
    let notice = run.stdout.find("nknown setting").expect("the notice");
    let badge = run.stdout.find("DRY RUN").expect("the badge");
    assert!(notice < badge, "{:?}", run.stdout);
    assert!(
        run.stdout
            .trim_end()
            .lines()
            .last()
            .expect("a last line")
            .contains("DRY RUN"),
        "{:?}",
        run.stdout
    );
}
