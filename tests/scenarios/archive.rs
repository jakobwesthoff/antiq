// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: `archive` moves folders into the store and leaves a version
//! that a person can check without antiq.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::outsider::{shasum_check, store_files, tar_extract, tar_members};
use crate::support::scenario::state::State;
use crate::support::scenario::world::{Setup, World, both_modes};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// A file name with spaces, a part that starts with a dash, a backslash,
/// brackets and a non-ASCII letter.
const AWKWARD: &str = "docs/my -notes\\draft (v2) \u{e9}.txt";

/// What a person would compare to say that two folders are the same: every
/// entry with its content, mode, time and link target, the groups of paths
/// that share an inode, and the mode of each directory.
struct Shape {
    state: State,
    hard_link_groups: Vec<Vec<String>>,
    directory_modes: BTreeMap<String, u32>,
}

fn shape(root: &Path) -> Shape {
    fn walk(
        base: &Path,
        dir: &Path,
        inodes: &mut BTreeMap<u64, Vec<String>>,
        modes: &mut BTreeMap<String, u32>,
    ) {
        for item in fs::read_dir(dir).expect("the directory lists") {
            let path = item.expect("an entry").path();
            let name = path
                .strip_prefix(base)
                .expect("below the root")
                .display()
                .to_string();
            let metadata = fs::symlink_metadata(&path).expect("the entry has metadata");
            if metadata.is_dir() {
                modes.insert(name, metadata.mode() & 0o7777);
                walk(base, &path, inodes, modes);
            } else if metadata.is_file() {
                inodes.entry(metadata.ino()).or_default().push(name);
            }
        }
    }
    let mut inodes = BTreeMap::new();
    let mut directory_modes = BTreeMap::new();
    walk(root, root, &mut inodes, &mut directory_modes);
    let mut hard_link_groups: Vec<Vec<String>> = inodes
        .into_values()
        .map(|mut paths| {
            paths.sort();
            paths
        })
        .collect();
    hard_link_groups.sort();
    Shape {
        state: State::raw(root),
        hard_link_groups,
        directory_modes,
    }
}

/// The text of a stored listing, decoded with the `zstd` crate.
fn listing_text(listing: &Path) -> String {
    let bytes = zstd::decode_all(fs::File::open(listing).expect("the listing opens"))
        .expect("the listing is zstd");
    String::from_utf8(bytes).expect("the listing is text")
}

/// The value of a header key of a listing, such as `files`.
fn header(listing: &str, key: &str) -> String {
    listing
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("# ")?.strip_prefix(key)?;
            // The key is padded with spaces; a longer key sharing the
            // start of this one has no space right after it.
            rest.starts_with(' ').then(|| rest.trim().to_string())
        })
        .unwrap_or_else(|| panic!("the listing has no header key {key}"))
}

/// The time of a version stamp as RFC 3339 in UTC.
fn stamp_time(stamp: &str) -> String {
    let (date, time) = stamp[..17].split_once('T').expect("a stamp has a T");
    format!("{date}T{}:{}:{}Z", &time[0..2], &time[2..4], &time[4..6])
}

fn expect_no_leftovers(world: &World) {
    let collection = world.collection();
    for area in ["trash", "doomed", "recovered", "tmp"] {
        let entries: Vec<_> = fs::read_dir(collection.path(&format!(".antiq/{area}")))
            .map(|items| items.map(|item| item.expect("an entry").path()).collect())
            .unwrap_or_default();
        assert!(entries.is_empty(), ".antiq/{area} holds {entries:?}");
    }
}

