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
fn an_uninitialised_collection_has_its_folders_and_no_antiq_directory() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .uninitialised()
        .piece("projects", |piece| {
            piece.age_days(40).file("src/main.rs", "fn main() {}\n");
        })
        .finish();
    assert!(collection.path("projects/src/main.rs").is_file());
    assert!(!collection.path(".antiq").exists());
    assert!(Library::find(&collection.root()).is_err());
}

#[test]
fn the_readme_of_antiq_exists() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    assert_eq!(
        fs::read_to_string(collection.path(".antiq/README.md")).expect("the README exists"),
        antiq::collection::README
    );
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
fn symlinks_carry_the_age_of_their_piece() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("projects/scanner", |piece| {
            piece
                .age_days(40)
                .file("src/main.rs", "fn main() {}")
                .symlink("src/latest", "main.rs");
        })
        .finish();
    let link = fs::symlink_metadata(collection.path("projects/scanner/src/latest"))
        .expect("the link exists");
    assert_eq!(link.mtime(), clock.days_ago(40));
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
            serde_json::to_string(&crate::support::cmd::machine_name()).expect("json"),
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

#[test]
fn the_located_rsync_is_a_current_rsync() {
    let rsync = crate::support::scenario::target::real_rsync();
    let output = std::process::Command::new(&rsync)
        .arg("--version")
        .output()
        .expect("the located rsync runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("rsync "), "{text}");
    assert!(rsync.is_absolute(), "{rsync:?}");
}

#[test]
fn a_collection_can_name_the_located_rsync_in_its_target_table() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    let target = Target::new();
    collection.use_target(&target.path());
    collection.use_real_rsync();
    let found = Library::find(&collection.root()).expect("the collection is found");
    assert_eq!(
        found.config().target.rsync,
        Some(crate::support::scenario::target::real_rsync())
    );
    assert_eq!(
        found.config().target.url.as_deref(),
        Some(target.path().to_str().expect("UTF-8"))
    );
}

#[test]
fn a_target_without_the_rsync_call_keeps_searching_path() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    collection.use_target(&Target::new().path());
    let found = Library::find(&collection.root()).expect("the collection is found");
    assert_eq!(found.config().target.rsync, None);
}

#[test]
fn two_names_of_one_file_are_the_same_file() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("a/b", |piece| {
            piece
                .file("one", "same")
                .hard_link("one", "two")
                .file("three", "same");
        })
        .finish();
    let same = crate::support::scenario::outsider::same_file;
    assert!(same(
        &collection.path("a/b/one"),
        &collection.path("a/b/two")
    ));
    assert!(!same(
        &collection.path("a/b/one"),
        &collection.path("a/b/three")
    ));
}

