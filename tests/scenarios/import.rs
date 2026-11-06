// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: `import` packs a folder from outside the collection as a
//! version, and the folder stays.

use crate::support::scenario::collection::{Clock, Collection};
use crate::support::scenario::outsider::{listing_check, shasum_check, store_files, tar_extract};
use crate::support::scenario::state::State;
use crate::support::scenario::versions::Source;
use crate::support::scenario::world::{Mode, Outcome, Setup, World, both_modes};
use crate::support::tree::Tree;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// One path below a folder as a person who copies it by hand would expect
/// it to stay: type, mode, link target, content and, for files, the
/// modification time, plus the sets of paths that are one file.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Entries {
    items: BTreeMap<String, String>,
    hard_links: BTreeSet<BTreeSet<String>>,
}

impl Entries {
    pub(super) fn of(dir: &Path) -> Entries {
        fn walk(
            base: &Path,
            dir: &Path,
            items: &mut BTreeMap<String, String>,
            inodes: &mut BTreeMap<(u64, u64), BTreeSet<String>>,
        ) {
            for entry in fs::read_dir(dir).expect("the directory lists") {
                let path = entry.expect("an entry").path();
                let name = path
                    .strip_prefix(base)
                    .expect("below the base")
                    .display()
                    .to_string();
                let metadata = fs::symlink_metadata(&path).expect("the entry has metadata");
                let mode = metadata.mode() & 0o7777;
                let shown = if metadata.is_symlink() {
                    let target = fs::read_link(&path).expect("the link reads");
                    format!("link to {}", target.display())
                } else if metadata.is_dir() {
                    walk(base, &path, items, inodes);
                    format!("dir {mode:o}")
                } else {
                    if metadata.nlink() > 1 {
                        inodes
                            .entry((metadata.dev(), metadata.ino()))
                            .or_default()
                            .insert(name.clone());
                    }
                    let content = fs::read(&path).expect("the file reads");
                    format!(
                        "file {mode:o} time {} {:?}",
                        metadata.mtime(),
                        String::from_utf8_lossy(&content)
                    )
                };
                items.insert(name, shown);
            }
        }
        let mut items = BTreeMap::new();
        let mut inodes = BTreeMap::new();
        walk(dir, dir, &mut items, &mut inodes);
        Entries {
            items,
            hard_links: inodes.into_values().collect(),
        }
    }

    /// The same record without the paths at or below `prefix`.
    pub(super) fn without(mut self, prefix: &str) -> Entries {
        self.items
            .retain(|path, _| path != prefix && !path.starts_with(&format!("{prefix}/")));
        self.hard_links.retain(|group| {
            group
                .iter()
                .all(|path| path != prefix && !path.starts_with(&format!("{prefix}/")))
        });
        self
    }
}

/// The version stamp of a time in seconds.
fn stamp_of(seconds: i64) -> String {
    jiff::Timestamp::from_second(seconds)
        .expect("the time is representable")
        .strftime("%Y-%m-%dT%H%M%S")
        .to_string()
}

/// The RFC 3339 time in UTC that JSON shows for a version stamp.
pub(super) fn utc_of(stamp: &str) -> String {
    let (date, time) = stamp.split_once('T').expect("a stamp holds a T");
    format!("{date}T{}:{}:{}Z", &time[0..2], &time[2..4], &time[4..6])
}

/// The value of a header line (`# key  value`) of a decoded listing.
pub(super) fn header(listing: &str, key: &str) -> String {
    listing
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("# ")?;
            let (name, value) = rest.split_once(char::is_whitespace)?;
            (name == key).then(|| value.trim().to_string())
        })
        .unwrap_or_else(|| panic!("no `{key}` header in the listing:\n{listing}"))
}

pub(super) fn decoded_listing(listing: &Path) -> String {
    let bytes = zstd::stream::decode_all(fs::File::open(listing).expect("the listing opens"))
        .expect("the listing decodes");
    String::from_utf8(bytes).expect("the listing is UTF-8")
}

pub(super) fn as_str(path: &Path) -> &str {
    path.to_str().expect("the path is UTF-8")
}

