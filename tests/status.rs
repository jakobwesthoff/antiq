// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq status` seen from outside the binary, on collections built with
//! the scenario fixture. The run has no terminal, so the table uses the
//! narrow layout without colour, and the time zone is UTC.

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection};

const OLD_STAMP: &str = "2024-06-30T081245";
const NEWER_STAMP: &str = "2024-07-01T081245";

fn status(collection: &Collection, args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .args(args)
        .arg("status")
        .run()
}

fn json(collection: &Collection) -> Value {
    let run = status(collection, &["--json"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    run.json()
}

/// Two levels of folders are pieces, as in the example of the spec.
fn example(clock: &Clock) -> Collection {
    Collection::build(clock)
        .patterns(&["*/*"])
        .idle_days(30)
        .soon_days(3)
        .piece("experiments/rust-gpu", |piece| {
            piece.age_days(33).file("src/main.rs", "fn main() {}\n");
        })
        .piece("old/client-site", |piece| {
            piece.age_days(141).file("index.html", "<html>");
        })
        .piece("media/camera-import", |piece| {
            piece
                .age_days(52)
                .file("one.jpg", "x")
                .file(".antiqignore", "broken\\\n");
        })
        .piece("projects/antiq", |piece| {
            piece.age_days(120).file("notes.md", "x").pinned();
        })
        .piece("tools/scan-old", |piece| {
            piece.age_days(28).file("scan.sh", "#!/bin/sh\n");
        })
        .piece("tools/fresh", |piece| {
            piece.age_days(2).file("fresh.sh", "#!/bin/sh\n");
        })
        .version("tools/scan-old", OLD_STAMP, &[("scan.sh", "#!/bin/sh\n")])
        .version("tools/scan-old", NEWER_STAMP, &[("scan.sh", "#!/bin/sh\n")])
        .finish()
}

#[test]
fn the_example_of_the_spec_comes_out_of_a_built_collection() {
    let collection = example(&Clock::now());

    let run = status(&collection, &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stderr, "");
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(
        lines[0].starts_with("6 folders · 2 versions (")
            && lines[0].ends_with(") · archive after 30 days idle"),
        "{}",
        lines[0]
    );
    assert_eq!(
        &lines[1..],
        [
            "",
            "STATE    PIECE                 IDLE  WHAT",
            "due      experiments/rust-gpu   33d  archives next run",
            "refused  media/camera-import    52d  rule file:",
            "                                     media/camera-import/.antiqignore:1",
            "due      old/client-site       141d  archives next run",
            "pinned   projects/antiq        120d  kept by .antiq-pin",
            "soon     tools/scan-old         28d  archives in 2 days",
        ]
    );
}

#[test]
fn json_has_the_states_the_reasons_and_every_key() {
    let collection = example(&Clock::now());

    let found = json(&collection);

    assert_eq!(found["format"], 1);
    assert_eq!(found["command"], "status");
    assert_eq!(found["folders"], 6);
    assert_eq!(found["versions"], 2);
    assert!(
        found["archive_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert_eq!(
        (found["idle_days"].clone(), found["soon_days"].clone()),
        (30.into(), 3.into())
    );
    let pieces: Vec<(&str, &str, i64)> = found["pieces"]
        .as_array()
        .expect("pieces is a list")
        .iter()
        .map(|row| {
            (
                row["piece"].as_str().expect("a path"),
                row["state"].as_str().expect("a state"),
                row["idle_for_days"].as_i64().expect("an idle time"),
            )
        })
        .collect();
    assert_eq!(
        pieces,
        [
            ("experiments/rust-gpu", "due", 33),
            ("media/camera-import", "refused", 52),
            ("old/client-site", "due", 141),
            ("projects/antiq", "pinned", 120),
            ("tools/scan-old", "soon", 28),
        ]
    );
    let refused = &found["pieces"][1];
    assert_eq!(
        refused["reasons"],
        serde_json::json!([{
            "kind": "rule_file",
            "path": "media/camera-import/.antiqignore",
            "line": 1,
            "text": "rule file: media/camera-import/.antiqignore:1",
        }])
    );
    assert_eq!(found["pieces"][4]["due_in_days"], 2);
    for key in [
        "future_times",
        "damaged_versions",
        "incomplete_versions",
        "newer_format_versions",
        "recovered",
        "trash",
        "doomed",
        "spelling",
        "unreadable_directories",
        "unparsable_files",
        "warnings",
    ] {
        assert_eq!(found[key], serde_json::json!([]), "{key}");
    }
    assert_eq!(found["store_error"], Value::Null);
    assert!(found.get("note").is_none());
}

#[test]
fn a_collection_with_nothing_to_say_prints_the_header_alone() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("notes", |piece| {
            piece.age_days(1).file("a.txt", "x");
        })
        .finish();

    let run = status(&collection, &[]);

    assert_eq!(run.code(), 0);
    assert_eq!(
        run.stdout,
        "1 folder · 0 versions (0 B) · archive after 30 days idle\n"
    );
}

// =========================================================
// The notes
// =========================================================

fn write(collection: &Collection, relative: &str, content: &str) -> PathBuf {
    let path = collection.path(relative);
    fs::create_dir_all(path.parent().expect("a parent")).expect("the chain is made");
    fs::write(&path, content).expect("the file is written");
    path
}

/// Sets a mode back on drop, so the temp directory can be removed after a
/// failed assertion.
struct Restore(PathBuf);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

fn locked_out(path: &Path) -> Restore {
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("the mode is set");
    assert!(
        fs::read_dir(path).is_err(),
        "the test needs a user that mode 000 locks out"
    );
    Restore(path.to_path_buf())
}

#[test]
fn every_note_shows_up_in_text_and_in_json() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["*/*"])
        .piece("tools/scan-old", |piece| {
            piece.age_days(5).file_aged("later.txt", "x", -3);
        })
        .version("tools/scan-old", OLD_STAMP, &[("scan.sh", "x")])
        .version("tools/scan-old", NEWER_STAMP, &[("scan.sh", "x")])
        .finish();
    collection.damage_remove_listing("tools/scan-old", OLD_STAMP);
    collection.damage_remove_archive("tools/scan-old", NEWER_STAMP);
    write(&collection, ".antiq/pieces/tools/scan-old/readme.txt", "x");
    write(
        &collection,
        ".antiq/recovered/2024-09-01T000000/lost/a.txt",
        "x",
    );
    write(
        &collection,
        ".antiq/trash/2024-09-02T000000/tools/scan-old/b.txt",
        "x",
    );
    write(
        &collection,
        ".antiq/doomed/2024-09-03T000000/gone/dir-y",
        "x",
    );

    let run = status(&collection, &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    let notes: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("! "))
        .collect();
    assert_eq!(
        notes[0],
        format!(
            "! 1 file with a time in the future: tools/scan-old/later.txt ({})",
            future_date(&clock, 3)
        )
    );
    assert_eq!(
        &notes[1..],
        [
            "! 1 damaged version: tools/scan-old 2024-06-30 08:12:45 (no listing)",
            "! 1 incomplete version: tools/scan-old 2024-07-01 08:12:45",
            "! 1 folder in recovered/: recovered/2024-09-01T000000/lost",
            "! 1 tree left in trash/: trash/2024-09-02T000000/tools",
            "! 1 leftover in doomed/: doomed/2024-09-03T000000/gone, first entry dir-y",
            "! 1 file in a store folder that is not a version file: pieces/tools/scan-old/readme.txt",
        ]
    );

    let found = json(&collection);
    assert_eq!(found["future_times"][0]["path"], "tools/scan-old/later.txt");
    assert_eq!(
        found["damaged_versions"],
        serde_json::json!([{"piece": "tools/scan-old", "version": OLD_STAMP, "reasons": ["no listing"]}])
    );
    assert_eq!(
        found["incomplete_versions"],
        serde_json::json!([{"piece": "tools/scan-old", "version": NEWER_STAMP}])
    );
    assert_eq!(found["recovered"][0], "recovered/2024-09-01T000000/lost");
    assert_eq!(found["trash"][0], "trash/2024-09-02T000000/tools");
    assert_eq!(
        found["doomed"][0]["first_entry"],
        "doomed/2024-09-03T000000/gone/dir-y"
    );
    assert_eq!(found["doomed"][0]["cause"], Value::Null);
    assert_eq!(
        found["unparsable_files"][0],
        "pieces/tools/scan-old/readme.txt"
    );
}

