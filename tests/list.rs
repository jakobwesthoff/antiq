// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq list` seen from outside the binary, on stores the scenario
//! harness writes by hand. The harness runs every command under
//! `TZ=Pacific/Auckland`, which is +13:00 in November 2026 and +12:00 in
//! April 2026, so local time and UTC never read alike.

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use support::cmd::{hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};

const SCANNER_NEW: &str = "2026-11-04T090211";
const SCANNER_OLD: &str = "2026-09-03T121500";
const SCAN_OLD: &str = "2025-02-11T184007";

fn world(mode: Mode, build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), mode)
}

fn three_versions(builder: &mut CollectionBuilder) {
    builder
        .version(
            "projects/scanner",
            SCANNER_OLD,
            &[("src/main.rs", "fn main() {}\n")],
        )
        .version(
            "projects/scanner",
            SCANNER_NEW,
            &[("src/main.rs", "fn main() {}\n"), ("README", "scan\n")],
        )
        .version("tools/scan-old", SCAN_OLD, &[("run.sh", "echo\n")]);
}

fn fields(line: &str) -> Vec<&str> {
    line.split('\t').collect()
}

#[test]
fn a_pipe_gets_tab_separated_lines_with_stamps_newest_first() {
    let world = world(Mode::Text, three_versions);
    let run = world.run(&["list"]);
    run.expect_code(0);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{:?}", run.stdout);
    let row = |index: usize| fields(lines[index]);
    assert_eq!(&row(0)[..2], ["projects/scanner", SCANNER_NEW]);
    assert_eq!(&row(1)[..2], ["projects/scanner", SCANNER_OLD]);
    assert_eq!(&row(2)[..2], ["tools/scan-old", SCAN_OLD]);
    assert_eq!((row(0)[3], row(0)[4]), ("2", "ok"));
    assert_eq!((row(1)[3], row(1)[4]), ("1", "ok"));
    for line in &lines {
        let bytes: u64 = fields(line)[2].parse().expect("raw archive bytes");
        assert!(bytes > 0, "{line:?}");
    }
    assert_eq!(run.stderr, "");
}

#[test]
fn a_terminal_gets_the_table_in_local_time() {
    let world = world(Mode::Text, three_versions);
    let run = world.run_tty(&["list"]);
    run.expect_code(0);
    let plain = run.plain();
    let lines: Vec<&str> = plain.lines().collect();
    assert_eq!(lines.len(), 4, "{:?}", run.stdout);
    // The header is dim, and no rule sits under it.
    assert!(run.stdout.starts_with("\x1b[2mPIECE"), "{:?}", run.stdout);
    assert!(lines[0].starts_with("PIECE"), "{:?}", lines[0]);
    assert!(lines[0].trim_end().ends_with("FILES"), "{:?}", lines[0]);
    // 09:02:11 UTC is 22:02:11 at +13:00, and the September stamp is at +12:00.
    assert!(
        lines[1].starts_with("projects/scanner   2026-11-04 22:02:11   "),
        "{:?}",
        lines[1]
    );
    assert!(lines[2].contains("2026-09-04 00:15:00"), "{:?}", lines[2]);
    assert!(lines[3].contains("2025-02-12 07:40:07"), "{:?}", lines[3]);
    assert!(!run.stdout.contains(SCANNER_NEW), "{:?}", run.stdout);
    assert!(lines[1].ends_with(" 2"), "{:?}", lines[1]);
}

#[test]
fn json_carries_utc_sizes_counts_and_the_stamp() {
    let world = world(Mode::Json, three_versions);
    let run = world.run(&["list"]);
    run.expect_code(0)
        .keys(&["format", "command", "versions", "warnings"]);
    run.json_only(|json| {
        assert_eq!(json["command"], "list");
        let versions = json["versions"].as_array().expect("a list");
        assert_eq!(versions.len(), 3);
        let first = versions[0].as_object().expect("an object");
        let keys: Vec<&str> = first.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "piece",
                "stamp",
                "version",
                "state",
                "archive_bytes",
                "bytes",
                "files",
                "damage"
            ]
        );
        assert_eq!(first["piece"], "projects/scanner");
        assert_eq!(first["stamp"], SCANNER_NEW);
        assert_eq!(first["version"], "2026-11-04T09:02:11Z");
        assert_eq!(first["state"], "ok");
        assert_eq!(first["files"], 2);
        assert_eq!(first["bytes"], 18);
        assert!(first["archive_bytes"].as_u64().expect("a number") > 0);
        assert_eq!(first["damage"], Value::Array(Vec::new()));
        assert_eq!(json["warnings"], Value::Array(Vec::new()));
    });
    assert_eq!(run.stderr, "");
}

