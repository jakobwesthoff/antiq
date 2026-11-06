// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the scenario harness itself.

use crate::support::scenario::collection::{Clock, Collection};
use crate::support::tree::COLLECTION_ID;
use antiq::collection::Collection as Library;
use std::fs;
use std::os::unix::fs::MetadataExt;

fn mtime(path: &std::path::Path) -> i64 {
    fs::symlink_metadata(path).expect("the path exists").mtime()
}

// =========================================================
// Building collections
// =========================================================

#[test]
fn a_built_collection_parses_back_with_the_keys_that_were_set() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["projects/*"])
        .ignore(&["node_modules/", ".DS_Store"])
        .timeless(&[".git/"])
        .idle_days(45)
        .soon_days(5)
        .finish();
    let found = Library::find(&collection.root()).expect("the collection is found");
    let config = &found.config().collection;
    assert_eq!(config.id.to_string(), COLLECTION_ID);
    assert_eq!(config.patterns, ["projects/*"]);
    assert_eq!(config.ignore, ["node_modules/", ".DS_Store"]);
    assert_eq!(config.timeless, [".git/"]);
    assert_eq!(config.idle_days, 45);
    assert_eq!(config.soon_days, 5);
}

#[test]
fn keys_that_were_not_set_keep_their_defaults() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    let found = Library::find(&collection.root()).expect("the collection is found");
    let config = &found.config().collection;
    assert_eq!(config.patterns, ["*"]);
    assert_eq!(config.ignore, [".DS_Store"]);
    assert_eq!(config.idle_days, 30);
    assert_eq!(config.soon_days, 3);
    let text =
        fs::read_to_string(collection.path(".antiq/config.toml")).expect("the config exists");
    assert!(!text.contains("idle_days"), "{text}");
}

#[test]
fn a_bare_directory_is_not_a_collection() {
    let clock = Clock::now();
    let collection = Collection::bare(&clock);
    assert!(Library::find(&collection.root()).is_err());
    assert!(!collection.path(".antiq").exists());
}

#[test]
fn the_layout_directories_exist() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    for name in ["tmp", "trash", "doomed", "recovered", "pieces"] {
        assert!(
            collection.path(&format!(".antiq/{name}")).is_dir(),
            "{name}"
        );
    }
}

// =========================================================
// Aged folders
// =========================================================

#[test]
fn files_carry_the_age_of_their_piece_and_their_own_when_given() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("projects/scanner", |piece| {
            piece
                .age_days(40)
                .file("src/main.rs", "fn main() {}")
                .file_aged("README.md", "docs", 90);
        })
        .finish();
    assert_eq!(
        mtime(&collection.path("projects/scanner/src/main.rs")),
        clock.days_ago(40)
    );
    assert_eq!(
        mtime(&collection.path("projects/scanner/README.md")),
        clock.days_ago(90)
    );
}

#[test]
fn directories_carry_the_fixed_directory_time() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("projects/scanner", |piece| {
            piece.age_days(40).file("src/deep/main.rs", "x");
        })
        .finish();
    for dir in [
        "",
        "projects",
        "projects/scanner",
        "projects/scanner/src/deep",
        ".antiq",
        ".antiq/pieces",
    ] {
        assert_eq!(mtime(&collection.path(dir)), clock.dir_time(), "{dir:?}");
    }
}

#[test]
fn two_builds_from_one_clock_have_equal_times() {
    let clock = Clock::now();
    let build = || {
        Collection::build(&clock)
            .piece("a", |piece| {
                piece.age_days(10).file("f", "x");
            })
            .finish()
    };
    let first = build();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = build();
    assert_eq!(mtime(&first.path("a/f")), mtime(&second.path("a/f")));
    assert_eq!(mtime(&first.path("a")), mtime(&second.path("a")));
}

// =========================================================
// Markers
// =========================================================

#[test]
fn a_pinned_piece_has_a_pin_file() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("a", |piece| {
            piece.age_days(60).file("f", "x").pinned();
        })
        .finish();
    assert!(collection.path("a/.antiq-pin").is_file());
}