#[test]
fn a_state_without_paths_drops_them_and_what_is_below() {
    use crate::support::scenario::state::State;
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("a/b", |piece| {
            piece.file("one", "1").file("sub/two", "2");
        })
        .finish();
    let state = State::raw(&collection.root()).without(&["a/b/sub", ".antiq/config.toml"]);
    let paths = state.paths();
    assert!(paths.contains(&"a/b/one"));
    assert!(!paths.contains(&"a/b/sub"));
    assert!(!paths.contains(&"a/b/sub/two"));
    assert!(!paths.contains(&".antiq/config.toml"));
    assert!(paths.contains(&".antiq"));
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
fn the_command_of_list_is_find() {
    assert_eq!(
        command_of(&args(&["--json", "list", "x"])).as_deref(),
        Some("find")
    );
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
    accepted(
        &["--json", "--version"],
        concat!("antiq ", env!("CARGO_PKG_VERSION"), "\n"),
        0,
    );
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
fn a_refused_piece_of_archive_exits_1_without_an_error() {
    let refused = "{\"format\":1,\"command\":\"archive\",\"archived\":[],\"skipped\":[],\
        \"refused\":[{\"piece\":\"old/pinned\",\"reasons\":[]}]}\n";
    accepted(&["--json", "archive", "old/pinned"], refused, 1);
}

#[test]
fn exit_1_of_archive_without_a_refused_piece_needs_an_error() {
    let empty = "{\"format\":1,\"command\":\"archive\",\"archived\":[],\"skipped\":[],\
        \"refused\":[]}\n";
    rejected(&["--json", "archive", "a"], empty, 1, "error");
    let absent = "{\"format\":1,\"command\":\"archive\"}\n";
    rejected(&["--json", "archive", "a"], absent, 1, "error");
}

#[test]
fn only_archive_may_exit_1_for_refused_pieces() {
    let refused = "{\"format\":1,\"command\":\"pin\",\"refused\":[{\"piece\":\"a\"}]}\n";
    rejected(&["--json", "pin", "a"], refused, 1, "error");
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
fn a_dry_run_has_actions_as_a_list_after_dry_run_true() {
    accepted(
        &["--json", "pin", "-n"],
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":true,\"actions\":[]}\n",
        0,
    );
    // The spec puts `lock_held` between the two when the lock is held.
    accepted(
        &["--json", "pin", "-n"],
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":true,\"lock_held\":{\"pid\":1},\"actions\":[]}\n",
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
        "{\"format\":1,\"command\":\"pin\",\"actions\":[],\"dry_run\":true}\n",
        0,
        "actions",
    );
    rejected(
        &["--json", "pin", "-n"],
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":true,\"actions\":{}}\n",
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

// =========================================================
// End states
// =========================================================

use crate::support::scenario::state::{Entry, State};
use std::path::Path;

fn text_of(state: &State, path: &str) -> String {
    match state.entry(path) {
        Some(Entry::File { content, .. }) => String::from_utf8(content.clone()).expect("UTF-8"),
        other => panic!("{path} is not a file in the state: {other:?}"),
    }
}

fn normalised_state(collection: &Collection) -> State {
    State::normalised(
        &collection.root(),
        &[(collection.root().as_path(), "<root>")],
    )
}

#[test]
fn a_raw_state_sees_modes_times_and_dangling_links() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("a", |piece| {
            piece.file("f", "x").symlink("dangling", "missing");
        })
        .finish();
    let before = State::raw(&collection.root());
    assert!(before.paths().contains(&"a/dangling"));
    assert_eq!(
        before.entry("a/dangling"),
        Some(&Entry::Link("missing".to_string()))
    );
    assert!(before.diff(&State::raw(&collection.root())).is_none());
    filetime::set_file_mtime(
        collection.path("a/f"),
        filetime::FileTime::from_unix_time(clock.days_ago(1), 0),
    )
    .expect("the time can be set");
    let diff = before
        .diff(&State::raw(&collection.root()))
        .expect("the time changed");
    assert!(diff.contains("a/f"), "{diff}");
}

#[test]
fn a_diff_names_added_removed_and_changed_paths() {
    let clock = Clock::now();
    let one = Collection::build(&clock)
        .piece("a", |piece| {
            piece
                .file("same", "x")
                .file("changed", "1")
                .file("gone", "g");
        })
        .finish();
    let two = Collection::build(&clock)
        .piece("a", |piece| {
            piece
                .file("same", "x")
                .file("changed", "2")
                .file("new", "n");
        })
        .finish();
    let diff = normalised_state(&one)
        .diff(&normalised_state(&two))
        .expect("the states differ");
    assert!(diff.contains("only in the first: a/gone"), "{diff}");
    assert!(diff.contains("only in the second: a/new"), "{diff}");
    assert!(diff.contains("changed: a/changed"), "{diff}");
    assert!(!diff.contains("a/same"), "{diff}");
}

#[test]
fn the_world_root_is_replaced_in_text_and_link_targets() {
    let clock = Clock::now();
    let build = || {
        let collection = Collection::build(&clock).finish();
        let root = collection.root();
        fs::write(collection.path("note"), format!("at {}\n", root.display())).expect("writes");
        std::os::unix::fs::symlink(&root, collection.path("back")).expect("links");
        collection
    };
    let (first, second) = (build(), build());
    let state = normalised_state(&first);
    assert_eq!(text_of(&state, "note"), "at <root>\n");
    assert_eq!(
        state.entry("back"),
        Some(&Entry::Link("<root>".to_string()))
    );
    assert!(state.diff(&normalised_state(&second)).is_none());
}

#[test]
fn the_collection_id_of_the_config_is_replaced_and_other_keys_are_not() {
    let clock = Clock::now();
    let build = |id: &str, idle_days: u32| {
        Collection::build(&clock)
            .id(id)
            .idle_days(idle_days)
            .finish()
    };
    let one = normalised_state(&build("01JBX0000000000000000000AA", 30));
    let other_id = normalised_state(&build("01JBX0000000000000000000BB", 30));
    assert_eq!(
        text_of(&one, ".antiq/config.toml").lines().nth(1),
        Some("id = \"<id>\"")
    );
    assert!(one.diff(&other_id).is_none());
    let other_days = normalised_state(&build("01JBX0000000000000000000AA", 45));
    assert!(one.diff(&other_days).is_some());
}

#[test]
fn a_damaged_archive_is_compared_by_its_bytes() {
    let clock = Clock::now();
    let build = || {
        let collection = Collection::build(&clock)
            .version("a", "2026-09-03T101500", &[("f", "content")])
            .finish();
        collection.damage_truncate_archive("a", "2026-09-03T101500");
        collection
    };
    let (one, two) = (build(), build());
    assert!(
        normalised_state(&one)
            .diff(&normalised_state(&two))
            .is_none()
    );
}

fn write_store_file(collection: &Collection, name: &str, content: &str) {
    let dir = collection.path(".antiq/pieces/a");
    fs::create_dir_all(&dir).expect("the store folder is made");
    fs::write(dir.join(name), content).expect("the file is written");
}

#[test]
fn stamps_are_replaced_by_their_order_and_stay_distinct() {
    let clock = Clock::now();
    let build = |first: &str, second: &str| {
        let collection = Collection::build(&clock).finish();
        write_store_file(
            &collection,
            &format!("a--{first}.tar.zst.sha256"),
            &format!("{first}\n"),
        );
        write_store_file(
            &collection,
            &format!("a--{second}.tar.zst.sha256"),
            &format!("{second}\n"),
        );
        collection
    };
    let one = normalised_state(&build("2026-09-03T101500", "2026-09-03T101500-2"));
    let two = normalised_state(&build("2026-11-04T090211", "2026-11-04T090211-2"));
    assert_eq!(
        one.paths()
            .iter()
            .filter(|path| path.contains("a--<v"))
            .count(),
        2
    );
    assert!(one.diff(&two).is_none(), "{:?}", one.diff(&two));
    let swapped = normalised_state(&build("2026-11-04T090211", "2026-11-05T090211"));
    assert!(swapped.paths().iter().any(|path| path.contains("<v2>")));
}

#[test]
fn stamps_of_two_pieces_get_the_same_labels_whether_or_not_a_second_lies_between() {
    let clock = Clock::now();
    let build = |for_a: &str, for_b: &str| {
        let collection = Collection::build(&clock).finish();
        for (piece, stamp) in [("a", for_a), ("b", for_b)] {
            let dir = collection.path(&format!(".antiq/pieces/{piece}"));
            fs::create_dir_all(&dir).expect("the store folder is made");
            fs::write(
                dir.join(format!("{piece}--{stamp}.tar.zst.sha256")),
                format!("{stamp}\n"),
            )
            .expect("the file is written");
        }
        collection
    };
    let same_second = normalised_state(&build("2026-11-04T090211", "2026-11-04T090211"));
    let across = normalised_state(&build("2026-11-04T090211", "2026-11-04T090212"));
    assert!(
        same_second.diff(&across).is_none(),
        "{:?}",
        same_second.diff(&across)
    );
}

#[test]
fn times_are_replaced_by_key_but_file_times_are_kept() {
    let clock = Clock::now();
    let build = |archived: &str, restored: &str| {
        let collection = Collection::build(&clock).finish();
        fs::write(
            collection.path("listing.txt"),
            format!(
                "# piece a\n# archived {archived}\n# imported {archived}\n# rebuilt {archived}\n\
                 file 0644 2026-10-30T17:44:02Z 5 f\n"
            ),
        )
        .expect("writes");
        fs::write(
            collection.path("restored"),
            format!("restored {restored}\n"),
        )
        .expect("writes");
        collection
    };
    let one = normalised_state(&build("2026-09-03T10:15:00Z", "2026-09-03T10:15:00Z"));
    let two = normalised_state(&build("2026-11-04T09:02:11Z", "2026-11-04T09:02:11Z"));
    assert!(one.diff(&two).is_none());
    assert!(text_of(&one, "listing.txt").contains("# archived <time>\n"));
    assert!(text_of(&one, "listing.txt").contains("file 0644 2026-10-30T17:44:02Z 5 f"));
    assert_eq!(text_of(&one, "restored"), "restored <time>\n");
}

fn tar_zst(members: &[(&str, &str, u64)], level: i32) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, content, mtime) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(*mtime);
        builder
            .append_data(&mut header, path, content.as_bytes())
            .expect("the member is appended");
    }
    let tar = builder.into_inner().expect("the tar is finished");
    zstd::stream::encode_all(&tar[..], level).expect("the tar compresses")
}

#[test]
fn archives_compare_by_members_not_by_bytes() {
    let clock = Clock::now();
    let members = [("a/f", "x", 100), ("a/g", "y", 200)];
    let build = |level: i32, members: &[(&str, &str, u64)]| {
        let collection = Collection::build(&clock).finish();
        let dir = collection.path(".antiq/pieces/a");
        fs::create_dir_all(&dir).expect("made");
        fs::write(
            dir.join("a--2026-09-03T101500.tar.zst"),
            tar_zst(members, level),
        )
        .expect("writes");
        collection
    };
    let fast = normalised_state(&build(1, &members));
    let slow = normalised_state(&build(19, &members));
    assert!(fast.diff(&slow).is_none());
    let changed = normalised_state(&build(1, &[("a/f", "x", 100), ("a/g", "CHANGED", 200)]));
    let diff = fast.diff(&changed).expect("a member differs");
    assert!(diff.contains(".tar.zst"), "{diff}");
    let retimed = normalised_state(&build(1, &[("a/f", "x", 100), ("a/g", "y", 201)]));
    assert!(fast.diff(&retimed).is_some());
}

#[test]
fn compressed_listings_and_checksums_are_compared_as_text() {
    let clock = Clock::now();
    let build = |hash: &str, archived: &str| {
        let collection = Collection::build(&clock).finish();
        let dir = collection.path(".antiq/pieces/a");
        fs::create_dir_all(&dir).expect("made");
        fs::write(
            dir.join("a--2026-09-03T101500.tar.zst.sha256"),
            format!("{hash}  a--2026-09-03T101500.tar.zst\n"),
        )
        .expect("writes");
        let listing = format!("# archived {archived}\n{hash}  a/f\n");
        fs::write(
            dir.join("a--2026-09-03T101500.sha256.zst"),
            zstd::stream::encode_all(listing.as_bytes(), 3).expect("compresses"),
        )
        .expect("writes");
        collection
    };
    let one = normalised_state(&build(&"a".repeat(64), "2026-09-03T10:15:00Z"));
    let two = normalised_state(&build(&"b".repeat(64), "2026-11-04T09:02:11Z"));
    let sums = text_of(&one, ".antiq/pieces/a/a--<v1>.tar.zst.sha256");
    assert_eq!(sums, "<sha256>  a--<v1>.tar.zst\n");
    assert!(text_of(&one, ".antiq/pieces/a/a--<v1>.sha256.zst").contains("# archived <time>\n"));
    assert!(
        one.diff(&two).is_some(),
        "entry hashes of the listing still differ"
    );
}

#[test]
fn the_lock_file_is_not_part_of_the_state() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    fs::write(collection.path(".antiq/lock"), "123").expect("writes");
    assert!(
        !normalised_state(&collection)
            .paths()
            .contains(&".antiq/lock")
    );
}