/// The UTC date of the file `days` after the clock, as `status` prints it.
fn future_date(clock: &Clock, days: i64) -> String {
    jiff::Timestamp::from_second(clock.days_ago(-days))
        .expect("in range")
        .strftime("%Y-%m-%d")
        .to_string()
}

#[test]
fn a_directory_selection_cannot_read_is_a_note() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["*/*"])
        .piece("ok/piece", |piece| {
            piece.age_days(1).file("a", "x");
        })
        .piece("area/piece", |piece| {
            piece.age_days(1).file("a", "x");
        })
        .finish();
    let _restore = locked_out(&collection.path("area"));

    let run = status(&collection, &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stdout.lines().any(|l| l == "! unreadable: area"),
        "{}",
        run.stdout
    );
    assert_eq!(
        json(&collection)["unreadable_directories"],
        serde_json::json!(["area"])
    );
}

#[test]
fn a_store_that_cannot_be_read_is_a_note_and_the_table_stays() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("old", |piece| {
            piece.age_days(60).file("a", "x");
        })
        .version("notes", OLD_STAMP, &[("a.txt", "x")])
        .finish();
    let _restore = locked_out(&collection.path(".antiq/pieces/notes"));

    let run = status(&collection, &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(run.stdout.contains("due      old"), "{}", run.stdout);
    assert!(
        run.stdout
            .starts_with("1 folder · archive after 30 days idle\n"),
        "{}",
        run.stdout
    );
    assert!(
        run.stderr.starts_with("note: cannot read the store: "),
        "{}",
        run.stderr
    );
    let found = status(&collection, &["--json"]).json();
    assert_eq!(found["versions"], Value::Null);
    assert!(
        found["store_error"]
            .as_str()
            .is_some_and(|text| text.starts_with("cannot read the store: "))
    );
}

