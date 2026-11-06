// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq init` seen from outside the binary. The binary runs detached from
//! any terminal, so every run here is one without a question; the
//! questions are tested against the library with scripted answers.

mod support;

use antiq::config::Config;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::tree::Tree;

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("the path exists")
}

fn config_of(root: &Path) -> Config {
    Config::read(&root.join(".antiq/config.toml")).expect("the config can be read")
}

/// `antiq -C <root> <args>`.
fn init_in(tree: &Tree, args: &[&str]) -> Run {
    Antiq::new()
        .arg("-C")
        .arg(tree.root())
        .arg("init")
        .args(args)
        .run()
}

/// Text with what differs from machine to machine replaced.
fn normalised(text: &str, tree: &Tree) -> String {
    let machine = antiq::machine::detect().expect("the machine has a name");
    text.replace(
        canonical(tree.root()).to_str().expect("UTF-8 path"),
        "<root>",
    )
    .replace(&machine, "<machine>")
}

/// A tree with the layout `archive.sh` leaves, next to the pieces `a` and `b`.
fn archive_sh_tree() -> Tree {
    let mut tree = Tree::new();
    tree.dir("a");
    tree.dir("b");
    tree.file("archive/one.tar.xz", "x");
    tree.file("archive/index/one.tar.xz.lst", "x");
    tree
}

fn pieces(json: &Value) -> Vec<&str> {
    json["pieces"]
        .as_array()
        .expect("pieces is a list")
        .iter()
        .map(|piece| piece.as_str().expect("a piece is a string"))
        .collect()
}

// =========================================================
// Creating
// =========================================================

#[test]
fn yes_with_json_creates_the_collection() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--json", "--yes"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.stderr, "");
    let json = run.json();
    assert_eq!(json["command"], "init");
    assert_eq!(json["created"], true);
    assert_eq!(
        json["path"],
        canonical(tree.root()).to_str().expect("UTF-8")
    );
    assert_eq!(json["machine"], antiq::machine::detect().expect("a name"));
    assert_eq!(json["patterns"], json!(["*"]));
    assert_eq!(json["exclude"], json!([]));
    assert_eq!(pieces(&json), ["a"]);
    assert_eq!(
        json["actions"],
        json!([{"action": "create_collection", "path": canonical(tree.root())}])
    );

    let config = config_of(tree.root());
    assert_eq!(json["id"], config.collection.id.to_string());
    assert_eq!(config.collection.patterns, ["*"]);
    assert_eq!(
        config.machine.name,
        Some(antiq::machine::detect().expect("a name"))
    );
    let mut names: Vec<String> = fs::read_dir(tree.path(".antiq"))
        .expect("the marker can be listed")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "config.toml",
            "doomed",
            "pieces",
            "recovered",
            "tmp",
            "trash"
        ]
    );
}

#[test]
fn the_json_of_a_created_collection_has_its_keys_in_order() {
    let tree = Tree::new();
    let run = init_in(&tree, &["--json", "--yes"]);
    let json = run.json();
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(
        keys,
        [
            "format", "command", "created", "id", "path", "machine", "patterns", "exclude",
            "pieces", "actions", "warnings"
        ]
    );
}

#[test]
fn yes_prints_a_summary_on_the_console() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--yes"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
    let id = config_of(tree.root()).collection.id.to_string();
    insta::assert_snapshot!(normalised(&run.stdout, &tree).replace(&id, "<id>"), @r"
    Created the collection at <root>
      id: <id>
      machine: <machine>
      patterns: *
      pieces: 1
    ");
}

#[test]
fn patterns_repeat() {
    let mut tree = Tree::new();
    tree.dir("a1");
    tree.dir("b1");
    tree.dir("c1");
    let run = init_in(
        &tree,
        &["--json", "--yes", "--pattern", "a*", "--pattern", "b*"],
    );
    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert_eq!(pieces(&run.json()), ["a1", "b1"]);
    assert_eq!(config_of(tree.root()).collection.patterns, ["a*", "b*"]);
}

