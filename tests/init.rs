// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq init` seen from outside the binary.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use antiq::config::Config;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::tree::{TempCollection, Tree};

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("the path exists")
}

fn config_of(root: &Path) -> Config {
    Config::read(&root.join(".antiq/config.toml")).expect("the config can be read")
}

/// `antiq -C <root> init <args>`.
fn init_at(root: &Path, args: &[&str]) -> Run {
    Antiq::new()
        .arg("-C")
        .arg(root)
        .arg("init")
        .args(args)
        .run()
}

fn init_in(tree: &Tree, args: &[&str]) -> Run {
    init_at(tree.root(), args)
}

/// Text with the collection's path replaced.
fn normalised(text: &str, tree: &Tree) -> String {
    text.replace(
        canonical(tree.root()).to_str().expect("UTF-8 path"),
        "<root>",
    )
}

fn keys(json: &Value) -> Vec<&String> {
    json.as_object().expect("an object").keys().collect()
}

// =========================================================
// Creating
// =========================================================

#[test]
fn init_creates_the_collection_without_asking() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &[]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.stderr, "");
    assert!(tree.path(".antiq/config.toml").is_file());
}

#[test]
fn init_with_json_describes_the_new_collection() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--json"]);
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
    assert_eq!(json["patterns"], json!(["*"]));
    assert!(json.get("actions").is_none(), "{json}");

    let config = config_of(tree.root());
    assert_eq!(json["id"], config.collection.id.to_string());
    assert_eq!(config.collection.patterns, ["*"]);
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
    let run = init_in(&tree, &["--json"]);
    assert_eq!(
        keys(&run.json()),
        [
            "format", "command", "created", "id", "path", "patterns", "due", "warnings"
        ]
    );
}

#[test]
fn init_prints_a_summary_on_the_console() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &[]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
    let id = config_of(tree.root()).collection.id.to_string();
    insta::assert_snapshot!(normalised(&run.stdout, &tree).replace(&id, "<id>"), @r"
    Created the collection at <root>
      id: <id>
      patterns: *
    The next archive would archive 0 folders.
    ");
}

#[test]
fn patterns_repeat() {
    let mut tree = Tree::new();
    tree.dir("a1");
    let run = init_in(&tree, &["--json", "--pattern", "a*", "--pattern", "b*"]);
    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert_eq!(run.json()["patterns"], json!(["a*", "b*"]));
    assert_eq!(config_of(tree.root()).collection.patterns, ["a*", "b*"]);
}

#[test]
fn the_directory_defaults_to_the_current_one() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = Antiq::new()
        .current_dir(tree.root())
        .args(["init", "--json"])
        .run();
    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert!(tree.path(".antiq/config.toml").is_file());
}