// =========================================================
// The lock, and reading only
// =========================================================

#[test]
fn a_held_lock_is_a_note_and_not_a_failure() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .piece("old", |piece| {
            piece.age_days(60).file("a", "x");
        })
        .finish();
    let _held = hold_lock(&collection.root());

    let run = status(&collection, &[]);

    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stderr
            .starts_with("note: another run holds the lock (pid "),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("imperius"), "{}", run.stderr);
    assert!(run.stdout.contains("due      old"), "{}", run.stdout);
    let found = status(&collection, &["--json"]).json();
    assert!(
        found["note"]
            .as_str()
            .is_some_and(|text| text.starts_with("another run holds the lock (pid ")),
        "{found}"
    );
}

#[test]
fn status_changes_nothing_below_the_antiq_directory() {
    let clock = Clock::now();
    let collection = example(&clock);
    write(&collection, ".antiq/tmp/leftover", "x");
    write(
        &collection,
        ".antiq/trash/2024-09-02T000000/tools/b.txt",
        "x",
    );
    write(&collection, ".antiq/doomed/2024-09-03T000000/gone/x", "x");
    let before = snapshot(&collection.root());

    let plain = status(&collection, &[]);
    let structured = status(&collection, &["--json"]);

    assert_eq!(plain.code(), 0, "stderr: {}", plain.stderr);
    assert_eq!(structured.code(), 0, "stderr: {}", structured.stderr);
    assert_eq!(snapshot(&collection.root()), before);
    assert!(
        !collection.path(".antiq/lock").exists(),
        "status took no lock"
    );
}

#[test]
fn outside_a_collection_status_fails_with_not_a_collection() {
    let dir = tempfile::tempdir().expect("the temp dir is writable");

    let run = Antiq::new()
        .args(["--json", "-C"])
        .arg(dir.path())
        .arg("status")
        .run();

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "not_a_collection");
}
