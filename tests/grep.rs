// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq grep` seen from outside the binary. The binary runs with its
//! stdout piped, so the output here is the pipe form and the JSON.

mod support;

use antiq::versions::names::VersionFile;
use serde_json::json;
use std::fs;
use std::path::Path;
use support::cmd::{Antiq, Run, hold_lock};
use support::scenario::collection::{Clock, Collection};
use support::tree::Tree;
use support::versions::{Store, StoredVersion};

const NEW: &str = "2026-11-04T100211";
const OLD: &str = "2026-09-03T121500";
const ANCIENT: &str = "2025-02-11T194007";

// =========================================================
// Fixtures
// =========================================================

fn source(files: &[&str]) -> Tree {
    let mut tree = Tree::new();
    for file in files {
        tree.file(file, "x");
    }
    tree
}

/// A store with `projects/scanner` in two versions and `tools/scan-old` in
/// one.
fn two_pieces() -> Store {
    let mut store = Store::new();
    let old = source(&[
        "projects/scanner/src/main.rs",
        "projects/scanner/tools/bench/main.rs",
    ]);
    store.add_version(&old, "projects/scanner", OLD);
    let new = source(&["projects/scanner/src/main.rs", "projects/scanner/README"]);
    store.add_version(&new, "projects/scanner", NEW);
    let other = source(&["tools/scan-old/cli/src/main.rs"]);
    store.add_version(&other, "tools/scan-old", ANCIENT);
    store
}

/// Two versions of `notes`, both holding `needle`; the older one comes back
/// for the test to spoil.
fn spoilable() -> (Store, StoredVersion) {
    let mut store = Store::new();
    let tree = source(&["notes/needle"]);
    let spoiled = store.add_version(&tree, "notes", OLD);
    store.add_version(&tree, "notes", NEW);
    (store, spoiled)
}

/// A collection whose store holds what `store` holds.
fn collection_of(store: &Store) -> Collection {
    let collection = Collection::build(&Clock::now()).finish();
    copy_into(&store.pieces_dir(), &collection.path(".antiq/pieces"));
    collection
}