#[test]
fn a_missing_path_is_none() {
    let clock = Clock::now();
    let collection = Collection::build(&clock).finish();
    assert!(State::raw(&collection.root()).entry("nothing").is_none());
    let _ = Path::new("");
}

// =========================================================
// Worlds, modes and the comparison of end states
// =========================================================

use crate::support::scenario::world::{Mode, Setup, World, both_modes};
use std::cell::{Cell, RefCell};

const FAKE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/scenario-fake.sh"
);

fn standin(clock: &Clock) -> Setup {
    Setup::new(Collection::build(clock).finish()).with_program(FAKE)
}

fn standin_world(mode: Mode) -> World {
    World::new(standin(&Clock::now()), mode)
}

#[test]
fn the_script_runs_once_per_mode_and_each_run_answers_in_its_mode() {
    let seen = RefCell::new(Vec::new());
    both_modes(standin, |world| {
        let outcome = world.run(&["put", "out", "a.txt"]);
        outcome.expect_code(0);
        outcome.text_only(|out| assert_eq!(out, "a.txt\n"));
        outcome.json_only(|value| assert_eq!(value["path"], "a.txt"));
        seen.borrow_mut().push((world.mode(), outcome.mode));
    });
    assert_eq!(
        *seen.borrow(),
        [(Mode::Text, Mode::Text), (Mode::Json, Mode::Json)]
    );
}