#[test]
fn a_restored_marker_holds_the_time_and_the_version() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("a", |piece| {
            piece.file("f", "x").restored(2, Some("2026-09-03T101500"));
        })
        .finish();
    let text = fs::read_to_string(collection.path("a/.antiq-restored")).expect("the marker exists");
    let expected = jiff::Timestamp::from_second(clock.days_ago(2)).expect("a valid time");
    assert_eq!(
        text.trim_end(),
        format!(
            "restored {} version 2026-09-03T101500",
            expected.strftime("%Y-%m-%dT%H:%M:%SZ")
        )
    );
}

#[test]
fn links_rule_files_exclude_and_outside_files_are_written() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .exclude(&["scratch"])
        .outside("notes.txt", "outside")
        .piece("a", |piece| {
            piece
                .age_days(5)
                .file("one", "same")
                .hard_link("one", "two")
                .symlink("dangling", "missing")
                .dir("empty")
                .rule_ignore(&["node_modules/"])
                .rule_timeless(&[".git/"]);
        })
        .finish();
    let one = fs::metadata(collection.path("a/one")).expect("one exists");
    let two = fs::metadata(collection.path("a/two")).expect("two exists");
    assert_eq!(one.ino(), two.ino());
    assert_eq!(
        fs::read_link(collection.path("a/dangling")).expect("a link"),
        std::path::Path::new("missing")
    );
    assert!(collection.path("a/empty").is_dir());
    assert_eq!(
        fs::read_to_string(collection.path("a/.antiqignore")).expect("rule file"),
        "node_modules/\n"
    );
    assert_eq!(
        fs::read_to_string(collection.path("a/.antiq-timeless")).expect("rule file"),
        ".git/\n"
    );
    assert_eq!(
        fs::read_to_string(collection.path("notes.txt")).expect("outside file"),
        "outside"
    );
    let found = Library::find(&collection.root()).expect("the collection is found");
    assert_eq!(found.config().collection.exclude.len(), 1);
}

// =========================================================
// The path target
// =========================================================

use crate::support::scenario::target::Target;

#[test]
fn the_marker_is_the_line_the_spec_describes() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    let target = Target::new();
    target.marker(&collection);
    let text = fs::read_to_string(target.path().join(".antiq-target")).expect("the marker exists");
    assert_eq!(
        text,
        format!(
            "{{\"format\":1,\"id\":\"{COLLECTION_ID}\",\"host\":{},\"path\":{}}}\n",
            serde_json::to_string(&antiq::machine::detect().expect("a name")).expect("json"),
            serde_json::to_string(collection.root().to_str().expect("UTF-8")).expect("json"),
        )
    );
}

#[test]
fn a_marker_can_name_another_machine_and_path() {
    let target = Target::new();
    target.marker_with(COLLECTION_ID, "laptop", "/elsewhere/projects");
    let text = fs::read_to_string(target.path().join(".antiq-target")).expect("the marker exists");
    assert_eq!(
        text,
        format!(
            "{{\"format\":1,\"id\":\"{COLLECTION_ID}\",\"host\":\"laptop\",\"path\":\"/elsewhere/projects\"}}\n"
        )
    );
}

#[test]
fn the_config_names_the_target_path() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    let target = Target::new();
    collection.use_target(&target.path());
    let found = Library::find(&collection.root()).expect("the collection is found");
    assert_eq!(
        found.config().target.url.as_deref(),
        Some(target.path().to_str().expect("UTF-8"))
    );
}

#[test]
fn a_removed_target_does_not_exist_and_keeps_its_name() {
    let target = Target::new();
    let path = target.path();
    target.remove();
    assert!(!path.exists());
    assert_eq!(target.path(), path);
}

