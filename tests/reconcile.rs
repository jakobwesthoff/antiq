// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq reconcile` seen from outside the binary: the local part, which
//! rebuilds the index.

mod support;

use antiq::meta::Lifecycle;
use antiq::tracking::EntryState;
use serde_json::json;
use std::fs;
use support::cmd::{Antiq, Run};
use support::objects::{
    archive_with_members, entry, hold_lock, id, metadata, put_entry, put_meta, snapshot,
};
use support::tree::{COLLECTION_ID, TempCollection};

/// `antiq -C <root> <args>`.
fn antiq_in(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

fn succeeded(run: &Run) {
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
}

/// Two archived objects whose listings are stored here.
fn world() -> TempCollection {
    let temp = TempCollection::minimal();
    for (number, at) in [(1, "alpha"), (2, "beta")] {
        let archive = archive_with_members(temp.root(), number, 1, 10, &["src/main.rs"]);
        put_meta(
            temp.root(),
            &metadata(
                number,
                at,
                Lifecycle::Archived,
                COLLECTION_ID,
                vec![archive],
            ),
        );
        put_entry(temp.root(), &entry(number, at, EntryState::Gone));
    }
    temp
}

fn cache_file(temp: &TempCollection, number: u8) -> std::path::PathBuf {
    temp.root()
        .join(format!(".antiq/index/{}.json", id(number)))
}

// =========================================================
// Rebuilding the index
// =========================================================

#[test]
fn a_deleted_index_is_rebuilt() {
    let temp = world();
    fs::remove_dir_all(temp.root().join(".antiq/index")).expect("the index is deleted");
    let run = antiq_in(&temp, &["--json", "reconcile"]);
    succeeded(&run);
    assert!(cache_file(&temp, 1).exists());
    assert!(cache_file(&temp, 2).exists());
    let json = run.json();
    assert_eq!(json["command"], "reconcile");
    assert_eq!(
        json["index"],
        json!({"written": 2, "removed": 0, "skipped": []})
    );
    assert_eq!(json["warnings"], json!([]));
    assert_eq!(run.stderr, "");
}

#[test]
fn what_does_not_belong_into_the_index_is_removed() {
    let temp = world();
    fs::write(temp.root().join(".antiq/index/stray.json"), "x").expect("a file");
    let run = antiq_in(&temp, &["--json", "reconcile"]);
    succeeded(&run);
    assert!(!temp.root().join(".antiq/index/stray.json").exists());
    let json = run.json();
    assert_eq!(json["index"]["removed"], 1);
}

#[test]
fn the_console_says_what_was_done() {
    let temp = world();
    let run = antiq_in(&temp, &["reconcile"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Rebuilt the index: 2 written, 0 removed\n");
    assert_eq!(run.stderr, "");
}

#[test]
fn a_listing_that_cannot_be_read_is_skipped_and_the_run_is_partial() {
    let temp = world();
    let directory = temp.root().join(format!(".antiq/objects/{}", id(1)));
    let listing = fs::read_dir(&directory)
        .expect("lists")
        .map(|item| item.expect("an item").path())
        .find(|path| path.to_string_lossy().ends_with(".listing.zst"))
        .expect("the listing file");
    fs::write(&listing, "damaged").expect("a file");

    let run = antiq_in(&temp, &["--json", "reconcile"]);
    assert_eq!(run.code(), 3, "{}", run.stdout);
    let json = run.json();
    assert_eq!(json["index"]["written"], 1);
    let skipped = json["index"]["skipped"].as_array().expect("a list").clone();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["id"], id(1).to_string());
    assert!(
        skipped[0]["reason"]
            .as_str()
            .expect("a reason")
            .contains("cannot be read"),
        "{skipped:?}"
    );
    assert_eq!(run.stderr, "");

    let run = antiq_in(&temp, &["reconcile"]);
    assert_eq!(run.code(), 3, "{}", run.stdout);
    // The first run left the file of the other object, which a rebuild
    // empties before it writes.
    assert_eq!(run.stdout, "Rebuilt the index: 1 written, 1 removed\n");
    assert!(
        run.stderr
            .starts_with(&format!("note: the listing of {} cannot be read", id(1))),
        "{}",
        run.stderr
    );
}

// =========================================================
// The repairs and the lock
// =========================================================

#[test]
fn a_real_run_empties_what_an_interrupted_run_left() {
    let mut temp = world();
    temp.tree().file(".antiq/tmp/leftover", "x");
    succeeded(&antiq_in(&temp, &["reconcile"]));
    assert!(!temp.root().join(".antiq/tmp/leftover").exists());
}

#[test]
fn what_the_repairs_report_is_a_note_and_a_warning() {
    let mut temp = world();
    temp.tree()
        .file(&format!(".antiq/tracking/{}.json", id(9)), "not json");
    let run = antiq_in(&temp, &["reconcile"]);
    succeeded(&run);
    assert!(
        run.stderr.starts_with(&format!(
            "note: the tracking entry of {} cannot be used",
            id(9)
        )),
        "{}",
        run.stderr
    );
    let json = antiq_in(&temp, &["--json", "reconcile"]).json();
    let warnings = json["warnings"].as_array().expect("a list");
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0]["message"]
            .as_str()
            .expect("a message")
            .contains("cannot be used"),
        "{warnings:?}"
    );
}

