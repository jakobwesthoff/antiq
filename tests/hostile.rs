// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every command, run over names that carry characters a terminal acts on.
//!
//! The collection of these tests holds pieces, files, a target and failure
//! cases whose names hold a newline, ESC, a bidi override, a tab and a `%`.
//! After each run, stdout and stderr hold no character of the escaped class
//! except the line ends and the tabs of a pipe table, and the names show in
//! the exact form of the rule: `%0A` for a newline, `%25` for a `%`.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use antiq::shown::{first_raw_character, strip_sgr};
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::fake_rsync::FakeRsync;
use support::scenario::collection::{Clock, Collection};
use support::scenario::world::{Mode, Setup, World};

/// The names of the world, as they are on disk and as the rule shows them.
const NEWLINE: (&str, &str) = ("projects/two\nlines", "projects/two%0Alines");
const ESCAPE: (&str, &str) = ("projects/ev\x1b[2Jil", "projects/ev%1B[2Jil");
const PERCENT: (&str, &str) = ("projects/100%", "projects/100%25");
const BIDI: (&str, &str) = ("projects/a\u{202e}gpj", "projects/a%E2%80%AEgpj");
const TAB: (&str, &str) = ("projects/ta\tb", "projects/ta%09b");

const HOSTILE: [(&str, &str); 5] = [NEWLINE, ESCAPE, PERCENT, BIDI, TAB];

/// A collection with five due pieces whose names are hostile.
fn collection() -> Collection {
    let mut builder = Collection::build(&Clock::now());
    builder.patterns(&["projects/*"]);
    for (raw, _) in HOSTILE {
        builder.piece(raw, |piece| {
            piece
                .age_days(40)
                .file("src/a\nb.rs", "fn main() {}\n")
                .file("100%.txt", "x")
                .file("\u{202e}rtl", "y");
        });
    }
    builder.finish()
}

fn antiq(collection: &Collection, args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .args(args)
        .run()
}

/// The collection after `archive`: every piece has a stored version and no
/// folder.
fn archived() -> Collection {
    let collection = collection();
    let run = antiq(&collection, &["archive"]);
    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    collection
}

/// The stamp of the version of `piece` that `find --json` lists.
fn stamp_of(collection: &Collection, piece: &str) -> String {
    let run = antiq(collection, &["--json", "find"]);
    let json = run.json();
    json["versions"]
        .as_array()
        .expect("a list of versions")
        .iter()
        .find(|version| version["piece"] == piece)
        .and_then(|version| version["stamp"].as_str())
        .unwrap_or_else(|| panic!("no version of {piece:?} in {}", run.stdout))
        .to_string()
}

/// Stdout and stderr hold no raw character of the escaped class.
fn assert_clean(label: &str, run: &Run) {
    for (stream, text) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
        for line in text.lines() {
            assert_eq!(
                first_raw_character(line),
                None,
                "{label}: a raw character in a line of {stream}: {line:?}\n{text:?}"
            );
        }
        let plain = strip_sgr(text);
        assert!(
            !plain.contains('\x1b') && !plain.contains('\r'),
            "{label}: a raw ESC or CR in {stream}: {text:?}"
        );
    }
}

/// The output of `run` holds `shown`, and no name is escaped twice.
fn assert_shows(label: &str, run: &Run, shown: &str) {
    let text = format!("{}{}", run.stdout, run.stderr);
    assert!(text.contains(shown), "{label}: no {shown:?} in {text:?}");
    assert!(
        !text.contains("%2525") && !text.contains("%250A"),
        "{label}: a name is escaped twice in {text:?}"
    );
}

fn run_clean(label: &str, collection: &Collection, args: &[&str], shown: &str) -> Run {
    let run = antiq(collection, args);
    assert_clean(label, &run);
    assert_shows(label, &run, shown);
    run
}

// =========================================================
// Reading commands
// =========================================================

#[test]
fn status_shows_hostile_names_by_the_rule() {
    let collection = collection();
    for (_, shown) in HOSTILE {
        run_clean("status", &collection, &["status"], shown);
    }
}

#[test]
fn find_shows_hostile_names_by_the_rule() {
    let collection = archived();
    for (_, shown) in HOSTILE {
        run_clean("find", &collection, &["find"], shown);
    }
    run_clean(
        "find with a query",
        &collection,
        &["find", "two"],
        NEWLINE.1,
    );
}

#[test]
fn grep_shows_hostile_names_by_the_rule() {
    let collection = archived();
    run_clean(
        "grep path",
        &collection,
        &["grep", "(?s)src/a.b"],
        "src/a%0Ab.rs",
    );
    run_clean("grep percent", &collection, &["grep", "100"], "100%25.txt");
    run_clean("grep bidi", &collection, &["grep", "rtl"], "%E2%80%AErtl");
}