#[test]
fn an_invalid_pattern_is_a_usage_error() {
    let tree = Tree::new();
    let run = init_in(&tree, &["--json", "--pattern", "a//b"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert!(
        run.error_message().contains("pattern a//b is invalid"),
        "{}",
        run.error_message()
    );
    assert!(!tree.path(".antiq").exists());
}

#[test]
fn a_folder_with_the_layout_of_archive_sh_stays_a_piece() {
    let mut tree = Tree::new();
    tree.dir("a");
    tree.file("archive/one.tar.xz", "x");
    tree.file("archive/index/one.tar.xz.lst", "x");
    let run = init_in(&tree, &[]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
    assert!(config_of(tree.root()).collection.exclude.is_empty());
}

#[test]
fn a_pattern_with_double_star_prints_a_note() {
    let mut tree = Tree::new();
    tree.dir("a/b");
    let run = init_in(&tree, &["--pattern", "a/**"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stderr
            .starts_with("warning: pattern a/** contains `**`"),
        "{}",
        run.stderr
    );
    assert_eq!(run.stderr.lines().count(), 1);
}

#[test]
fn under_json_the_note_is_in_the_warnings_and_stderr_stays_empty() {
    let mut tree = Tree::new();
    tree.dir("a/b");
    let run = init_in(&tree, &["--json", "--pattern", "a/**"]);
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

#[test]
fn the_written_config_parses_back_to_the_defaults_and_the_given_patterns() {
    let tree = Tree::new();
    assert_eq!(init_in(&tree, &[]).code(), 0);
    let config = config_of(tree.root());
    assert_eq!(config, Config::new(config.collection.id));

    let tree = Tree::new();
    assert_eq!(init_in(&tree, &["--pattern", "a*"]).code(), 0);
    let config = config_of(tree.root());
    let mut expected = Config::new(config.collection.id);
    expected.collection.patterns = vec!["a*".into()];
    assert_eq!(config, expected);
}

#[test]
fn the_written_config_names_every_setting() {
    let tree = Tree::new();
    assert_eq!(init_in(&tree, &[]).code(), 0);
    let text = fs::read_to_string(tree.path(".antiq/config.toml")).expect("the config exists");
    for key in [
        "id",
        "patterns",
        "exclude",
        "ignore",
        "timeless",
        "idle_days",
        "soon_days",
        "url",
        "rsync",
        "rsync_path",
        "fsync",
        "rsync_args",
    ] {
        assert!(
            text.lines().any(|line| {
                line.strip_prefix("# ")
                    .unwrap_or(line)
                    .starts_with(&format!("{key} = "))
            }),
            "{key} is missing in:\n{text}"
        );
    }
    assert!(text.contains("\n# [target]\n"), "{text}");
}

/// A tree with `old` folders last changed 400 days ago and one folder
/// changed just now.
fn tree_with_old_folders(old: usize) -> Tree {
    let mut tree = Tree::new();
    let long_ago = support::tree::days_ago(400);
    for number in 0..old {
        tree.file_with(&format!("old{number}/file"), "x", 0o644, long_ago);
    }
    tree.file("fresh/file", "x");
    tree
}

#[test]
fn init_says_how_many_folders_the_next_archive_would_archive() {
    let tree = tree_with_old_folders(2);
    let run = init_in(&tree, &[]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(
        run.stdout.lines().last(),
        Some("The next archive would archive 2 folders.")
    );
}

#[test]
fn one_folder_is_not_plural() {
    let tree = tree_with_old_folders(1);
    let run = init_in(&tree, &[]);
    assert_eq!(
        run.stdout.lines().last(),
        Some("The next archive would archive 1 folder.")
    );
}

#[test]
fn the_json_has_the_number_of_folders_due() {
    let tree = tree_with_old_folders(3);
    let run = init_in(&tree, &["--json"]);
    assert_eq!(run.json()["due"], 3);
}

#[test]
fn a_dry_run_counts_nothing() {
    let tree = tree_with_old_folders(2);
    let text = init_in(&tree, &["-n"]);
    assert!(!text.stdout.contains("would archive"), "{}", text.stdout);
    assert!(!text.stderr.contains("would archive"), "{}", text.stderr);
    let json = init_in(&tree, &["--json", "-n"]).json();
    assert!(json.get("due").is_none(), "{json}");
}

// =========================================================
// Dry runs
// =========================================================

#[test]
fn a_directory_name_with_a_newline_cannot_forge_a_line_of_the_result() {
    let mut tree = Tree::new();
    let forging = tree.dir("evil\nCreated fake");

    let run = init_at(&forging, &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(
        lines[0].ends_with("evil%0ACreated fake")
            && lines[0].starts_with("Created the collection at "),
        "{lines:?}"
    );
    assert_eq!(lines.len(), 4, "{lines:?}");
}

#[test]
fn a_percent_sign_in_the_directory_name_is_shown_as_percent_25_in_the_result() {
    let mut tree = Tree::new();
    let folder = tree.dir("100% done");

    let run = init_at(&folder, &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let first = run.stdout.lines().next().expect("a first line");
    assert!(first.ends_with("/100%25 done"), "{first}");
}

#[test]
fn a_percent_sign_in_the_directory_name_is_shown_as_percent_25_in_a_dry_run() {
    let mut tree = Tree::new();
    let folder = tree.dir("100% done");

    let run = init_at(&folder, &["-n"]);

    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert!(run.stderr.contains("/100%25 done"), "{:?}", run.stderr);
}

#[test]
fn the_json_path_of_a_directory_with_a_percent_sign_stays_as_it_is() {
    let mut tree = Tree::new();
    let folder = tree.dir("100% done");

    let run = init_at(&folder, &["--json"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let path = run.json()["path"].as_str().expect("a path").to_string();
    assert!(path.ends_with("/100% done"), "{path}");
}

#[test]
fn a_directory_name_with_a_newline_cannot_forge_a_line_of_a_dry_run() {
    let mut tree = Tree::new();
    let forging = tree.dir("evil\nWould create fake");

    let run = init_at(&forging, &["-n"]);

    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert_eq!(run.stderr.lines().count(), 1, "{:?}", run.stderr);
    assert!(
        run.stderr.ends_with("evil%0AWould create fake\n"),
        "{:?}",
        run.stderr
    );
}

#[test]
fn an_escape_character_in_a_pattern_stays_out_of_the_config_a_dry_run_prints() {
    let mut tree = Tree::new();
    tree.dir("a");

    let run = init_in(&tree, &["-n", "--pattern", "a\u{1b}*"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(!run.stdout.contains('\u{1b}'), "{:?}", run.stdout);
}

#[test]
fn a_dry_run_prints_the_config_it_would_write() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["-n", "--pattern", "a*"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(!tree.path(".antiq").exists());

    // The run mints the ID only when it is real, so the dump has a comment
    // where the ID goes.
    assert!(
        run.stdout.contains("\n# id is minted by the real run\n"),
        "{}",
        run.stdout
    );
    let with_id = run.stdout.replace(
        "# id is minted by the real run",
        "id = \"01ARZ3NDEKTSV4RRFFQ69G5FAV\"",
    );
    let config = Config::parse(&with_id, None).expect("stdout is the config");
    assert_eq!(config.collection.patterns, ["a*"]);
    assert_eq!(
        normalised(&run.stderr, &tree),
        "Would create a collection at <root>\n"
    );
}

#[test]
fn a_json_dry_run_carries_the_config() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = init_in(&tree, &["--json", "-n"]);
    assert_eq!(run.code(), 0);
    assert_eq!(run.stderr, "");
    let json = run.json();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["created"], false);
    assert_eq!(
        keys(&json),
        [
            "format", "command", "dry_run", "created", "path", "patterns", "config", "actions",
            "warnings"
        ]
    );
    assert_eq!(json["actions"][0]["kind"], "create_collection");
    let config = Config::parse(json["config"].as_str().expect("the config text"), None)
        .expect("the text is a config");
    assert_eq!(config.collection.patterns, ["*"]);
    assert!(!tree.path(".antiq").exists());
}

// =========================================================
// Where init refuses
// =========================================================

/// Every file and directory below `root` without following links: its kind
/// and, for a file, its content.
fn contents(root: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(base: &Path, directory: &Path, seen: &mut BTreeMap<PathBuf, String>) {
        for item in fs::read_dir(directory).expect("lists") {
            let path = item.expect("an item").path();
            let kind = fs::symlink_metadata(&path).expect("stats").file_type();
            let shown = if kind.is_dir() {
                "directory".to_string()
            } else if kind.is_symlink() {
                format!("link to {}", fs::read_link(&path).expect("reads").display())
            } else {
                fs::read_to_string(&path).expect("reads")
            };
            seen.insert(path.strip_prefix(base).expect("below").into(), shown);
            if kind.is_dir() {
                walk(base, &path, seen);
            }
        }
    }
    let mut seen = BTreeMap::new();
    walk(root, root, &mut seen);
    seen
}

/// `init` in `directory` refuses with `expected`, as text and as JSON, for
/// a real run and for a dry run, and leaves everything below `watched`
/// as it was.
fn assert_refused_everywhere(directory: &Path, watched: &Path, expected: &str) {
    let before = contents(watched);
    for flags in [&[][..], &["-n"]] {
        let text = init_at(directory, flags);
        assert_eq!(text.code(), 1, "{flags:?}, stderr: {}", text.stderr);
        assert_eq!(text.stdout, "", "{flags:?}");
        assert!(text.stderr.contains(expected), "{flags:?}: {}", text.stderr);

        let mut json_flags = vec!["--json"];
        json_flags.extend_from_slice(flags);
        let json = init_at(directory, &json_flags);
        assert_eq!(json.code(), 1, "{json_flags:?}, stdout: {}", json.stdout);
        assert_eq!(json.stderr, "", "{json_flags:?}");
        assert_eq!(json.error_kind(), "other", "{json_flags:?}");
        assert_eq!(json.error_message(), expected, "{json_flags:?}");

        assert_eq!(contents(watched), before, "{flags:?} changed the tree");
    }
}

#[test]
fn init_in_a_collection_refuses_naming_it() {
    let collection = TempCollection::minimal();
    assert_refused_everywhere(
        collection.root(),
        collection.root(),
        &format!(
            "{} is already a collection",
            canonical(collection.root()).display()
        ),
    );
}

#[test]
fn init_below_a_collection_refuses_naming_the_root() {
    let mut collection = TempCollection::minimal();
    let below = collection.tree().dir("projects/web");
    assert_refused_everywhere(
        &below,
        collection.root(),
        &format!(
            "{} lies inside the collection at {}",
            canonical(&below).display(),
            canonical(collection.root()).display()
        ),
    );
}

#[test]
fn a_file_named_antiq_refuses() {
    let mut tree = Tree::new();
    tree.file(".antiq", "mine");
    assert_refused_everywhere(
        tree.root(),
        tree.root(),
        &format!(
            "{} exists and is not a directory",
            canonical(tree.root()).join(".antiq").display()
        ),
    );
}

#[test]
fn a_symlink_named_antiq_refuses_whatever_it_points_at() {
    for target_is_directory in [true, false] {
        let mut tree = Tree::new();
        let target = if target_is_directory {
            tree.dir("elsewhere")
        } else {
            tree.path("missing")
        };
        tree.symlink(".antiq", target);
        assert_refused_everywhere(
            tree.root(),
            tree.root(),
            &format!(
                "{} exists and is not a directory",
                canonical(tree.root()).join(".antiq").display()
            ),
        );
    }
}

#[test]
fn folders_that_hold_a_collection_refuse_naming_each() {
    let mut tree = Tree::new();
    tree.dir("plain");
    tree.dir("first/.antiq");
    tree.dir("second/.antiq");
    let root = canonical(tree.root());
    assert_refused_everywhere(
        tree.root(),
        tree.root(),
        &format!(
            "these folders would be pieces and hold a collection of their own: {}, {}",
            root.join("first").display(),
            root.join("second").display()
        ),
    );
}

#[test]
fn a_folder_holding_antiq_as_a_file_is_a_piece_like_any_other() {
    let mut tree = Tree::new();
    tree.file("plain/.antiq", "not a collection");
    let run = init_in(&tree, &["--json"]);
    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert!(tree.path(".antiq/config.toml").is_file());
}

#[test]
fn a_missing_directory_fails() {
    let tree = Tree::new();
    let run = init_at(&tree.path("missing"), &["--json"]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
}
