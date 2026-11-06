// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq pin` and `antiq unpin` seen from outside the binary.

mod support;

use antiq::meta::{Lifecycle, PayloadKind};
use antiq::tracking::{EntryState, LocalPayload};
use serde_json::json;
use support::cmd::{Antiq, Run};
use support::objects::{entry, id, metadata, payload, put_entry, put_meta};
use support::tree::{COLLECTION_ID, TempCollection};

/// A collection of another machine, which owns objects this one only
/// knows from their metadata.
const OTHER_COLLECTION: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

/// `antiq -C <root> <args>`.
fn antiq_in(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

fn collection_with_piece(piece: &str) -> TempCollection {
    let mut temp = TempCollection::minimal();
    temp.tree().file(&format!("{piece}/notes.txt"), "x");
    temp
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

fn pin_file(temp: &TempCollection, piece: &str) -> std::path::PathBuf {
    temp.root().join(piece).join(".antiq-pin")
}

/// An archived object at `at` whose folder is gone, as `archive` leaves it.
fn archived_object(temp: &TempCollection, number: u8, at: &str) {
    let mut local = entry(number, at, EntryState::Gone);
    local.local_payloads = vec![LocalPayload {
        kind: PayloadKind::Archive,
        version: 1,
        file: payload(PayloadKind::Archive, 1, 1).file,
    }];
    put_meta(
        temp.root(),
        &metadata(
            number,
            at,
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 1)],
        ),
    );
    put_entry(temp.root(), &local);
}