#[test]
fn the_directory_defaults_to_the_current_one() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = Antiq::new()
        .current_dir(tree.root())
        .args(["init", "--json", "--yes"])
        .run();
    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert!(tree.path(".antiq/config.toml").is_file());
}

#[test]
fn an_invalid_pattern_is_a_usage_error() {
    let tree = Tree::new();
    let run = init_in(&tree, &["--json", "--yes", "--pattern", "a//b"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert!(
        run.error_message().contains("pattern a//b is invalid"),
        "{}",
        run.error_message()
    );
    assert!(!tree.path(".antiq").exists());
}

// =========================================================
// Without a question
// =========================================================

#[test]
fn without_yes_and_a_terminal_it_creates_nothing_and_exits_3() {
    let mut tree = Tree::new();
    tree.dir("a");
    tree.dir("b");
    let run = init_in(&tree, &[]);
    assert_eq!(run.code(), 3, "stderr: {}", run.stderr);
    assert!(!tree.path(".antiq").exists());
    insta::assert_snapshot!(normalised(&run.stdout, &tree), @r"
    Would create a collection at <root>
      machine: <machine>
      patterns: *
    pieces (2):
      a
      b
    ");
    insta::assert_snapshot!(run.stderr, @"nothing was created; run with --yes to create the collection without asking
");
}

#[test]
fn json_without_yes_creates_nothing_and_exits_3() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--json"]);
    assert_eq!(run.code(), 3, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
    let json = run.json();
    assert_eq!(json["created"], false);
    assert!(json.get("id").is_none());
    assert!(json.get("dry_run").is_none());
    assert_eq!(pieces(&json), ["a"]);
    assert!(!tree.path(".antiq").exists());
}

#[test]
fn the_plan_without_yes_shows_the_proposal_and_yes_agrees() {
    let tree = archive_sh_tree();
    let plan = init_in(&tree, &["--json"]);
    assert_eq!(plan.code(), 3);
    let planned = plan.json();
    assert_eq!(planned["exclude"], json!(["archive"]));
    assert_eq!(pieces(&planned), ["a", "b"]);
    assert!(!tree.path(".antiq").exists());

    let done = init_in(&tree, &["--json", "--yes"]);
    assert_eq!(done.code(), 0);
    let created = done.json();
    assert_eq!(created["exclude"], planned["exclude"]);
    assert_eq!(created["pieces"], planned["pieces"]);
}

#[test]
fn the_console_plan_marks_the_proposed_exclude() {
    let tree = archive_sh_tree();
    let run = init_in(&tree, &[]);
    assert_eq!(run.code(), 3);
    insta::assert_snapshot!(normalised(&run.stdout, &tree), @r"
    Would create a collection at <root>
      machine: <machine>
      patterns: *
      exclude: archive (proposed: archive.sh layout)
    pieces (2):
      a
      b
    ");
}

#[test]
fn yes_takes_the_proposed_exclude() {
    let tree = archive_sh_tree();
    let run = init_in(&tree, &["--yes"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let exclude: Vec<String> = config_of(tree.root())
        .collection
        .exclude
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(exclude, ["archive"]);
}

#[test]
fn a_pattern_with_double_star_prints_a_note() {
    let mut tree = Tree::new();
    tree.dir("a/b");
    let run = init_in(&tree, &["--yes", "--pattern", "a/**"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stderr.starts_with("note: pattern a/** contains `**`"),
        "{}",
        run.stderr
    );
    assert_eq!(run.stderr.lines().count(), 1);
}

#[test]
fn under_json_the_note_is_in_the_warnings_and_stderr_stays_empty() {
    let mut tree = Tree::new();
    tree.dir("a/b");
    let run = init_in(&tree, &["--json", "--yes", "--pattern", "a/**"]);
    assert_eq!(run.code(), 0);
    assert_eq!(run.stderr, "");
    let warnings = run.json()["warnings"].clone();
    assert_eq!(warnings.as_array().expect("a list").len(), 1);
    assert!(
        warnings[0]["message"]
            .as_str()
            .expect("a message")
            .contains("a/**")
    );
}

// =========================================================
// Dry runs
// =========================================================

#[test]
fn a_dry_run_creates_nothing_and_exits_0() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
    assert!(run.stdout.starts_with("Would create a collection at "));
    assert!(!tree.path(".antiq").exists());
}

#[test]
fn a_dry_run_with_yes_still_creates_nothing() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--dry-run", "--yes"]);
    assert_eq!(run.code(), 0);
    assert!(!tree.path(".antiq").exists());
}

#[test]
fn a_json_dry_run_says_so() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--json", "--dry-run"]);
    assert_eq!(run.code(), 0);
    assert_eq!(run.stderr, "");
    let json = run.json();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["created"], false);
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(
        keys,
        [
            "format", "command", "dry_run", "created", "path", "machine", "patterns", "exclude",
            "pieces", "actions", "warnings"
        ]
    );
}

// =========================================================
// Where init refuses
// =========================================================

fn assert_refused(run: &Run, expected: &str) {
    assert_eq!(run.code(), 1, "stdout: {}", run.stdout);
    assert_eq!(run.stderr, "");
    assert_eq!(run.error_kind(), "other");
    assert_eq!(run.error_message(), expected);
}

#[test]
fn init_in_a_collection_refuses_naming_it() {
    let tree = Tree::new();
    assert_eq!(init_in(&tree, &["--yes"]).code(), 0);
    let before = fs::read_to_string(tree.path(".antiq/config.toml")).expect("a config");
    let run = init_in(&tree, &["--json", "--yes"]);
    assert_refused(
        &run,
        &format!(
            "{} is already a collection",
            canonical(tree.root()).display()
        ),
    );
    assert_eq!(
        fs::read_to_string(tree.path(".antiq/config.toml")).expect("a config"),
        before
    );
}

#[test]
fn init_below_a_collection_refuses_naming_the_root() {
    let mut tree = Tree::new();
    assert_eq!(init_in(&tree, &["--yes"]).code(), 0);
    let below = tree.dir("projects/web");
    let run = Antiq::new()
        .arg("-C")
        .arg(&below)
        .args(["--json", "init", "--yes"])
        .run();
    assert_refused(
        &run,
        &format!(
            "{} lies inside the collection at {}",
            canonical(&below).display(),
            canonical(tree.root()).display()
        ),
    );
    assert!(!below.join(".antiq").exists());
}

#[test]
fn a_dry_run_in_a_collection_refuses_too() {
    let tree = Tree::new();
    assert_eq!(init_in(&tree, &["--yes"]).code(), 0);
    let run = init_in(&tree, &["--json", "--dry-run"]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
}

#[test]
fn a_file_named_antiq_refuses() {
    let mut tree = Tree::new();
    let file = tree.file(".antiq", "mine");
    let run = init_in(&tree, &["--json", "--yes"]);
    assert_refused(
        &run,
        &format!(
            "{} exists and is not a directory",
            canonical(tree.root()).join(".antiq").display()
        ),
    );
    assert_eq!(fs::read_to_string(file).expect("still there"), "mine");
}

#[test]
fn a_folder_that_holds_a_collection_refuses_naming_it() {
    let mut tree = Tree::new();
    tree.dir("plain");
    tree.dir("inner/.antiq");
    let run = init_in(&tree, &["--json", "--yes"]);
    assert_refused(
        &run,
        &format!(
            "these folders would be pieces and hold a collection of their own: {}",
            canonical(tree.root()).join("inner").display()
        ),
    );
    assert!(!tree.path(".antiq").exists());
}

#[test]
fn a_missing_directory_fails() {
    let tree = Tree::new();
    let run = Antiq::new()
        .arg("-C")
        .arg(tree.path("missing"))
        .args(["--json", "init", "--yes"])
        .run();
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
}
