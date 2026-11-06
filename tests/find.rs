// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq find` seen from outside the binary. The binary runs detached from
//! any terminal, so every run here is the one a script sees: the narrow
//! layout, no colour, no picker.

mod support;

use antiq::meta::{Lifecycle, PayloadKind};
use antiq::relpath::Name;
use antiq::tracking::{EntryState, LocalPayload};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use support::cmd::{Antiq, Run};
use support::objects::{
    archive_with_members, entry, hold_lock, id, metadata, payload, put_entry, put_meta, snapshot,
};
use support::tree::{COLLECTION_ID, TempCollection};

const OTHER_COLLECTION: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

/// `antiq -C <root> <args>`.
fn antiq_in(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

fn root_of(temp: &TempCollection) -> PathBuf {
    fs::canonicalize(temp.root()).expect("the root exists")
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

/// An archived object held locally, as `archive` leaves it.
fn archived(temp: &TempCollection, number: u8, at: &str, size: u64, members: &[&str]) {
    let archive = archive_with_members(temp.root(), number, 1, size, members);
    let mut local = entry(number, at, EntryState::Gone);
    local.local_payloads = vec![LocalPayload {
        kind: PayloadKind::Archive,
        version: 1,
        file: archive.file.clone(),
    }];
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
    put_entry(temp.root(), &local);
}

/// A live object at `at` with its folder.
fn live(temp: &mut TempCollection, number: u8, at: &str) {
    temp.tree().file(&format!("{at}/notes.txt"), "x");
    put_meta(
        temp.root(),
        &metadata(number, at, Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    put_entry(temp.root(), &entry(number, at, EntryState::Live));
}

/// Three objects: an archive of 12 MiB with a listing, a live piece and an
/// archive that exists only on the remote.
fn world() -> TempCollection {
    let mut temp = TempCollection::minimal();
    archived(
        &temp,
        1,
        "2026_09_canu",
        12_582_912,
        &["src/main.rs", "README.md"],
    );
    live(&mut temp, 2, "scratch");
    put_meta(
        temp.root(),
        &metadata(
            3,
            "old_notes",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 512)],
        ),
    );
    temp
}

fn paths(json: &Value) -> Vec<&str> {
    json["matches"]
        .as_array()
        .expect("matches is a list")
        .iter()
        .map(|found| found["path"].as_str().expect("a path"))
        .collect()
}

// =========================================================
// The JSON list
// =========================================================

#[test]
fn json_lists_every_object_with_its_fields() {
    let temp = world();
    let run = antiq_in(&temp, &["--json", "find"]);
    succeeded(&run);
    let json = run.json();
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(keys, ["format", "command", "query", "matches", "warnings"]);
    assert_eq!(json["command"], "find");
    assert_eq!(json["query"], Value::Null);
    assert_eq!(paths(&json), ["2026_09_canu", "scratch", "old_notes"]);
    assert_eq!(
        json["matches"][0],
        json!({
            "id": id(1).to_string(),
            "path": "2026_09_canu",
            "abs_path": format!("{}/2026_09_canu", root_of(&temp).display()),
            "name": "2026_09_canu",
            "state": "archived",
            "created": "2026-01-01T00:00:00Z",
            "size": 12_582_912,
        })
    );
    assert_eq!(json["matches"][1]["size"], Value::Null);
    assert_eq!(run.stderr, "");
}

#[test]
fn every_object_has_the_state_it_is_in() {
    let mut temp = world();
    temp.tree().file("unearthed/notes.txt", "x");
    let mut back = entry(4, "unearthed", EntryState::Live);
    back.unearthed = Some("2026-02-01T00:00:00Z".parse().expect("a time"));
    put_meta(
        temp.root(),
        &metadata(
            4,
            "unearthed",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 1)],
        ),
    );
    put_entry(temp.root(), &back);
    put_meta(
        temp.root(),
        &metadata(5, "theirs", Lifecycle::Live, OTHER_COLLECTION, Vec::new()),
    );
    put_meta(
        temp.root(),
        &metadata(6, "limbo", Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    put_meta(
        temp.root(),
        &metadata(7, "lost", Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    put_entry(temp.root(), &entry(7, "lost", EntryState::Gone));

    let json = antiq_in(&temp, &["--json", "find"]).json();
    let state_of = |path: &str| {
        json["matches"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|found| found["path"] == path)
            .unwrap_or_else(|| panic!("{path} is listed"))["state"]
            .clone()
    };
    assert_eq!(state_of("2026_09_canu"), "archived");
    assert_eq!(state_of("scratch"), "live");
    assert_eq!(state_of("old_notes"), "remote-only");
    assert_eq!(state_of("unearthed"), "unearthed");
    assert_eq!(state_of("theirs"), "live-on-another-machine");
    assert_eq!(state_of("lost"), "gone");
    // Own live metadata without an entry is the crash window the repairs
    // close; until then the object is an error.
    assert_eq!(state_of("limbo"), "error");
}

#[test]
fn an_object_whose_folder_cannot_be_looked_at_is_in_error_and_find_goes_on() {
    let mut temp = world();
    let locked = temp.tree().unreadable_dir("locked");
    assert!(
        fs::read_dir(&locked).is_err(),
        "the test needs a directory its user cannot enter, so it cannot run as root"
    );
    put_meta(
        temp.root(),
        &metadata(
            4,
            "locked/inner",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 1)],
        ),
    );
    put_entry(temp.root(), &entry(4, "locked/inner", EntryState::Gone));
    let run = antiq_in(&temp, &["--json", "find"]);
    succeeded(&run);
    let json = run.json();
    let inner = json["matches"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|found| found["path"] == "locked/inner")
        .expect("the object is listed");
    assert_eq!(inner["state"], "error");
    assert_eq!(json["matches"].as_array().expect("a list").len(), 4);
}

#[test]
fn an_object_with_a_damaged_tracking_file_is_in_error() {
    let temp = world();
    fs::write(
        temp.root().join(format!(".antiq/tracking/{}.json", id(3))),
        "not json",
    )
    .expect("a file");
    let json = antiq_in(&temp, &["--json", "find", "old_notes"]).json();
    assert_eq!(json["matches"][0]["path"], "old_notes");
    assert_eq!(json["matches"][0]["state"], "error");
}

#[test]
fn a_listing_that_cannot_be_read_is_a_warning_and_the_piece_is_still_listed() {
    let temp = world();
    let listing = fs::read_dir(temp.root().join(format!(".antiq/objects/{}", id(1))))
        .expect("lists")
        .map(|item| item.expect("an item").path())
        .find(|path| path.to_string_lossy().ends_with(".listing.zst"))
        .expect("the listing file");
    fs::write(&listing, "damaged").expect("a file");
    let run = antiq_in(&temp, &["--json", "find"]);
    succeeded(&run);
    assert_eq!(run.json()["matches"].as_array().expect("a list").len(), 3);
    // A query that only a member could answer meets the damaged listing.
    let run = antiq_in(&temp, &["--json", "find", "main.rs"]);
    succeeded(&run);
    let json = run.json();
    assert_eq!(json["matches"], json!([]));
    let message = json["warnings"][0]["message"].as_str().expect("a message");
    assert!(
        message.contains(&id(1).to_string()) && message.contains("cannot be searched"),
        "{message}"
    );
}

// =========================================================
// Queries
// =========================================================

#[test]
fn without_a_query_the_newest_object_comes_first() {
    let temp = TempCollection::minimal();
    for (number, at, created) in [
        (1, "oldest", "2025-01-01T00:00:00Z"),
        (2, "newest", "2026-06-01T00:00:00Z"),
        (3, "middle", "2025-09-01T00:00:00Z"),
    ] {
        let mut object = metadata(
            number,
            at,
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 1)],
        );
        object.created = created.parse().expect("a time");
        put_meta(temp.root(), &object);
    }
    let json = antiq_in(&temp, &["--json", "find"]).json();
    assert_eq!(paths(&json), ["newest", "middle", "oldest"]);
}

