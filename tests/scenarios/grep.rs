// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: finding a path in the archived files of several versions.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Mode, Setup, World, both_modes};

const SCANNER_OLD: &str = "2026-09-03T121500";
const SCANNER_NEW: &str = "2026-11-04T090211";
const SCAN_OLD: &str = "2025-02-11T184007";

#[test]
fn grep_on_a_terminal_sets_the_right_column_in_italics() {
    let collection = Collection::build(&crate::support::scenario::collection::Clock::now())
        .version("projects/scanner", SCANNER_OLD, &[("src/main.rs", "a")])
        .version("projects/scanner", SCANNER_NEW, &[("src/main.rs", "a")])
        .finish();
    let world = World::new(Setup::new(collection), Mode::Text);

    let run = world.run_tty(&["grep", r"main\.rs"]);

    run.expect_code(0);
    assert!(
        run.stdout.contains("\x1b[3mboth versions\x1b[0m"),
        "{:?}",
        run.stdout
    );
}

#[test]
fn grep_finds_a_path_in_two_versions_of_two_pieces() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .version(
                    "projects/scanner",
                    SCANNER_OLD,
                    &[("src/main.rs", "a"), ("tools/bench/main.rs", "b")],
                )
                .version(
                    "projects/scanner",
                    SCANNER_NEW,
                    &[("src/main.rs", "a"), ("README", "c")],
                )
                .version("tools/scan-old", SCAN_OLD, &[("cli/src/main.rs", "d")])
                .finish();
            Setup::new(collection)
        },
        |world| {
            world.assert_unchanged(|| {
                let hits = world.run(&["grep", r"main\.rs"]);
                hits.expect_code(0);
                assert_eq!(hits.stderr, "");
                hits.text_only(|out| {
                    let expected = [
                        format!("projects/scanner\t{SCANNER_NEW}\tsrc/main.rs"),
                        format!("projects/scanner\t{SCANNER_OLD}\tsrc/main.rs"),
                        format!("projects/scanner\t{SCANNER_OLD}\ttools/bench/main.rs"),
                        format!("tools/scan-old\t{SCAN_OLD}\tcli/src/main.rs"),
                    ];
                    assert_eq!(out.lines().collect::<Vec<_>>(), expected);
                });
                hits.json_only(|json| {
                    hits.keys(&[
                        "format", "command", "pattern", "pieces", "skipped", "warnings",
                    ]);
                    assert_eq!(json["pattern"], r"main\.rs");
                    assert_eq!(json["skipped"], serde_json::json!([]));
                    assert_eq!(json["warnings"], serde_json::json!([]));
                    let pieces = json["pieces"].as_array().expect("a list of pieces");
                    assert_eq!(pieces.len(), 2);

                    let scanner = &pieces[0];
                    assert_eq!(scanner["piece"], "projects/scanner");
                    assert_eq!(scanner["name_match"], false);
                    assert_eq!(scanner["live"], false);
                    assert_eq!(
                        scanner["versions"],
                        serde_json::json!([
                            {"stamp": SCANNER_NEW, "version": "2026-11-04T09:02:11Z"},
                            {"stamp": SCANNER_OLD, "version": "2026-09-03T12:15:00Z"},
                        ])
                    );
                    assert_eq!(
                        scanner["files"],
                        serde_json::json!([
                            {"path": "src/main.rs", "live": false, "versions": [SCANNER_NEW, SCANNER_OLD]},
                            {"path": "tools/bench/main.rs", "live": false, "versions": [SCANNER_OLD]},
                        ])
                    );

                    let old = &pieces[1];
                    assert_eq!(old["piece"], "tools/scan-old");
                    assert_eq!(old["name_match"], false);
                    assert_eq!(
                        old["files"],
                        serde_json::json!([
                            {"path": "cli/src/main.rs", "live": false, "versions": [SCAN_OLD]},
                        ])
                    );
                });

                // Grouped by piece, each path once with the versions that
                // hold it, as local dates (New Zealand is ahead of UTC).
                if world.mode() == Mode::Text {
                    let tty = world.run_tty(&["grep", r"main\.rs"]);
                    tty.expect_code(0);
                    let expected = "\
projects/scanner        2 versions, newest 2026-11-04
  src/main.rs           both versions
  tools/bench/main.rs   2026-09-04
tools/scan-old          1 version, 2025-02-12
  cli/src/main.rs       2025-02-12

3 files in 2 pieces \u{b7} antiq unearth <piece> [--version <version>]
";
                    assert_eq!(tty.plain(), expected);
                }

                // A pattern that matches the name of a piece gives the piece
                // one row per version and no file list.
                let named = world.run(&["grep", "scanner"]);
                named.expect_code(0);
                assert_eq!(named.stderr, "");
                named.text_only(|out| {
                    let expected = format!(
                        "projects/scanner\t{SCANNER_NEW}\t-\nprojects/scanner\t{SCANNER_OLD}\t-\n"
                    );
                    assert_eq!(out, expected);
                });
                named.json_only(|json| {
                    let pieces = json["pieces"].as_array().expect("a list of pieces");
                    assert_eq!(pieces.len(), 1);
                    assert_eq!(pieces[0]["piece"], "projects/scanner");
                    assert_eq!(pieces[0]["name_match"], true);
                    assert_eq!(
                        pieces[0]["versions"],
                        serde_json::json!([
                            {"stamp": SCANNER_NEW, "version": "2026-11-04T09:02:11Z", "files": 2},
                            {"stamp": SCANNER_OLD, "version": "2026-09-03T12:15:00Z", "files": 2},
                        ])
                    );
                    assert_eq!(pieces[0]["files"], serde_json::json!([]));
                });
                if world.mode() == Mode::Text {
                    let tty = world.run_tty(&["grep", "scanner"]);
                    tty.expect_code(0);
                    let expected = "\
projects/scanner   piece name matches, 2 versions, newest 2026-11-04, 2 files

1 piece name \u{b7} antiq unearth <piece> [--version <version>]
";
                    assert_eq!(tty.plain(), expected);
                }

                // No hit is no failure.
                let none = world.run(&["grep", "zzz"]);
                none.expect_code(0);
                assert_eq!(none.stderr, "no path matches zzz\n");
                none.text_only(|out| assert_eq!(out, ""));
                none.json_only(|json| assert_eq!(json["pieces"], serde_json::json!([])));
            });
        },
    );
}

