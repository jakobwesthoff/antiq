// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The picker of `antiq find`, driven on a pseudo terminal the way a person
//! drives it. Every step waits for a marker that only the state it expects
//! shows, so no test sleeps.

mod support;

use antiq::meta::Lifecycle;
use antiq::tracking::EntryState;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use support::cmd::Antiq;
use support::objects::{entry, metadata, put_entry, put_meta};
use support::pty::{COLS, CTRL_F, CTRL_K, CTRL_X, ENTER, ESC, Options, Pty, Stream};
use support::tree::{COLLECTION_ID, TempCollection};

fn days_ago(days: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64;
    now - days * 86_400
}

/// A live object at `at` with its folder, as `find` lists it.
fn live_object(temp: &mut TempCollection, number: u8, at: &str) {
    temp.tree().file(&format!("{at}/notes.txt"), "x");
    put_meta(
        temp.root(),
        &metadata(number, at, Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    put_entry(temp.root(), &entry(number, at, EntryState::Live));
}

/// A collection with the piece `old-app`, archived and gone from disk, and
/// the live pieces `alpha` and `beta`.
fn collection() -> TempCollection {
    let mut temp = TempCollection::minimal();
    temp.tree()
        .file_with("old-app/main.txt", "hello", 0o644, days_ago(40));
    let archived = Antiq::new().arg("-C").arg(temp.root()).arg("archive").run();
    assert_eq!(archived.code(), 0, "stderr: {}", archived.stderr);
    assert!(
        !temp.root().join("old-app").exists(),
        "the piece is archived"
    );
    live_object(&mut temp, 11, "alpha");
    live_object(&mut temp, 12, "beta");
    temp
}

fn root(temp: &TempCollection) -> PathBuf {
    fs::canonicalize(temp.root()).expect("the root exists")
}

/// Start `find` with `args` and wait for the first frame, which shows the
/// number of pieces.
fn open_picker(temp: &TempCollection, args: &[&str], options: Options) -> Pty {
    let mut all = vec!["find"];
    all.extend_from_slice(args);
    let mut pty = Pty::start(temp.root(), &all, options);
    pty.wait_for("the first frame", |screen| screen.contains("1/3 · 3 total"));
    pty
}

#[test]
fn enter_unearths_the_selected_piece_and_ends_the_picker() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    assert!(pty.screen().contains("old-app"), "{}", pty.screen().text());
    pty.type_text("old-app");
    pty.wait_for("one match", |screen| screen.contains("1/1 · 3 total"));
    pty.send(ENTER);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert_eq!(
        fs::read_to_string(temp.root().join("old-app/main.txt")).expect("the folder is back"),
        "hello"
    );
}

#[test]
fn escape_ends_the_picker_without_an_action() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    pty.send(ESC);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert_eq!(finished.stdout, "");
    assert_eq!(finished.stderr, "");
    assert!(!temp.root().join("old-app").exists());
    assert!(finished.screen.alt_entered());
    assert!(!finished.screen.on_alt(), "the alternate screen is left");
}

#[test]
fn the_picker_also_opens_with_stdin_on_the_terminal() {
    let temp = collection();
    let options = Options {
        stdin: Stream::Terminal,
        stdout: Stream::Pipe,
    };
    let mut pty = open_picker(&temp, &[], options);
    pty.send(ESC);
    assert_eq!(pty.finish().code(), 0);
}

#[test]
fn typing_narrows_the_rows() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    pty.type_text("alp");
    pty.wait_for("alpha alone", |screen| {
        screen.contains("1/1 · 3 total") && screen.contains("alpha") && !screen.contains("beta")
    });
    pty.send(ESC);
    pty.finish();
}

#[test]
fn print_queues_the_folder_on_enter_and_keeps_the_picker_open() {
    let temp = collection();
    let mut pty = open_picker(&temp, &["--print"], Options::piped());
    pty.type_text("beta");
    pty.send(ENTER);
    let folder = root(&temp).join("beta");
    pty.wait_for("the notice", |screen| screen.contains("printed /"));
    assert!(pty.running(), "the picker stays open");
    pty.send(ESC);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert_eq!(finished.stdout, format!("{}\n", folder.display()));
    assert!(
        !temp.root().join("old-app").exists(),
        "nothing is unearthed"
    );
}

#[test]
fn print_to_a_terminal_shows_the_path_after_the_picker_has_left_the_screen() {
    let temp = collection();
    let options = Options {
        stdin: Stream::Pipe,
        stdout: Stream::Terminal,
    };
    let mut pty = open_picker(&temp, &["--print"], options);
    pty.type_text("beta");
    pty.send(ENTER);
    let folder = root(&temp).join("beta");
    pty.wait_for("the notice", |screen| screen.contains("printed /"));
    pty.send(ESC);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0);
    let screen = finished.screen;
    assert!(!screen.on_alt());
    let shown = format!("{}", folder.display());
    assert_eq!(
        screen.lines().iter().filter(|line| **line == shown).count(),
        1,
        "{}",
        screen.text()
    );
}

#[test]
fn print_lists_every_queued_folder_in_order() {
    let temp = collection();
    let mut pty = open_picker(&temp, &["--print"], Options::piped());
    pty.type_text("beta");
    pty.send(ENTER);
    let beta = root(&temp).join("beta");
    pty.wait_for("the first notice", |screen| screen.contains("printed /"));
    pty.send(b"\x17");
    pty.wait_for("the counter back", |screen| {
        screen.contains("1/3 · 3 total") && !screen.contains("printed /")
    });
    pty.type_text("alpha");
    pty.send(ENTER);
    let alpha = root(&temp).join("alpha");
    pty.wait_for("the second notice", |screen| screen.contains("printed /"));
    pty.send(ESC);
    let finished = pty.finish();
    assert_eq!(
        finished.stdout,
        format!("{}\n{}\n", beta.display(), alpha.display())
    );
}

