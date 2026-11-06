// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The brake on `remote remove`, seen from outside the binary: the piece
//! count at which it holds, what a run without a terminal does, and what a
//! person at a terminal can answer.

mod support;

use antiq::meta::{Lifecycle, PayloadKind};
use antiq::tracking::{EntryState, LocalPayload};
use std::fs;
use support::cmd::{Antiq, Run};
use support::objects::{entry, metadata, payload, put_entry, put_meta, snapshot};
use support::pty::{Options, Pty};
use support::tree::{COLLECTION_ID, TempCollection, Tree};

/// `antiq -C <root> <args>`.
fn antiq_in(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

/// A collection with a path remote attached, `kept` archived pieces whose
/// archive this machine holds and `only` pieces that only the remote holds.
fn world(kept: u8, only: u8) -> (TempCollection, Tree) {
    let temp = TempCollection::minimal();
    let remotes = Tree::new();
    let remote = remotes.path("antiq");
    let attached = antiq_in(
        &temp,
        &[
            "remote",
            "add",
            "--init",
            remote.to_str().expect("a UTF-8 path"),
        ],
    );
    assert_eq!(attached.code(), 0, "stderr: {}", attached.stderr);
    for number in 1..=kept + only {
        let at = format!("projects/piece-{number}");
        let archive = payload(PayloadKind::Archive, 1, 10);
        let held = number <= kept;
        let file = archive.file.clone();
        put_meta(
            temp.root(),
            &metadata(
                number,
                &at,
                Lifecycle::Archived,
                COLLECTION_ID,
                vec![archive],
            ),
        );
        if held {
            let mut tracked = entry(number, &at, EntryState::Gone);
            tracked.local_payloads = vec![LocalPayload {
                kind: PayloadKind::Archive,
                version: 1,
                file,
            }];
            put_entry(temp.root(), &tracked);
        }
    }
    (temp, remotes)
}

fn config_of(temp: &TempCollection) -> String {
    fs::read_to_string(temp.root().join(".antiq/config.toml")).expect("the config is read")
}

#[test]
fn without_a_terminal_removing_three_of_ten_is_refused_and_changes_nothing() {
    let (temp, _remotes) = world(7, 3);
    let before = snapshot(temp.root());
    let run = antiq_in(&temp, &["--json", "remote", "remove"]);
    assert_eq!(run.code(), 1, "stdout: {}", run.stdout);
    assert_eq!(run.error_kind(), "brake");
    let error = &run.json()["error"];
    assert_eq!(error["planned"], 3);
    assert_eq!(error["set_size"], 10);
    assert_eq!(error["items"].as_array().expect("items").len(), 3);
    assert_eq!(snapshot(temp.root()), before);
}

#[test]
fn the_console_names_the_refusal() {
    let (temp, _remotes) = world(7, 3);
    let run = antiq_in(&temp, &["remote", "remove"]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr.contains("the brake holds 3 of 10 objects"),
        "{}",
        run.stderr
    );
    assert!(config_of(&temp).contains("[[remote]]"));
}

#[test]
fn force_detaches_although_the_brake_holds() {
    let (temp, _remotes) = world(7, 3);
    let run = antiq_in(&temp, &["remote", "remove", "--force"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(!config_of(&temp).contains("[[remote]]"));
}

#[test]
fn one_of_ten_passes_without_a_question() {
    let (temp, _remotes) = world(9, 1);
    let run = antiq_in(&temp, &["remote", "remove"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(!config_of(&temp).contains("[[remote]]"));
}

#[test]
fn a_dry_run_says_where_the_brake_would_hold_and_changes_nothing() {
    let (temp, remotes) = world(7, 3);
    let before = snapshot(temp.root());
    let run = antiq_in(&temp, &["remote", "remove", "--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let remote = remotes.path("antiq");
    let shown = run
        .stdout
        .replace(remote.to_str().expect("a UTF-8 path"), "<remote>");
    insta::assert_snapshot!(shown, @r"
    Would detach the remote at <remote>
    The brake would hold at 3 of 10:
      projects/piece-8 (01EJ3PX0080000000000000008)
      projects/piece-9 (01EJ3PX0090000000000000009)
      projects/piece-10 (01EJ3PX00A000000000000000A)
    These remote-only pieces would no longer be reachable:
      projects/piece-8 (01EJ3PX0080000000000000008)
      projects/piece-9 (01EJ3PX0090000000000000009)
      projects/piece-10 (01EJ3PX00A000000000000000A)
    ");
    assert_eq!(snapshot(temp.root()), before);

    let json = antiq_in(&temp, &["--json", "remote", "remove", "--dry-run"]);
    assert_eq!(json.code(), 0);
    assert_eq!(json.json()["brake"]["planned"], 3);
    assert_eq!(json.json()["brake"]["set_size"], 10);
}

/// Starts `remote remove` on a terminal and waits for the question.
fn ask(temp: &TempCollection) -> Pty {
    let mut pty = Pty::start(temp.root(), &["remote", "remove"], Options::piped());
    pty.wait_for("the question", |screen| screen.contains("Choose [1-2"));
    pty
}

#[test]
fn a_person_at_a_terminal_can_go_on() {
    let (temp, _remotes) = world(7, 3);
    let mut pty = ask(&temp);
    assert!(
        pty.screen()
            .contains("Detach the remote? 3 of 10 objects exist only there."),
        "{}",
        pty.screen().text()
    );
    pty.send(b"2\r");
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert!(!config_of(&temp).contains("[[remote]]"));
}

#[test]
fn a_person_at_a_terminal_can_stop() {
    let (temp, _remotes) = world(7, 3);
    let mut pty = ask(&temp);
    pty.send(b"1\r");
    let finished = pty.finish();
    assert_eq!(finished.code(), 1, "stderr: {}", finished.stderr);
    assert!(config_of(&temp).contains("[[remote]]"));
}
