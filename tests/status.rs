// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq status` seen from outside the binary. The binary runs detached
//! from any terminal, so stdout is a pipe: the narrow layout, no colour.

mod support;

use antiq::id::ObjectId;
use antiq::meta::{
    Checksum, CreatedSource, FileName, HistoryAction, HistoryEntry, Lifecycle, Metadata, Payload,
    PayloadKind,
};
use antiq::relpath::{Name, RelPath};
use antiq::tracking::{EntryState, LocalPayload, TrackingEntry, VersionTakenOut};
use jiff::Timestamp;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use support::cmd::{Antiq, Run};
use support::tree::{COLLECTION_ID, TempCollection};

const OTHER: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

fn id(number: u8) -> ObjectId {
    ObjectId::from_ulid(ulid::Ulid::from_parts(
        1_600_000_000_000 + u64::from(number),
        u128::from(number),
    ))
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64
}

fn days_ago(days: i64) -> i64 {
    now_seconds() - days * 86_400
}

/// A collection on disk with the folders, objects and entries of a test.
struct World {
    temp: TempCollection,
}

fn world() -> World {
    World {
        temp: TempCollection::minimal(),
    }
}

fn payload(kind: PayloadKind, file: &str) -> Payload {
    Payload {
        kind,
        version: 1,
        file: FileName::new(file).expect("a plain file name"),
        package_format: "tar+zstd/1".into(),
        size: 1,
        checksum: Checksum::sha256("00"),
        listing: None,
        machine: "laptop".into(),
        created: "2026-01-01T00:00:00Z".parse().expect("a time"),
        verified: None,
        superseded: false,
        encoder: None,
        extra: serde_json::Map::new(),
    }
}

fn metadata(
    number: u8,
    at: &str,
    lifecycle: Lifecycle,
    owner: &str,
    payloads: Vec<Payload>,
) -> Metadata {
    let path = RelPath::parse(at).expect("a valid path");
    let name =
        Name::from_bytes(path.components().last().expect("a name").to_vec()).expect("a valid name");
    Metadata {
        format: 1,
        id: id(number),
        collection: owner.parse().expect("an ID"),
        path,
        name,
        created: "2026-01-01T00:00:00Z".parse().expect("a time"),
        created_source: CreatedSource::Birth,
        lifecycle,
        payloads,
        history: Vec::new(),
        extra: serde_json::Map::new(),
    }
}

impl World {
    fn root(&self) -> &Path {
        self.temp.root()
    }

    fn antiq(&self, path: &str) -> PathBuf {
        self.root().join(".antiq").join(path)
    }

    fn put_meta(&self, known: &Metadata) {
        let directory = self.antiq(&format!("objects/{}", known.id));
        fs::create_dir_all(&directory).expect("a directory");
        fs::write(
            directory.join("meta.1"),
            antiq::meta::to_bytes(known).expect("metadata serialises"),
        )
        .expect("a file");
    }

    fn put_entry(&self, entry: &TrackingEntry) {
        fs::write(
            self.antiq(&format!("tracking/{}.json", entry.id)),
            serde_json::to_vec(entry).expect("an entry serialises"),
        )
        .expect("a file");
    }

    fn entry(&self, number: u8, at: &str, state: EntryState) -> TrackingEntry {
        TrackingEntry::new(
            id(number),
            RelPath::parse(at).expect("a valid path"),
            state,
            VersionTakenOut::Committed(1),
            "2026-01-01T00:00:00Z".parse().expect("a time"),
        )
    }

    fn file(&mut self, relative: &str, age_days: i64) {
        self.temp
            .tree()
            .file_with(relative, "x", 0o644, days_ago(age_days));
    }

    fn configure(&self, extra: &str) {
        fs::write(
            self.antiq("config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{extra}\n"),
        )
        .expect("a config");
    }

    fn status(&self, args: &[&str]) -> Run {
        Antiq::new()
            .arg("-C")
            .arg(self.root())
            .args(args)
            .arg("status")
            .run()
    }