#[test]
fn pin_and_unpin_keep_the_picker_open() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    pty.type_text("alpha");
    pty.send(CTRL_K);
    pty.wait_for("the pin notice", |screen| screen.contains("Pinned alpha"));
    assert!(temp.root().join("alpha/.antiq-pin").exists());
    assert!(pty.running());
    pty.send(CTRL_X);
    pty.wait_for("the unpin notice", |screen| {
        screen.contains("Unpinned alpha")
    });
    assert!(!temp.root().join("alpha/.antiq-pin").exists());
    pty.send(ESC);
    assert_eq!(pty.finish().code(), 0);
}

#[test]
fn the_next_key_takes_the_notice_away() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    pty.type_text("alpha");
    pty.send(CTRL_K);
    pty.wait_for("the pin notice", |screen| screen.contains("Pinned alpha"));
    pty.send(b"\x7f");
    pty.wait_for("the counter back", |screen| {
        screen.contains("1/1 · 3 total") && !screen.contains("Pinned alpha")
    });
    pty.send(ESC);
    pty.finish();
}

#[test]
fn pinning_an_archived_piece_shows_the_refusal_and_stays_open() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    pty.type_text("old-app");
    pty.send(CTRL_K);
    pty.wait_for("the refusal", |screen| screen.contains("error: "));
    assert!(pty.running());
    pty.send(ESC);
    assert_eq!(pty.finish().code(), 0);
}

#[test]
fn fork_gives_an_unearthed_piece_a_new_id_and_ends_the_picker() {
    let temp = collection();
    let unearthed = Antiq::new()
        .arg("-C")
        .arg(temp.root())
        .args(["unearth", "old-app"])
        .run();
    assert_eq!(unearthed.code(), 0, "stderr: {}", unearthed.stderr);
    let entries = || {
        fs::read_dir(temp.root().join(".antiq/tracking"))
            .expect("the tracking directory exists")
            .count()
    };
    let before = entries();

    let mut pty = Pty::start(temp.root(), &["find"], Options::piped());
    pty.wait_for("the first frame", |screen| screen.contains("1/3 · 3 total"));
    pty.type_text("old-app");
    pty.send(CTRL_F);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert_eq!(entries(), before + 1, "stdout: {}", finished.stdout);
}

#[test]
fn a_resize_draws_the_picker_at_the_new_width() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    let full = "─".repeat(COLS.into());
    assert!(pty.screen().lines().contains(&full));
    pty.resize(60);
    let narrow = "─".repeat(60);
    pty.wait_for("the narrow frame", |screen| {
        screen.lines().contains(&narrow)
    });
    pty.send(ESC);
    pty.finish();
}

#[test]
fn json_prints_the_object_and_never_opens_the_picker() {
    let temp = collection();
    let mut pty = Pty::start(temp.root(), &["--json", "find"], Options::piped());
    pty.wait_for("the process to end", |_| true);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0);
    assert!(!finished.screen.alt_entered());
    let json: serde_json::Value =
        serde_json::from_str(&finished.stdout).expect("stdout is the JSON object");
    assert_eq!(json["command"], "find");
}

#[test]
fn nothing_found_says_so_without_opening_the_picker() {
    let temp = collection();
    let mut pty = Pty::start(temp.root(), &["find", "zzzz"], Options::piped());
    pty.wait_for("the process to end", |_| true);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0);
    assert!(!finished.screen.alt_entered());
    assert_eq!(finished.stdout, "");
    assert!(
        finished.stderr.contains("no piece matches"),
        "{}",
        finished.stderr
    );
}

#[test]
fn a_member_path_finds_the_piece() {
    let temp = collection();
    let mut pty = open_picker(&temp, &[], Options::piped());
    pty.type_text("main");
    pty.wait_for("the piece found through its member", |screen| {
        screen.contains("1/1 · 3 total") && screen.contains("old-app")
    });
    pty.send(ENTER);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert!(temp.root().join("old-app/main.txt").exists());
}

#[test]
fn a_corrupt_listing_is_noted_once() {
    let temp = collection();
    let objects = temp.root().join(".antiq/objects");
    let listing = fs::read_dir(&objects)
        .expect("the objects directory")
        .flat_map(|object| fs::read_dir(object.expect("an object").path()).expect("lists"))
        .map(|item| item.expect("an item").path())
        .find(|path| path.to_string_lossy().ends_with(".listing.zst"))
        .expect("the listing file");
    fs::write(&listing, "damaged").expect("a file");
    // The member cache would answer without the listing.
    let cache = temp.root().join(".antiq/index");
    assert!(cache.is_dir(), "the archive left a member cache");
    fs::remove_dir_all(&cache).expect("the cache is removed");

    let mut pty = open_picker(&temp, &[], Options::piped());
    pty.type_text("main");
    pty.wait_for("nothing found", |screen| {
        screen.contains("no matches · 3 total")
    });
    pty.send(ESC);
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert_eq!(
        finished.stderr.matches("cannot be searched").count(),
        1,
        "stderr: {}",
        finished.stderr
    );
}