#[test]
fn copying_back_keeps_content_and_hard_links() {
    let clock = Clock::now();
    let mut target = Target::new();
    let tree = target.tree_mut();
    tree.file("a/one", "same");
    tree.hard_link("a/one", "a/two");
    target.marker_with(COLLECTION_ID, "laptop", "/elsewhere");
    let copy = target.copy_back(&clock);
    let one = fs::metadata(copy.path("a/one")).expect("one is copied");
    let two = fs::metadata(copy.path("a/two")).expect("two is copied");
    assert_eq!(one.ino(), two.ino());
    assert_eq!(
        fs::read_to_string(copy.path("a/one")).expect("reads"),
        "same"
    );
    assert!(copy.path(".antiq-target").is_file());
}

#[test]
#[should_panic(expected = "cannot run rsync")]
fn a_missing_rsync_is_named() {
    let clock = Clock::now();
    Target::new().copy_back_using("antiq-no-such-rsync", &clock);
}

#[test]
#[should_panic(expected = "rsync failed")]
fn a_failing_rsync_is_named() {
    let clock = Clock::now();
    Target::new().copy_back_using("false", &clock);
}

// =========================================================
// The JSON envelope
// =========================================================

use crate::support::scenario::envelope::{check, command_of};

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

fn accepted(items: &[&str], stdout: &str, code: i32) {
    if let Err(reason) = check(&args(items), stdout, code) {
        panic!("rejected {stdout:?}: {reason}");
    }
}

fn rejected(items: &[&str], stdout: &str, code: i32, reason: &str) {
    match check(&args(items), stdout, code) {
        Ok(()) => panic!("accepted {stdout:?}, expected a rejection for {reason}"),
        Err(message) => assert!(message.contains(reason), "{message:?} lacks {reason:?}"),
    }
}

#[test]
fn the_command_is_the_first_argument_that_is_no_option() {
    assert_eq!(
        command_of(&args(&["-C", "/x", "--json", "archive", "-n"])).as_deref(),
        Some("archive")
    );
    assert_eq!(command_of(&args(&["--json", "--bogus"])), None);
}

#[test]
fn a_success_object_is_accepted() {
    accepted(
        &["--json", "pin", "a"],
        "{\"format\":1,\"command\":\"pin\",\"x\":2}\n",
        0,
    );
    accepted(
        &["--json", "archive"],
        "{\"format\":1,\"command\":\"archive\"}\n",
        3,
    );
}

#[test]
fn a_usage_error_has_a_null_command_and_exit_2() {
    accepted(
        &["--json", "bogus"],
        "{\"format\":1,\"command\":null,\"error\":{\"kind\":\"usage\",\"message\":\"m\"}}\n",
        2,
    );
}

#[test]
fn help_and_version_are_text() {
    accepted(&["--json", "--help"], "Usage: antiq\n", 0);
    accepted(&["--json", "--version"], "antiq 0.1.0\n", 0);
}

#[test]
fn an_unreachable_target_may_exit_0_under_if_reachable() {
    accepted(
        &["--json", "push", "--if-reachable"],
        "{\"format\":1,\"command\":\"push\",\"error\":{\"kind\":\"target_unreachable\",\"message\":\"m\"}}\n",
        0,
    );
}

#[test]
fn text_instead_of_json_is_rejected() {
    rejected(&["--json", "pin"], "pinned a\n", 0, "not JSON");
}

#[test]
fn more_than_one_line_is_rejected() {
    rejected(
        &["--json", "pin"],
        "{\"format\":1,\"command\":\"pin\"}\n{\"format\":1,\"command\":\"pin\"}\n",
        0,
        "one line",
    );
}

#[test]
fn a_missing_or_wrong_format_is_rejected() {
    rejected(&["--json", "pin"], "{\"command\":\"pin\"}\n", 0, "format");
    rejected(
        &["--json", "pin"],
        "{\"format\":2,\"command\":\"pin\"}\n",
        0,
        "format",
    );
}

#[test]
fn command_must_be_the_second_key() {
    rejected(
        &["--json", "pin"],
        "{\"format\":1,\"x\":1,\"command\":\"pin\"}\n",
        0,
        "second key",
    );
}

