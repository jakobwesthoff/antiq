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