    fn json(&self) -> Value {
        let run = self.status(&["--json"]);
        assert_eq!(
            run.code(),
            0,
            "stdout: {}, stderr: {}",
            run.stdout,
            run.stderr
        );
        run.json()
    }
}

/// The pieces of a report as `(path, state, flags)`.
fn pieces(report: &Value) -> Vec<(String, String, Vec<String>)> {
    report["pieces"]
        .as_array()
        .expect("pieces is a list")
        .iter()
        .map(|piece| {
            (
                piece["path"].as_str().expect("a path").to_string(),
                piece["state"].as_str().expect("a state").to_string(),
                piece["flags"]
                    .as_array()
                    .expect("flags is a list")
                    .iter()
                    .map(|flag| flag.as_str().expect("a flag").to_string())
                    .collect(),
            )
        })
        .collect()
}

fn entry(path: &str, state: &str, flags: &[&str]) -> (String, String, Vec<String>) {
    (
        path.to_string(),
        state.to_string(),
        flags.iter().map(ToString::to_string).collect(),
    )
}

// =========================================================
// The JSON report
// =========================================================

#[test]
fn new_pinned_empty_unreadable_and_future_pieces_are_reported() {
    let mut world = world();
    world.file("busy/note.txt", 1);
    world.file("fresh/note.txt", 1);
    world.file("future/old.txt", 30);
    world.file("future/new.txt", -5);
    world.file("pinned/.antiq-pin", 30);
    world.file("pinned/note.txt", 30);
    world.temp.tree().dir("empty/sub");
    world.file("old/note.txt", 30);
    world.file("blocked/note.txt", 30);
    world.temp.tree().unreadable_dir("blocked/sub");
    assert!(
        fs::read_dir(world.root().join("blocked/sub")).is_err(),
        "the test needs a directory that cannot be read; it must not run as root"
    );

    let report = world.json();

    assert_eq!(report["format"], 1);
    assert_eq!(report["command"], "status");
    assert_eq!(report["collection"]["id"], COLLECTION_ID);
    assert_eq!(report["remote"], json!({"state": "none"}));
    assert_eq!(
        pieces(&report),
        [
            entry("blocked", "new", &["unreadable"]),
            entry("busy", "new", &[]),
            entry("empty", "new", &["empty"]),
            entry("fresh", "new", &[]),
            entry("future", "new", &["future_mtime", "idle"]),
            entry("old", "new", &["idle"]),
            entry("pinned", "new", &["pinned"]),
        ]
    );
    assert_eq!(report["pieces"][0]["id"], Value::Null);
    assert_eq!(
        report["pieces"][0]["detail"]["unreadable_first"],
        "blocked/sub"
    );
    assert_eq!(
        report["pieces"][4]["detail"]["future_files"],
        json!(["future/new.txt"])
    );
    assert_eq!(report["pieces"][1]["age_days"], 1);
    assert_eq!(report["pieces"][1]["archives_in_days"], 13);
    assert_eq!(
        report["counts"],
        json!({"live": 7, "archived": 0, "remote_only": 0, "pinned": 1, "empty": 1})
    );
}

