// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: reading the list of versions as a person, a script or a
//! pipe does.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Mode, Setup, World, both_modes};
use serde_json::json;
use std::fs;

const SCANNER_OLD: &str = "2026-09-03T121500";
const SCANNER_NEW: &str = "2026-11-04T090211";
const SCAN_OLD: &str = "2025-02-11T184007";
const SCAN_DAMAGED: &str = "2025-03-01T080000";

/// One row as the pipe prints it: `piece stamp archive-bytes files state`.
struct Row {
    piece: &'static str,
    stamp: &'static str,
    archive_bytes: u64,
    files: u64,
    state: &'static str,
}

fn archive_size(world: &World, piece: &str, stamp: &str) -> u64 {
    fs::metadata(world.collection().version_files(piece, stamp).archive)
        .expect("the archive exists")
        .len()
}

/// The rows `find` has to show, newest version first within a piece. The
/// archive of the damaged version is cut in half and its listing still
/// records the size it had before, which is twice the size on disk or one
/// byte more.
fn expected_rows(world: &World) -> Vec<Row> {
    let recorded = 2 * archive_size(world, "tools/scan-old", SCAN_DAMAGED);
    let row = |piece, stamp, archive_bytes, files, state| Row {
        piece,
        stamp,
        archive_bytes,
        files,
        state,
    };
    vec![
        row(
            "projects/scanner",
            SCANNER_NEW,
            archive_size(world, "projects/scanner", SCANNER_NEW),
            2,
            "ok",
        ),
        row(
            "projects/scanner",
            SCANNER_OLD,
            archive_size(world, "projects/scanner", SCANNER_OLD),
            1,
            "ok",
        ),
        row("tools/scan-old", SCAN_DAMAGED, recorded, 1, "damaged"),
        row(
            "tools/scan-old",
            SCAN_OLD,
            archive_size(world, "tools/scan-old", SCAN_OLD),
            1,
            "ok",
        ),
    ]
}

/// Whether the recorded size of a damaged row is the one expected: the cut
/// archive is `floor(size / 2)` bytes long.
fn same_size(row: &Row, found: u64) -> bool {
    if row.state == "damaged" {
        found == row.archive_bytes || found == row.archive_bytes + 1
    } else {
        found == row.archive_bytes
    }
}