#[test]
fn a_query_keeps_the_matching_pieces_in_every_form() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, |builder| {
            three_versions(builder);
            builder.version("notes", SCANNER_NEW, &[("a.txt", "a")]);
        });
        let run = world.run(&["list", "scan"]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(out.lines().count(), 3, "{out:?}");
            assert!(!out.contains("notes"), "{out:?}");
        });
        run.json_only(|json| {
            let pieces: Vec<&str> = json["versions"]
                .as_array()
                .expect("a list")
                .iter()
                .map(|row| row["piece"].as_str().expect("a piece"))
                .collect();
            assert_eq!(
                pieces,
                ["projects/scanner", "projects/scanner", "tools/scan-old"]
            );
        });
    }
}

#[test]
fn the_utc_offset_tells_apart_versions_that_render_alike() {
    // New Zealand's summer time ends at 03:00 on 2026-04-05, so 02:30 happens twice.
    let world = world(Mode::Text, |builder| {
        builder
            .version("notes", "2026-04-04T133000", &[("a.txt", "a")])
            .version("notes", "2026-04-04T143000", &[("a.txt", "a")]);
    });
    let run = world.run_tty(&["list"]);
    run.expect_code(0);
    assert!(
        run.stdout.contains("2026-04-05 02:30:00 +12:00"),
        "{:?}",
        run.stdout
    );
    assert!(
        run.stdout.contains("2026-04-05 02:30:00 +13:00"),
        "{:?}",
        run.stdout
    );
}

fn zstd_listing(text: &str) -> Vec<u8> {
    zstd::stream::encode_all(text.as_bytes(), 3).expect("the listing compresses")
}

#[test]
fn damaged_incomplete_and_newer_format_versions_are_marked() {
    let world = world(Mode::Text, |builder| {
        for piece in [
            "a-sound",
            "b-truncated",
            "c-incomplete",
            "d-newer",
            "e-nolisting",
        ] {
            builder.version(piece, SCANNER_NEW, &[("f", "content")]);
        }
    });
    let collection = world.collection();
    collection.damage_truncate_archive("b-truncated", SCANNER_NEW);
    collection.damage_remove_archive("c-incomplete", SCANNER_NEW);
    collection.damage_remove_listing("e-nolisting", SCANNER_NEW);
    fs::write(
        collection.version_files("d-newer", SCANNER_NEW).listing,
        zstd_listing("# antiq listing 99\n#\n"),
    )
    .expect("the listing is replaced");

    let run = world.run(&["list"]);
    run.expect_code(0);
    let states: Vec<(String, String)> = run
        .stdout
        .lines()
        .map(|line| (fields(line)[0].to_string(), fields(line)[4].to_string()))
        .collect();
    let pair = |piece: &str, state: &str| (piece.to_string(), state.to_string());
    assert_eq!(
        states,
        [
            pair("a-sound", "ok"),
            pair("b-truncated", "damaged"),
            pair("c-incomplete", "incomplete"),
            pair("d-newer", "newer-format"),
            pair("e-nolisting", "damaged"),
        ]
    );
    assert!(
        run.stderr
            .contains("note: b-truncated 2026-11-04T090211 is damaged: the archive has "),
        "{:?}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("note: e-nolisting 2026-11-04T090211 is damaged: the listing is missing"),
        "{:?}",
        run.stderr
    );

    let tty = world.run_tty(&["list"]);
    // A terminal is coloured: red, yellow and magenta.
    assert!(
        tty.stdout.contains("   \x1b[31mdamaged\x1b[0m"),
        "{:?}",
        tty.stdout
    );
    assert!(
        tty.stdout.contains("   \x1b[33mincomplete\x1b[0m"),
        "{:?}",
        tty.stdout
    );
    assert!(
        tty.stdout.contains("   \x1b[35mnewer format\x1b[0m"),
        "{:?}",
        tty.stdout
    );
}