#[test]
fn a_query_narrows_the_list_by_piece_path() {
    let temp = world();
    let run = antiq_in(&temp, &["--json", "find", "canu"]);
    succeeded(&run);
    let json = run.json();
    assert_eq!(json["query"], "canu");
    assert_eq!(paths(&json), ["2026_09_canu"]);
}

#[test]
fn a_query_matches_member_paths_from_the_listings() {
    let temp = world();
    let json = antiq_in(&temp, &["--json", "find", "main.rs"]).json();
    assert_eq!(paths(&json), ["2026_09_canu"]);
}

#[test]
fn a_query_matches_the_name_where_it_differs_from_the_path() {
    let temp = world();
    let mut imported = metadata(
        4,
        "imports/x",
        Lifecycle::Archived,
        COLLECTION_ID,
        vec![payload(PayloadKind::Archive, 1, 1)],
    );
    imported.name = Name::from_bytes(b"Holiday photos".to_vec()).expect("a name");
    put_meta(temp.root(), &imported);
    let json = antiq_in(&temp, &["--json", "find", "holiday"]).json();
    assert_eq!(paths(&json), ["imports/x"]);
    assert_eq!(json["matches"][0]["name"], "Holiday photos");
}

#[test]
fn path_matches_come_before_name_matches_and_those_before_member_matches() {
    let temp = TempCollection::minimal();
    // Matches through its path.
    put_meta(
        temp.root(),
        &metadata(
            1,
            "wolf_pack",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 1)],
        ),
    );
    // Matches through its name only.
    let mut renamed = metadata(
        2,
        "imports/x",
        Lifecycle::Archived,
        COLLECTION_ID,
        vec![payload(PayloadKind::Archive, 1, 1)],
    );
    renamed.name = Name::from_bytes(b"wolf notes".to_vec()).expect("a name");
    put_meta(temp.root(), &renamed);
    // Matches through a member only.
    let archive = archive_with_members(temp.root(), 3, 1, 1, &["wolf.txt"]);
    put_meta(
        temp.root(),
        &metadata(3, "zzz", Lifecycle::Archived, COLLECTION_ID, vec![archive]),
    );
    let json = antiq_in(&temp, &["--json", "find", "wolf"]).json();
    assert_eq!(paths(&json), ["wolf_pack", "imports/x", "zzz"]);
}

