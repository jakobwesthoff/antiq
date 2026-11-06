// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq archive` seen from outside the binary. The binary runs detached
//! from any terminal, so stdout is a pipe: the narrow layout, no colour.

mod support;

use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use support::cmd::{Antiq, Run};
use support::tree::TempCollection;

fn days_ago(days: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64;
    now - days * 86_400
}

/// `antiq -C <root> <args>`.
fn antiq(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

#[test]
fn dry_run_prints_would_lines() {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file_with("a/note.txt", "x".repeat(2_500), 0o644, days_ago(40));
    temp.tree()
        .file_with("b/note.txt", "x", 0o644, days_ago(40));
    temp.tree()
        .file_with("busy/note.txt", "x", 0o644, days_ago(1));
    let run = antiq(&temp, &["archive", "--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    insta::assert_snapshot!(run.stdout, @r"
    Would archive a (2.5 kB)
    Would archive b (1 B)
    ");
    assert_eq!(run.stderr, "");
}

#[test]
fn dry_run_json() {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file_with("a/note.txt", "12345", 0o644, days_ago(40));
    let run = antiq(&temp, &["--json", "archive", "--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let mut json = run.json();
    assert_eq!(json["command"], "archive");
    let envelope = json.as_object_mut().expect("the envelope is an object");
    envelope.remove("format");
    envelope.remove("command");
    assert_eq!(
        json,
        json!({
            "dry_run": true,
            "actions": [{"action": "archive", "path": "a", "id": null, "size": 5}],
            "warnings": [],
        })
    );
    // The keys come in the order the spec lists them.
    let text = run.stdout;
    let positions: Vec<usize> = ["\"dry_run\"", "\"actions\"", "\"warnings\""]
        .iter()
        .map(|key| text.find(key).expect("the key is printed"))
        .collect();
    assert!(positions.is_sorted(), "{text}");
}

#[test]
fn nothing_to_do() {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file_with("busy/note.txt", "x", 0o644, days_ago(1));
    let run = antiq(&temp, &["archive", "--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "Nothing to do.\n");
}

#[test]
fn a_dry_run_names_the_lock_holder_on_stderr() {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file_with("a/note.txt", "x", 0o644, days_ago(40));
    let _lock = antiq::lock::acquire(
        &temp.root().join(".antiq/lock"),
        "laptop",
        &antiq::clock::SystemClock,
    )
    .expect("the lock is free");
    let run = antiq(&temp, &["archive", "--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stderr
            .starts_with("note: another run holds the lock (pid "),
        "stderr: {}",
        run.stderr
    );
}

// =========================================================
// Real runs
// =========================================================

/// The pieces `a` (idle, 5 bytes) and `busy` (active).
fn collection_with_an_idle_piece() -> TempCollection {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file_with("a/note.txt", "12345", 0o644, days_ago(40));
    temp.tree()
        .file_with("busy/note.txt", "x", 0o644, days_ago(1));
    temp
}

#[test]
fn an_idle_piece_is_archived_and_its_folder_removed() {
    let temp = collection_with_an_idle_piece();
    let run = antiq(&temp, &["--json", "archive"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    let [row] = json["archived"].as_array().expect("a list").as_slice() else {
        panic!("one piece is archived: {json}");
    };
    assert_eq!(row["path"], "a");
    assert_eq!(row["size"], 5);
    assert_eq!(row["payload_version"], 1);
    assert!(row["archive_size"].as_u64().expect("a number") > 0);
    let id = row["id"].as_str().expect("an ID");
    assert_eq!(id.len(), 26);
    assert_eq!(json["skipped"], json!([]));
    assert_eq!(json["kept"], json!([]));
    assert_eq!(json["unfinished"], json!([]));
    assert_eq!(json["warnings"], json!([]));
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(
        keys,
        [
            "format",
            "command",
            "archived",
            "skipped",
            "kept",
            "unfinished",
            "warnings"
        ]
    );

    assert!(!temp.root().join("a").exists(), "the folder is removed");
    assert!(temp.root().join("busy/note.txt").exists());
    let object = temp.root().join(".antiq/objects").join(id);
    let files: Vec<String> = std::fs::read_dir(&object)
        .expect("the object exists")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        files.iter().any(|name| name.ends_with(".tar.zst")),
        "{files:?}"
    );
    assert!(
        files.iter().any(|name| name.ends_with(".listing.zst")),
        "{files:?}"
    );

    let status = antiq(&temp, &["--json", "status"]).json();
    let pieces = status["pieces"].as_array().expect("a list");
    let piece = pieces
        .iter()
        .find(|piece| piece["path"] == "a")
        .expect("the archived piece is listed");
    assert_eq!(piece["state"], "archived");
    assert!(
        piece["flags"]
            .as_array()
            .expect("a list")
            .contains(&json!("pending_upload")),
        "{piece}"
    );
}

#[test]
fn the_console_lists_one_finished_row_per_piece() {
    let temp = collection_with_an_idle_piece();
    let run = antiq(&temp, &["archive"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines[0], "archived 1 piece \u{b7} 5 B");
    assert!(lines[1].is_empty(), "{lines:?}");
    assert!(lines[2].starts_with("  a  5 B  removed  "), "{lines:?}");
    assert!(lines[2].ends_with(" archive, verified"), "{lines:?}");
    assert_eq!(lines.len(), 3);
    assert_eq!(run.stderr, "");
}

#[test]
fn active_pinned_and_unreadable_pieces_stay() {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file_with("busy/note.txt", "x", 0o644, days_ago(1));
    temp.tree()
        .file_with("pinned/note.txt", "x", 0o644, days_ago(40));
    temp.tree()
        .file_with("pinned/.antiq-pin", "", 0o644, days_ago(40));
    temp.tree().unreadable_dir("locked");
    let run = antiq(&temp, &["archive"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.stdout, "Nothing to do.\n");
    for path in ["busy/note.txt", "pinned/note.txt"] {
        assert!(temp.root().join(path).exists(), "{path}");
    }
}

#[test]
fn a_second_run_has_nothing_to_do() {
    let temp = collection_with_an_idle_piece();
    assert_eq!(antiq(&temp, &["archive"]).code(), 0);
    let run = antiq(&temp, &["archive"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "Nothing to do.\n");
}

#[test]
fn a_dry_run_changes_nothing() {
    let temp = collection_with_an_idle_piece();
    let run = antiq(&temp, &["archive", "--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(temp.root().join("a/note.txt").exists());
    assert!(
        std::fs::read_dir(temp.root().join(".antiq/objects"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    );
}

#[test]
fn outside_a_collection_it_says_so() {
    let dir = tempfile::tempdir().expect("the temp dir is writable");
    let run = Antiq::new()
        .args(["--json", "-C"])
        .arg(dir.path())
        .arg("archive")
        .run();
    assert_eq!(run.code(), 1, "stdout: {}", run.stdout);
    assert_eq!(run.error_kind(), "not_a_collection");
    assert_eq!(run.json()["command"], "archive");
}

#[test]
fn a_held_lock_fails_the_run() {
    let temp = collection_with_an_idle_piece();
    let _lock = antiq::lock::acquire(
        &temp.root().join(".antiq/lock"),
        "laptop",
        &antiq::clock::SystemClock,
    )
    .expect("the lock is free");
    let run = antiq(&temp, &["--json", "archive"]);
    assert_eq!(run.code(), 1, "stdout: {}", run.stdout);
    assert_eq!(run.error_kind(), "lock_held");
    assert!(temp.root().join("a/note.txt").exists());
}