#[test]
fn list_shows_versions_newest_first_in_the_form_each_reader_needs() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
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
                .version("tools/scan-old", SCAN_OLD, &[("run.sh", "echo\n")])
                .version("tools/scan-old", SCAN_DAMAGED, &[("run.sh", "echo\n")])
                .finish();
            collection.damage_truncate_archive("tools/scan-old", SCAN_DAMAGED);
            Setup::new(collection)
        },
        |world| {
            let rows = expected_rows(world);
            let fault_note = format!("note: tools/scan-old {SCAN_DAMAGED} is damaged: ");

            world.assert_unchanged(|| {
                let list = world.run(&["find"]);
                list.expect_code(0);
                // A damaged version is a note on stderr in the text run and
                // exit 0 all the same. The JSON run carries the fault in the
                // `damage` list of the row and prints no note.
                list.text_only(|_| {
                    assert_eq!(list.stderr.lines().count(), 1, "{:?}", list.stderr);
                    assert!(list.stderr.starts_with(&fault_note), "{:?}", list.stderr);
                });
                list.json_only(|_| assert_eq!(list.stderr, ""));

                list.text_only(|out| {
                    let lines: Vec<Vec<&str>> =
                        out.lines().map(|line| line.split('\t').collect()).collect();
                    assert_eq!(lines.len(), rows.len(), "{out:?}");
                    for (line, row) in lines.iter().zip(&rows) {
                        assert_eq!(line.len(), 5, "{line:?}");
                        assert_eq!((line[0], line[1]), (row.piece, row.stamp));
                        let bytes: u64 = line[2].parse().expect("unformatted bytes");
                        assert!(
                            same_size(row, bytes),
                            "{line:?} against {}",
                            row.archive_bytes
                        );
                        assert_eq!(line[3], row.files.to_string());
                        assert_eq!(line[4], row.state);
                    }
                });

                list.json_only(|json| {
                    assert_eq!(json["command"], "find");
                    let versions = json["versions"].as_array().expect("a list");
                    assert_eq!(versions.len(), rows.len());
                    for (version, row) in versions.iter().zip(&rows) {
                        let keys: Vec<&str> = version
                            .as_object()
                            .expect("an object")
                            .keys()
                            .map(String::as_str)
                            .collect();
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
                        assert_eq!(version["piece"], row.piece);
                        assert_eq!(version["stamp"], row.stamp);
                        assert_eq!(version["state"], row.state);
                        assert_eq!(version["files"], row.files);
                        let bytes = version["archive_bytes"].as_u64().expect("a number");
                        assert!(
                            same_size(row, bytes),
                            "{version} against {}",
                            row.archive_bytes
                        );
                        let damage = version["damage"].as_array().expect("a list");
                        assert_eq!(damage.is_empty(), row.state == "ok", "{version}");
                    }
                    // UTC, RFC 3339, whole seconds, from the stamp.
                    assert_eq!(versions[0]["version"], "2026-11-04T09:02:11Z");
                    assert_eq!(versions[1]["version"], "2026-09-03T12:15:00Z");
                    assert_eq!(versions[2]["version"], "2025-03-01T08:00:00Z");
                    assert_eq!(versions[3]["version"], "2025-02-11T18:40:07Z");
                    // The unpacked size is the sum of the file contents.
                    let unpacked: Vec<u64> = versions
                        .iter()
                        .map(|version| version["bytes"].as_u64().expect("a number"))
                        .collect();
                    assert_eq!(unpacked, [18, 13, 5, 5]);
                    assert_eq!(json["warnings"], json!([]));
                });

                // A person on a terminal gets the table, in local time
                // (+13:00 in November and March, +12:00 in September).
                if world.mode() == Mode::Text {
                    let tty = world.run_tty(&["find"]);
                    tty.expect_code(0);
                    let plain = tty.plain();
                    let lines: Vec<Vec<&str>> = plain
                        .lines()
                        .map(|line| line.split_whitespace().collect())
                        .collect();
                    assert_eq!(lines.len(), rows.len() + 1, "{plain:?}");
                    assert_eq!(lines[0], ["PIECE", "VERSION", "SIZE", "FILES"]);
                    let local = [
                        ("2026-11-04", "22:02:11"),
                        ("2026-09-04", "00:15:00"),
                        ("2025-03-01", "21:00:00"),
                        ("2025-02-12", "07:40:07"),
                    ];
                    for ((line, row), (date, time)) in lines[1..].iter().zip(&rows).zip(local) {
                        assert_eq!(line[..3], [row.piece, date, time], "{line:?}");
                        assert_eq!(line[4], "B", "{line:?}");
                        let bytes: u64 = line[3].parse().expect("a size in bytes");
                        assert!(same_size(row, bytes), "{line:?}");
                        assert_eq!(line[5], row.files.to_string(), "{line:?}");
                        let mark = line.get(6).copied();
                        assert_eq!(mark, (row.state == "damaged").then_some("damaged"));
                    }
                    assert!(!plain.contains(SCANNER_NEW), "{plain:?}");
                }
            });
        },
    );
}

#[test]
fn list_tells_apart_two_versions_in_the_repeated_hour() {
    // Summer time in New Zealand ends at 03:00 on 2026-04-05, so 02:30
    // happens twice: 13:30 UTC is the first, 14:30 UTC the second.
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .version("notes", "2026-04-04T133000", &[("a.txt", "a")])
                .version("notes", "2026-04-04T143000", &[("a.txt", "a")])
                .finish();
            Setup::new(collection)
        },
        |world| {
            world.assert_unchanged(|| {
                let list = world.run(&["find"]);
                list.expect_code(0);
                list.text_only(|out| {
                    let stamps: Vec<&str> = out
                        .lines()
                        .map(|line| line.split('\t').nth(1).expect("a stamp"))
                        .collect();
                    assert_eq!(stamps, ["2026-04-04T143000", "2026-04-04T133000"]);
                    assert!(!out.contains('+'), "{out:?}");
                });
                list.json_only(|json| {
                    let times: Vec<&str> = json["versions"]
                        .as_array()
                        .expect("a list")
                        .iter()
                        .map(|row| row["version"].as_str().expect("a time"))
                        .collect();
                    assert_eq!(times, ["2026-04-04T14:30:00Z", "2026-04-04T13:30:00Z"]);
                });

                if world.mode() == Mode::Text {
                    let tty = world.run_tty(&["find"]);
                    tty.expect_code(0);
                    let plain = tty.plain();
                    let rows: Vec<Vec<&str>> = plain
                        .lines()
                        .skip(1)
                        .map(|line| line.split_whitespace().collect())
                        .collect();
                    assert_eq!(rows.len(), 2, "{plain:?}");
                    // The newer version is the second 02:30, the older the
                    // first; the offset tells them apart.
                    assert_eq!(rows[0][..4], ["notes", "2026-04-05", "02:30:00", "+12:00"]);
                    assert_eq!(rows[1][..4], ["notes", "2026-04-05", "02:30:00", "+13:00"]);
                }
            });
        },
    );
}