#[test]
#[should_panic(expected = "in the collection:\nchanged: out/a.txt")]
fn end_states_that_differ_between_the_modes_fail() {
    both_modes(standin, |world| {
        world.run(&["put", "out", "a.txt", "modedep"]);
    });
}

#[test]
#[should_panic(expected = "in the target:\nchanged: a.txt")]
fn a_target_that_differs_between_the_modes_fails() {
    both_modes(
        |clock| standin(clock).with_target(Target::new()),
        |world| {
            let target = world.target().expect("the setup has a target").path();
            world.run(&["put", target.to_str().expect("UTF-8"), "a.txt", "modedep"]);
        },
    );
}

#[test]
fn a_removed_target_compares_as_empty() {
    both_modes(
        |clock| standin(clock).with_target(Target::new()),
        |world| world.target().expect("the setup has a target").remove(),
    );
}

#[test]
#[should_panic(expected = "stdout is not JSON")]
fn output_that_breaks_the_envelope_fails_the_run_in_json_mode() {
    standin_world(Mode::Json).run(&["badjson"]);
}

#[test]
fn text_mode_does_not_check_the_envelope() {
    let outcome = standin_world(Mode::Text).run(&["badjson"]);
    assert_eq!(outcome.stdout, "this is not json\n");
}

#[test]
fn a_failing_command_reports_its_exit_code_and_error_kind() {
    let text = standin_world(Mode::Text).run(&["fail"]);
    assert_eq!(text.code(), 1);
    assert_eq!(text.stderr, "error: it failed\n");
    assert_eq!(text.error_kind(), None);
    let json = standin_world(Mode::Json).run(&["fail"]);
    assert_eq!(json.code(), 1);
    assert_eq!(json.error_kind().as_deref(), Some("other"));
}