#[test]
fn every_state_flag_and_tier_of_an_object_is_reported() {
    let mut world = world();
    world.configure(
        "\n[[remote]]\nurl = \"ssh://nas.local/volume1/antiq\"\nid = \"01BX5ZZKBKACTAV9WEVGEMMVRY\"\n",
    );
    // Archived here and checked on the remote 30 days after its commit.
    let mut kept = metadata(
        1,
        "kept",
        Lifecycle::Archived,
        COLLECTION_ID,
        vec![payload(
            PayloadKind::Archive,
            "0123456789abcdef-kept.tar.zst",
        )],
    );
    kept.payloads[0].verified = Some(Some("2026-06-01T00:00:00Z".parse().expect("a time")));
    let commit = Timestamp::from_second(days_ago(30)).expect("a time");
    kept.history
        .push(HistoryEntry::new(commit, "laptop", HistoryAction::Archive));
    kept.payloads[0].created = commit;
    world.put_meta(&kept);
    let mut kept_entry = world.entry(1, "kept", EntryState::Gone);
    kept_entry.local_payloads = vec![LocalPayload {
        kind: PayloadKind::Archive,
        version: 1,
        file: FileName::new("0123456789abcdef-kept.tar.zst").expect("a name"),
    }];
    world.put_entry(&kept_entry);
    // Remote-only: no entry.
    world.put_meta(&metadata(
        2,
        "far",
        Lifecycle::Archived,
        OTHER,
        vec![payload(
            PayloadKind::Archive,
            "fedcba9876543210-far.tar.zst",
        )],
    ));
    // Gone: a live entry whose folder is missing.
    world.put_meta(&metadata(
        3,
        "lost",
        Lifecycle::Live,
        COLLECTION_ID,
        vec![payload(PayloadKind::Live, "0011223344556677-lost.tar.zst")],
    ));
    world.put_entry(&world.entry(3, "lost", EntryState::Live));
    // Live on another machine.
    world.put_meta(&metadata(
        4,
        "theirs",
        Lifecycle::Live,
        OTHER,
        vec![payload(
            PayloadKind::Live,
            "8899aabbccddeeff-theirs.tar.zst",
        )],
    ));
    // A stale backup that needs a decision, and a missing archive.
    world.file("busy/note.txt", 1);
    world.put_meta(&metadata(
        5,
        "busy",
        Lifecycle::Live,
        COLLECTION_ID,
        vec![payload(PayloadKind::Live, "1122334455667788-busy.tar.zst")],
    ));
    let mut busy = world.entry(5, "busy", EntryState::Live);
    busy.stale_backup_since = Some("2026-10-30T00:00:00Z".parse().expect("a time"));
    busy.decide(antiq::tracking::DecisionKind::VersionConflict);
    busy.missing_on_remote = true;
    world.put_entry(&busy);
    // A recovered tree.
    world.temp.tree().dir(".antiq/recovered/01ARZ-2026-11-01");

    let report = world.json();

    assert_eq!(
        pieces(&report),
        [
            entry(
                "busy",
                "live",
                &["stale_backup", "needs_decision", "missing_on_remote"]
            ),
            entry("far", "remote-only", &[]),
            entry("kept", "archived", &[]),
            entry("lost", "gone", &[]),
            entry("theirs", "live-on-another-machine", &[]),
        ]
    );
    assert_eq!(report["pieces"][2]["remote_only_in_days"], 150);
    assert_eq!(report["pieces"][4]["detail"]["machine"], "laptop");
    assert_eq!(
        report["pieces"][0]["detail"]["decisions"],
        json!([{"kind": "version_conflict", "options": ["mine", "theirs", "fork"]}])
    );
    assert_eq!(report["recovered"], json!(["01ARZ-2026-11-01"]));
    assert_eq!(report["remote"]["url"], "ssh://nas.local/volume1/antiq");
    assert_eq!(
        report["counts"],
        json!({"live": 1, "archived": 2, "remote_only": 1, "pinned": 0, "empty": 0})
    );
}

#[test]
fn an_unparsable_highest_version_is_reported_and_the_run_still_succeeds() {
    let world = world();
    world.put_meta(&metadata(
        1,
        "old",
        Lifecycle::Archived,
        OTHER,
        vec![payload(
            PayloadKind::Archive,
            "fedcba9876543210-old.tar.zst",
        )],
    ));
    fs::write(
        world.antiq(&format!("objects/{}/meta.2", id(1))),
        "not json",
    )
    .expect("a file");
    let report = world.json();
    let warnings = report["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .expect("a message")
            .contains("meta.2"),
        "{warnings:?}"
    );
    assert_eq!(pieces(&report), [entry("old", "remote-only", &[])]);
}

