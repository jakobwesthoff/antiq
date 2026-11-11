// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: deleting one given version of a piece, and all of them.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Setup, World, both_modes};
use serde_json::json;
use std::fs;

const PIECE: &str = "tools/scan";
/// 21:00:00 on 2026-03-01 in Auckland.
const OLDEST: &str = "2026-03-01T080000";
/// 00:15:00 on 2026-09-04 in Auckland, the day after its UTC date.
const MIDDLE: &str = "2026-09-03T121500";
const NEWEST: &str = "2026-11-04T090211";
/// Two versions whose archive is gone: only sidecars are left.
const INCOMPLETE_ONE: &str = "2026-10-01T000000";
const INCOMPLETE_TWO: &str = "2026-10-02T000000";

fn scan_world(clock: &crate::support::scenario::collection::Clock) -> Setup {
    let mut builder = Collection::build(clock);
    builder.patterns(&["*/*"]);
    for stamp in [OLDEST, MIDDLE, NEWEST, INCOMPLETE_ONE, INCOMPLETE_TWO] {
        builder.version(PIECE, stamp, &[("run.sh", "echo\n"), ("lib/a.txt", "a\n")]);
    }
    builder.version("docs/notes", "2026-01-05T101500", &[("one.md", "one\n")]);
    let collection = builder.finish();
    collection.damage_remove_archive(PIECE, INCOMPLETE_ONE);
    collection.damage_remove_archive(PIECE, INCOMPLETE_TWO);
    Setup::new(collection)
}

/// Which of the archive, the listing and the checksum file of a version are
/// still in the store.
fn present(world: &World, stamp: &str) -> [bool; 3] {
    let files = world.collection().version_files(PIECE, stamp);
    [files.archive, files.listing, files.checksum].map(|path| path.exists())
}

/// The bare names of a version's files in the order `forget` deletes them:
/// the archive first, then the listing and the checksum file.
fn file_names(world: &World, stamp: &str) -> Vec<String> {
    let files = world.collection().version_files(PIECE, stamp);
    [files.archive, files.listing, files.checksum]
        .iter()
        .map(|path| {
            path.file_name()
                .expect("a file name")
                .to_string_lossy()
                .to_string()
        })
        .collect()
}

fn utc_of(stamp: &str) -> String {
    let (date, time) = stamp.split_once('T').expect("a stamp holds a T");
    format!("{date}T{}:{}:{}Z", &time[0..2], &time[2..4], &time[4..6])
}

/// The pieces `find` shows, to see what the store still holds.
fn listed_pieces(world: &World) -> Vec<String> {
    let list = world.run(&["find"]);
    list.expect_code(0);
    let mut pieces = Vec::new();
    list.text_only(|out| {
        pieces = out
            .lines()
            .map(|line| line.split('\t').next().expect("a piece").to_string())
            .collect();
    });
    list.json_only(|json| {
        pieces = json["versions"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|row| row["piece"].as_str().expect("a piece").to_string())
            .collect();
    });
    pieces.dedup();
    pieces
}

/// A command that is refused: the answer is an error object in JSON mode
/// and a message on stderr in text mode, and nothing is on stdout.
fn refused(world: &World, args: &[&str], code: i32, kind: &str) {
    let outcome = world.run(args);
    outcome.expect_code(code);
    outcome.keys(&["format", "command", "error"]);
    outcome.json_only(|_| assert_eq!(outcome.error_kind().as_deref(), Some(kind), "{args:?}"));
    outcome.text_only(|out| {
        assert_eq!(out, "", "{args:?}");
        assert!(!outcome.stderr.is_empty(), "{args:?}");
    });
}

