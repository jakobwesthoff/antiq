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