#[test]
fn a_query_that_matches_nothing_gives_an_empty_list_and_a_note() {
    let temp = world();
    let run = antiq_in(&temp, &["find", "zzzz"]);
    succeeded(&run);
    assert_eq!(run.stdout, "");
    assert_eq!(run.stderr, "no piece matches \"zzzz\"\n");
}

#[test]
fn a_query_that_matches_nothing_in_json_has_an_empty_list_and_no_stderr() {
    let temp = world();
    let run = antiq_in(&temp, &["--json", "find", "zzzz"]);
    succeeded(&run);
    assert_eq!(run.json()["matches"], json!([]));
    assert_eq!(run.stderr, "");
}

#[test]
fn a_collection_without_objects_says_so() {
    let temp = TempCollection::minimal();
    let run = antiq_in(&temp, &["find"]);
    succeeded(&run);
    assert_eq!(run.stdout, "");
    assert_eq!(run.stderr, "no pieces\n");
}

// =========================================================
// --print
// =========================================================

#[test]
fn print_gives_the_absolute_path_of_every_match() {
    let temp = world();
    let run = antiq_in(&temp, &["find", "--print"]);
    succeeded(&run);
    let root = root_of(&temp);
    assert_eq!(
        run.stdout,
        format!(
            "{root}/2026_09_canu\n{root}/scratch\n{root}/old_notes\n",
            root = root.display()
        )
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn print_with_a_query_prints_only_the_matches() {
    let temp = world();
    let run = antiq_in(&temp, &["find", "canu", "--print"]);
    succeeded(&run);
    assert_eq!(
        run.stdout,
        format!("{}/2026_09_canu\n", root_of(&temp).display())
    );
}

#[test]
fn print_with_json_adds_nothing() {
    let temp = world();
    let plain = antiq_in(&temp, &["--json", "find"]);
    let printed = antiq_in(&temp, &["--json", "find", "--print"]);
    succeeded(&printed);
    assert_eq!(printed.stdout, plain.stdout);
}

#[test]
fn print_writes_the_bytes_of_the_path_not_its_escaped_form() {
    let temp = TempCollection::minimal();
    put_meta(
        temp.root(),
        &metadata(
            1,
            "caf%FF",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 1)],
        ),
    );
    let out = tempfile::NamedTempFile::new().expect("a temp file");
    let run = Antiq::new()
        .arg("-C")
        .arg(temp.root())
        .args(["find", "--print"])
        .stdout(out.reopen().expect("reopens"))
        .run();
    succeeded(&run);
    let mut expected = root_of(&temp).into_os_string().into_encoded_bytes();
    expected.extend_from_slice(b"/caf\xff\n");
    assert_eq!(fs::read(out.path()).expect("reads"), expected);
}