#[test]
fn commands_run_in_a_fixed_time_zone() {
    let outcome = standin_world(Mode::Text).run(&["tz"]);
    assert_eq!(outcome.stdout, "Pacific/Auckland\n");
}

#[test]
fn directory_times_are_restored_after_every_run() {
    let clock = Clock::now();
    let setup = Setup::new(
        Collection::build(&clock)
            .piece("p", |piece| {
                piece.file("sub/f", "x");
            })
            .finish(),
    )
    .with_program(FAKE);
    let world = World::new(setup, Mode::Text);
    world.run(&["touchdir", "p/sub"]);
    assert!(world.collection().path("p/sub/.touched").exists());
    assert_eq!(
        mtime(&world.collection().path("p/sub")),
        clock.dir_time(),
        "settle gives the touched directory its time again"
    );
}

#[test]
fn run_in_runs_with_the_given_directory_as_the_current_one() {
    for mode in [Mode::Text, Mode::Json] {
        let clock = Clock::now();
        let setup = Setup::new(
            Collection::build(&clock)
                .piece("p", |piece| {
                    piece.file("f", "x");
                })
                .finish(),
        )
        .with_program(FAKE);
        let world = World::new(setup, mode);
        world
            .run_in(&world.collection().path("p"), &["put", "out", "a.txt"])
            .expect_code(0);
        assert_eq!(
            fs::read_to_string(world.collection().path("p/out/a.txt")).expect("the file exists"),
            "same"
        );
    }
}

#[test]
fn run_as_runs_against_another_collection() {
    for mode in [Mode::Text, Mode::Json] {
        let world = standin_world(mode);
        let other = Collection::build(world.collection().clock()).finish();
        world
            .run_as(&other, &["put", "out", "a.txt"])
            .expect_code(0);
        assert!(other.path("out/a.txt").exists());
        assert!(!world.collection().path("out/a.txt").exists());
    }
}

#[test]
#[should_panic(expected = "expected exit 3")]
fn an_unexpected_exit_code_fails() {
    standin_world(Mode::Text)
        .run(&["put", "out", "a.txt"])
        .expect_code(3);
}

#[test]
fn keys_are_the_ordered_keys_of_the_json_object_and_ignored_in_text() {
    standin_world(Mode::Json)
        .run(&["put", "out", "a.txt"])
        .keys(&["format", "command", "path"]);
    standin_world(Mode::Text)
        .run(&["put", "out", "a.txt"])
        .keys(&["anything"]);
}

#[test]
#[should_panic(expected = "keys of the JSON object")]
fn other_keys_fail() {
    standin_world(Mode::Json)
        .run(&["put", "out", "a.txt"])
        .keys(&["format", "path", "command"]);
}

#[test]
fn json_only_and_text_only_run_in_their_mode_only() {
    for mode in [Mode::Text, Mode::Json] {
        let ran = Cell::new((false, false));
        let outcome = standin_world(mode).run(&["put", "out", "a.txt"]);
        outcome.json_only(|_| ran.set((true, ran.get().1)));
        outcome.text_only(|_| ran.set((ran.get().0, true)));
        assert_eq!(ran.get(), (mode == Mode::Json, mode == Mode::Text));
    }
}

#[test]
fn commands_that_change_nothing_pass_the_unchanged_check() {
    let world = standin_world(Mode::Text);
    world.assert_unchanged(|| {
        world.run(&["tz"]);
    });
}

#[test]
#[should_panic(expected = "changed the collection:\nonly in the second: out")]
fn a_change_of_the_collection_fails_the_unchanged_check() {
    let world = standin_world(Mode::Text);
    world.assert_unchanged(|| {
        world.run(&["put", "out", "a.txt"]);
    });
}

#[test]
#[should_panic(expected = "changed the target:\nonly in the second: a.txt")]
fn a_change_of_the_target_fails_the_unchanged_check() {
    let world = World::new(
        standin(&Clock::now()).with_target(Target::new()),
        Mode::Text,
    );
    let target = world.target().expect("the setup has a target").path();
    world.assert_unchanged(|| {
        world.run(&["put", target.to_str().expect("UTF-8"), "a.txt"]);
    });
}

// =========================================================
// Running on a terminal
// =========================================================

#[test]
fn a_piped_run_sees_no_terminal_and_a_tty_run_sees_one() {
    let world = standin_world(Mode::Text);
    assert_eq!(world.run(&["tty"]).stdout, "pipe\n");
    assert_eq!(world.run_tty(&["tty"]).stdout, "tty\n");
}

#[test]
fn a_tty_run_ignores_the_json_mode_and_returns_text_with_plain_newlines() {
    let outcome = standin_world(Mode::Json).run_tty(&["tz"]);
    assert_eq!(outcome.mode, Mode::Text);
    assert_eq!(outcome.stdout, "Pacific/Auckland\n");
    assert_eq!(outcome.code(), 0);
}

