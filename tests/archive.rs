// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq archive` seen from outside the binary, on collections built with
//! the scenario fixture.

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection};

fn archive(collection: &Collection, global: &[&str], args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .args(global)
        .arg("archive")
        .args(args)
        .run()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(read) => read
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Two levels of folders are pieces. `old/site` is due, `new/work` is not.
fn world() -> Collection {
    Collection::build(&Clock::now())
        .patterns(&["*/*"])
        .idle_days(30)
        .ignore(&["node_modules/"])
        .piece("old/site", |piece| {
            piece
                .age_days(40)
                .file("index.html", "<html>")
                .file("node_modules/dep/index.js", "ignored");
        })
        .piece("new/work", |piece| {
            piece.age_days(1).file("notes.md", "fresh");
        })
        .finish()
}

fn assert_no_leftovers(collection: &Collection) {
    for below in ["trash", "doomed", "tmp"] {
        let dir = collection.path(&format!(".antiq/{below}"));
        let left = if below == "tmp" {
            names(&dir.join("pack"))
        } else {
            names(&dir)
        };
        assert!(left.is_empty(), "{below}: {left:?}");
    }
}

fn assert_stored(collection: &Collection, piece: &str) {
    let files = names(&collection.path(&format!(".antiq/pieces/{piece}")));
    assert_eq!(files.len(), 3, "{files:?}");
}

#[test]
fn archives_the_due_piece_and_prints_json() {
    let collection = world();

    let run = archive(&collection, &["--json"], &[]);

    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["command"], "archive");
    let archived = &json["archived"];
    assert_eq!(archived.as_array().expect("a list").len(), 1);
    assert_eq!(archived[0]["piece"], "old/site");
    for key in ["stamp", "version", "archive_bytes", "files", "bytes"] {
        assert!(!archived[0][key].is_null(), "{key}");
    }
    assert_eq!(json["skipped"], serde_json::json!([]));
    assert_eq!(json["refused"], serde_json::json!([]));
    assert!(!collection.path("old/site").exists());
    assert!(collection.path("new/work/notes.md").exists());
    assert_stored(&collection, "old/site");
    assert_no_leftovers(&collection);
}

/// Pieces that hold names of one file, as after `cp -al`, are all archived:
/// the run keeps them on one worker, so the removal of one does not make
/// another differ. The timing that goes wrong without it is forced by a unit
/// test; here the pieces only have to come out right.
#[test]
fn pieces_that_share_a_hard_linked_file_are_all_archived() {
    let names = ["old/a", "old/b", "old/c", "old/d", "old/e", "old/f"];
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["*/*"]).idle_days(30);
    for name in names {
        builder.piece(name, |piece| {
            piece
                .age_days(40)
                .file("own.txt", name)
                .file("shared.txt", "one inode");
        });
    }
    let collection = builder.finish();
    for name in &names[1..] {
        let shared = collection.path(&format!("{name}/shared.txt"));
        fs::remove_file(&shared).expect("the copy goes");
        fs::hard_link(collection.path("old/a/shared.txt"), &shared).expect("the link is made");
    }

    let run = archive(&collection, &["--json"], &[]);

    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(
        json["archived"].as_array().expect("a list").len(),
        names.len()
    );
    assert_eq!(json["skipped"], serde_json::json!([]));
    for name in names {
        assert!(!collection.path(name).exists(), "{name}");
    }
    assert_no_leftovers(&collection);
}