// =========================================================
// Dry runs and commands that change things
// =========================================================

#[test]
fn archive_dry_run_shows_hostile_names_by_the_rule() {
    let collection = collection();
    for (_, shown) in HOSTILE {
        run_clean("archive -n", &collection, &["archive", "-n"], shown);
    }
}

#[test]
fn archive_shows_hostile_names_by_the_rule() {
    let collection = collection();
    let run = run_clean("archive", &collection, &["archive"], PERCENT.1);
    assert_eq!(run.code(), 0, "{}", run.stderr);
    for (_, shown) in HOSTILE {
        assert_shows("archive", &run, shown);
    }
}

#[test]
fn pin_and_unpin_show_hostile_names_by_the_rule() {
    let collection = collection();
    run_clean("pin -n", &collection, &["pin", "-n", NEWLINE.0], NEWLINE.1);
    let pinned = run_clean("pin", &collection, &["pin", NEWLINE.0], NEWLINE.1);
    assert_eq!(pinned.code(), 0, "{}", pinned.stderr);
    run_clean(
        "unpin -n",
        &collection,
        &["unpin", "-n", NEWLINE.0],
        NEWLINE.1,
    );
    run_clean("unpin", &collection, &["unpin", NEWLINE.0], NEWLINE.1);
}

#[test]
fn unearth_shows_hostile_names_by_the_rule() {
    let collection = archived();
    run_clean(
        "unearth -n",
        &collection,
        &["unearth", "-n", NEWLINE.0],
        NEWLINE.1,
    );
    let run = run_clean("unearth", &collection, &["unearth", NEWLINE.0], NEWLINE.1);
    assert_eq!(run.code(), 0, "{}", run.stderr);
}

#[test]
fn fork_shows_hostile_names_by_the_rule() {
    let collection = archived();
    run_clean(
        "fork -n",
        &collection,
        &["fork", "-n", PERCENT.0],
        PERCENT.1,
    );
    run_clean(
        "fork --name",
        &collection,
        &["fork", PERCENT.0, "--name", "projects/copy\n100%"],
        "projects/copy%0A100%25",
    );
}

#[test]
fn forget_shows_hostile_names_by_the_rule() {
    let collection = archived();
    let stamp = stamp_of(&collection, ESCAPE.0);
    run_clean(
        "forget -n",
        &collection,
        &["forget", "-n", ESCAPE.0, &stamp],
        ESCAPE.1,
    );
    run_clean(
        "forget",
        &collection,
        &["forget", ESCAPE.0, &stamp],
        ESCAPE.1,
    );
}

#[test]
fn rebuild_shows_hostile_names_by_the_rule() {
    let collection = archived();
    let stamp = stamp_of(&collection, BIDI.0);
    run_clean(
        "rebuild -n",
        &collection,
        &["rebuild", "-n", BIDI.0, &stamp],
        BIDI.1,
    );
}

#[test]
fn import_shows_hostile_names_by_the_rule() {
    let collection = collection();
    let outside = tempfile::tempdir().expect("a temporary directory");
    let folder = outside.path().join("im\nported%");
    fs::create_dir_all(folder.join("sub\ndir")).expect("a folder is made");
    fs::write(folder.join("sub\ndir/f\u{202e}x"), "1").expect("a file is written");
    let run = antiq(
        &collection,
        &["import", "-n", folder.to_str().expect("a UTF-8 path")],
    );
    assert_clean("import -n", &run);
    assert_shows("import -n", &run, "im%0Aported%25");
}

#[test]
fn init_shows_hostile_names_by_the_rule() {
    let outside = tempfile::tempdir().expect("a temporary directory");
    let folder = outside.path().join("ne\nw%");
    fs::create_dir_all(folder.join("pro\njects")).expect("a folder is made");
    for args in [&["init", "-n"][..], &["init"][..]] {
        let run = Antiq::new().arg("-C").arg(&folder).args(args).run();
        assert_clean("init", &run);
        assert_shows("init", &run, "ne%0Aw%25");
    }
}

// =========================================================
// Push, the target and the lines of rsync
// =========================================================

fn target_folder(outside: &Path) -> PathBuf {
    let target = outside.join("tar\nget%");
    fs::create_dir_all(&target).expect("a folder is made");
    target
}

#[test]
fn push_shows_a_hostile_target_by_the_rule() {
    let collection = archived();
    let outside = tempfile::tempdir().expect("a temporary directory");
    let target = target_folder(outside.path());
    collection.use_target(&target);
    collection.use_real_rsync();
    for args in [&["push", "-n", "--init"][..], &["push", "--init"][..]] {
        let run = antiq(&collection, args);
        assert_clean("push", &run);
        assert_shows("push", &run, "tar%0Aget%25");
    }
}