#[test]
fn forget_deletes_the_given_version_and_then_all_that_are_left() {
    both_modes(scan_world, |world| {
        // What `forget` cannot do without being told which versions.
        world.assert_unchanged(|| {
            refused(world, &["forget", PIECE], 2, "usage");
            refused(world, &["forget", PIECE, "--all-versions"], 2, "usage");
            refused(world, &["forget", PIECE, ""], 2, "usage");
            // The spelling of the input decides how it is matched: the UTC
            // date of the middle version is not its local date.
            refused(world, &["forget", PIECE, "2026-09-03"], 1, "other");
        });

        // The plan of a dry run names the files in the order they go.
        world.assert_unchanged(|| {
            let dry = world.run(&["forget", "-n", PIECE, MIDDLE]);
            dry.expect_code(0);
            dry.keys(&["format", "command", "dry_run", "actions", "warnings"]);
            assert_eq!(dry.stderr, "");
            let names = file_names(world, MIDDLE);
            dry.text_only(|out| {
                assert_eq!(
                    out,
                    format!(
                        "Would delete version {MIDDLE} of {PIECE}: {}\n",
                        names.join(", ")
                    )
                );
            });
            dry.json_only(|json| {
                assert_eq!(json["dry_run"], true);
                assert_eq!(
                    json["actions"],
                    json!([{
                        "kind": "forget_version",
                        "piece": PIECE,
                        "stamp": MIDDLE,
                        "version": utc_of(MIDDLE),
                        "incomplete": false,
                        "files": names,
                    }])
                );
                let action = json["actions"][0].as_object().expect("an action");
                let keys: Vec<&str> = action.keys().map(String::as_str).collect();
                assert_eq!(
                    keys,
                    ["kind", "piece", "stamp", "version", "incomplete", "files"]
                );
                assert_eq!(json["warnings"], json!([]));
            });
        });

        // One version, named by its local time.
        let names = file_names(world, MIDDLE);
        let forget = world.run(&["forget", PIECE, "2026-09-04 00:15:00"]);
        forget.expect_code(0);
        assert_eq!(forget.stderr, "");
        forget.keys(&[
            "format",
            "command",
            "forgotten",
            "failed",
            "repairs",
            "warnings",
        ]);
        forget.text_only(|out| {
            assert_eq!(out, format!("Deleted version {MIDDLE} of {PIECE}\n"));
        });
        forget.json_only(|json| {
            assert_eq!(
                json["forgotten"],
                json!([{
                    "piece": PIECE,
                    "stamp": MIDDLE,
                    "version": utc_of(MIDDLE),
                    "incomplete": false,
                    "files": names,
                }])
            );
            let keys: Vec<&str> = json["forgotten"][0]
                .as_object()
                .expect("a version")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, ["piece", "stamp", "version", "incomplete", "files"]);
            for list in ["failed", "repairs", "warnings"] {
                assert_eq!(json[list], json!([]), "{list}");
            }
        });
        assert_eq!(present(world, MIDDLE), [false; 3]);
        for kept in [OLDEST, NEWEST] {
            assert_eq!(present(world, kept), [true; 3], "{kept}");
        }

        // On a terminal the version is named by its local time. The run
        // changes the store in both worlds alike.
        let tty = world.run_tty(&["forget", PIECE, OLDEST]);
        tty.expect_code(0);
        assert_eq!(
            tty.plain(),
            format!("Deleted version 2026-03-01 21:00:00 of {PIECE}\n")
        );
        assert_eq!(present(world, OLDEST), [false; 3]);

        // A version without an archive goes by its stamp and is said to be
        // incomplete; it has the sidecars only.
        let incomplete = world.run(&["forget", PIECE, INCOMPLETE_ONE]);
        incomplete.expect_code(0);
        incomplete.text_only(|out| {
            assert_eq!(
                out,
                format!("Deleted incomplete version {INCOMPLETE_ONE} of {PIECE}\n")
            );
        });
        incomplete.json_only(|json| {
            let forgotten = &json["forgotten"][0];
            assert_eq!(forgotten["stamp"], INCOMPLETE_ONE);
            assert_eq!(forgotten["incomplete"], true);
            let sidecars = file_names(world, INCOMPLETE_ONE);
            assert_eq!(forgotten["files"], json!(sidecars[1..]));
        });
        assert_eq!(present(world, INCOMPLETE_ONE), [false; 3]);

        // Every version that is left, the other incomplete one included.
        let all = world.run(&["forget", PIECE, "--all-versions", "--force"]);
        all.expect_code(0);
        assert_eq!(all.stderr, "");
        all.text_only(|out| {
            let mut lines: Vec<&str> = out.lines().collect();
            lines.sort_unstable();
            let mut expected = vec![
                format!("Deleted incomplete version {INCOMPLETE_TWO} of {PIECE}"),
                format!("Deleted version {NEWEST} of {PIECE}"),
            ];
            expected.sort_unstable();
            assert_eq!(lines, expected);
        });
        all.json_only(|json| {
            let forgotten = json["forgotten"].as_array().expect("a list");
            let mut found: Vec<(String, bool)> = forgotten
                .iter()
                .map(|each| {
                    (
                        each["stamp"].as_str().expect("a stamp").to_string(),
                        each["incomplete"].as_bool().expect("a flag"),
                    )
                })
                .collect();
            found.sort();
            assert_eq!(
                found,
                [
                    (INCOMPLETE_TWO.to_string(), true),
                    (NEWEST.to_string(), false)
                ]
            );
        });
        for stamp in [NEWEST, INCOMPLETE_TWO] {
            assert_eq!(present(world, stamp), [false; 3], "{stamp}");
        }

        // The empty store folder and its parent are gone; the other piece
        // is as it was.
        assert!(!world.collection().path(".antiq/pieces/tools").exists());
        assert_eq!(listed_pieces(world), ["docs/notes"]);
        assert_eq!(world.collection().versions("docs/notes").len(), 1);
        let tmp = fs::read_dir(world.collection().path(".antiq/tmp"))
            .expect("the staging area exists")
            .count();
        assert_eq!(tmp, 0, "left in .antiq/tmp");

        // The piece is not in the store any more.
        world.assert_unchanged(|| {
            refused(
                world,
                &["forget", PIECE, "--all-versions", "--force"],
                1,
                "unknown_reference",
            );
        });
    });
}