#[test]
fn a_tty_run_collects_output_larger_than_the_terminal_buffer() {
    let world = standin_world(Mode::Text);
    let outcome = world.run_tty(&["flood"]);
    assert_eq!(outcome.stdout, "line\n".repeat(20_000));
}

#[test]
#[should_panic(expected = "did not finish within")]
fn a_command_on_a_terminal_that_does_not_finish_is_killed_and_fails() {
    standin_world(Mode::Text).run_tty_within(&["hang"], std::time::Duration::from_millis(300));
}

#[test]
fn run_within_kills_a_command_at_the_deadline_and_returns_finished_runs() {
    use crate::support::cmd::Antiq;
    use std::time::{Duration, Instant};
    let started = Instant::now();
    let hung = Antiq::program(FAKE)
        .arg("hang")
        .run_within(Duration::from_millis(300));
    assert!(hung.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the child was killed"
    );
    // `Antiq::program` passes the environment of the test run on, so the
    // zone of the developer's shell has to be taken out to see that the
    // program itself sets none.
    let done = Antiq::program(FAKE)
        .env_remove("TZ")
        .args(["tz"])
        .run_within(Duration::from_secs(20))
        .expect("tz finishes");
    assert_eq!(done.stdout, "unset\n");
    assert_eq!(done.code(), 0);
}

// =========================================================
// Stored versions, damage, ageing and sources
// =========================================================

use crate::support::scenario::versions::Source;
use std::process::Command;

const STAMP: &str = "2026-09-03T101500";

fn collection_with_version() -> Collection {
    Collection::build(&Clock::now())
        .version(
            "old/client-site",
            STAMP,
            &[("src/main.rs", "fn main() {}\n"), ("README", "hello\n")],
        )
        .finish()
}

fn shasum(folder: &std::path::Path, file: &std::path::Path) -> bool {
    Command::new("shasum")
        .args(["-a", "256", "-c"])
        .arg(file)
        .current_dir(folder)
        .output()
        .expect("shasum runs")
        .status
        .success()
}

#[test]
fn a_written_version_has_three_files_named_after_the_piece_and_stamp() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let folder = collection.path(".antiq/pieces/old/client-site");
    for (path, name) in [
        (&files.archive, "old-client-site--2026-09-03T101500.tar.zst"),
        (
            &files.checksum,
            "old-client-site--2026-09-03T101500.tar.zst.sha256",
        ),
        (
            &files.listing,
            "old-client-site--2026-09-03T101500.sha256.zst",
        ),
    ] {
        assert_eq!(path, &folder.join(name));
        assert!(path.exists(), "{name} is written");
    }
}

#[test]
fn the_checksum_file_passes_shasum_in_the_store_folder() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let folder = collection.path(".antiq/pieces/old/client-site");
    let text = fs::read_to_string(&files.checksum).expect("the checksum file exists");
    assert!(
        text.ends_with("  old-client-site--2026-09-03T101500.tar.zst\n"),
        "{text:?}"
    );
    assert!(shasum(&folder, &files.checksum));
}

#[test]
fn the_listing_is_a_valid_listing_of_the_files_and_names_the_archive_size() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let listing = antiq::listing::read_full(fs::File::open(&files.listing).expect("it exists"))
        .expect("antiq reads the listing in full");
    assert_eq!(listing.header.piece.to_string(), "old/client-site");
    assert_eq!(
        listing.header.archive,
        "old-client-site--2026-09-03T101500.tar.zst"
    );
    assert_eq!(
        listing.header.archive_bytes,
        fs::metadata(&files.archive).expect("it exists").len()
    );
    assert_eq!(listing.header.archived.to_string(), "2026-09-03T10:15:00Z");
    let paths: Vec<String> = listing.entries.iter().map(|e| e.path.to_string()).collect();
    assert_eq!(paths, ["README", "src", "src/main.rs"]);
    assert_eq!((listing.files, listing.bytes), (2, 19));
}

#[test]
fn the_archive_unpacks_to_one_directory_named_after_the_folder() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let bytes = zstd::stream::decode_all(fs::File::open(&files.archive).expect("it exists"))
        .expect("the archive decompresses");
    let mut names: Vec<String> = tar::Archive::new(&bytes[..])
        .entries()
        .expect("the archive lists")
        .map(|e| {
            e.expect("an entry")
                .path()
                .expect("a path")
                .display()
                .to_string()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "client-site",
            "client-site/README",
            "client-site/src",
            "client-site/src/main.rs"
        ]
    );
}

#[test]
fn the_name_part_keeps_safe_characters_and_collapses_the_rest() {
    let collection = Collection::build(&Clock::now())
        .version("My Docs/Süd", STAMP, &[("a", "x")])
        .finish();
    let folder = collection.path(".antiq/pieces/My Docs/Süd");
    assert!(
        folder
            .join("My-Docs-S-d--2026-09-03T101500.tar.zst")
            .exists()
    );
}