#[test]
fn a_doubled_slot_names_every_file_of_it() {
    let world = world(Mode::Json, |builder| {
        builder.version("notes", SCANNER_NEW, &[("a.txt", "a")]);
    });
    let archive = world
        .collection()
        .version_files("notes", SCANNER_NEW)
        .archive;
    let twin = archive.with_file_name(format!("other--{SCANNER_NEW}.tar.zst"));
    fs::copy(&archive, &twin).expect("the archive is copied");

    let run = world.run(&["list"]);
    run.expect_code(0);
    run.json_only(|json| {
        let row = &json["versions"][0];
        assert_eq!(row["state"], "damaged");
        assert_eq!(row["archive_bytes"], Value::Null);
        let damage = row["damage"][0].as_str().expect("a text");
        assert!(damage.contains("other--"), "{damage}");
        assert!(damage.contains("notes--"), "{damage}");
    });
}

#[test]
fn an_empty_store_exits_0_with_a_note_in_every_form() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, |_| {});
        let run = world.run(&["list"]);
        run.expect_code(0);
        assert_eq!(run.stderr, "note: no versions in the collection\n");
        run.text_only(|out| assert_eq!(out, ""));
        run.json_only(|json| assert_eq!(json["versions"], Value::Array(Vec::new())));
    }
}

#[test]
fn a_query_without_a_match_exits_0_with_a_note_in_every_form() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        let run = world.run(&["list", "zzz"]);
        run.expect_code(0);
        assert_eq!(run.stderr, "note: no piece matches `zzz`\n");
        run.text_only(|out| assert_eq!(out, ""));
        run.json_only(|json| assert_eq!(json["versions"], Value::Array(Vec::new())));
    }
}

/// Text with the pid and the start time of the lock holder replaced.
fn without_holder(text: &str) -> String {
    let pid = std::process::id().to_string();
    let mut out = text.replace(&format!("pid {pid},"), "pid <pid>,");
    if let Some(from) = out.find("started ") {
        let to = out[from..].find(')').expect("the note ends") + from;
        out.replace_range(from..to, "started <time>");
    }
    out
}

#[test]
fn a_held_lock_is_a_note_and_the_listing_goes_on() {
    let world = world(Mode::Text, three_versions);
    let _lock = hold_lock(&world.collection().root());
    let before = snapshot(&world.collection().root());

    let run = world.run(&["list", "scan-old"]);
    run.expect_code(0);
    assert_eq!(run.stdout.lines().count(), 1, "{:?}", run.stdout);
    insta::assert_snapshot!(without_holder(&run.stderr), @"note: another run holds the lock (pid <pid>, machine imperius, started <time>)
");
    // The holder's own lock file is the one thing that differs between the
    // two states of the world.
    let after = snapshot(&world.collection().root());
    assert_eq!(after, before);
}

#[test]
fn a_held_lock_is_a_note_field_in_json_and_nothing_on_stderr() {
    let world = world(Mode::Json, three_versions);
    let _lock = hold_lock(&world.collection().root());
    let run = world.run(&["list"]);
    run.expect_code(0)
        .keys(&["format", "command", "versions", "warnings", "note"]);
    run.json_only(|json| {
        assert_eq!(json["versions"].as_array().expect("a list").len(), 3);
        let note = json["note"].as_str().expect("a text");
        assert!(
            note.contains(&format!("pid {}", std::process::id())),
            "{note}"
        );
    });
    assert_eq!(run.stderr, "");
}

#[test]
fn a_listing_that_cannot_be_read_keeps_its_row_and_is_a_warning() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, |builder| {
            builder
                .version("notes", SCANNER_NEW, &[("a.txt", "a")])
                .version("other", SCANNER_NEW, &[("a.txt", "a")]);
        });
        let listing = world
            .collection()
            .version_files("notes", SCANNER_NEW)
            .listing;
        fs::set_permissions(&listing, fs::Permissions::from_mode(0o000)).expect("the mode changes");
        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
            }
        }
        let _restore = Restore(listing.clone());
        assert!(
            fs::File::open(&listing).is_err(),
            "the test needs a non-root user"
        );

        let run = world.run(&["list"]);
        run.expect_code(0);
        run.text_only(|out| {
            let lines: Vec<&str> = out.lines().collect();
            assert_eq!(lines.len(), 2, "{out:?}");
            assert_eq!(
                fields(lines[0]),
                ["notes", SCANNER_NEW, "-", "-", "unreadable"]
            );
            assert_eq!(fields(lines[1])[4], "ok");
            assert!(
                run.stderr.starts_with("warning: cannot open notes/"),
                "{:?}",
                run.stderr
            );
            assert!(run.stderr.contains("notes--"), "{:?}", run.stderr);
        });
        run.json_only(|json| {
            assert_eq!(json["versions"][0]["state"], "unreadable");
            assert_eq!(json["versions"][0]["files"], Value::Null);
            let warnings = json["warnings"].as_array().expect("a list");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            let message = warnings[0]["message"].as_str().expect("a text");
            assert!(message.starts_with("cannot open notes/"), "{message}");
            assert_eq!(run.stderr, "");
        });
    }
}