pub(super) fn no_staging_left(world: &World) {
    for area in ["tmp", "trash", "doomed", "recovered"] {
        let found: Vec<_> = fs::read_dir(world.collection().path(&format!(".antiq/{area}")))
            .expect("the area exists")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert!(found.is_empty(), "left in .antiq/{area}: {found:?}");
    }
}

const README: &str = "scan the folders\n";
const NOTES: &str = "notes\n";
const MAIN: &str = "fn main() {}\n";
const RECENT: &str = "recent\n";

/// A collection with a pinned live folder where the import will go, and two
/// pieces that must stay as they are.
fn scanner_world(clock: &Clock) -> Setup {
    let collection = Collection::build(clock)
        .patterns(&["*/*"])
        .idle_days(30)
        .ignore(&["node_modules/"])
        .piece("projects/scanner", |piece| {
            piece.age_days(10).file("old.txt", "live\n").pinned();
        })
        .piece("projects/live", |piece| {
            piece.age_days(10).file("a.txt", "a\n");
        })
        .version(
            "projects/other",
            "2026-01-05T101500",
            &[("f.txt", "other\n")],
        )
        .finish();
    Setup::new(collection)
}

/// Every kind of entry an import has to pack, an ignored folder newer than
/// the rest, and a marker that is left out.
fn scanner_source(clock: &Clock) -> Source {
    Source::new(clock, |piece| {
        piece
            .age_days(50)
            .file("README", README)
            .file("my notes.txt", NOTES)
            .file("src/main.rs", MAIN)
            .hard_link("src/main.rs", "src/main-copy.rs")
            .symlink("src/latest", "main.rs")
            .dir("empty")
            .file_aged("recent.txt", RECENT, 20)
            .file_aged("node_modules/dep/index.js", "ignored\n", 1)
            .restored(5, None);
    })
}