#[test]
fn a_counter_stamp_is_part_of_the_file_names() {
    let collection = Collection::build(&Clock::now())
        .version("p", "2026-09-03T101500-2", &[("a", "x")])
        .finish();
    assert!(
        collection
            .path(".antiq/pieces/p/p--2026-09-03T101500-2.sha256.zst")
            .exists()
    );
}

#[test]
fn damage_helpers_break_one_file_each() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let folder = collection.path(".antiq/pieces/old/client-site");
    let size = fs::metadata(&files.archive).expect("it exists").len();

    collection.damage_truncate_archive("old/client-site", STAMP);
    let truncated = fs::metadata(&files.archive).expect("it exists").len();
    assert!(truncated < size, "{truncated} < {size}");
    assert!(
        !shasum(&folder, &files.checksum),
        "the checksum no longer matches"
    );

    collection.damage_corrupt_checksum("old/client-site", STAMP);
    let text = fs::read_to_string(&files.checksum).expect("it exists");
    assert!(!text.contains("tar.zst"), "{text:?}");

    collection.damage_remove_listing("old/client-site", STAMP);
    assert!(!files.listing.exists());
    assert!(files.archive.exists() && files.checksum.exists());

    collection.damage_remove_archive("old/client-site", STAMP);
    assert!(!files.archive.exists());
    assert!(files.checksum.exists());
}

#[test]
fn set_age_ages_the_files_of_a_piece_and_rewrites_the_restored_marker() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("p", |piece| {
            piece
                .age_days(10)
                .file("a", "x")
                .file("sub/b", "y")
                .restored(10, Some(STAMP));
        })
        .finish();
    collection.set_age("p", 50);
    assert_eq!(mtime(&collection.path("p/a")), clock.days_ago(50));
    assert_eq!(mtime(&collection.path("p/sub/b")), clock.days_ago(50));
    assert_eq!(
        mtime(&collection.path("p/.antiq-restored")),
        clock.days_ago(50)
    );
    let marker = fs::read_to_string(collection.path("p/.antiq-restored")).expect("it exists");
    let time = jiff::Timestamp::from_second(clock.days_ago(50))
        .expect("representable")
        .strftime("%Y-%m-%dT%H:%M:%SZ");
    assert_eq!(marker, format!("restored {time} version {STAMP}\n"));
}

#[test]
fn set_age_leaves_a_piece_without_marker_without_one() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("p", |piece| {
            piece.file("a", "x");
        })
        .finish();
    collection.set_age("p", 50);
    assert!(!collection.path("p/.antiq-restored").exists());
}

#[test]
fn a_source_is_a_folder_outside_the_collection_with_aged_files() {
    let clock = Clock::now();
    let source = Source::new(&clock, |piece| {
        piece.age_days(40).file("notes/a.txt", "x");
    });
    let file = source.path().join("notes/a.txt");
    assert_eq!(fs::read_to_string(&file).expect("it exists"), "x");
    assert_eq!(mtime(&file), clock.days_ago(40));
}

#[test]
fn the_directories_of_a_source_carry_the_clocks_directory_time() {
    let clock = Clock::now();
    let source = Source::new(&clock, |piece| {
        piece.age_days(40).file("notes/deep/a.txt", "x");
    });
    for dir in ["", "notes", "notes/deep"] {
        assert_eq!(
            mtime(&source.path().join(dir)),
            clock.dir_time(),
            "directory {dir:?}"
        );
    }
}

// =========================================================
// Checks without antiq
// =========================================================

use crate::support::scenario::outsider::{
    StoredVersion, shasum_check, shasum_check_with, store_files, tar_members,
};

#[test]
fn shasum_check_accepts_a_matching_checksum_file() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    shasum_check(
        &collection.path(".antiq/pieces/old/client-site"),
        &files.checksum,
    );
}

#[test]
#[should_panic(expected = "shasum -c failed")]
fn shasum_check_rejects_a_changed_archive() {
    let collection = collection_with_version();
    collection.damage_truncate_archive("old/client-site", STAMP);
    let files = collection.version_files("old/client-site", STAMP);
    shasum_check(
        &collection.path(".antiq/pieces/old/client-site"),
        &files.checksum,
    );
}

#[test]
#[should_panic(expected = "cannot run no-such-shasum")]
fn a_missing_shasum_is_named() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    shasum_check_with(
        "no-such-shasum",
        &collection.path(".antiq/pieces/old/client-site"),
        &files.checksum,
    );
}

#[test]
fn tar_members_lists_the_archive_sorted() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    assert_eq!(
        tar_members(&files.archive),
        [
            "client-site",
            "client-site/README",
            "client-site/src",
            "client-site/src/main.rs"
        ]
    );
}