#[test]
fn archive_takes_every_due_piece_and_leaves_a_version_that_unpacks_to_the_original() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("projects/scanner", |piece| {
                    piece
                        .age_days(40)
                        .file("src/main.rs", "fn main() {}\n")
                        .hard_link("src/main.rs", "src/main-copy.rs")
                        .symlink("src/latest", "main.rs")
                        .symlink("src/dangling", "nowhere/at/all")
                        .dir("empty")
                        .file(AWKWARD, "draft\n")
                        .file("bin/run.sh", "#!/bin/sh\n")
                        .file("secret.txt", "private\n");
                })
                .piece("tools/scan-old", |piece| {
                    piece.age_days(35).file("scan.sh", "#!/bin/sh\n");
                })
                .piece("notes/daily", |piece| {
                    piece.age_days(5).file("today.md", "fresh\n");
                })
                .piece("old/pinned", |piece| {
                    piece.age_days(120).file("index.html", "<html>").pinned();
                })
                .finish();
            for (path, mode) in [
                ("projects/scanner/bin/run.sh", 0o755),
                ("projects/scanner/secret.txt", 0o600),
            ] {
                fs::set_permissions(collection.path(path), fs::Permissions::from_mode(mode))
                    .expect("the mode is set");
            }
            Setup::new(collection)
        },
        |world| {
            let collection = world.collection();
            let before = shape(&collection.path("projects/scanner"));
            let notes_before = State::raw(&collection.path("notes/daily"));
            let pinned_before = State::raw(&collection.path("old/pinned"));

            let archive = world.run(&["archive"]);
            archive.expect_code(0);
            archive.text_only(|out| {
                assert_eq!(
                    out,
                    "archived projects/scanner\n\
                     archived tools/scan-old\n\
                     Archived 2, skipped 0, refused 0.\n"
                );
            });
            assert_eq!(archive.stderr, "");

            let mut sizes = BTreeMap::new();
            for piece in ["projects/scanner", "tools/scan-old"] {
                let versions = store_files(collection, piece);
                assert_eq!(versions.len(), 1, "{piece} has one version");
                let version = &versions[0];
                let archive_file = version.archive.as_ref().expect("the archive is stored");
                let checksum = version
                    .checksum
                    .as_ref()
                    .expect("the checksum file is stored");
                let listing = version.listing.as_ref().expect("the listing is stored");
                sizes.insert(
                    piece,
                    (
                        version.stamp.clone(),
                        fs::metadata(archive_file)
                            .expect("the archive exists")
                            .len(),
                        listing.clone(),
                    ),
                );
                // The checksum file names the archive beside it, so the
                // check runs in the store folder.
                shasum_check(checksum.parent().expect("the file has a folder"), checksum);
            }

            archive.json_only(|json| {
                archive.keys(&[
                    "format", "command", "archived", "skipped", "refused", "repairs", "warnings",
                ]);
                let archived = json["archived"].as_array().expect("a list");
                assert_eq!(archived.len(), 2);
                for element in archived {
                    let piece = element["piece"].as_str().expect("a piece");
                    let (stamp, archive_bytes, listing) = &sizes[piece];
                    let object = element.as_object().expect("an object");
                    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
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
                    assert_eq!(element["stamp"], stamp.as_str());
                    assert_eq!(element["version"], stamp_time(stamp).as_str());
                    assert_eq!(element["archive_bytes"], *archive_bytes);
                    let text = listing_text(listing);
                    assert_eq!(
                        element["files"].to_string(),
                        header(&text, "files"),
                        "{piece}"
                    );
                    assert_eq!(
                        element["bytes"].to_string(),
                        header(&text, "bytes"),
                        "{piece}"
                    );
                }
                for key in ["skipped", "refused", "repairs", "warnings"] {
                    assert_eq!(json[key], Value::Array(vec![]), "{key}");
                }
            });

            // Both folders are gone; the others are as they were.
            assert!(!collection.path("projects/scanner").exists());
            assert!(!collection.path("tools/scan-old").exists());
            assert_eq!(State::raw(&collection.path("notes/daily")), notes_before);
            assert_eq!(State::raw(&collection.path("old/pinned")), pinned_before);
            expect_no_leftovers(world);

            // A person unpacks the archive with tar and gets the folder
            // back as it was.
            let scratch = tempfile::tempdir().expect("a scratch directory");
            let version = &store_files(collection, "projects/scanner")[0];
            let archive_file = version.archive.as_ref().expect("the archive is stored");
            tar_extract(archive_file, scratch.path());
            let after = shape(&scratch.path().join("scanner"));
            if let Some(diff) = before.state.diff(&after.state) {
                panic!("the unpacked folder differs from the original:\n{diff}");
            }
            assert_eq!(after.hard_link_groups, before.hard_link_groups);
            assert_eq!(after.directory_modes, before.directory_modes);
            let members = tar_members(archive_file);
            for member in [
                format!("scanner/{AWKWARD}"),
                "scanner/src/dangling".to_string(),
                "scanner/empty/".to_string(),
            ] {
                assert!(members.contains(&member), "{member} is not in {members:?}");
            }

            // Nothing is due any more.
            world.assert_unchanged(|| {
                let again = world.run(&["archive"]);
                again.expect_code(0);
                again.text_only(|out| assert_eq!(out, "Archived 0, skipped 0, refused 0.\n"));
                again.json_only(|json| assert_eq!(json["archived"], Value::Array(vec![])));
            });
        },
    );
}

