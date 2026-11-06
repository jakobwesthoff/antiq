// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: `fork` extracts a working copy of a version, to the original
//! path or to a path of its own, and the version stays.

use super::import::{Entries, no_staging_left, utc_of};
use crate::support::scenario::collection::{Clock, Collection};
use crate::support::scenario::outsider::{listing_check, store_files, tar_extract};
use crate::support::scenario::state::State;
use crate::support::scenario::world::{Mode, Outcome, Setup, World, both_modes};
use crate::support::tree::Tree;
use serde_json::json;
use std::fs;

const PIECE: &str = "projects/scanner";
const V1: &str = "2026-02-10T080000";
const V2: &str = "2026-09-03T101500";

const V1_FILES: [(&str, &str); 3] = [
    ("README", "scanner, first version\n"),
    ("src/main.rs", "fn first() {}\n"),
    ("deep/er/file.txt", "deeper\n"),
];
const V2_FILES: [(&str, &str); 3] = [
    ("README", "scanner, second version\n"),
    ("src/main.rs", "fn second() {}\n"),
    ("deep/er/file.txt", "deeper\n"),
];

/// A piece that exists only in the store, as two versions with different
/// content, next to a stored and a live piece that must stay as they are.
fn scanner_world(clock: &Clock) -> Setup {
    let collection = Collection::build(clock)
        .patterns(&["*/*"])
        .idle_days(30)
        .piece("projects/live", |piece| {
            piece.age_days(10).file("a.txt", "a\n");
        })
        .version(PIECE, V1, &V1_FILES)
        .version(PIECE, V2, &V2_FILES)
        .version(
            "projects/other",
            "2026-01-05T101500",
            &[("f.txt", "other\n")],
        )
        .finish();
    Setup::new(collection)
}

fn size_of(files: &[(&str, &str); 3]) -> u64 {
    files.iter().map(|(_, content)| content.len() as u64).sum()
}

/// The version `stamp` unpacked by `tar`, without antiq, and checked against
/// its listing: what a fork has to give back. The archive's top directory is
/// named after the piece's folder.
fn unpacked(world: &World, scratch: &Tree, stamp: &str) -> Entries {
    let stored = store_files(world.collection(), PIECE);
    let version = stored
        .iter()
        .find(|version| version.stamp == stamp)
        .expect("the version is stored");
    let dest = scratch.path(stamp);
    tar_extract(version.archive.as_ref().expect("the archive exists"), &dest);
    let top = dest.join("scanner");
    listing_check(
        version.listing.as_ref().expect("the listing exists"),
        &top,
        &scratch.path(&format!("{stamp}-check")),
    );
    Entries::of(&top)
}

/// The forked folder without the marker, whose content is checked on its
/// own.
fn forked(world: &World, path: &str) -> Entries {
    Entries::of(&world.collection().path(path)).without(".antiq-restored")
}