#[test]
fn grep_still_finds_a_path_after_one_of_its_versions_is_unearthed() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .version("scanner", SCANNER_OLD, &[("src/main.rs", "a")])
                .version("scanner", SCANNER_NEW, &[("src/main.rs", "b")])
                .finish();
            Setup::new(collection)
        },
        |world| {
            let before = world.run(&["grep", r"main\.rs"]);
            before.expect_code(0);
            before.text_only(|out| {
                assert_eq!(
                    out,
                    format!(
                        "scanner\t{SCANNER_NEW}\tsrc/main.rs\nscanner\t{SCANNER_OLD}\tsrc/main.rs\n"
                    )
                );
            });

            let unearthed = world.run(&["unearth", "scanner", "--version", SCANNER_NEW]);
            unearthed.expect_code(0);

            // The folder is live and the older version is stored: the same
            // pattern finds the path in both, the live piece first.
            let after = world.run(&["grep", r"main\.rs"]);
            after.expect_code(0);
            assert_eq!(after.stderr, "");
            after.text_only(|out| {
                assert_eq!(
                    out,
                    format!("scanner\tlive\tsrc/main.rs\nscanner\t{SCANNER_OLD}\tsrc/main.rs\n")
                );
            });
            after.json_only(|json| {
                assert_eq!(json["pieces"][0]["live"], true);
                assert_eq!(
                    json["pieces"][0]["files"],
                    serde_json::json!([
                        {"path": "src/main.rs", "live": true, "versions": [SCANNER_OLD]},
                    ])
                );
            });

            if world.mode() == Mode::Text {
                let tty = world.run_tty(&["grep", r"main\.rs"]);
                tty.expect_code(0);
                assert_eq!(
                    tty.plain(),
                    "\
scanner         live, 1 version, 2026-09-04
  src/main.rs   live and 2026-09-04

1 file in 1 piece
"
                );
            }
        },
    );
}

#[test]
fn grep_finds_a_path_of_the_last_unearthed_version_as_a_live_piece_only() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .version("scanner", SCANNER_NEW, &[("src/main.rs", "b")])
                .finish();
            Setup::new(collection)
        },
        |world| {
            world.run(&["unearth", "scanner"]).expect_code(0);

            let found = world.run(&["grep", r"main\.rs"]);
            found.expect_code(0);
            found.text_only(|out| assert_eq!(out, "scanner\tlive\tsrc/main.rs\n"));

            let archived = world.run(&["grep", "--archived", r"main\.rs"]);
            archived.expect_code(0);
            assert_eq!(archived.stderr, "no archived path matches main\\.rs\n");
            archived.text_only(|out| assert_eq!(out, ""));

            if world.mode() == Mode::Text {
                let tty = world.run_tty(&["grep", r"main\.rs"]);
                assert_eq!(
                    tty.plain(),
                    "scanner         live\n  src/main.rs   live\n\n1 file in 1 piece\n"
                );
            }
        },
    );
}