#[test]
fn a_run_fails_while_another_run_holds_the_lock() {
    let temp = world();
    let _held = hold_lock(temp.root());
    let run = antiq_in(&temp, &["--json", "reconcile"]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "lock_held");
    assert!(!cache_file(&temp, 1).exists());
}

// =========================================================
// Dry runs
// =========================================================

#[test]
fn a_dry_run_lists_the_rebuild_and_changes_nothing() {
    let mut temp = world();
    temp.tree().file(".antiq/tmp/leftover", "x");
    let before = snapshot(temp.root());
    let run = antiq_in(&temp, &["reconcile", "--dry-run"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Would rebuild the index (2 member lists)\n");
    assert_eq!(run.stderr, "");
    assert_eq!(snapshot(temp.root()), before);
    assert!(!temp.root().join(".antiq/lock").exists());
}

#[test]
fn a_dry_run_counts_one_member_list_in_the_singular() {
    let temp = TempCollection::minimal();
    let archive = archive_with_members(temp.root(), 1, 1, 10, &["a"]);
    put_meta(
        temp.root(),
        &metadata(
            1,
            "alpha",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive],
        ),
    );
    let run = antiq_in(&temp, &["reconcile", "--dry-run"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Would rebuild the index (1 member list)\n");
}

#[test]
fn a_dry_run_in_json_is_the_plan() {
    let temp = world();
    let run = antiq_in(&temp, &["--json", "reconcile", "--dry-run"]);
    succeeded(&run);
    assert_eq!(
        run.stdout,
        "{\"format\":1,\"command\":\"reconcile\",\"dry_run\":true,\
         \"actions\":[{\"action\":\"rebuild_index\",\"member_lists\":2}]}\n"
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn a_dry_run_names_the_run_that_holds_the_lock() {
    let temp = world();
    let _held = hold_lock(temp.root());
    let run = antiq_in(&temp, &["reconcile", "--dry-run"]);
    succeeded(&run);
    let pid = std::process::id();
    assert!(
        run.stderr.starts_with(&format!(
            "note: another run holds the lock (pid {pid}, machine imperius, started "
        )),
        "{}",
        run.stderr
    );
    let run = antiq_in(&temp, &["--json", "reconcile", "--dry-run"]);
    succeeded(&run);
    assert_eq!(run.stderr, "");
}

// =========================================================
// Flags without a remote
// =========================================================

#[test]
fn force_and_if_reachable_change_nothing_without_a_remote() {
    let temp = world();
    let run = antiq_in(&temp, &["reconcile", "--force", "--if-reachable"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Rebuilt the index: 2 written, 0 removed\n");
    assert_eq!(run.stderr, "");
}

// =========================================================
// What every command tells about the collection
// =========================================================

fn configure(temp: &TempCollection, lines: &str) {
    fs::write(
        temp.root().join(".antiq/config.toml"),
        format!("[collection]\nid = \"{COLLECTION_ID}\"\n{lines}\n"),
    )
    .expect("a config can be written");
}

const UNKNOWN_KEY: &str = "the key `bogus` under [collection] is unknown and ignored";

#[test]
fn an_unknown_collection_key_is_noted_by_a_real_run() {
    let temp = world();
    configure(&temp, "bogus = 1");
    let run = antiq_in(&temp, &["reconcile"]);
    succeeded(&run);
    assert_eq!(run.stderr, format!("note: {UNKNOWN_KEY}\n"));
    let json = antiq_in(&temp, &["--json", "reconcile"]).json();
    assert_eq!(json["warnings"], json!([{"message": UNKNOWN_KEY}]));
}

#[test]
fn an_unknown_collection_key_is_noted_by_a_dry_run() {
    let temp = world();
    configure(&temp, "bogus = 1");
    let run = antiq_in(&temp, &["reconcile", "--dry-run"]);
    succeeded(&run);
    assert_eq!(run.stderr, format!("note: {UNKNOWN_KEY}\n"));
}

#[test]
fn outside_a_collection_reconcile_fails_with_its_kind() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let run = Antiq::new()
        .args(["--json", "-C"])
        .arg(dir.path())
        .arg("reconcile")
        .run();
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "not_a_collection");
}