/// The marker must hold the time of the fork and no version stamp: the
/// repairs would otherwise take the copy for an unearth.
fn assert_marker_without_stamp(world: &World, path: &str) {
    let text = fs::read_to_string(world.collection().path(&format!("{path}/.antiq-restored")))
        .expect("the marker exists");
    let time = text
        .strip_prefix("restored ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("a marker holds `restored <time>` and nothing else: {text:?}"));
    let time: jiff::Timestamp = time.parse().expect("the marker holds an RFC 3339 time");
    let age = jiff::Timestamp::now().as_second() - time.as_second();
    assert!((0..120).contains(&age), "the marker is {age} seconds old");
}

/// A refused fork: exit 1, kind `other`, and the message says the path is
/// taken, naming `--name` only where the original path was the default.
fn assert_taken(outcome: &Outcome, names_the_option: bool) {
    outcome.expect_code(1);
    outcome.keys(&["format", "command", "error"]);
    let message = match outcome.mode {
        Mode::Text => outcome.stderr.clone(),
        Mode::Json => {
            assert_eq!(outcome.error_kind().as_deref(), Some("other"));
            let mut message = String::new();
            outcome.json_only(|json| {
                message = json["error"]["message"]
                    .as_str()
                    .expect("a message")
                    .to_string();
            });
            message
        }
    };
    assert!(message.contains("it is taken"), "{message:?}");
    assert_eq!(
        message.contains("`--name`"),
        names_the_option,
        "{message:?}"
    );
}

/// What must hold after any fork: both versions and the other piece are as
/// they were, and nothing is left in the staging areas.
fn assert_version_stays(world: &World, store_before: &State) {
    assert_eq!(
        store_before.diff(&State::raw(&world.collection().path(".antiq/pieces"))),
        None
    );
    assert_eq!(world.collection().versions(PIECE), [V1, V2]);
    no_staging_left(world);
}

#[test]
fn fork_without_a_name_goes_to_the_original_path() {
    both_modes(scanner_world, |world| {
        let store_before = State::raw(&world.collection().path(".antiq/pieces"));
        let scratch = Tree::new();
        let newest = unpacked(world, &scratch, V2);
        let bytes = size_of(&V2_FILES);

        // The dry run names the newest version and the destination, and
        // changes nothing.
        world.assert_unchanged(|| {
            let dry = world.run(&["fork", "-n", PIECE]);
            dry.expect_code(0);
            dry.keys(&["format", "command", "dry_run", "actions", "warnings"]);
            assert_eq!(dry.stderr, "");
            dry.text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(
                    lines[0],
                    format!("Would fork {PIECE} (version {V2}) to {PIECE}")
                );
                assert_eq!(lines.len(), 6, "{out:?}");
                assert_eq!(lines[1], format!("  check the version {V2} is sound"));
                assert_eq!(lines[2], format!("  check the destination {PIECE} is free"));
                assert!(
                    lines[3].starts_with("  check there is room for "),
                    "{out:?}"
                );
                assert!(lines[4].starts_with("  check the member names"), "{out:?}");
                assert!(
                    lines[5].starts_with("  check the extracted tree"),
                    "{out:?}"
                );
            });
            dry.json_only(|json| {
                assert_eq!(json["dry_run"], true);
                let actions = json["actions"].as_array().expect("a list");
                assert_eq!(actions.len(), 1, "{actions:?}");
                let keys: Vec<&str> = actions[0]
                    .as_object()
                    .expect("an object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    keys,
                    [
                        "kind", "piece", "stamp", "version", "path", "bytes", "checks"
                    ]
                );
                assert_eq!(actions[0]["kind"], "fork");
                assert_eq!(actions[0]["piece"], PIECE);
                assert_eq!(actions[0]["stamp"], V2);
                assert_eq!(actions[0]["version"], utc_of(V2));
                assert_eq!(actions[0]["path"], PIECE);
                assert_eq!(actions[0]["bytes"], bytes);
                assert_eq!(
                    actions[0]["checks"],
                    json!([
                        "version",
                        "destination",
                        "free_space",
                        "member_names",
                        "extracted_tree"
                    ])
                );
                assert_eq!(json["warnings"], json!([]));
            });
        });

        let fork = world.run(&["fork", PIECE]);
        fork.expect_code(0);
        assert_eq!(fork.stderr, "");
        fork.keys(&[
            "format", "command", "forked", "piece", "stamp", "version", "path", "bytes", "repairs",
            "warnings",
        ]);
        fork.text_only(|out| {
            assert_eq!(out, format!("Forked {PIECE} (version {V2}) to {PIECE}\n"));
        });
        fork.json_only(|json| {
            assert_eq!(json["forked"], true);
            assert_eq!(json["piece"], PIECE);
            assert_eq!(json["stamp"], V2);
            assert_eq!(json["version"], utc_of(V2));
            assert_eq!(json["path"], PIECE);
            assert_eq!(json["bytes"], bytes);
            assert_eq!(json["repairs"], json!([]));
            assert_eq!(json["warnings"], json!([]));
        });

        // The copy is the newest version as `tar` unpacks it, and the
        // listing's checksums hold for it.
        assert_eq!(forked(world, PIECE), newest);
        assert_marker_without_stamp(world, PIECE);
        assert_version_stays(world, &store_before);

        // The path is taken now, and the refusal names the way out. Neither
        // the folder nor the store changes.
        world.assert_unchanged(|| {
            assert_taken(&world.run(&["fork", PIECE]), true);
        });
    });
}

#[test]
fn fork_with_a_name_leaves_the_original_path_free() {
    both_modes(scanner_world, |world| {
        let store_before = State::raw(&world.collection().path(".antiq/pieces"));
        let scratch = Tree::new();
        let first = unpacked(world, &scratch, V1);
        let newest = unpacked(world, &scratch, V2);
        let copy = "copies/scanner-v1";

        // The folders above the name do not exist yet.
        assert!(!world.collection().path("copies").exists());
        let fork = world.run(&["fork", PIECE, "--version", V1, "--name", copy]);
        fork.expect_code(0);
        assert_eq!(fork.stderr, "");
        fork.text_only(|out| {
            assert_eq!(out, format!("Forked {PIECE} (version {V1}) to {copy}\n"));
        });
        fork.json_only(|json| {
            assert_eq!(json["forked"], true);
            assert_eq!(json["piece"], PIECE);
            assert_eq!(json["stamp"], V1);
            assert_eq!(json["version"], utc_of(V1));
            assert_eq!(json["path"], copy);
            assert_eq!(json["bytes"], size_of(&V1_FILES));
            assert_eq!(json["repairs"], json!([]));
            assert_eq!(json["warnings"], json!([]));
        });
        assert_eq!(forked(world, copy), first);
        assert_marker_without_stamp(world, copy);

        // The original path stays free, and the version stays.
        assert!(!world.collection().path(PIECE).exists());
        assert_version_stays(world, &store_before);

        // A taken name is refused without a hint to `--name`, which was
        // given already.
        world.assert_unchanged(|| {
            let again = world.run(&["fork", PIECE, "--version", V1, "--name", copy]);
            assert_taken(&again, false);
        });

        // Without `--version` the newest version is forked.
        let other = "copies/scanner-latest";
        let latest = world.run(&["fork", PIECE, "--name", other]);
        latest.expect_code(0);
        latest.json_only(|json| {
            assert_eq!(json["stamp"], V2);
            assert_eq!(json["path"], other);
        });
        assert_eq!(forked(world, other), newest);
        assert_eq!(
            fs::read_to_string(world.collection().path(&format!("{other}/README")))
                .expect("the copy has a README"),
            V2_FILES[0].1
        );
        assert_version_stays(world, &store_before);
    });
}