#[test]
fn the_command_must_be_the_one_that_ran() {
    rejected(
        &["--json", "pin"],
        "{\"format\":1,\"command\":\"unpin\"}\n",
        0,
        "command",
    );
}

#[test]
fn a_null_command_is_only_for_a_usage_error() {
    rejected(
        &["--json", "pin"],
        "{\"format\":1,\"command\":null}\n",
        0,
        "null command",
    );
}

#[test]
fn an_unknown_error_kind_is_rejected() {
    rejected(
        &["--json", "pin"],
        "{\"format\":1,\"command\":\"pin\",\"error\":{\"kind\":\"weird\",\"message\":\"m\"}}\n",
        1,
        "kind",
    );
}

#[test]
fn an_error_needs_a_message() {
    rejected(
        &["--json", "pin"],
        "{\"format\":1,\"command\":\"pin\",\"error\":{\"kind\":\"other\"}}\n",
        1,
        "message",
    );
}

#[test]
fn an_error_kind_has_its_exit_code() {
    let usage =
        "{\"format\":1,\"command\":null,\"error\":{\"kind\":\"usage\",\"message\":\"m\"}}\n";
    rejected(&["--json", "bogus"], usage, 1, "exit");
    let other =
        "{\"format\":1,\"command\":\"pin\",\"error\":{\"kind\":\"other\",\"message\":\"m\"}}\n";
    rejected(&["--json", "pin"], other, 0, "exit");
    let unreachable = "{\"format\":1,\"command\":\"push\",\"error\":{\"kind\":\"target_unreachable\",\"message\":\"m\"}}\n";
    rejected(&["--json", "push"], unreachable, 1, "exit");
}

#[test]
fn exits_1_2_and_4_need_an_error() {
    for code in [1, 2, 4] {
        rejected(
            &["--json", "pin"],
            "{\"format\":1,\"command\":\"pin\"}\n",
            code,
            "error",
        );
    }
}

#[test]
fn lock_held_carries_pid_and_started_together_or_neither() {
    let only_pid = "{\"format\":1,\"command\":\"pin\",\"error\":{\"kind\":\"lock_held\",\"message\":\"m\",\"pid\":1}}\n";
    rejected(&["--json", "pin"], only_pid, 1, "pid and started");
    let neither =
        "{\"format\":1,\"command\":\"pin\",\"error\":{\"kind\":\"lock_held\",\"message\":\"m\"}}\n";
    accepted(&["--json", "pin"], neither, 1);
}

#[test]
fn candidates_are_a_list_of_paths() {
    let bad = "{\"format\":1,\"command\":\"pin\",\"error\":{\"kind\":\"ambiguous_reference\",\"message\":\"m\",\"candidates\":[\"a\"]}}\n";
    rejected(&["--json", "pin"], bad, 1, "candidates");
    let good = "{\"format\":1,\"command\":\"pin\",\"error\":{\"kind\":\"ambiguous_reference\",\"message\":\"m\",\"candidates\":[{\"path\":\"a\"}]}}\n";
    accepted(&["--json", "pin"], good, 1);
}

#[test]
fn a_dry_run_has_actions_right_after_dry_run_true() {
    accepted(
        &["--json", "pin", "-n"],
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":true,\"actions\":[]}\n",
        0,
    );
    rejected(
        &["--json", "pin", "-n"],
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":true}\n",
        0,
        "actions",
    );
    rejected(
        &["--json", "pin", "-n"],
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":true,\"x\":1,\"actions\":[]}\n",
        0,
        "actions",
    );
    rejected(
        &["--json", "pin", "-n"],
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":false,\"actions\":[]}\n",
        0,
        "dry_run",
    );
}

#[test]
fn output_must_end_in_a_newline_and_be_an_object() {
    rejected(
        &["--json", "pin"],
        "{\"format\":1,\"command\":\"pin\"}",
        0,
        "newline",
    );
    rejected(&["--json", "pin"], "[1]\n", 0, "object");
}