#[test]
fn the_json_form_of_the_same_path_is_escaped() {
    let temp = TempCollection::minimal();
    put_meta(
        temp.root(),
        &metadata(
            1,
            "caf%FF",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload(PayloadKind::Archive, 1, 1)],
        ),
    );
    let json = antiq_in(&temp, &["--json", "find"]).json();
    assert_eq!(
        json["matches"][0]["abs_path"],
        format!("{}/caf%FF", root_of(&temp).display())
    );
}

// =========================================================
// The console list
// =========================================================

#[test]
fn the_list_is_a_table_of_state_piece_size_and_the_shortest_id() {
    let temp = world();
    let run = antiq_in(&temp, &["find"]);
    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @r"
    STATE        PIECE           SIZE  ID
    archived     2026_09_canu  12 MiB  01EJ3PX001
    live         scratch            -  01EJ3PX002
    remote-only  old_notes      512 B  01EJ3PX003
    ");
    assert_eq!(run.stderr, "");
}

// =========================================================
// Notes
// =========================================================

#[test]
fn a_run_that_holds_the_lock_is_named_in_a_note_and_find_still_lists() {
    let temp = world();
    let _held = hold_lock(temp.root());
    let run = antiq_in(&temp, &["find", "--print"]);
    succeeded(&run);
    assert_eq!(run.stdout.lines().count(), 3);
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
fn in_json_the_lock_note_is_a_warning_and_stderr_stays_empty() {
    let temp = world();
    let _held = hold_lock(temp.root());
    let run = antiq_in(&temp, &["--json", "find"]);
    succeeded(&run);
    assert_eq!(run.stderr, "");
    let warnings = run.json()["warnings"].clone();
    assert_eq!(warnings.as_array().expect("a list").len(), 1);
    assert!(
        warnings[0]["message"]
            .as_str()
            .expect("a message")
            .starts_with("another run holds the lock"),
        "{warnings}"
    );
}

#[test]
fn an_unknown_collection_key_is_noted() {
    let temp = world();
    fs::write(
        temp.root().join(".antiq/config.toml"),
        format!("[collection]\nid = \"{COLLECTION_ID}\"\nbogus = 1\n"),
    )
    .expect("a config can be written");
    let run = antiq_in(&temp, &["find"]);
    succeeded(&run);
    assert_eq!(
        run.stderr,
        "note: the key `bogus` under [collection] is unknown and ignored\n"
    );
}

#[test]
fn an_object_that_cannot_be_read_is_a_warning_and_the_others_are_listed() {
    let temp = world();
    fs::create_dir_all(temp.root().join(format!(".antiq/objects/{}", id(9)))).expect("a dir");
    let run = antiq_in(&temp, &["--json", "find"]);
    succeeded(&run);
    let json = run.json();
    assert_eq!(json["matches"].as_array().expect("a list").len(), 3);
    let message = json["warnings"][0]["message"].as_str().expect("a message");
    assert!(
        message.contains(&id(9).to_string()) && message.contains("cannot be read"),
        "{message}"
    );
}

#[test]
fn find_changes_nothing_in_the_collection() {
    let temp = world();
    let before = snapshot(temp.root());
    for args in [
        &["find"][..],
        &["find", "main.rs"],
        &["--json", "find"],
        &["find", "--print"],
    ] {
        succeeded(&antiq_in(&temp, args));
    }
    assert_eq!(snapshot(temp.root()), before);
    assert!(!temp.root().join(".antiq/lock").exists());
}

#[test]
fn outside_a_collection_find_fails_with_its_kind() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let run = Antiq::new()
        .args(["--json", "-C"])
        .arg(dir.path())
        .arg("find")
        .run();
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "not_a_collection");
    assert_eq!(run.json()["command"], "find");
}