fn copy_into(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("the directory lists") {
        let entry = entry.expect("an entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a type").is_dir() {
            fs::create_dir_all(&target).expect("the directory is made");
            copy_into(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("the file is copied");
        }
    }
}

fn grep(collection: &Collection, args: &[&str]) -> Run {
    Antiq::new()
        .arg("-C")
        .arg(collection.root())
        .arg("grep")
        .args(args)
        .run()
}

fn grep_json(collection: &Collection, pattern: &str) -> Run {
    Antiq::new()
        .arg("-C")
        .arg(collection.root())
        .arg("--json")
        .arg("grep")
        .arg(pattern)
        .run()
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

// =========================================================
// Pipe lines
// =========================================================

#[test]
fn a_pipe_gets_one_line_per_path_and_version() {
    let collection = collection_of(&two_pieces());

    let run = grep(&collection, &[r"main\.rs$"]);

    assert_eq!(run.code(), 0);
    assert_eq!(
        lines(&run.stdout),
        [
            "projects/scanner\t2026-11-04T100211\tsrc/main.rs",
            "projects/scanner\t2026-09-03T121500\tsrc/main.rs",
            "projects/scanner\t2026-09-03T121500\ttools/bench/main.rs",
            "tools/scan-old\t2025-02-11T194007\tcli/src/main.rs",
        ]
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn a_piece_name_match_is_a_line_per_version_with_a_dash() {
    let collection = collection_of(&two_pieces());

    let run = grep(&collection, &["scanner"]);

    assert_eq!(run.code(), 0);
    assert_eq!(
        lines(&run.stdout),
        [
            "projects/scanner\t2026-11-04T100211\t-",
            "projects/scanner\t2026-09-03T121500\t-",
        ]
    );
}

#[test]
fn a_name_with_a_tab_and_a_backslash_is_matched_unescaped_and_printed_raw() {
    let mut store = Store::new();
    store.add_version(&source(&["notes/a\tb\\c", "notes/plain"]), "notes", NEW);
    let collection = collection_of(&store);

    let run = grep(&collection, &[r"a\tb\\c"]);

    assert_eq!(run.code(), 0);
    assert_eq!(run.stdout, "notes\t2026-11-04T100211\ta\tb\\c\n");
}

// =========================================================
// JSON
// =========================================================

#[test]
fn json_lists_pieces_versions_and_files() {
    let collection = collection_of(&two_pieces());

    let run = grep_json(&collection, r"bench/main\.rs$");

    assert_eq!(run.code(), 0);
    assert_eq!(
        run.json(),
        json!({
            "format": 1,
            "command": "grep",
            "pattern": r"bench/main\.rs$",
            "pieces": [{
                "piece": "projects/scanner",
                "name_match": false,
                "versions": [
                    {"stamp": NEW, "version": "2026-11-04T10:02:11Z"},
                    {"stamp": OLD, "version": "2026-09-03T12:15:00Z"},
                ],
                "files": [{"path": "tools/bench/main.rs", "versions": [OLD]}],
            }],
            "skipped": [],
            "warnings": [],
        })
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn json_gives_a_name_match_the_file_count_of_each_version() {
    let collection = collection_of(&two_pieces());

    let run = grep_json(&collection, "scan-old");

    assert_eq!(
        run.json()["pieces"],
        json!([{
            "piece": "tools/scan-old",
            "name_match": true,
            "versions": [{"stamp": ANCIENT, "version": "2025-02-11T19:40:07Z", "files": 1}],
            "files": [],
        }])
    );
}

#[test]
fn json_carries_paths_in_the_escaped_form() {
    let mut store = Store::new();
    store.add_version(&source(&["notes/100%", "notes/a\tb"]), "notes", NEW);
    let collection = collection_of(&store);

    let run = grep_json(&collection, r"100%|a\tb");

    let files = &run.json()["pieces"][0]["files"];
    assert_eq!(files[0]["path"], "100%25");
    assert_eq!(files[1]["path"], "a\tb");
}

// =========================================================
// Nothing found, and errors
// =========================================================

#[test]
fn no_hit_exits_0_and_says_so_on_stderr() {
    let collection = collection_of(&two_pieces());

    let text = grep(&collection, &["qxzv"]);
    assert_eq!(text.code(), 0);
    assert_eq!(text.stdout, "");
    assert_eq!(text.stderr, "no archived path matches qxzv\n");

    let json = grep_json(&collection, "qxzv");
    assert_eq!(json.code(), 0);
    assert_eq!(json.json()["pieces"], json!([]));
    assert_eq!(json.stderr, "no archived path matches qxzv\n");
}

#[test]
fn an_invalid_pattern_is_a_usage_error() {
    let collection = collection_of(&two_pieces());

    let text = grep(&collection, &["(unclosed"]);
    assert_eq!(text.code(), 2);
    assert_eq!(text.stdout, "");
    assert!(text.stderr.starts_with("error: "), "{}", text.stderr);

    let json = grep_json(&collection, "(unclosed");
    assert_eq!(json.code(), 2);
    assert_eq!(json.error_kind(), "usage");
}

#[test]
fn an_invalid_pattern_outside_a_collection_is_still_a_usage_error() {
    let outside = Tree::new();

    let run = Antiq::new()
        .arg("-C")
        .arg(outside.root())
        .args(["--json", "grep", "(unclosed"])
        .run();

    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
}

#[test]
fn outside_a_collection_is_not_a_collection() {
    let outside = Tree::new();

    let run = Antiq::new()
        .arg("-C")
        .arg(outside.root())
        .args(["--json", "grep", "x"])
        .run();

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "not_a_collection");
    assert_eq!(run.json()["command"], "grep");
}

#[test]
fn a_flag_grep_does_not_take_is_a_usage_error() {
    let collection = collection_of(&two_pieces());
    let run = grep(&collection, &["x", "--dry-run"]);
    assert_eq!(run.code(), 2);
}

// =========================================================
// Versions that are not searched
// =========================================================

#[test]
fn an_undecodable_listing_is_a_warning_and_the_rest_is_printed() {
    let (store, spoiled) = spoilable();
    spoiled.cut_listing();
    let collection = collection_of(&store);

    let text = grep(&collection, &["needle"]);
    assert_eq!(text.code(), 0);
    assert_eq!(text.stdout, "notes\t2026-11-04T100211\tneedle\n");
    assert_eq!(
        text.stderr,
        "warning: cannot search version 2026-09-03T121500 of notes: \
         cannot write or read the listing: incomplete frame\n"
    );

    let json = grep_json(&collection, "needle");
    assert_eq!(json.code(), 0);
    assert_eq!(
        json.json()["skipped"],
        json!([{
            "piece": "notes",
            "stamp": OLD,
            "reason": "cannot write or read the listing: incomplete frame",
        }])
    );
    assert_eq!(json.stderr, "");
}

#[test]
fn an_incomplete_version_is_left_out_without_a_word() {
    let (store, spoiled) = spoilable();
    spoiled.remove(VersionFile::Archive);
    let collection = collection_of(&store);

    let run = grep(&collection, &["needle"]);

    assert_eq!(run.stdout, "notes\t2026-11-04T100211\tneedle\n");
    assert_eq!(run.stderr, "");
}

// =========================================================
// A held lock
// =========================================================

/// The text of a lock note with what differs from run to run replaced.
fn normalised_note(text: &str) -> String {
    let pid = std::process::id().to_string();
    let text = text.replace(&pid, "<pid>");
    let (before, rest) = text.split_once("started ").expect("the note names a start");
    let after = rest.find('\n').map_or("", |end| &rest[end..]);
    format!("{before}started <time>{after}")
}

#[test]
fn a_held_lock_is_a_note_and_the_search_goes_on() {
    let collection = collection_of(&two_pieces());
    let _lock = hold_lock(&collection.root());

    let run = grep(&collection, &["README"]);

    assert_eq!(run.code(), 0);
    assert_eq!(run.stdout, "projects/scanner\t2026-11-04T100211\tREADME\n");
    insta::assert_snapshot!(normalised_note(&run.stderr), @"note: another antiq run holds the lock: its pid is <pid> on machine imperius, started <time>
");
}

#[test]
fn a_held_lock_is_the_note_field_under_json() {
    let collection = collection_of(&two_pieces());
    let _lock = hold_lock(&collection.root());

    let run = grep_json(&collection, "README");

    assert_eq!(run.code(), 0);
    let json = run.json();
    let note = json["note"].as_str().expect("a note");
    assert!(
        normalised_note(note)
            .starts_with("another antiq run holds the lock: its pid is <pid> on machine imperius"),
        "{note}"
    );
    assert_eq!(json["pieces"][0]["files"][0]["path"], "README");
    assert_eq!(run.stderr, "");
}

#[test]
fn a_lock_file_that_cannot_be_read_is_a_warning() {
    let collection = collection_of(&two_pieces());
    fs::create_dir(collection.path(".antiq/lock")).expect("the lock path is a directory");

    let text = grep(&collection, &["README"]);
    assert_eq!(text.code(), 0);
    assert!(
        text.stderr
            .starts_with("warning: cannot read the lock file: "),
        "{}",
        text.stderr
    );

    let json = grep_json(&collection, "README");
    assert_eq!(json.code(), 0);
    let warning = json.json()["warnings"][0]["message"]
        .as_str()
        .expect("a warning")
        .to_string();
    assert!(
        warning.starts_with("cannot read the lock file: "),
        "{warning}"
    );
    assert!(json.json().get("note").is_none());
}

// =========================================================
// Notes on the settings
// =========================================================

/// Puts `line` into the `[collection]` table of the collection's config.
fn with_config_line(collection: &Collection, line: &str) {
    let path = collection.path(".antiq/config.toml");
    let config = fs::read_to_string(&path).expect("the config exists");
    let config = config.replacen("[collection]\n", &format!("[collection]\n{line}\n"), 1);
    fs::write(&path, config).expect("the config is written");
}

#[test]
fn an_unknown_config_key_is_a_note_and_a_warning() {
    let collection = collection_of(&two_pieces());
    with_config_line(&collection, "bogus = 1");

    let text = grep(&collection, &["qxzv"]);
    assert_eq!(text.code(), 0);
    assert_eq!(
        text.stderr,
        "warning: unknown setting `bogus` under [collection], which antiq ignores\n\
         no archived path matches qxzv\n"
    );

    let json = grep_json(&collection, "README");
    assert_eq!(
        json.json()["warnings"],
        json!([{
            "code": "antiq::config::unknown_key",
            "message": "unknown setting `bogus` under [collection], which antiq ignores",
            "fix": "Correct the name or remove the line in `.antiq/config.toml`."
        }])
    );
    assert_eq!(json.stderr, "");
}

#[test]
fn the_search_leaves_the_store_as_it_was() {
    let collection = collection_of(&two_pieces());
    let before = support::cmd::snapshot(&collection.root());

    grep(&collection, &["main"]);

    assert_eq!(support::cmd::snapshot(&collection.root()), before);
}