#[test]
fn import_a_folder_as_a_version_and_the_folder_stays() {
    both_modes(scanner_world, |world| {
        let clock = *world.collection().clock();
        let source = scanner_source(&clock);
        let canonical = fs::canonicalize(source.path()).expect("the source exists");
        let canonical = as_str(&canonical).to_string();
        let stamp = stamp_of(clock.days_ago(20));
        let files = 4;
        let bytes = (README.len() + NOTES.len() + MAIN.len() + RECENT.len()) as u64;
        let before = State::raw(&source.path());
        let before_entries = Entries::of(&source.path());
        let live_before = State::raw(&world.collection().path("projects/scanner"));
        let bystander_before = State::raw(&world.collection().path("projects/live"));
        let other_before = world.collection().versions("projects/other");

        // The dry run names the version and what it leaves out, and changes
        // nothing.
        world.assert_unchanged(|| {
            let dry = world.run(&[
                "import",
                as_str(&source.path()),
                "--as",
                "projects/scanner",
                "-n",
            ]);
            dry.expect_code(0);
            dry.keys(&["format", "command", "dry_run", "actions", "warnings"]);
            assert_eq!(dry.stderr, "");
            dry.text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(
                    lines[0],
                    format!("Would import {canonical} as projects/scanner, version {stamp}")
                );
                let left_out: BTreeSet<&str> = lines[1..].iter().copied().collect();
                assert_eq!(
                    left_out,
                    BTreeSet::from([
                        "Would leave out node_modules/",
                        "Would leave out .antiq-restored"
                    ])
                );
                assert_eq!(lines.len(), 3, "{out:?}");
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
                    ["kind", "source", "piece", "stamp", "version", "left_out"]
                );
                assert_eq!(actions[0]["kind"], "import");
                assert_eq!(actions[0]["source"], canonical.as_str());
                assert_eq!(actions[0]["piece"], "projects/scanner");
                assert_eq!(actions[0]["stamp"], stamp.as_str());
                assert_eq!(actions[0]["version"], utc_of(&stamp));
                let left_out: BTreeSet<&str> = actions[0]["left_out"]
                    .as_array()
                    .expect("a list")
                    .iter()
                    .map(|path| path.as_str().expect("a path"))
                    .collect();
                assert_eq!(
                    left_out,
                    BTreeSet::from(["node_modules/", ".antiq-restored"])
                );
                assert_eq!(json["warnings"], json!([]));
            });
        });

        // The real run.
        let run = world.run(&["import", as_str(&source.path()), "--as", "projects/scanner"]);
        run.expect_code(0);
        assert_eq!(run.stderr, "");
        run.keys(&[
            "format", "command", "imported", "skipped", "repairs", "warnings",
        ]);
        run.text_only(|out| {
            assert_eq!(out, format!("imported projects/scanner: version {stamp}\n"));
        });

        // The store holds exactly the version, checked without antiq.
        assert_eq!(
            world.collection().versions("projects/scanner"),
            vec![stamp.as_str()]
        );
        let stored = store_files(world.collection(), "projects/scanner");
        assert_eq!(stored.len(), 1, "{stored:?}");
        let archive = stored[0].archive.clone().expect("the archive exists");
        let checksum = stored[0]
            .checksum
            .clone()
            .expect("the checksum file exists");
        let listing = stored[0].listing.clone().expect("the listing exists");
        run.json_only(|json| {
            assert_eq!(json["imported"]["piece"], "projects/scanner");
            assert_eq!(json["imported"]["stamp"], stamp.as_str());
            assert_eq!(json["imported"]["version"], utc_of(&stamp));
            assert_eq!(json["imported"]["files"], files);
            assert_eq!(json["imported"]["bytes"], bytes);
            let archive_bytes = fs::metadata(&archive).expect("the archive exists").len();
            assert_eq!(json["imported"]["archive_bytes"], archive_bytes);
            let keys: Vec<&str> = json["imported"]
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
                    "archive_bytes",
                    "files",
                    "bytes"
                ]
            );
            assert!(json["skipped"].is_null());
            assert_eq!(json["repairs"], json!([]));
            assert_eq!(json["warnings"], json!([]));
        });
        shasum_check(
            &world.collection().path(".antiq/pieces/projects/scanner"),
            &checksum,
        );
        let scratch = Tree::new();
        tar_extract(&archive, &scratch.path("unpacked"));
        let tops: Vec<_> = fs::read_dir(scratch.path("unpacked"))
            .expect("the unpacked folder lists")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(
            tops,
            ["source"],
            "one top-level directory, the folder's name"
        );
        let unpacked = scratch.path("unpacked/source");
        listing_check(&listing, &unpacked, &scratch.path("check"));
        let expected = Entries::of(&source.path())
            .without("node_modules")
            .without(".antiq-restored");
        assert_eq!(Entries::of(&unpacked), expected);
        assert_eq!(
            expected.hard_links,
            BTreeSet::from([BTreeSet::from([
                "src/main-copy.rs".to_string(),
                "src/main.rs".to_string()
            ])])
        );

        // The listing says when the import happened and what it holds.
        let text = decoded_listing(&listing);
        assert_eq!(header(&text, "piece"), "projects/scanner");
        assert_eq!(header(&text, "archived"), utc_of(&stamp));
        assert_eq!(header(&text, "files"), files.to_string());
        assert_eq!(header(&text, "bytes"), bytes.to_string());
        let imported: jiff::Timestamp = header(&text, "imported")
            .parse()
            .expect("the import time is RFC 3339");
        let age = jiff::Timestamp::now().as_second() - imported.as_second();
        assert!((0..120).contains(&age), "the import is {age} seconds old");
        assert!(!text.contains("node_modules"), "{text}");
        assert!(!text.contains(".antiq-restored"), "{text}");

        // The source is as it was, marker and ignored folder included.
        assert_eq!(before.diff(&State::raw(&source.path())), None);
        assert_eq!(Entries::of(&source.path()), before_entries);

        // The pinned live folder at the target kept its folder and pin; the
        // other pieces are as they were; nothing is left in the staging
        // areas.
        assert_eq!(
            live_before.diff(&State::raw(&world.collection().path("projects/scanner"))),
            None
        );
        assert!(
            world
                .collection()
                .path("projects/scanner/.antiq-pin")
                .exists()
        );
        assert_eq!(
            bystander_before.diff(&State::raw(&world.collection().path("projects/live"))),
            None
        );
        assert_eq!(world.collection().versions("projects/other"), other_before);
        no_staging_left(world);
    });
}

/// A source with one file, as old as `days`.
fn plain_source(clock: &Clock, days: i64) -> Source {
    Source::new(clock, |piece| {
        piece.age_days(days).file("a.txt", "alpha\n");
    })
}