use crate::support::scenario::outsider::{listing_check, tar_extract, tar_extract_with};

#[test]
fn tar_extract_unpacks_the_archive_into_the_given_directory() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let dest = scratch.path().join("unpacked");
    tar_extract(&files.archive, &dest);
    assert_eq!(
        fs::read_to_string(dest.join("client-site/src/main.rs")).expect("the file is there"),
        "fn main() {}\n"
    );
    assert_eq!(
        fs::read_to_string(dest.join("client-site/README")).expect("the file is there"),
        "hello\n"
    );
}

#[test]
#[should_panic(expected = "cannot run no-such-tar")]
fn a_missing_tar_is_named() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let scratch = tempfile::tempdir().expect("a scratch directory");
    tar_extract_with("no-such-tar", &files.archive, &scratch.path().join("out"));
}

#[test]
#[should_panic(expected = "tar failed")]
fn a_damaged_archive_fails_the_extraction() {
    let collection = collection_with_version();
    collection.damage_truncate_archive("old/client-site", STAMP);
    let files = collection.version_files("old/client-site", STAMP);
    let scratch = tempfile::tempdir().expect("a scratch directory");
    tar_extract(&files.archive, &scratch.path().join("out"));
}

#[test]
fn listing_check_accepts_the_unpacked_tree() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let dest = scratch.path().join("unpacked");
    tar_extract(&files.archive, &dest);
    listing_check(
        &files.listing,
        &dest.join("client-site"),
        &scratch.path().join("listing"),
    );
}

#[test]
#[should_panic(expected = "shasum -c failed")]
fn listing_check_rejects_a_changed_file() {
    let collection = collection_with_version();
    let files = collection.version_files("old/client-site", STAMP);
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let dest = scratch.path().join("unpacked");
    tar_extract(&files.archive, &dest);
    fs::write(dest.join("client-site/README"), "changed\n").expect("the file is rewritten");
    listing_check(
        &files.listing,
        &dest.join("client-site"),
        &scratch.path().join("listing"),
    );
}

fn stored(stamp: &str, folder: &std::path::Path, stem: &str, parts: [bool; 3]) -> StoredVersion {
    let file = |present: bool, suffix: &str| {
        present.then(|| folder.join(format!("{stem}--{stamp}{suffix}")))
    };
    StoredVersion {
        stamp: stamp.to_string(),
        archive: file(parts[0], ".tar.zst"),
        checksum: file(parts[1], ".tar.zst.sha256"),
        listing: file(parts[2], ".sha256.zst"),
    }
}

#[test]
fn store_files_finds_each_version_by_suffix_in_stamp_order() {
    let collection = Collection::build(&Clock::now())
        .version("p/q", "2026-09-03T101500-10", &[("a", "x")])
        .version("p/q", "2026-09-03T101500", &[("a", "x")])
        .version("p/q", "2026-09-03T101500-2", &[("a", "x")])
        .version("p/q", "2026-08-01T000000", &[("a", "x")])
        .finish();
    collection.damage_remove_archive("p/q", "2026-08-01T000000");
    let folder = collection.path(".antiq/pieces/p/q");
    assert_eq!(
        store_files(&collection, "p/q"),
        [
            stored("2026-08-01T000000", &folder, "p-q", [false, true, true]),
            stored("2026-09-03T101500", &folder, "p-q", [true, true, true]),
            stored("2026-09-03T101500-2", &folder, "p-q", [true, true, true]),
            stored("2026-09-03T101500-10", &folder, "p-q", [true, true, true]),
        ]
    );
}

#[test]
fn a_version_exists_when_its_archive_does() {
    let collection = Collection::build(&Clock::now())
        .version("p", "2026-09-03T101500-2", &[("a", "x")])
        .version("p", "2026-09-03T101500", &[("a", "x")])
        .version("p", "2026-08-01T000000", &[("a", "x")])
        .finish();
    collection.damage_remove_archive("p", "2026-08-01T000000");
    assert_eq!(
        collection.versions("p"),
        ["2026-09-03T101500", "2026-09-03T101500-2"]
    );
}

#[test]
fn a_piece_without_a_store_folder_has_no_versions() {
    let collection = Collection::build(&Clock::now()).finish();
    assert!(store_files(&collection, "nothing").is_empty());
    assert!(collection.versions("nothing").is_empty());
}

// =========================================================
// Terminal output without colour
// =========================================================

#[test]
fn colour_sequences_are_removed_and_text_is_kept() {
    use crate::support::scenario::world::strip_colour;
    assert_eq!(strip_colour("\x1b[1;31mred\x1b[0m text"), "red text");
    assert_eq!(strip_colour("plain\n  indented\n"), "plain\n  indented\n");
    assert_eq!(strip_colour("a\x1b[0mb\x1b[32mc"), "abc");
}