#[test]
fn prints_a_line_per_folder_and_a_summary() {
    let collection = world();

    let run = archive(&collection, &[], &[]);

    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(
        run.stdout.lines().collect::<Vec<_>>(),
        ["archived old/site", "Archived 1, skipped 0, refused 0."]
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn the_ignored_subtree_goes_with_the_folder_and_is_not_in_the_listing() {
    let collection = world();

    let run = archive(&collection, &["--json"], &[]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    assert!(!collection.path("old/site/node_modules").exists());
    let files = names(&collection.path(".antiq/pieces/old/site"));
    let listing = files
        .iter()
        .find(|name| name.ends_with(".sha256.zst"))
        .expect("a listing file");
    let file = fs::File::open(collection.path(&format!(".antiq/pieces/old/site/{listing}")))
        .expect("the listing opens");
    let text = antiq::listing::decode_all(file).expect("the listing decodes");
    let text = String::from_utf8_lossy(&text);
    assert!(text.contains("index.html"), "{text}");
    assert!(!text.contains("node_modules"), "{text}");
}

#[test]
fn a_held_lock_fails_with_the_holder_in_json_and_in_text() {
    let collection = world();
    let _held = hold_lock(&collection.root());

    let json = archive(&collection, &["--json"], &[]);

    assert_eq!(json.code(), 1, "{}", json.stdout);
    assert_eq!(json.error_kind(), "lock_held");
    assert_eq!(json.json()["command"], "archive");
    let error = &json.json()["error"];
    assert_eq!(error["pid"], std::process::id());
    assert!(error["started"].is_string(), "{error}");
    let text = archive(&collection, &[], &[]);
    assert_eq!(text.code(), 1);
    assert!(
        text.stderr.contains(&std::process::id().to_string()),
        "{}",
        text.stderr
    );
    assert!(collection.path("old/site/index.html").exists());
}

#[test]
fn a_dry_run_with_a_held_lock_names_the_holder_and_lists_nothing() {
    let collection = world();
    fs::create_dir_all(collection.path(".antiq/doomed/2026-11-03T000000/old")).expect("leftover");
    fs::write(
        collection.path(".antiq/doomed/2026-11-03T000000/old/f"),
        "x",
    )
    .expect("a file");
    let _held = hold_lock(&collection.root());

    let json = archive(&collection, &["--json"], &["-n"]);

    assert_eq!(json.code(), 0, "{}", json.stderr);
    let value = json.json();
    assert_eq!(value["dry_run"], true);
    assert_eq!(value["actions"], serde_json::json!([]));
    assert_eq!(value["lock_held"]["pid"], std::process::id());
    let text = archive(&collection, &[], &["-n"]);
    assert_eq!(text.code(), 0);
    assert!(
        text.stdout
            .starts_with(&format!("The lock is held by pid {}", std::process::id())),
        "{}",
        text.stdout
    );
}

/// A leftover in `doomed/` that makes a repair the first line of the run.
fn plant_doomed_leftover(collection: &Collection) {
    fs::create_dir_all(collection.path(".antiq/doomed/2026-11-03T000000/old")).expect("leftover");
    fs::write(
        collection.path(".antiq/doomed/2026-11-03T000000/old/f"),
        "x",
    )
    .expect("a file");
}

#[test]
fn a_dry_run_lists_the_repairs_then_the_archives_with_what_they_leave_out_and_changes_nothing() {
    let collection = world();
    plant_doomed_leftover(&collection);
    let before = snapshot(&collection.root());

    let text = archive(&collection, &[], &["-n"]);

    assert_eq!(text.code(), 0, "{}", text.stderr);
    let lines: Vec<&str> = text.stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(
        lines[0].starts_with("Would delete .antiq/doomed/2026-11-03T000000/old"),
        "{lines:?}"
    );
    assert!(lines[1].starts_with("Would remove "), "{lines:?}");
    assert_eq!(lines[2], "Would archive old/site");
    assert_eq!(lines[3], "Would leave out node_modules/");
    assert_eq!(snapshot(&collection.root()), before);
}

#[test]
fn a_dry_run_in_json_carries_what_each_archive_leaves_out_and_changes_nothing() {
    let collection = world();
    plant_doomed_leftover(&collection);
    let before = snapshot(&collection.root());

    let json = archive(&collection, &["--json"], &["--dry-run"]);

    let value = json.json();
    assert_eq!(value["dry_run"], true);
    let actions = value["actions"].as_array().expect("actions");
    assert_eq!(actions.len(), 3);
    assert_eq!(actions[0]["kind"], "repair");
    assert_eq!(
        actions[2],
        serde_json::json!({
            "kind": "archive",
            "piece": "old/site",
            "left_out": ["node_modules/"],
        })
    );
    assert_eq!(snapshot(&collection.root()), before);
}

#[test]
fn a_dry_run_of_a_named_piece_lists_its_restored_marker_as_left_out() {
    let collection = world();
    fs::write(
        collection.path("new/work/.antiq-restored"),
        "restored 2020-01-01T00:00:00Z\n",
    )
    .expect("the marker is written");

    let run = archive(&collection, &[], &["-n", "new/work"]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    assert_eq!(
        run.stdout.lines().collect::<Vec<_>>(),
        ["Would archive new/work", "Would leave out .antiq-restored"]
    );
}

#[test]
fn a_dry_run_of_a_piece_without_ignored_entries_has_an_empty_left_out() {
    let collection = world();

    let json = archive(&collection, &["--json"], &["-n", "new/work"]);

    assert_eq!(
        json.json()["actions"],
        serde_json::json!([{"kind": "archive", "piece": "new/work", "left_out": []}])
    );
}

#[test]
fn a_named_piece_is_archived_however_recently_it_changed() {
    let collection = world();

    let run = archive(&collection, &["--json"], &["new/work"]);

    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.json()["archived"][0]["piece"], "new/work");
    assert!(collection.path("old/site/index.html").exists());
    assert_stored(&collection, "new/work");
}

fn refusing_world() -> Collection {
    Collection::build(&Clock::now())
        .patterns(&["*/*"])
        .piece("a/pinned", |piece| {
            piece.age_days(40).file("f", "x").pinned();
        })
        .piece("a/nested", |piece| {
            piece.age_days(40).file("f", "x").dir("inner/.antiq");
        })
        .piece("a/ok", |piece| {
            piece.age_days(40).file("f", "x");
        })
        .finish()
}

#[test]
fn a_named_pinned_piece_exits_1_and_names_the_reason() {
    let collection = refusing_world();

    let json = archive(&collection, &["--json"], &["a/pinned"]);
    let text = archive(&collection, &[], &["a/pinned"]);

    assert_eq!(json.code(), 1, "{}", json.stderr);
    let reason = &json.json()["refused"][0]["reasons"][0];
    assert_eq!(reason["kind"], "pinned");
    assert_eq!(reason["path"], "a/pinned/.antiq-pin");
    assert_eq!(text.code(), 1);
    assert_eq!(
        text.stdout.lines().collect::<Vec<_>>(),
        [
            "refused a/pinned: pinned: a/pinned/.antiq-pin",
            "Archived 0, skipped 0, refused 1."
        ]
    );
    assert!(collection.path("a/pinned/f").exists());
}

#[test]
fn a_named_piece_the_walk_refuses_exits_1_and_names_the_reason() {
    let collection = refusing_world();

    let json = archive(&collection, &["--json"], &["a/nested"]);

    assert_eq!(json.code(), 1, "{}", json.stderr);
    let reason = &json.json()["refused"][0]["reasons"][0];
    assert_eq!(reason["kind"], "nested_collection");
    assert_eq!(reason["path"], "a/nested/inner");
    assert!(collection.path("a/nested/f").exists());
}

#[test]
fn without_a_named_piece_refusals_leave_the_exit_code_at_0() {
    let collection = refusing_world();

    let json = archive(&collection, &["--json"], &[]);
    let value = json.json();

    assert_eq!(json.code(), 0, "{}", json.stderr);
    assert_eq!(value["archived"][0]["piece"], "a/ok");
    let refused = value["refused"].as_array().expect("refused");
    assert_eq!(
        refused.len(),
        1,
        "a pinned piece is not listed: {refused:?}"
    );
    assert_eq!(refused[0]["piece"], "a/nested");
    let text = archive(&collection, &[], &[]);
    assert_eq!(text.code(), 0);
    assert!(
        text.stdout
            .contains("refused a/nested: nested collection: a/nested/inner"),
        "{}",
        text.stdout
    );
}

#[test]
fn an_empty_piece_argument_is_a_usage_error() {
    let collection = world();

    let run = archive(&collection, &["--json"], &[""]);

    assert_eq!(run.code(), 2, "{}", run.stdout);
    assert_eq!(run.error_kind(), "usage");
}

#[test]
fn an_unknown_piece_and_an_ambiguous_one_are_errors_with_their_kind() {
    let collection = Collection::build(&Clock::now())
        .patterns(&["*/*"])
        .piece("p/scanner", |piece| {
            piece.age_days(40).file("f", "x");
        })
        .piece("p/scan-old", |piece| {
            piece.age_days(40).file("f", "x");
        })
        .finish();

    let unknown = archive(&collection, &["--json"], &["nothing-like-it"]);
    let ambiguous = archive(&collection, &["--json"], &["scan"]);

    assert_eq!(
        (unknown.code(), unknown.error_kind().as_str()),
        (1, "unknown_reference")
    );
    assert_eq!(
        (ambiguous.code(), ambiguous.error_kind().as_str()),
        (1, "ambiguous_reference")
    );
    let candidates = ambiguous.json()["error"]["candidates"].clone();
    assert_eq!(candidates.as_array().expect("candidates").len(), 2);
}

/// Restores the mode of a directory when dropped, so a failing test leaves
/// nothing the temp dir cannot remove.
struct Mode(PathBuf, u32);

impl Drop for Mode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
    }
}

#[test]
fn a_piece_that_cannot_enter_trash_is_skipped_with_exit_3_and_the_next_run_archives_it() {
    let collection = world();
    let trash = collection.path(".antiq/trash");
    let _restore = Mode(trash.clone(), 0o755);
    fs::set_permissions(&trash, fs::Permissions::from_mode(0o555)).expect("trash is locked");
    assert!(
        fs::create_dir(trash.join("probe")).is_err(),
        "the test needs a user that mode 555 locks out"
    );

    let json = archive(&collection, &["--json"], &[]);
    let text = archive(&collection, &[], &[]);

    assert_eq!(
        json.code(),
        3,
        "stdout: {}, stderr: {}",
        json.stdout,
        json.stderr
    );
    let value: Value = json.json();
    assert_eq!(value["archived"], serde_json::json!([]));
    let skipped = &value["skipped"][0];
    assert_eq!(skipped["piece"], "old/site");
    assert_eq!(skipped["reason"], "error");
    assert!(
        skipped["message"]
            .as_str()
            .expect("a message")
            .starts_with("cannot move old/site into trash/"),
        "{skipped}"
    );
    assert_eq!(text.code(), 3);
    assert!(
        text.stdout
            .contains("skipped old/site: cannot move old/site into trash/"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("Archived 0, skipped 1, refused 0."),
        "{}",
        text.stdout
    );
    assert!(collection.path("old/site/index.html").exists());
    assert!(
        names(&collection.path(".antiq/pieces/old/site")).is_empty(),
        "the version is taken back"
    );

    fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).expect("trash is open again");
    let again = archive(&collection, &["--json"], &[]);
    assert_eq!(again.code(), 0, "{}", again.stderr);
    assert_stored(&collection, "old/site");
    assert_no_leftovers(&collection);
}

#[test]
fn notes_on_the_settings_go_to_stderr_and_into_the_warnings() {
    let collection = world();
    let config = collection.path(".antiq/config.toml");
    let mut text = fs::read_to_string(&config).expect("the config exists");
    text.push_str("\nmystery = 1\n");
    fs::write(&config, text).expect("the config is written");

    let plain = archive(&collection, &[], &["-n"]);
    let json = archive(&collection, &["--json"], &["-n"]);

    let note = "warning: unknown setting `mystery` under [collection], which antiq ignores";
    assert_eq!(plain.code(), 0, "{}", plain.stderr);
    assert_eq!(plain.stderr.lines().collect::<Vec<_>>(), [note]);
    assert_eq!(
        json.json()["warnings"][0]["message"],
        "unknown setting `mystery` under [collection], which antiq ignores"
    );
}
