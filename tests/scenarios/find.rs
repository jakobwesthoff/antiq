// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: finding a piece by a search word or by its path.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Mode, Outcome, Setup, World, both_modes};
use serde_json::json;
use std::collections::BTreeSet;

/// The piece of every row `find` printed, in the order printed, read from
/// the pipe lines or from the JSON, so one expectation serves both modes.
fn pieces_of(world: &World, outcome: &Outcome) -> Vec<String> {
    match world.mode() {
        Mode::Text => outcome
            .stdout
            .lines()
            .map(|line| line.split('\t').next().expect("a piece field").to_string())
            .collect(),
        Mode::Json => {
            let mut pieces = Vec::new();
            outcome.json_only(|json| {
                pieces = json["versions"]
                    .as_array()
                    .expect("a list of versions")
                    .iter()
                    .map(|row| row["piece"].as_str().expect("a piece").to_string())
                    .collect();
            });
            pieces
        }
    }
}

fn expect_no_match(world: &World, query: &str) {
    let outcome = world.run(&["find", query]);
    outcome.expect_code(0);
    assert!(
        pieces_of(world, &outcome).is_empty(),
        "{query}: {:?}",
        outcome.stdout
    );
    assert_eq!(
        outcome.stderr,
        format!("note: no piece matches `{query}`\n")
    );
}

#[test]
fn list_finds_pieces_by_search_word() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .version("projects/scanner", "2026-09-03T121500", &[("a.rs", "a")])
                .version("projects/scanner", "2026-11-04T090211", &[("a.rs", "a")])
                .version("tools/scan-old", "2025-02-11T184007", &[("run.sh", "b")])
                .version("notes/Ideas", "2026-01-05T101500", &[("one.md", "c")])
                .version("docs/readme", "2026-01-06T101500", &[("two.md", "d")])
                .finish();
            Setup::new(collection)
        },
        |world| {
            world.assert_unchanged(|| {
                // A word that is part of two piece names finds both, the
                // versions of each newest first.
                let both = world.run(&["find", "scan"]);
                both.expect_code(0);
                assert_eq!(
                    pieces_of(world, &both),
                    ["projects/scanner", "projects/scanner", "tools/scan-old"]
                );
                assert_eq!(both.stderr, "");

                let one = world.run(&["find", "scanner"]);
                one.expect_code(0);
                assert_eq!(
                    pieces_of(world, &one),
                    ["projects/scanner", "projects/scanner"]
                );

                // A word without a hit is no error.
                expect_no_match(world, "zzz");

                // Smart case: a lower case word matches any case, a capital
                // makes the match exact.
                for query in ["ideas", "Ideas"] {
                    let found = world.run(&["find", query]);
                    found.expect_code(0);
                    assert_eq!(pieces_of(world, &found), ["notes/Ideas"], "{query}");
                }
                expect_no_match(world, "IDEAS");
                expect_no_match(world, "Scanner");
            });
        },
    );
}

#[test]
fn a_path_names_its_piece_where_the_same_word_matches_several() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .piece("projects/scan", |piece| {
                    piece.age_days(1).file("a.txt", "a");
                })
                .piece("projects/scanner", |piece| {
                    piece.age_days(1).file("b.txt", "b");
                })
                .version("projects/scan", "2026-09-03T121500", &[("a.txt", "a")])
                .version("projects/scanner", "2026-09-03T121500", &[("b.txt", "b")])
                .finish();
            Setup::new(collection)
        },
        |world| {
            let expect_only_scan = |outcome: &Outcome| {
                outcome.expect_code(0);
                outcome.text_only(|out| {
                    assert!(out.contains(" of projects/scan: "), "{out:?}");
                    assert!(!out.contains("projects/scanner"), "{out:?}");
                });
                outcome.json_only(|json| {
                    let actions = json["actions"].as_array().expect("a list of actions");
                    assert_eq!(actions.len(), 1, "{actions:?}");
                    assert_eq!(actions[0]["kind"], "forget_version");
                    assert_eq!(actions[0]["piece"], "projects/scan");
                });
            };
            world.assert_unchanged(|| {
                // As a word, `projects/scan` matches both pieces. Written
                // as the path of a piece it names that piece.
                let word = world.run(&["find", "projects/scan"]);
                assert_eq!(
                    pieces_of(world, &word),
                    ["projects/scan", "projects/scanner"]
                );
                expect_only_scan(&world.run(&[
                    "forget",
                    "-n",
                    "projects/scan",
                    "--all-versions",
                    "--force",
                ]));

                // A path is resolved against the current directory when no
                // `-C` is given.
                let projects = world.collection().path("projects");
                expect_only_scan(&world.run_in(
                    &projects,
                    &["forget", "-n", "./scan", "--all-versions", "--force"],
                ));
            });
        },
    );
}