#[test]
fn archive_takes_one_piece_now_and_refuses_a_pinned_one() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("projects/scanner", |piece| {
                    piece.age_days(3).file("src/main.rs", "fn main() {}\n");
                })
                .piece("tools/scan-old", |piece| {
                    piece.age_days(40).file("scan.sh", "#!/bin/sh\n");
                })
                .piece("old/pinned", |piece| {
                    piece.age_days(120).file("index.html", "<html>").pinned();
                })
                .finish();
            Setup::new(collection)
        },
        |world| {
            let collection = world.collection();

            // The piece is not due, and only the one named goes.
            let one = world.run(&["archive", "projects/scanner"]);
            one.expect_code(0);
            assert_eq!(one.stderr, "");
            one.text_only(|out| {
                assert_eq!(
                    out,
                    "archived projects/scanner\nArchived 1, skipped 0, refused 0.\n"
                );
            });
            one.json_only(|json| {
                one.keys(&[
                    "format", "command", "archived", "skipped", "refused", "repairs", "warnings",
                ]);
                let archived = json["archived"].as_array().expect("a list");
                assert_eq!(archived.len(), 1);
                assert_eq!(archived[0]["piece"], "projects/scanner");
            });
            assert_eq!(collection.versions("projects/scanner").len(), 1);
            assert!(!collection.path("projects/scanner").exists());
            assert!(collection.path("tools/scan-old/scan.sh").exists());
            assert!(collection.versions("tools/scan-old").is_empty());

            // A pinned piece is refused, and the run exits 1.
            world.assert_unchanged(|| {
                let pinned = world.run(&["archive", "old/pinned"]);
                pinned.expect_code(1);
                pinned.text_only(|out| {
                    assert_eq!(
                        out,
                        "refused old/pinned: pinned: old/pinned/.antiq-pin\nArchived 0, skipped 0, refused 1.\n"
                    );
                });
                pinned.json_only(|json| {
                    pinned.keys(&[
                        "format", "command", "archived", "skipped", "refused", "repairs",
                        "warnings",
                    ]);
                    assert_eq!(json["archived"], Value::Array(vec![]));
                    let refused = json["refused"].as_array().expect("a list");
                    assert_eq!(refused.len(), 1);
                    assert_eq!(refused[0]["piece"], "old/pinned");
                    let reasons = refused[0]["reasons"].as_array().expect("a list");
                    assert_eq!(reasons.len(), 1);
                    assert_eq!(reasons[0]["kind"], "pinned");
                    assert_eq!(reasons[0]["path"], "old/pinned/.antiq-pin");
                    assert_eq!(reasons[0]["text"], "pinned: old/pinned/.antiq-pin");
                });
            });
        },
    );
}