fn names_world(clock: &Clock) -> Setup {
    let collection = Collection::build(clock)
        .patterns(&["*/*", "source"])
        .idle_days(30)
        .finish();
    Setup::new(collection)
}

#[test]
fn import_names_the_piece_and_takes_the_version_time_from_the_folder_or_the_flag() {
    both_modes(names_world, |world| {
        let clock = *world.collection().clock();
        let source = plain_source(&clock, 50);
        let path = as_str(&source.path()).to_string();
        let stamp_of_files = stamp_of(clock.days_ago(50));

        // Without `--as` the piece is named after the folder, and the
        // version time is the newest file's.
        let run = world.run(&["import", &path]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(out, format!("imported source: version {stamp_of_files}\n"));
        });
        run.json_only(|json| {
            assert_eq!(json["imported"]["piece"], "source");
            assert_eq!(json["imported"]["stamp"], stamp_of_files.as_str());
        });
        assert_eq!(world.collection().versions("source"), [stamp_of_files]);

        // `--time` as a UTC stamp, as a local time in the zone the harness
        // runs in (UTC+12 on that date), and the same stamp a second time,
        // which gets a counter.
        let renamed = |extra: &[&str]| {
            let mut args = vec!["import", path.as_str(), "--as", "projects/renamed"];
            args.extend_from_slice(extra);
            world.run(&args)
        };
        let run = renamed(&["--time", "2026-09-03T101500"]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(
                out,
                "imported projects/renamed: version 2026-09-03T101500\n"
            );
        });
        run.json_only(|json| {
            assert_eq!(json["imported"]["piece"], "projects/renamed");
            assert_eq!(json["imported"]["stamp"], "2026-09-03T101500");
            assert_eq!(json["imported"]["version"], "2026-09-03T10:15:00Z");
        });
        let run = renamed(&["--time", "2026-09-03 10:15:00"]);
        run.expect_code(0);
        run.json_only(|json| {
            assert_eq!(json["imported"]["stamp"], "2026-09-02T221500");
            assert_eq!(json["imported"]["version"], "2026-09-02T22:15:00Z");
        });
        let run = renamed(&["--time", "2026-09-03T101500"]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(
                out,
                "imported projects/renamed: version 2026-09-03T101500-2\n"
            );
        });
        run.json_only(|json| {
            assert_eq!(json["imported"]["stamp"], "2026-09-03T101500-2");
        });
        assert_eq!(
            world.collection().versions("projects/renamed"),
            [
                "2026-09-02T221500",
                "2026-09-03T101500",
                "2026-09-03T101500-2"
            ]
        );

        // The listing of the first one carries the given time.
        let stored = store_files(world.collection(), "projects/renamed");
        let listing = stored[1].listing.clone().expect("the listing exists");
        assert_eq!(
            header(&decoded_listing(&listing), "archived"),
            "2026-09-03T10:15:00Z"
        );
        no_staging_left(world);
    });
}

/// Runs a refused import and checks exit code, error kind and a word of the
/// message, in whichever mode the world runs.
fn refused(outcome: &Outcome, code: i32, kind: &str, needle: &str) {
    outcome.expect_code(code);
    outcome.keys(&["format", "command", "error"]);
    match outcome.mode {
        Mode::Text => assert!(
            outcome.stderr.contains(needle),
            "{needle:?} not in {:?}",
            outcome.stderr
        ),
        Mode::Json => {
            assert_eq!(outcome.error_kind().as_deref(), Some(kind));
            outcome.json_only(|json| {
                let message = json["error"]["message"].as_str().expect("a message");
                assert!(message.contains(needle), "{needle:?} not in {message:?}");
            });
        }
    }
}

