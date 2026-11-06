// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq archive` taking a piece that was unearthed, seen from outside the
//! binary. The binary runs detached from any terminal, so nobody can be
//! asked: a decision is recorded and the run exits 3.

mod support;

use serde_json::{Value, json};
use std::fs;
use support::cmd::{Antiq, Run};
use support::pieces::{days_ago, unearthed};
use support::tree::TempCollection;

/// `antiq -C <root> <args>`.
fn antiq(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

/// A collection with the unearthed piece `a`.
fn world() -> (TempCollection, String) {
    let mut temp = TempCollection::minimal();
    let id = unearthed(&mut temp, "a", &[("a.txt", "one"), ("b.txt", "two")]);
    (temp, id)
}

fn without_envelope(mut json: Value) -> Value {
    let envelope = json.as_object_mut().expect("the envelope is an object");
    envelope.remove("format");
    envelope.remove("command");
    json
}

#[test]
fn a_piece_that_was_not_touched_is_reclaimed() {
    let (temp, _) = world();

    let run = antiq(&temp, &["archive"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    insta::assert_snapshot!(run.stdout, @"
    reclaimed 1 piece · 6 B

      a  6 B  removed  unchanged, the archive stays as it was
    ");
    assert!(!temp.root().join("a").exists());
    assert_eq!(antiq(&temp, &["archive"]).stdout, "Nothing to do.\n");
}

#[test]
fn a_reclaim_in_json_has_a_row_and_no_new_archive() {
    let (temp, id) = world();

    let run = antiq(&temp, &["--json", "archive"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(
        without_envelope(run.json()),
        json!({
            "archived": [],
            "reclaimed": [{"id": id, "path": "a", "size": 6}],
            "skipped": [],
            "kept": [],
            "unfinished": [],
            "forked": [],
            "needs_decision": [],
            "warnings": [],
        })
    );
}

#[test]
fn a_dry_run_names_the_reclaim() {
    let (temp, _) = world();

    let run = antiq(&temp, &["archive", "--dry-run"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "Would reclaim a (6 B)\n");
    assert!(temp.root().join("a").exists());
}

#[test]
fn a_file_added_to_the_piece_makes_a_new_archive_without_asking() {
    let (temp, id) = world();
    old_file(&temp, "a/c.txt", "three");

    let run = antiq(&temp, &["--json", "archive"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let json = run.json();
    assert_eq!(json["archived"][0]["id"], json!(id));
    assert_eq!(json["archived"][0]["payload_version"], 2);
    assert_eq!(json["needs_decision"], json!([]));
    assert!(!temp.root().join("a").exists());
}

/// A file with an old modification time, so that the piece stays idle.
fn old_file(temp: &TempCollection, relative: &str, content: &str) {
    let path = temp.root().join(relative);
    fs::write(&path, content).expect("written");
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(days_ago(40), 0))
        .expect("the time is set");
}

#[test]
fn a_lost_file_is_a_decision_nobody_can_take() {
    let (temp, _) = world();
    fs::remove_file(temp.root().join("a/a.txt")).expect("removed");

    let run = antiq(&temp, &["archive"]);

    assert_eq!(run.code(), 3, "stderr: {}", run.stderr);
    insta::assert_snapshot!(run.stdout, @"
    archived 0 pieces · 0 B, 1 pending

      a                                                                            -
        pending  replacing the archive would lose 1 entry, `antiq resolve` decides
    ");
    assert!(temp.root().join("a").exists());
}

#[test]
fn the_decision_in_json_lists_what_would_be_lost() {
    let (temp, id) = world();
    fs::remove_file(temp.root().join("a/a.txt")).expect("removed");

    let run = antiq(&temp, &["--json", "archive"]);

    assert_eq!(run.code(), 3, "stderr: {}", run.stderr);
    assert_eq!(
        run.json()["needs_decision"],
        json!([{
            "id": id,
            "path": "a",
            "decision": "replacing_with_loss",
            "options": ["replace", "fork", "skip"],
            "lost_count": 1,
            "lost": [{"path": "a.txt", "change": "removed"}],
        }])
    );
}

#[test]
fn a_dry_run_names_the_question() {
    let (temp, _) = world();
    fs::remove_file(temp.root().join("a/a.txt")).expect("removed");

    let run = antiq(&temp, &["archive", "--dry-run"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(
        run.stdout,
        "Would ask whether to replace the archive of a, which would lose 1 entry\n"
    );
}