#[test]
fn a_failed_rsync_run_shows_its_lines_and_the_hostile_names_escaped() {
    let collection = archived();
    let outside = tempfile::tempdir().expect("a temporary directory");
    let target = target_folder(outside.path());
    collection.use_target(&target);
    let fake = FakeRsync::new("rsync  version 3.2.7  protocol version 31\n");
    let path = serde_json::to_string(fake.path().to_str().expect("a UTF-8 path"))
        .expect("a string serialises");
    let config = collection.path(".antiq/config.toml");
    let text = fs::read_to_string(&config).expect("the config exists");
    fs::write(
        &config,
        text.replacen("[target]\n", &format!("[target]\nrsync = {path}\n"), 1),
    )
    .expect("the config can be written");
    fake.answer_all(
        "",
        "rsync: send_files failed to open \"/x/a\nerror: forged\x1b[2J\u{202e}\": denied\n\
         rsync error: some files could not be transferred (code 23)\n",
        23,
    );
    let run = antiq(&collection, &["push", "--init"]);
    assert_clean("push over a failing rsync", &run);
    assert_shows(
        "push over a failing rsync",
        &run,
        "error: forged%1B[2J%E2%80%AE",
    );
    let errors = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("error: "))
        .count();
    assert_eq!(errors, 1, "{:?}", run.stderr);
}

// =========================================================
// Failures
// =========================================================

#[test]
fn an_unknown_reference_shows_the_hostile_word_by_the_rule() {
    let collection = collection();
    run_clean(
        "unknown piece",
        &collection,
        &["unearth", "no\nsuch%piece"],
        "no%0Asuch%25piece",
    );
}

#[test]
fn an_inexact_reference_offers_the_path_in_ansi_c_quoting_without_a_raw_character() {
    let collection = archived();
    let run = run_clean(
        "inexact piece",
        &collection,
        &["unearth", "two"],
        "$'projects/two\\nlines'",
    );
    assert_eq!(run.code(), 1, "{}", run.stderr);
    let json = antiq(&collection, &["--json", "unearth", "two"]);
    assert!(
        json.error_fix().contains("$'projects/two\\nlines'"),
        "{}",
        json.error_fix()
    );
}

#[test]
fn an_ambiguous_reference_shows_the_hostile_candidates_by_the_rule() {
    let collection = collection();
    let run = run_clean(
        "ambiguous piece",
        &collection,
        &["pin", "-n", "projects/"],
        "projects/two%0Alines",
    );
    assert_eq!(run.code(), 1, "{}", run.stderr);
}

#[test]
fn a_directory_that_is_no_collection_shows_its_hostile_name_by_the_rule() {
    let outside = tempfile::tempdir().expect("a temporary directory");
    let folder = outside.path().join("no\ncollection%");
    fs::create_dir_all(&folder).expect("a folder is made");
    let run = Antiq::new().arg("-C").arg(&folder).arg("status").run();
    assert_clean("not a collection", &run);
    assert_shows("not a collection", &run, "no%0Acollection%25");
}

#[test]
fn a_usage_error_shows_a_hostile_argument_by_the_rule() {
    let run = Antiq::new().args(["status", "a\nerror: forged%"]).run();
    assert_clean("usage", &run);
    assert_shows("usage", &run, "a%0Aerror: forged%25");
}

#[test]
fn a_usage_error_shows_an_argument_that_is_not_utf_8_by_its_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let run = Antiq::new()
        .arg("status")
        .arg(std::ffi::OsString::from_vec(b"x\xff".to_vec()))
        .run();
    assert_clean("usage", &run);
    assert_shows("usage", &run, "x%FF");
}

// =========================================================
// A stray file in the store
// =========================================================

/// Copies the listing of the version with `stamp` to a file whose name holds
/// a `%`, so that the slot of the listing holds two files.
fn add_stray_listing(collection: &Collection, stamp: &str) {
    fn find(dir: &Path, stamp: &str) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).expect("a directory reads") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                if let Some(found) = find(&path, stamp) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .ends_with(&format!("{stamp}.sha256.zst"))
            }) {
                return Some(path);
            }
        }
        None
    }
    let listing = find(&collection.path(".antiq/pieces"), stamp).expect("a listing exists");
    let stray = listing.with_file_name(format!("a%b--{stamp}.sha256.zst"));
    fs::copy(&listing, stray).expect("the file is copied");
}