#[test]
fn control_characters_of_a_piece_are_escaped_in_a_pipe() {
    let world = world(Mode::Text, |builder| {
        builder.version("odd\tname", SCANNER_NEW, &[("a.txt", "a")]);
    });
    let run = world.run(&["list"]);
    run.expect_code(0);
    assert_eq!(run.stdout.lines().count(), 1, "{:?}", run.stdout);
    assert!(run.stdout.starts_with("odd%09name\t"), "{:?}", run.stdout);
}

#[test]
fn an_error_on_a_stderr_terminal_is_a_block_coloured_unless_no_color_is_set() {
    let world = world(Mode::Text, three_versions);

    let coloured = world.run_tty_stderr(&["pin", "no-such-piece"], &[]);
    coloured.expect_code(1);
    assert!(
        coloured
            .stderr
            .starts_with("\x1b[30m\x1b[41m ✖ ERROR \x1b[0m\n"),
        "{:?}",
        coloured.stderr
    );

    let plain = world.run_tty_stderr(&["pin", "no-such-piece"], &[("NO_COLOR", "1")]);
    plain.expect_code(1);
    assert_eq!(
        plain.stderr,
        " ✖ ERROR\n\
         ▌ No piece matches no-such-piece, as none of the pieces has a path that fits it\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   Check the spelling, or look for the piece with antiq list.\n\
         ▌\n\
         ▌ What antiq was doing\n\
         ▌   Attempting to find the piece to pin.\n\
         \n\
         \x20ℹ INFO  Nothing was pinned.\n"
    );
}

#[test]
fn an_error_on_a_stderr_pipe_stays_plain_while_stdout_is_a_terminal() {
    let world = world(Mode::Text, three_versions);

    let run = world.run_tty(&["pin", "no-such-piece"]);
    run.expect_code(1);
    assert_eq!(
        run.stderr,
        "error: no piece matches `no-such-piece`, as none of the pieces has a path that fits it\n  \
         fix: Check the spelling, or look for the piece with antiq list.\n  \
         while attempting to pin the piece\n"
    );
}

#[test]
fn a_usage_error_on_a_stderr_terminal_is_an_error_block_with_the_usage_as_fix() {
    let world = world(Mode::Text, three_versions);

    let run = world.run_tty_stderr(&["unearth", "piece", "--to", "out"], &[("NO_COLOR", "1")]);
    run.expect_code(2);
    assert_eq!(
        run.stderr,
        " ✖ ERROR\n\
         ▌ Unexpected argument --to\n\
         ▌\n\
         ▌ Why it happened\n\
         ▌   antiq unearth does not take this argument.\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   To pass it as a value, write -- --to.\n\
         ▌   Usage: antiq unearth <PIECE>\n\
         ▌   Run antiq unearth --help to see what it takes.\n"
    );
}