#[test]
fn a_dry_run_names_what_the_next_run_does_and_changes_nothing() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .exclude(&["media/camera-import/card"])
                .piece("experiments/rust-gpu", |piece| {
                    piece.age_days(33).file("src/main.rs", "fn main() {}\n");
                })
                .piece("old/client-site", |piece| {
                    piece.age_days(141).file("index.html", "<html>");
                })
                .piece("notes/daily", |piece| {
                    piece.age_days(5).file("today.md", "fresh\n");
                })
                .piece("projects/antiq", |piece| {
                    piece.age_days(120).file("src/lib.rs", "").pinned();
                })
                .piece("media/camera-import", |piece| {
                    piece
                        .age_days(52)
                        .file("one.jpg", "x")
                        .file("card/two.jpg", "y");
                })
                .finish();
            Setup::new(collection)
        },
        |world| {
            world.assert_unchanged(|| {
                let plan = world.run(&["archive", "-n"]);
                plan.expect_code(0);
                assert_eq!(plan.stderr, "");
                plan.text_only(|out| {
                    assert_eq!(
                        out,
                        "Would archive experiments/rust-gpu\n\
                         Would archive old/client-site\n\
                         Refused media/camera-import: exclude entry: media/camera-import/card\n"
                    );
                });
                plan.json_only(|json| {
                    plan.keys(&[
                        "format", "command", "dry_run", "actions", "refused", "warnings",
                    ]);
                    assert_eq!(
                        json["actions"],
                        serde_json::json!([
                            {"kind": "archive", "piece": "experiments/rust-gpu"},
                            {"kind": "archive", "piece": "old/client-site"},
                        ])
                    );
                    let refused = json["refused"].as_array().expect("a list");
                    assert_eq!(refused.len(), 1);
                    assert_eq!(refused[0]["piece"], "media/camera-import");
                    assert_eq!(refused[0]["reasons"][0]["kind"], "exclude_entry");
                    assert_eq!(json["warnings"], Value::Array(vec![]));
                });
            });

            // The real run archives what the plan named and refuses the same.
            let run = world.run(&["archive"]);
            run.expect_code(0);
            run.text_only(|out| {
                assert_eq!(
                    out,
                    "archived experiments/rust-gpu\n\
                     archived old/client-site\n\
                     refused media/camera-import: exclude entry: media/camera-import/card\n\
                     Archived 2, skipped 0, refused 1.\n"
                );
            });
            run.json_only(|json| {
                let pieces: Vec<&str> = json["archived"]
                    .as_array()
                    .expect("a list")
                    .iter()
                    .map(|element| element["piece"].as_str().expect("a piece"))
                    .collect();
                assert_eq!(pieces, ["experiments/rust-gpu", "old/client-site"]);
            });
            assert!(
                world
                    .collection()
                    .path("media/camera-import/one.jpg")
                    .exists()
            );
        },
    );
}

#[test]
fn a_dry_run_with_nothing_due_says_so() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("notes/daily", |piece| {
                    piece.age_days(5).file("today.md", "fresh\n");
                })
                .finish();
            Setup::new(collection)
        },
        |world| {
            world.assert_unchanged(|| {
                let plan = world.run(&["archive", "-n"]);
                plan.expect_code(0);
                plan.text_only(|out| assert_eq!(out, "Nothing to do.\n"));
                plan.json_only(|json| {
                    plan.keys(&[
                        "format", "command", "dry_run", "actions", "refused", "warnings",
                    ]);
                    assert_eq!(json["actions"], Value::Array(vec![]));
                    assert_eq!(json["refused"], Value::Array(vec![]));
                });
            });
        },
    );
}

#[test]
fn ignored_files_are_neither_packed_nor_counted_and_go_with_the_folder() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .ignore(&["node_modules/"])
                .timeless(&[".git/"])
                .piece("web/app", |piece| {
                    piece
                        .age_days(40)
                        .file("src/index.js", "console.log(1);\n")
                        .file_aged("node_modules/dep/index.js", "module.exports = 1;\n", 1)
                        .file_aged(".git/HEAD", "ref: refs/heads/main\n", 1);
                })
                .finish();
            Setup::new(collection)
        },
        |world| {
            let collection = world.collection();
            let run = world.run(&["archive"]);
            run.expect_code(0);
            run.text_only(|out| {
                assert_eq!(out, "archived web/app\nArchived 1, skipped 0, refused 0.\n");
            });

            let versions = store_files(collection, "web/app");
            assert_eq!(versions.len(), 1);
            let archive = versions[0].archive.as_ref().expect("the archive is stored");
            let listing =
                listing_text(versions[0].listing.as_ref().expect("the listing is stored"));

            // Timeless is packed, ignored is not.
            let members = tar_members(archive);
            assert!(
                members.contains(&"app/.git/HEAD".to_string()),
                "{members:?}"
            );
            assert!(
                members
                    .iter()
                    .all(|member| !member.contains("node_modules")),
                "{members:?}"
            );
            assert!(listing.contains(".git/HEAD"));
            assert!(!listing.contains("node_modules"));
            assert_eq!(header(&listing, "files"), "2");
            run.json_only(|json| {
                assert_eq!(json["archived"][0]["files"], 2);
            });

            // The folder goes with its ignored files.
            assert!(!collection.path("web/app").exists());
            expect_no_leftovers(world);
        },
    );
}
