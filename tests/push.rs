// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq push` seen from outside the binary.

mod support;

use std::fs;
use std::path::Path;
use support::cmd::{Antiq, Run, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection};

fn collection() -> Collection {
    Collection::build(&Clock::now())
        .piece("projects/a", |piece| {
            piece.age_days(3).file("a.txt", "a");
        })
        .finish()
}

/// Appends a `[target]` table with the given body lines to the config.
fn add_target_table(collection: &Collection, lines: &str) {
    let path = collection.path(".antiq/config.toml");
    let mut text = fs::read_to_string(&path).expect("the config exists");
    text.push_str(&format!("\n[target]\n{lines}\n"));
    fs::write(&path, text).expect("the config can be written");
}

fn set_url(collection: &Collection, url: &str) {
    let quoted = serde_json::to_string(url).expect("a string serialises");
    add_target_table(collection, &format!("url = {quoted}"));
}

fn push(collection: &Collection, args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .arg("--json")
        .arg("push")
        .args(args)
        .run()
}

fn push_text(collection: &Collection, args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .arg("push")
        .args(args)
        .run()
}

#[test]
fn a_config_without_a_target_table_is_no_target() {
    let collection = collection();

    let run = push(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "no_target");
    let message = run.error_message();
    assert!(message.contains("[target] url"), "{message}");
    assert!(message.contains("push --init"), "{message}");
}

#[test]
fn a_target_table_without_a_url_is_no_target() {
    let collection = collection();
    add_target_table(&collection, "fsync = false");

    let run = push(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "no_target");
}

#[test]
fn no_target_is_named_on_the_console_too() {
    let collection = collection();

    let run = push_text(&collection, &[]);

    assert_eq!(run.code(), 1);
    assert!(run.stderr.contains("[target] url"), "{}", run.stderr);
    assert!(run.stderr.contains("push --init"), "{}", run.stderr);
}

#[test]
fn a_url_that_is_no_target_names_the_key() {
    for url in ["", "-host:p", "relative/path", "host:"] {
        let collection = collection();
        set_url(&collection, url);

        let run = push(&collection, &[]);

        assert_eq!(run.code(), 1, "url {url:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "other", "url {url:?}");
        let message = run.error_message();
        assert!(message.contains("[target] url"), "url {url:?}: {message}");
    }
}

#[test]
fn the_target_is_checked_before_the_lock_is_taken() {
    let collection = collection();
    let _held = hold_lock(&collection.root());

    let run = push(&collection, &[]);

    assert_eq!(run.error_kind(), "no_target");
}

#[test]
fn a_held_lock_is_exit_1_with_the_holder_also_under_if_reachable() {
    for args in [&[][..], &["--if-reachable"][..]] {
        let collection = collection();
        set_url(&collection, "/nonexistent/target");
        let _held = hold_lock(&collection.root());

        let run = push(&collection, args);

        assert_eq!(run.code(), 1, "{args:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "lock_held", "{args:?}");
        let error = &run.json()["error"];
        assert_eq!(error["pid"], std::process::id(), "{args:?}");
        assert!(error["started"].is_string(), "{args:?}");
    }
}

#[test]
fn a_dry_run_with_the_lock_held_names_the_holder_and_lists_nothing() {
    let collection = collection();
    set_url(&collection, "/nonexistent/target");
    let _held = hold_lock(&collection.root());

    let run = push(&collection, &["-n"]);

    assert_eq!(run.code(), 0, "{}", run.stdout);
    let json = run.json();
    assert_eq!(json["command"], "push");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["lock_held"]["pid"], std::process::id());
    assert_eq!(json["actions"], serde_json::json!([]));

    let text = push_text(&collection, &["-n"]);
    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert!(
        text.stdout.contains("The lock is held by pid"),
        "{}",
        text.stdout
    );
}

#[test]
fn a_dry_run_changes_nothing_and_takes_no_lock() {
    let collection = collection();
    set_url(&collection, "/nonexistent/target");
    let before = snapshot(&collection.root());

    let run = push(&collection, &["-n"]);

    // The checks behind the lock are not built yet.
    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
    assert_eq!(snapshot(&collection.root()), before);
}

#[test]
fn a_real_run_repairs_before_it_goes_on() {
    let collection = collection();
    set_url(&collection, "/nonexistent/target");
    let stale = collection.path(".antiq/tmp/stale-entry");
    fs::create_dir_all(stale.parent().expect("has a parent")).expect("tmp exists");
    fs::write(&stale, "left behind").expect("the entry is written");

    let run = push(&collection, &[]);

    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
    assert!(!Path::new(&stale).exists(), "repair 2 removes the entry");
}

#[test]
fn init_and_force_are_usage_errors_with_if_reachable() {
    let collection = collection();
    for flag in ["--init", "--force"] {
        let run = push(&collection, &[flag, "--if-reachable"]);

        assert_eq!(run.code(), 2, "{flag}: {}", run.stdout);
        assert_eq!(run.error_kind(), "usage", "{flag}");
    }
}

#[test]
fn every_flag_combination_parses_and_needs_a_collection() {
    for args in [
        &["push"][..],
        &["push", "--init", "--dry-run"],
        &["push", "--force"],
        &["push", "--if-reachable", "-n"],
    ] {
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let run = Antiq::new()
            .args(["--json", "-C"])
            .arg(dir.path())
            .args(args)
            .run();

        assert_eq!(run.code(), 1, "{args:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "not_a_collection", "{args:?}");
        assert_eq!(run.json()["command"], "push", "{args:?}");
    }
}
