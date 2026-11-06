// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: finding a piece by a search word or by its path.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Mode, Outcome, Setup, World, both_modes};

/// The piece of every row `list` printed, in the order printed, read from
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
    let outcome = world.run(&["list", query]);
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
                let both = world.run(&["list", "scan"]);
                both.expect_code(0);
                assert_eq!(
                    pieces_of(world, &both),
                    ["projects/scanner", "projects/scanner", "tools/scan-old"]
                );
                assert_eq!(both.stderr, "");

                let one = world.run(&["list", "scanner"]);
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
                    let found = world.run(&["list", query]);
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
                let word = world.run(&["list", "projects/scan"]);
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