#[test]
fn import_refuses_what_the_spec_names_and_changes_nothing() {
    both_modes(scanner_world, |world| {
        let clock = *world.collection().clock();
        let source = plain_source(&clock, 50);
        let path = as_str(&source.path()).to_string();

        world.assert_unchanged(|| {
            // A folder inside the collection.
            let inside = world.collection().path("projects/live");
            let run = world.run(&["import", as_str(&inside), "--as", "projects/elsewhere"]);
            refused(&run, 1, "other", "collection");

            // A pin at the top of the source.
            let pinned = Source::new(&clock, |piece| {
                piece.age_days(50).file("a.txt", "a\n").pinned();
            });
            let run = world.run(&["import", as_str(&pinned.path()), "--as", "projects/pinned"]);
            refused(&run, 1, "other", ".antiq-pin");

            // Nothing countable, also with a time.
            let empty = Source::new(&clock, |piece| {
                piece.age_days(50).file("node_modules/x.js", "x\n");
            });
            let run = world.run(&["import", as_str(&empty.path()), "--as", "projects/empty"]);
            refused(&run, 1, "other", "empty");
            let run = world.run(&[
                "import",
                as_str(&empty.path()),
                "--as",
                "projects/empty",
                "--time",
                "2026-09-03T101500",
            ]);
            refused(&run, 1, "other", "empty");

            // A path the patterns do not select.
            let run = world.run(&["import", &path, "--as", "loose"]);
            refused(&run, 1, "other", "loose");

            // A file with a time in the future is refused by name.
            let future = Source::new(&clock, |piece| {
                piece
                    .age_days(50)
                    .file("a.txt", "a\n")
                    .file_aged("later.txt", "l\n", -3);
            });
            let run = world.run(&["import", as_str(&future.path()), "--as", "projects/future"]);
            refused(&run, 1, "other", "later.txt");

            // A nested collection.
            let nested = Source::new(&clock, |piece| {
                piece
                    .age_days(50)
                    .file("a.txt", "a\n")
                    .file(".antiq/config.toml", "");
            });
            let run = world.run(&["import", as_str(&nested.path()), "--as", "projects/nested"]);
            refused(&run, 1, "other", "nested collection");

            // A time that is not one.
            let run = world.run(&[
                "import",
                &path,
                "--as",
                "projects/dated",
                "--time",
                "2026-09-03",
            ]);
            refused(&run, 2, "usage", "time");
        });

        // The file in the future does not matter once a time in the past
        // names the version.
        let future = Source::new(&clock, |piece| {
            piece
                .age_days(50)
                .file("a.txt", "a\n")
                .file_aged("later.txt", "l\n", -3);
        });
        let run = world.run(&[
            "import",
            as_str(&future.path()),
            "--as",
            "projects/future",
            "--time",
            "2026-09-03T101500",
        ]);
        run.expect_code(0);
        assert_eq!(
            world.collection().versions("projects/future"),
            ["2026-09-03T101500"]
        );
    });
}

#[cfg(test)]
mod entries {
    use super::*;
    use filetime::FileTime;

    /// A folder with a file, its hard link and a symlink.
    fn folder() -> Tree {
        let mut tree = Tree::new();
        tree.file_with("a/one.txt", "one", 0o644, 1_700_000_000);
        tree.hard_link("a/one.txt", "a/uno.txt");
        tree.symlink("a/latest", "one.txt");
        tree
    }

    fn differs(change: impl FnOnce(&Tree)) -> bool {
        let original = folder();
        let copy = folder();
        change(&copy);
        Entries::of(&original.path("a")) != Entries::of(&copy.path("a"))
    }

    #[test]
    fn an_identical_folder_is_equal() {
        assert!(!differs(|_| ()));
    }

    #[test]
    fn a_changed_modification_time_is_a_difference() {
        assert!(differs(|tree| {
            filetime::set_file_mtime(
                tree.path("a/one.txt"),
                FileTime::from_unix_time(1_700_000_001, 0),
            )
            .expect("the time is set");
        }));
    }

    #[test]
    fn a_hard_link_replaced_by_a_copy_is_a_difference() {
        assert!(differs(|tree| {
            let link = tree.path("a/uno.txt");
            fs::remove_file(&link).expect("the link goes");
            fs::write(&link, "one").expect("the copy is written");
            filetime::set_file_mtime(&link, FileTime::from_unix_time(1_700_000_000, 0))
                .expect("the time is set");
        }));
    }

    #[test]
    fn a_changed_link_target_is_a_difference() {
        assert!(differs(|tree| {
            let link = tree.path("a/latest");
            fs::remove_file(&link).expect("the link goes");
            std::os::unix::fs::symlink("uno.txt", &link).expect("the link is made");
        }));
    }
}
