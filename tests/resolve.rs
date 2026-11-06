// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq resolve` seen from outside the binary. The binary runs detached
//! from any terminal, so nobody can be asked: the decisions are listed, and
//! an answer on the command line is executed at once.

mod support;

use serde_json::{Value, json};
use std::fs;
use support::cmd::{Antiq, Run};
use support::pieces::unearthed;
use support::tree::TempCollection;

/// `antiq -C <root> <args>`.
fn antiq(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

/// A collection with the unearthed piece `a` that lost `a.txt`, and the
/// decision about it recorded by an `archive` that could ask nobody.
fn undecided() -> (TempCollection, String) {
    let mut temp = TempCollection::minimal();
    let id = unearthed(&mut temp, "a", &[("a.txt", "one"), ("b.txt", "two")]);
    fs::remove_file(temp.root().join("a/a.txt")).expect("removed");
    let run = antiq(&temp, &["archive"]);
    assert_eq!(run.code(), 3, "{} {}", run.stdout, run.stderr);
    (temp, id)
}

fn without_envelope(mut json: Value) -> Value {
    let envelope = json.as_object_mut().expect("the envelope is an object");
    envelope.remove("format");
    envelope.remove("command");
    json
}

#[test]
fn without_a_terminal_the_pending_decisions_are_listed() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["resolve"]);

    assert_eq!(run.code(), 3, "stderr: {}", run.stderr);
    insta::assert_snapshot!(run.stdout, @"
    1 pending decision

      a  replacing with loss: --replace, --fork or --skip
    ");
    assert_eq!(run.stderr, "");
}

#[test]
fn the_listing_in_json_names_the_id_and_the_answers() {
    let (temp, id) = undecided();

    let run = antiq(&temp, &["--json", "resolve"]);

    assert_eq!(run.code(), 3, "stderr: {}", run.stderr);
    assert_eq!(
        without_envelope(run.json()),
        json!({
            "pending": [{
                "id": id,
                "path": "a",
                "decision": "replacing_with_loss",
                "options": ["replace", "fork", "skip"],
            }],
            "warnings": [],
        })
    );
}

#[test]
fn a_piece_without_an_answer_lists_its_own_decisions() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["resolve", "a"]);

    assert_eq!(run.code(), 3, "stderr: {}", run.stderr);
    assert!(run.stdout.contains("a "), "{}", run.stdout);
}

#[test]
fn nothing_pending_is_nothing_to_decide() {
    let temp = TempCollection::minimal();

    let run = antiq(&temp, &["resolve"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "Nothing to decide.\n");
}

#[test]
fn replace_archives_the_piece_with_the_loss() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["resolve", "a", "--replace"]);

    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(run.stdout.contains("removed"), "{}", run.stdout);
    assert!(!temp.root().join("a").exists());
    let again = antiq(&temp, &["resolve"]);
    assert_eq!(
        (again.code(), again.stdout.as_str()),
        (0, "Nothing to decide.\n")
    );
}

#[test]
fn replace_in_json_has_the_rows_of_archive_and_what_was_resolved() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["--json", "resolve", "a", "--replace"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let json = run.json();
    assert_eq!(
        json["resolved"],
        json!({"kind": "replacing_with_loss", "answer": "replace"})
    );
    assert_eq!(json["archived"][0]["path"], "a");
    assert_eq!(json["archived"][0]["payload_version"], 2);
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(
        keys,
        [
            "format",
            "command",
            "resolved",
            "archived",
            "reclaimed",
            "skipped",
            "kept",
            "unfinished",
            "forked",
            "warnings"
        ]
    );
}

#[test]
fn skip_drops_the_decision_and_succeeds() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["resolve", "a", "--skip"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    insta::assert_snapshot!(run.stdout, @"Resolved: replacing with loss, answered with --skip");
    assert!(temp.root().join("a").exists());
    let again = antiq(&temp, &["resolve"]);
    assert_eq!(again.code(), 0);
}

#[test]
fn fork_gives_the_folder_a_new_id() {
    let (temp, id) = undecided();

    let run = antiq(&temp, &["--json", "resolve", "a", "--fork"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let json = run.json();
    assert_eq!(json["forked"][0]["from"], json!(id));
    assert_eq!(json["forked"][0]["path"], "a");
    assert!(temp.root().join("a").exists());
}

#[test]
fn an_answer_that_does_not_fit_is_a_usage_error() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["--json", "resolve", "a", "--mine"]);

    assert_eq!(run.code(), 2, "stderr: {}", run.stderr);
    assert_eq!(run.error_kind(), "usage");
    assert!(run.error_message().contains("--replace"));
}

#[test]
fn a_piece_nothing_is_pending_for_takes_no_answer() {
    let mut temp = TempCollection::minimal();
    unearthed(&mut temp, "a", &[("a.txt", "one")]);

    let run = antiq(&temp, &["--json", "resolve", "a", "--replace"]);

    assert_eq!(run.code(), 2, "stderr: {}", run.stderr);
    assert_eq!(run.error_kind(), "usage");
}

#[test]
fn an_unknown_piece_is_an_error() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["--json", "resolve", "nowhere", "--replace"]);

    assert_eq!(run.code(), 1, "stderr: {}", run.stderr);
    assert_eq!(run.error_kind(), "unknown_reference");
}

#[test]
fn a_dry_run_prints_the_action_and_changes_nothing() {
    let (temp, _) = undecided();

    let run = antiq(&temp, &["resolve", "a", "--replace", "--dry-run"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    insta::assert_snapshot!(run.stdout, @"Would replace the archive of a, losing 1 entry (3 B)");
    assert!(temp.root().join("a").exists());
    assert_eq!(antiq(&temp, &["resolve"]).code(), 3, "still pending");
}

#[test]
fn a_dry_run_in_json_has_an_action() {
    let (temp, id) = undecided();

    let run = antiq(&temp, &["--json", "resolve", "a", "--fork", "--dry-run"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(
        without_envelope(run.json()),
        json!({"dry_run": true, "actions": [{"action": "fork", "path": "a", "id": id}]})
    );
}

#[test]
fn a_piece_in_use_is_left_for_the_next_archive_with_a_note() {
    let (temp, _) = undecided();
    fs::write(temp.root().join("a/b.txt"), "touched").expect("written");

    let run = antiq(&temp, &["resolve", "a", "--replace"]);

    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.stdout, "Nothing to do.\n");
    assert_eq!(
        run.stderr,
        "note: the piece is in use; the next archive takes it\n"
    );
    assert_eq!(antiq(&temp, &["resolve"]).code(), 3, "still pending");
}

#[test]
fn a_dry_run_says_why_a_piece_would_be_skipped() {
    let (temp, id) = undecided();
    let object = temp.root().join(".antiq/objects").join(&id);
    for entry in fs::read_dir(&object).expect("lists") {
        let path = entry.expect("an entry").path();
        if path.to_string_lossy().ends_with(".listing.zst") {
            fs::remove_file(path).expect("removed");
        }
    }

    let run = antiq(&temp, &["resolve", "a", "--replace", "--dry-run"]);

    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert_eq!(run.stdout, "Nothing to do.\n");
    assert!(
        run.stderr.starts_with("note: `a` would be skipped"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_dry_run_while_another_run_holds_the_lock_prints_the_note() {
    let (temp, _) = undecided();
    let _held = support::objects::hold_lock(temp.root());

    let run = antiq(&temp, &["resolve", "a", "--fork", "--dry-run"]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stderr
            .starts_with("note: another run holds the lock (pid "),
        "{}",
        run.stderr
    );
}