/// A live object at `at` with its folder and a `live` entry.
fn live_object(temp: &mut TempCollection, number: u8, at: &str) {
    temp.tree().file(&format!("{at}/notes.txt"), "x");
    put_meta(
        temp.root(),
        &metadata(number, at, Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    put_entry(temp.root(), &entry(number, at, EntryState::Live));
}

#[test]
fn pinning_a_new_piece_creates_the_pin_file() {
    let temp = collection_with_piece("a");
    let run = antiq_in(&temp, &["--json", "pin", "a"]);
    succeeded(&run);
    assert!(pin_file(&temp, "a").exists());
    let json = run.json();
    assert_eq!(json["command"], "pin");
    assert_eq!(json["id"], json!(null));
    assert_eq!(json["path"], "a");
    assert_eq!(json["pinned"], true);
    assert_eq!(json["changed"], true);
    assert_eq!(json["warnings"], json!([]));
    assert_eq!(run.stderr, "");
}

#[test]
fn a_piece_that_is_pinned_already_is_reported_as_such() {
    let temp = collection_with_piece("a");
    succeeded(&antiq_in(&temp, &["pin", "a"]));
    let run = antiq_in(&temp, &["pin", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "a is already pinned\n");
    let json = antiq_in(&temp, &["--json", "pin", "a"]).json();
    assert_eq!(json["pinned"], true);
    assert_eq!(json["changed"], false);
}

#[test]
fn unpinning_removes_the_pin_file() {
    let temp = collection_with_piece("a");
    succeeded(&antiq_in(&temp, &["pin", "a"]));
    let run = antiq_in(&temp, &["unpin", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Unpinned a\n");
    assert!(!pin_file(&temp, "a").exists());
}

#[test]
fn a_piece_that_is_not_pinned_is_reported_as_such() {
    let temp = collection_with_piece("a");
    let run = antiq_in(&temp, &["unpin", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "a is not pinned\n");
    let json = antiq_in(&temp, &["--json", "unpin", "a"]).json();
    assert_eq!(json["command"], "unpin");
    assert_eq!(json["pinned"], false);
    assert_eq!(json["changed"], false);
}

#[test]
fn a_pin_prints_the_path_of_the_piece_and_notes_nothing() {
    let temp = collection_with_piece("a");
    let run = antiq_in(&temp, &["pin", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Pinned a\n");
    assert_eq!(run.stderr, "");
}

// =========================================================
// Which object a reference means
// =========================================================

/// Runs `command` on `piece` and checks the refusal.
fn refused_for_a_missing_folder(temp: &TempCollection, command: &str, piece: &str) {
    let run = antiq_in(temp, &["--json", command, piece]);
    assert_eq!(run.code(), 1, "stdout: {}", run.stdout);
    assert_eq!(run.error_kind(), "other");
    assert!(
        run.error_message().contains("is not present"),
        "{}",
        run.error_message()
    );
}

#[test]
fn an_archived_piece_cannot_be_pinned_or_unpinned() {
    let temp = TempCollection::minimal();
    archived_object(&temp, 1, "old");
    refused_for_a_missing_folder(&temp, "pin", "old");
    refused_for_a_missing_folder(&temp, "unpin", "old");
    assert!(!temp.root().join("old").exists(), "no folder was made");
}

#[test]
fn a_remote_only_piece_cannot_be_pinned() {
    let temp = TempCollection::minimal();
    put_meta(
        temp.root(),
        &metadata(1, "far", Lifecycle::Archived, COLLECTION_ID, Vec::new()),
    );
    refused_for_a_missing_folder(&temp, "pin", "far");
}

#[test]
fn a_gone_piece_cannot_be_pinned() {
    let temp = TempCollection::minimal();
    put_meta(
        temp.root(),
        &metadata(1, "lost", Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    put_entry(temp.root(), &entry(1, "lost", EntryState::Gone));
    refused_for_a_missing_folder(&temp, "pin", "lost");
}

#[test]
fn a_live_piece_of_another_machine_cannot_be_pinned() {
    let temp = TempCollection::minimal();
    put_meta(
        temp.root(),
        &metadata(1, "theirs", Lifecycle::Live, OTHER_COLLECTION, Vec::new()),
    );
    refused_for_a_missing_folder(&temp, "pin", "theirs");
}

#[test]
fn a_new_folder_at_the_path_of_an_archived_piece_is_a_new_piece() {
    let mut temp = TempCollection::minimal();
    archived_object(&temp, 1, "again");
    temp.tree().file("again/notes.txt", "x");
    let run = antiq_in(&temp, &["--json", "pin", "again"]);
    succeeded(&run);
    assert_eq!(run.json()["id"], json!(null));
    assert!(pin_file(&temp, "again").exists());
}

#[test]
fn a_live_piece_is_pinned_by_path_and_by_id_prefix() {
    let mut temp = TempCollection::minimal();
    live_object(&mut temp, 1, "work");
    let run = antiq_in(&temp, &["--json", "pin", "work"]);
    succeeded(&run);
    assert_eq!(run.json()["id"], id(1).to_string());
    let prefix = id(1).to_string()[..8].to_string();
    let run = antiq_in(&temp, &["--json", "unpin", &prefix]);
    succeeded(&run);
    assert_eq!(run.json()["id"], id(1).to_string());
    assert_eq!(run.json()["changed"], true);
    assert!(!pin_file(&temp, "work").exists());
}

#[test]
fn a_gone_entry_with_live_metadata_and_a_folder_back_is_that_piece() {
    let mut temp = TempCollection::minimal();
    live_object(&mut temp, 1, "back");
    put_entry(temp.root(), &entry(1, "back", EntryState::Gone));
    let run = antiq_in(&temp, &["--json", "pin", "back"]);
    succeeded(&run);
    assert_eq!(run.json()["id"], id(1).to_string());
}

#[test]
fn a_folder_that_is_no_piece_is_an_unknown_reference() {
    let mut temp = TempCollection::minimal();
    temp.tree().file(".hidden/notes.txt", "x");
    for given in [".hidden", "missing", ".antiq", "."] {
        let run = antiq_in(&temp, &["--json", "pin", given]);
        assert_eq!(run.code(), 1, "{given}: {}", run.stdout);
        assert_eq!(run.error_kind(), "unknown_reference", "{given}");
    }
    assert!(!temp.root().join(".hidden/.antiq-pin").exists());
}

#[test]
fn a_path_outside_the_collection_is_refused() {
    let temp = TempCollection::minimal();
    let outside = tempfile::tempdir().expect("a temp directory");
    let run = antiq_in(
        &temp,
        &["--json", "pin", outside.path().to_str().expect("UTF-8")],
    );
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "outside_collection");
}

#[test]
fn an_ambiguous_prefix_lists_its_candidates() {
    let temp = TempCollection::minimal();
    archived_object(&temp, 1, "one");
    archived_object(&temp, 2, "two");
    let prefix = id(1).to_string()[..6].to_string();
    let run = antiq_in(&temp, &["--json", "pin", &prefix]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "ambiguous_reference");
    let candidates = run.json()["error"]["candidates"].clone();
    assert_eq!(
        candidates,
        json!([
            {"id": id(1).to_string(), "path": "one"},
            {"id": id(2).to_string(), "path": "two"},
        ])
    );
}

#[test]
fn a_directory_named_like_the_pin_file_is_refused_by_both_commands() {
    let mut temp = collection_with_piece("a");
    temp.tree().dir("a/.antiq-pin");
    for command in ["pin", "unpin"] {
        let run = antiq_in(&temp, &["--json", command, "a"]);
        assert_eq!(run.code(), 1, "{command}: {}", run.stdout);
        assert_eq!(run.error_kind(), "other", "{command}");
        assert!(
            run.error_message().contains("is a directory"),
            "{command}: {}",
            run.error_message()
        );
    }
    assert!(pin_file(&temp, "a").is_dir(), "the directory is untouched");
}

#[test]
fn an_object_whose_entry_has_no_metadata_cannot_be_pinned() {
    let mut temp = collection_with_piece("broken");
    temp.tree().dir(".antiq/tracking");
    put_entry(temp.root(), &entry(1, "broken", EntryState::Live));
    let run = antiq_in(&temp, &["--json", "pin", "broken"]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other");
    assert!(
        run.error_message().contains("broken cannot be pinned"),
        "{}",
        run.error_message()
    );
    assert!(!pin_file(&temp, "broken").exists());
}

#[test]
fn an_object_without_metadata_and_without_a_readable_entry_cannot_be_pinned() {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file(&format!(".antiq/tracking/{}.json", id(9)), "not json");
    let prefix = id(9).to_string()[..10].to_string();
    let run = antiq_in(&temp, &["--json", "pin", &prefix]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other");
    assert!(
        run.error_message().contains("neither metadata nor"),
        "{}",
        run.error_message()
    );
}

#[test]
fn a_piece_that_cannot_be_looked_into_reports_the_failure() {
    let mut temp = TempCollection::minimal();
    let locked = temp.tree().unreadable_dir("locked");
    assert!(
        std::fs::read_dir(&locked).is_err(),
        "the test needs a directory its user cannot enter, so it cannot run as root"
    );
    let run = antiq_in(&temp, &["--json", "pin", "locked"]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other");
    assert!(
        run.error_message().contains("cannot look for the pin file"),
        "{}",
        run.error_message()
    );
    let run = antiq_in(&temp, &["--json", "pin", "locked/inside"]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert!(
        run.error_message().contains("cannot look at"),
        "{}",
        run.error_message()
    );
}

// =========================================================
// Dry runs
// =========================================================

/// Every path below `root` with its content, to compare before and after.
fn snapshot(root: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        seen: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
    ) {
        for item in std::fs::read_dir(dir).expect("lists") {
            let path = item.expect("an item").path();
            let name = path
                .strip_prefix(base)
                .expect("below")
                .display()
                .to_string();
            if path.is_dir() {
                seen.insert(name, None);
                walk(base, &path, seen);
            } else {
                seen.insert(name, Some(std::fs::read(&path).expect("reads")));
            }
        }
    }
    let mut seen = std::collections::BTreeMap::new();
    walk(root, root, &mut seen);
    seen
}

#[test]
fn a_dry_run_lists_the_pin_and_changes_nothing() {
    let mut temp = collection_with_piece("a");
    temp.tree().file(".antiq/tmp/leftover", "x");
    temp.tree().file(".antiq/trash/gone/f", "x");
    let before = snapshot(temp.root());
    let run = antiq_in(&temp, &["pin", "--dry-run", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Would pin a\n");
    assert_eq!(run.stderr, "");
    assert_eq!(snapshot(temp.root()), before);
    assert!(!temp.root().join(".antiq/lock").exists());
}

#[test]
fn a_control_character_in_a_name_does_not_reach_the_console() {
    let temp = collection_with_piece("a\nb");
    let run = antiq_in(&temp, &["pin", "--dry-run", "a\nb"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Would pin a%0Ab\n");
    let run = antiq_in(&temp, &["pin", "a\nb"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Pinned a%0Ab\n");
}

#[test]
fn a_dry_run_of_a_pinned_piece_has_nothing_to_do() {
    let temp = collection_with_piece("a");
    succeeded(&antiq_in(&temp, &["pin", "a"]));
    let run = antiq_in(&temp, &["pin", "--dry-run", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Nothing to do.\n");
}

#[test]
fn a_dry_run_of_unpin_lists_the_removal() {
    let temp = collection_with_piece("a");
    succeeded(&antiq_in(&temp, &["pin", "a"]));
    let run = antiq_in(&temp, &["unpin", "--dry-run", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Would unpin a\n");
    assert!(pin_file(&temp, "a").exists());
}

#[test]
fn a_dry_run_in_json_is_the_plan() {
    let temp = collection_with_piece("a");
    let run = antiq_in(&temp, &["--json", "pin", "--dry-run", "a"]);
    succeeded(&run);
    assert_eq!(
        run.stdout,
        "{\"format\":1,\"command\":\"pin\",\"dry_run\":true,\
         \"actions\":[{\"action\":\"pin\",\"id\":null,\"path\":\"a\"}]}\n"
    );
    assert_eq!(run.stderr, "");
    assert!(!pin_file(&temp, "a").exists());
}

#[test]
fn a_dry_run_names_the_object_it_would_pin() {
    let mut temp = TempCollection::minimal();
    live_object(&mut temp, 1, "work");
    let run = antiq_in(&temp, &["--json", "pin", "--dry-run", "work"]);
    succeeded(&run);
    assert_eq!(
        run.json()["actions"],
        json!([{"action": "pin", "id": id(1).to_string(), "path": "work"}])
    );
}

#[test]
fn a_dry_run_refuses_what_a_real_run_refuses() {
    let temp = TempCollection::minimal();
    archived_object(&temp, 1, "old");
    let run = antiq_in(&temp, &["--json", "pin", "--dry-run", "old"]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other");
}

#[test]
fn a_dry_run_refuses_a_directory_named_like_the_pin_file() {
    let mut temp = collection_with_piece("a");
    temp.tree().dir("a/.antiq-pin");
    for command in ["pin", "unpin"] {
        let run = antiq_in(&temp, &["--json", command, "--dry-run", "a"]);
        assert_eq!(run.code(), 1, "{command}: {}", run.stdout);
        assert!(
            run.error_message().contains("is a directory"),
            "{command}: {}",
            run.error_message()
        );
    }
}

// =========================================================
// The lock
// =========================================================

/// Holds the run lock of the collection in this process.
fn hold_lock(temp: &TempCollection) -> antiq::lock::RunLock {
    antiq::lock::acquire(
        &temp.root().join(".antiq/lock"),
        "imperius",
        &antiq::clock::SystemClock,
    )
    .expect("the lock is free")
}

#[test]
fn a_real_run_fails_while_another_run_holds_the_lock() {
    let temp = collection_with_piece("a");
    let _held = hold_lock(&temp);
    let run = antiq_in(&temp, &["--json", "pin", "a"]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "lock_held");
    let error = run.json()["error"].clone();
    assert_eq!(error["pid"], std::process::id());
    assert!(error["started"].is_string(), "{error}");
    assert!(!pin_file(&temp, "a").exists());
}

#[test]
fn a_dry_run_names_the_run_that_holds_the_lock() {
    let temp = collection_with_piece("a");
    let _held = hold_lock(&temp);
    let run = antiq_in(&temp, &["pin", "--dry-run", "a"]);
    succeeded(&run);
    assert_eq!(run.stdout, "Would pin a\n");
    let pid = std::process::id();
    assert!(
        run.stderr.starts_with(&format!(
            "note: another run holds the lock (pid {pid}, machine imperius, started "
        )),
        "{}",
        run.stderr
    );
}

#[test]
fn a_dry_run_in_json_prints_nothing_to_stderr_while_the_lock_is_held() {
    let temp = collection_with_piece("a");
    let _held = hold_lock(&temp);
    let run = antiq_in(&temp, &["--json", "pin", "--dry-run", "a"]);
    succeeded(&run);
    assert_eq!(run.stderr, "");
    assert_eq!(run.json()["dry_run"], true);
}

// =========================================================
// The repairs at the start of a run
// =========================================================

#[test]
fn a_real_run_empties_what_an_interrupted_run_left() {
    let mut temp = collection_with_piece("a");
    temp.tree().file(".antiq/tmp/leftover", "x");
    succeeded(&antiq_in(&temp, &["pin", "a"]));
    assert!(!temp.root().join(".antiq/tmp/leftover").exists());
}

#[test]
fn what_the_repairs_report_is_a_note_and_a_warning() {
    let mut temp = collection_with_piece("a");
    temp.tree()
        .file(&format!(".antiq/tracking/{}.json", id(9)), "not json");
    let run = antiq_in(&temp, &["pin", "a"]);
    succeeded(&run);
    assert!(
        run.stderr.starts_with(&format!(
            "note: the tracking entry of {} cannot be used",
            id(9)
        )),
        "{}",
        run.stderr
    );
    let json = antiq_in(&temp, &["--json", "unpin", "a"]).json();
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

// =========================================================
// Where relative references start
// =========================================================

#[test]
fn a_relative_reference_starts_at_the_directory_of_dash_c() {
    let mut temp = collection_with_piece("a");
    temp.tree().dir("work");
    let work = temp.root().join("work");
    let run = Antiq::new()
        .arg("-C")
        .arg(&work)
        .args(["--json", "pin", "../a"])
        .run();
    succeeded(&run);
    assert_eq!(run.json()["path"], "a");
}

#[test]
fn without_dash_c_a_relative_reference_starts_at_the_current_directory() {
    let mut temp = collection_with_piece("a");
    temp.tree().dir("work");
    let run = Antiq::new()
        .current_dir(temp.root().join("work"))
        .args(["--json", "pin", "../a"])
        .run();
    succeeded(&run);
    assert_eq!(run.json()["path"], "a");
}

#[test]
fn dash_c_may_name_the_directory_through_a_symlink() {
    let mut temp = collection_with_piece("a");
    temp.tree().dir("work");
    let links = tempfile::tempdir().expect("a temp directory");
    let link = links.path().join("link");
    std::os::unix::fs::symlink(temp.root().join("work"), &link).expect("a symlink");
    let run = Antiq::new()
        .arg("-C")
        .arg(&link)
        .args(["--json", "pin", "../a"])
        .run();
    succeeded(&run);
    assert_eq!(run.json()["path"], "a");
}