#[test]
fn a_damaged_entry_and_a_pending_decision_still_exit_zero() {
    let world = world();
    fs::write(world.antiq(&format!("tracking/{}.json", id(1))), "not json").expect("a file");
    let report = world.json();
    assert_eq!(report["damaged"][0]["id"], id(1).to_string());
}

// =========================================================
// Reading only
// =========================================================

/// Every path below `.antiq/` with its size and modification time.
fn listing(root: &Path) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut found = Vec::new();
    let mut pending = vec![root.join(".antiq")];
    while let Some(directory) = pending.pop() {
        for item in fs::read_dir(&directory).expect("a directory") {
            let item = item.expect("an entry");
            let metadata = item.metadata().expect("metadata");
            if metadata.is_dir() {
                pending.push(item.path());
            }
            found.push((item.path(), metadata.len(), metadata.modified().ok()));
        }
    }
    found.sort();
    found
}

#[test]
fn status_changes_nothing_below_the_antiq_directory() {
    let mut world = world();
    world.file("old/note.txt", 30);
    fs::write(
        world.antiq("remote-state.json"),
        r#"{"last_sync":"2026-11-01T00:00:00Z"}"#,
    )
    .expect("a file");
    fs::write(world.antiq("tmp/leftover"), "x").expect("a file");
    fs::write(world.antiq("trash/leftover"), "x").expect("a file");
    let before = listing(world.root());
    world.json();
    let plain = world.status(&[]);
    assert_eq!(plain.code(), 0, "stderr: {}", plain.stderr);
    assert_eq!(listing(world.root()), before);
    assert!(!world.antiq("lock").exists(), "status took no lock");
}

#[test]
fn a_run_that_holds_the_lock_is_named_in_a_note_on_stderr() {
    let world = world();
    let lock = world.antiq("lock");
    // The test process stands in for the other run: its pid is alive.
    fs::write(
        &lock,
        format!(
            r#"{{"pid":{},"machine":"desktop","started":"2026-11-01T11:00:00Z"}}"#,
            std::process::id()
        ),
    )
    .expect("a lock file");
    let run = world.status(&[]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stderr
            .starts_with("note: another run holds the lock (pid "),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("desktop"), "{}", run.stderr);
    let json = world.status(&["--json"]);
    assert_eq!(json.stderr, "");
    assert!(
        json.json()["warnings"][0]["message"]
            .as_str()
            .expect("a message")
            .contains("holds the lock")
    );
}

#[test]
fn an_unknown_collection_key_is_noted_once() {
    let world = world();
    world.configure("idle_dyas = 3");
    let run = world.status(&[]);
    assert_eq!(run.code(), 0);
    assert_eq!(run.stderr.matches("idle_dyas").count(), 1, "{}", run.stderr);
}

// =========================================================
// The console
// =========================================================

#[test]
fn a_plain_run_prints_the_narrow_table_without_colour() {
    let mut world = world();
    world.file("old/note.txt", 30);
    world.file("busy/note.txt", 2);
    let run = world.status(&[]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(!run.stdout.contains('\x1b'), "{}", run.stdout);
    let mut lines = run.stdout.lines();
    assert_eq!(lines.next(), Some("no remote · 2 live · 0 archived"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), Some("STATE   PIECE  AGE  WHAT"));
    assert_eq!(lines.next(), Some("due     old    30d  archives next run"));
    assert_eq!(lines.next(), Some("new     busy    2d  ID at next sync"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), Some("every piece: antiq find"));
    assert_eq!(lines.next(), None);
}

#[test]
fn a_collection_with_nothing_to_do_says_so() {
    let mut world = world();
    world.file("pinned/.antiq-pin", 30);
    let run = world.status(&[]);
    assert_eq!(run.code(), 0);
    assert!(
        run.stdout
            .contains("nothing to do: no decisions, backups current, nothing due"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("\n1 pinned"), "{}", run.stdout);
}

#[test]
fn outside_a_collection_status_fails_with_its_kind() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let run = Antiq::new()
        .args(["--json", "-C"])
        .arg(dir.path())
        .arg("status")
        .run();
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "not_a_collection");
}