#[test]
fn unearth_refuses_a_word_that_matches_several_pieces_and_takes_a_path() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .version("projects/scan", "2026-09-03T121500", &[("a.txt", "a")])
                .version("projects/scanner", "2026-09-03T121500", &[("b.txt", "b")])
                .version("tools/scan-old", "2025-02-11T184007", &[("c.txt", "c")])
                .finish();
            Setup::new(collection)
        },
        |world| {
            world.assert_unchanged(|| {
                // The word is a part of three piece names, so no piece is
                // meant for sure and the candidates are listed.
                let several = world.run(&["unearth", "scan"]);
                several.expect_code(1);
                several.keys(&["format", "command", "error"]);
                several.text_only(|out| assert_eq!(out, ""));
                several.json_only(|json| {
                    let error = json["error"].as_object().expect("an error object");
                    let keys: Vec<&str> = error.keys().map(String::as_str).collect();
                    assert_eq!(keys, ["kind", "code", "message", "fix", "candidates"]);
                    assert_eq!(error["kind"], "ambiguous_reference");
                    assert_eq!(
                        error["candidates"],
                        json!([
                            {"path": "projects/scan"},
                            {"path": "projects/scanner"},
                            {"path": "tools/scan-old"}
                        ])
                    );
                });
                if world.mode() == Mode::Text {
                    // Each candidate is a line of its own, and no line, so no
                    // sentence of the message, appears twice.
                    for candidate in ["projects/scan", "projects/scanner", "tools/scan-old"] {
                        let line = format!("    candidate: {candidate}");
                        let count = several.stderr.lines().filter(|l| *l == line).count();
                        assert_eq!(count, 1, "{:?}", several.stderr);
                    }
                    let lines: Vec<&str> = several.stderr.lines().collect();
                    let distinct: BTreeSet<&str> = lines.iter().copied().collect();
                    assert_eq!(lines.len(), distinct.len(), "{:?}", several.stderr);
                }

                // A word that fits one piece only offers it, in a dry run too.
                let offered = world.run(&["unearth", "-n", "scanner"]);
                offered.expect_code(1);
                assert!(
                    !offered.stdout.contains("Would unearth"),
                    "{:?}",
                    offered.stdout
                );
                offered.json_only(|_| {
                    assert_eq!(offered.error_kind().as_deref(), Some("inexact_reference"));
                });

                // The path of the piece names it.
                let one = world.run(&["unearth", "-n", "projects/scanner"]);
                one.expect_code(0);
                one.text_only(|out| {
                    assert!(
                        out.starts_with("Would unearth projects/scanner (version "),
                        "{out:?}"
                    );
                });
                one.json_only(|json| {
                    assert_eq!(json["actions"][0]["piece"], "projects/scanner");
                });

                // A piece outside the collection is no piece.
                let outside = world.collection().root().with_file_name("elsewhere");
                let outside = outside.to_str().expect("the path is UTF-8");
                let refused = world.run(&["unearth", outside]);
                refused.expect_code(1);
                refused.keys(&["format", "command", "error"]);
                refused.json_only(|_| {
                    assert_eq!(refused.error_kind().as_deref(), Some("outside_collection"));
                });
            });

            // As the path of a piece, the same text names that piece alone.
            let path = world.run(&["unearth", "projects/scan"]);
            path.expect_code(0);
            assert!(world.collection().path("projects/scan/a.txt").is_file());
            assert!(!world.collection().path("projects/scanner").exists());
            assert_eq!(
                world.collection().versions("projects/scan"),
                Vec::<String>::new()
            );
            assert_eq!(world.collection().versions("projects/scanner").len(), 1);
            assert_eq!(world.collection().versions("tools/scan-old").len(), 1);
        },
    );
}