#[test]
fn a_stray_file_with_a_percent_sign_in_the_store_is_shown_once_escaped() {
    let collection = archived();
    let stamp = stamp_of(&collection, PERCENT.1);
    add_stray_listing(&collection, &stamp);
    for args in [
        &["forget", "-n", PERCENT.0, &stamp][..],
        &["status"][..],
        &["push", "-n", "--init"][..],
    ] {
        let run = antiq(&collection, args);
        assert_clean("stray", &run);
        let text = format!("{}{}", run.stdout, run.stderr);
        assert!(!text.contains("%2525"), "{args:?}: {text:?}");
    }
    let run = antiq(&collection, &["status"]);
    assert_shows("stray status", &run, "a%25b--");
}

// =========================================================
// Commands to paste
// =========================================================

/// Deletes the listing of the version of the piece whose file names start
/// with `stem`, so that the version is damaged and `rebuild` repairs it.
fn remove_listing(collection: &Collection, stem: &str, stamp: &str) {
    fn find(dir: &Path, stem: &str, stamp: &str) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).expect("a directory reads") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                if let Some(found) = find(&path, stem, stamp) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(stem) && name.ends_with(&format!("{stamp}.sha256.zst"))
            }) {
                return Some(path);
            }
        }
        None
    }
    let listing = find(&collection.path(".antiq/pieces"), stem, stamp).expect("a listing exists");
    fs::remove_file(listing).expect("the listing is removed");
}

/// The command `antiq -C <dir> rebuild <piece> <stamp>` that `text` tells to
/// run, as written.
fn pasted_rebuild(text: &str, piece: &str) -> String {
    let pattern = format!(
        r"antiq -C \S+ rebuild {} \d{{4}}-\d{{2}}-\d{{2}}T\d{{6}}",
        regex::escape(piece)
    );
    regex::Regex::new(&pattern)
        .expect("a valid pattern")
        .find(text)
        .unwrap_or_else(|| panic!("no rebuild command in {text:?}"))
        .as_str()
        .to_string()
}

/// Runs a command as a shell reads it, with antiq as the binary under test.
fn paste(command: &str) -> Run {
    let binary = env!("CARGO_BIN_EXE_antiq");
    Antiq::program("bash")
        .arg("-c")
        .arg(command.replacen("antiq ", &format!("{binary} "), 1))
        .run()
}

#[test]
fn the_rebuild_command_of_a_status_fix_names_the_exact_piece_and_runs() {
    let collection = archived();
    let stamp = stamp_of(&collection, PERCENT.1);
    remove_listing(&collection, "projects-100-", &stamp);
    let world = World::new(Setup::new(collection), Mode::Text);

    let run = world.run_tty_sized(&["status"], 250, 24, &[]);

    let text = strip_sgr(&run.stdout);
    let command = pasted_rebuild(&text, "projects/100%");
    let pasted = paste(&command);
    assert_eq!(
        pasted.code(),
        0,
        "{command}: {}{}",
        pasted.stdout,
        pasted.stderr
    );
    assert_clean("rebuild pasted from status", &pasted);
    let after = world.run(&["status"]);
    assert!(!after.stdout.contains("damaged"), "{:?}", after.stdout);
}

#[test]
fn the_rebuild_command_of_a_refused_push_names_the_exact_piece_and_runs() {
    let collection = archived();
    let stamp = stamp_of(&collection, PERCENT.1);
    remove_listing(&collection, "projects-100-", &stamp);
    let outside = tempfile::tempdir().expect("a temporary directory");
    let target = target_folder(outside.path());
    collection.use_target(&target);
    collection.use_real_rsync();

    let run = antiq(&collection, &["push", "--init"]);

    assert_eq!(run.code(), 1, "{}", run.stderr);
    let command = pasted_rebuild(&run.stderr, "projects/100%");
    let pasted = paste(&command);
    assert_eq!(
        pasted.code(),
        0,
        "{command}: {}{}",
        pasted.stdout,
        pasted.stderr
    );
    let again = antiq(&collection, &["push", "-n", "--init"]);
    assert!(!again.stderr.contains("damaged"), "{:?}", again.stderr);
}

// =========================================================
// JSON
// =========================================================

#[test]
fn json_carries_hostile_names_as_they_are_and_does_not_panic() {
    let collection = archived();
    for command in [
        &["status"][..],
        &["find"][..],
        &["grep", "100"][..],
        &["archive", "-n"][..],
        &["unearth", "-n", NEWLINE.0][..],
    ] {
        let mut args = vec!["--json"];
        args.extend(command);
        let run = antiq(&collection, &args);
        assert_eq!(run.code(), 0, "{command:?}: {}{}", run.stdout, run.stderr);
        assert_eq!(run.json()["format"], 1, "{command:?}");
    }
    let run = antiq(&collection, &["--json", "find"]);
    assert!(
        run.stdout.contains(r#""piece":"projects/two\nlines""#),
        "the piece is the name as it is, a newline as a JSON escape: {:?}",
        run.stdout
    );
}
