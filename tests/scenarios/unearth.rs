// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: bringing a version back with `unearth`, by name and by a
//! given version, and archiving the folder again.

use crate::support::scenario::collection::{Clock, Collection};
use crate::support::scenario::outsider::{shasum_check, store_files, tar_members};
use crate::support::scenario::world::{Mode, Setup, World, both_modes};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// What one path below a folder is, as far as a person who copies the
/// folder by hand would expect it to stay.
#[derive(Debug, PartialEq, Eq)]
enum Item {
    Dir {
        mode: u32,
    },
    Link {
        target: String,
    },
    File {
        mode: u32,
        mtime: i64,
        content: Vec<u8>,
    },
}

/// A folder as `unearth` has to give it back: every path with its type,
/// mode, content or link target and, for files, the modification time,
/// plus the sets of paths that are one file under several names.
///
/// Times of directories are left out. The harness resets them after every
/// command, so a run cannot show whether they were restored.
struct Likeness {
    items: BTreeMap<String, Item>,
    hard_links: BTreeSet<BTreeSet<String>>,
}

impl Likeness {
    fn of(dir: &Path) -> Likeness {
        fn walk(
            base: &Path,
            dir: &Path,
            items: &mut BTreeMap<String, Item>,
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
                if metadata.is_symlink() {
                    let target = fs::read_link(&path).expect("the link reads");
                    let target = target.display().to_string();
                    items.insert(name, Item::Link { target });
                } else if metadata.is_dir() {
                    items.insert(name, Item::Dir { mode });
                    walk(base, &path, items, inodes);
                } else {
                    if metadata.nlink() > 1 {
                        inodes
                            .entry((metadata.dev(), metadata.ino()))
                            .or_default()
                            .insert(name.clone());
                    }
                    let content = fs::read(&path).expect("the file reads");
                    items.insert(
                        name,
                        Item::File {
                            mode,
                            mtime: metadata.mtime(),
                            content,
                        },
                    );
                }
            }
        }
        let mut items = BTreeMap::new();
        let mut inodes = BTreeMap::new();
        walk(dir, dir, &mut items, &mut inodes);
        Likeness {
            items,
            hard_links: inodes.into_values().collect(),
        }
    }

    /// The unpacked size of the folder: the content of every regular file,
    /// a file with several names counted once.
    fn unpacked_bytes(&self) -> u64 {
        let size = |path: &String| match self.items.get(path) {
            Some(Item::File { content, .. }) => content.len() as u64,
            _ => 0,
        };
        let total: u64 = self.items.keys().map(size).sum();
        let repeated: u64 = self
            .hard_links
            .iter()
            .map(|group| {
                let first = group.iter().next().expect("a group has a path");
                size(first) * (group.len() as u64 - 1)
            })
            .sum();
        total - repeated
    }

    /// The number of regular files in the folder, a file with several names
    /// counted once, as the `files` of a listing counts them.
    fn stored_files(&self) -> u64 {
        let files = self
            .items
            .values()
            .filter(|item| matches!(item, Item::File { .. }))
            .count() as u64;
        let repeated: u64 = self
            .hard_links
            .iter()
            .map(|group| group.len() as u64 - 1)
            .sum();
        files - repeated
    }

    /// The same record without the path, for the files a comparison does
    /// not cover, such as the marker `unearth` adds.
    fn without(mut self, path: &str) -> Likeness {
        self.items.remove(path);
        self
    }

    /// One line per difference between `self`, the original, and `other`.
    fn differences(&self, other: &Likeness) -> Vec<String> {
        let mut found = Vec::new();
        let paths: BTreeSet<&String> = self.items.keys().chain(other.items.keys()).collect();
        for path in paths {
            match (self.items.get(path), other.items.get(path)) {
                (Some(original), Some(copy)) if original != copy => {
                    found.push(format!("{path}: {original:?} became {copy:?}"));
                }
                (Some(_), None) => found.push(format!("{path}: missing")),
                (None, Some(_)) => found.push(format!("{path}: not in the original")),
                _ => {}
            }
        }
        // Names that were one file must still be one file, and no file may
        // have become linked that was not.
        for group in self.hard_links.symmetric_difference(&other.hard_links) {
            let side = if self.hard_links.contains(group) {
                "the original"
            } else {
                "the copy"
            };
            found.push(format!("hard link group only in {side}: {group:?}"));
        }
        found
    }

    fn assert_like(&self, other: &Likeness) {
        let differences = self.differences(other);
        assert!(
            differences.is_empty(),
            "the folder differs from the original:\n{}",
            differences.join("\n")
        );
    }
}

#[cfg(test)]
mod likeness {
    use super::*;
    use crate::support::tree::Tree;
    use filetime::FileTime;

    /// A folder with a file, its hard link, a symlink and a directory.
    fn folder() -> Tree {
        let mut tree = Tree::new();
        tree.file_with("a/one.txt", "one", 0o644, 1_700_000_000);
        tree.hard_link("a/one.txt", "a/uno.txt");
        tree.symlink("a/latest", "one.txt");
        tree.dir("a/empty");
        tree
    }

    fn compare(change: impl FnOnce(&Tree)) -> Vec<String> {
        let original = folder();
        let copy = folder();
        change(&copy);
        Likeness::of(&original.path("a")).differences(&Likeness::of(&copy.path("a")))
    }

    #[test]
    fn an_identical_folder_has_no_difference() {
        assert_eq!(compare(|_| ()), Vec::<String>::new());
    }

    #[test]
    fn a_changed_mode_is_a_difference() {
        let found = compare(|tree| {
            fs::set_permissions(tree.path("a/one.txt"), fs::Permissions::from_mode(0o600))
                .expect("the mode is set");
        });
        // The file has two names, and both show the new mode.
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_changed_modification_time_is_a_difference() {
        let found = compare(|tree| {
            filetime::set_file_mtime(
                tree.path("a/one.txt"),
                FileTime::from_unix_time(1_700_000_001, 0),
            )
            .expect("the time is set");
        });
        assert!(!found.is_empty());
    }

    #[test]
    fn a_hard_link_replaced_by_a_copy_is_a_difference() {
        let found = compare(|tree| {
            let link = tree.path("a/uno.txt");
            fs::remove_file(&link).expect("the link goes");
            fs::copy(tree.path("a/one.txt"), &link).expect("the copy is made");
            filetime::set_file_mtime(&link, FileTime::from_unix_time(1_700_000_000, 0))
                .expect("the time is set");
        });
        assert!(
            found.iter().any(|line| line.contains("hard link")),
            "{found:?}"
        );
    }

    #[test]
    fn a_changed_link_target_is_a_difference() {
        let found = compare(|tree| {
            let link = tree.path("a/latest");
            fs::remove_file(&link).expect("the link goes");
            std::os::unix::fs::symlink("uno.txt", &link).expect("the link is made");
        });
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn an_extra_and_a_missing_path_are_differences() {
        let extra = compare(|tree| {
            fs::write(tree.path("a/extra"), "x").expect("the file is written");
        });
        assert_eq!(extra.len(), 1, "{extra:?}");
        let missing = compare(|tree| {
            fs::remove_dir(tree.path("a/empty")).expect("the directory goes");
        });
        assert_eq!(missing.len(), 1, "{missing:?}");
    }
}

const SCANNER: &str = "projects/scanner";
const SCAN_OLD_STAMP: &str = "2025-02-11T184007";

/// A forty day old piece with every kind of entry an unearth has to give
/// back, next to a stored piece that must stay as it is.
fn scanner_world(clock: &Clock) -> Setup {
    let collection = Collection::build(clock)
        .patterns(&["*/*"])
        .idle_days(30)
        .piece(SCANNER, |piece| {
            piece
                .age_days(40)
                .file("README", "scan the folders\n")
                .file("my notes.txt", "notes\n")
                .file("src/main.rs", "fn main() {}\n")
                .hard_link("src/main.rs", "src/main-copy.rs")
                .symlink("src/latest", "main.rs")
                .symlink("src/gone", "missing.rs")
                .symlink("docs/up", "../README")
                .file("bin/run.sh", "#!/bin/sh\necho scan\n")
                .file("secret.key", "key\n")
                .file("ro.txt", "read only\n")
                .file_aged("old.txt", "old\n", 200)
                .dir("empty")
                .file("private/inside.txt", "inside\n");
        })
        .version("tools/scan-old", SCAN_OLD_STAMP, &[("run.sh", "echo\n")])
        .finish();
    for (path, mode) in [
        ("bin/run.sh", 0o755),
        ("secret.key", 0o600),
        ("ro.txt", 0o444),
        ("private", 0o700),
    ] {
        fs::set_permissions(
            collection.path(&format!("{SCANNER}/{path}")),
            fs::Permissions::from_mode(mode),
        )
        .expect("the mode is set");
    }
    collection.settle();
    Setup::new(collection)
}

fn folder(world: &World, piece: &str) -> Likeness {
    Likeness::of(&world.collection().path(piece))
}

/// The folder that `unearth` gave back, without the marker it adds, whose
/// content is checked on its own.
fn unearthed(world: &World, piece: &str) -> Likeness {
    folder(world, piece).without(".antiq-restored")
}

fn marker_time(world: &World, piece: &str) -> jiff::Timestamp {
    let text = fs::read_to_string(world.collection().path(&format!("{piece}/.antiq-restored")))
        .expect("the marker exists");
    let time = text
        .strip_prefix("restored ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("a marker holds `restored <time>` and nothing else: {text:?}"));
    time.parse().expect("the marker holds an RFC 3339 time")
}

/// The version stamp as the RFC 3339 time JSON shows.
fn utc_of(stamp: &str) -> String {
    let (date, time) = stamp.split_once('T').expect("a stamp holds a T");
    format!("{date}T{}:{}:{}Z", &time[0..2], &time[2..4], &time[4..6])
}

fn no_staging_left(world: &World) {
    let collection = world.collection();
    let tmp: Vec<_> = fs::read_dir(collection.path(".antiq/tmp"))
        .expect("the staging area exists")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert!(tmp.is_empty(), "left in .antiq/tmp: {tmp:?}");
    for area in ["trash", "doomed", "recovered"] {
        let found = fs::read_dir(collection.path(&format!(".antiq/{area}")))
            .expect("the area exists")
            .count();
        assert_eq!(found, 0, "{found} entries left in .antiq/{area}");
    }
}

#[test]
fn unearth_by_name_gives_the_folder_back_as_it_was() {
    both_modes(scanner_world, |world| {
        let before = folder(world, SCANNER);

        let archived = world.run(&["archive", SCANNER]);
        archived.expect_code(0);
        assert!(!world.collection().path(SCANNER).exists());
        let stamps = world.collection().versions(SCANNER);
        assert_eq!(stamps.len(), 1, "{stamps:?}");
        let stamp = stamps[0].clone();
        let kept = world.collection().versions("tools/scan-old");

        world.assert_unchanged(|| {
            let dry = world.run(&["unearth", "-n", "scanner"]);
            dry.expect_code(0);
            dry.keys(&["format", "command", "dry_run", "actions", "warnings"]);
            assert_eq!(dry.stderr, "");
            dry.text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(
                    lines[0],
                    format!("Would unearth {SCANNER} (version {stamp})")
                );
                assert_eq!(lines.len(), 6, "{out:?}");
                assert!(lines[1..].iter().all(|line| line.starts_with("  check ")));
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
                assert_eq!(actions[0]["kind"], "unearth");
                assert_eq!(actions[0]["piece"], SCANNER);
                assert_eq!(actions[0]["path"], SCANNER);
                assert_eq!(actions[0]["stamp"], stamp.as_str());
                assert_eq!(actions[0]["version"], utc_of(&stamp));
                assert_eq!(actions[0]["bytes"], before.unpacked_bytes());
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

        let unearth = world.run(&["unearth", "scanner"]);
        unearth.expect_code(0);
        assert_eq!(unearth.stderr, "");
        unearth.keys(&[
            "format",
            "command",
            "unearthed",
            "piece",
            "stamp",
            "version",
            "path",
            "bytes",
            "repairs",
            "warnings",
        ]);
        unearth.text_only(|out| {
            assert_eq!(out, format!("Unearthed {SCANNER} (version {stamp})\n"));
        });
        unearth.json_only(|json| {
            assert_eq!(json["unearthed"], true);
            assert_eq!(json["piece"], SCANNER);
            assert_eq!(json["stamp"], stamp.as_str());
            assert_eq!(json["path"], SCANNER);
            assert_eq!(json["version"], utc_of(&stamp));
            assert_eq!(json["bytes"], before.unpacked_bytes());
            assert_eq!(json["repairs"], json!([]));
            assert_eq!(json["warnings"], json!([]));
        });

        // Content, modes, file times, link targets and the hard link are
        // the original's.
        before.assert_like(&unearthed(world, SCANNER));

        // The marker holds the time of the unearth and no version stamp.
        let marker = marker_time(world, SCANNER);
        let age = jiff::Timestamp::now().as_second() - marker.as_second();
        assert!((0..60).contains(&age), "the marker is {age} seconds old");

        // The version is gone, with the store folders it emptied; the other
        // piece is as it was.
        assert_eq!(world.collection().versions(SCANNER), Vec::<String>::new());
        assert!(!world.collection().path(".antiq/pieces/projects").exists());
        assert_eq!(world.collection().versions("tools/scan-old"), kept);
        no_staging_left(world);

        // The piece is not in the store any more.
        world.assert_unchanged(|| {
            let again = world.run(&["unearth", "scanner"]);
            again.expect_code(1);
            again.keys(&["format", "command", "error"]);
            again.json_only(|_| {
                assert_eq!(again.error_kind().as_deref(), Some("unknown_reference"));
            });
            if world.mode() == Mode::Text {
                assert!(again.stderr.contains("scanner"), "{:?}", again.stderr);
            }
        });
    });
}

const JOURNAL: &str = "notes/journal";
const V0: &str = "2026-02-10T080000";
const VA: &str = "2026-04-04T133000";
const VB: &str = "2026-04-04T143000";
const VC: &str = "2026-09-03T121500";
const VD: &str = "2026-11-04T090211";

/// Five versions of one piece that is not in the collection as a folder.
/// Each holds its name in `entry.txt`, so a scenario sees which one came
/// back. Summer time in New Zealand ends at 03:00 on 2026-04-05, so the
/// local time 02:30 happens twice: at 13:30Z (+13:00) for `VA` and at
/// 14:30Z (+12:00) for `VB`. `VC` is stamped on 2026-09-03 in UTC and lies
/// on 2026-09-04 in local time.
fn journal_world(clock: &Clock) -> Setup {
    let mut builder = Collection::build(clock);
    builder.patterns(&["*/*"]);
    for (stamp, name) in [(V0, "v0"), (VA, "va"), (VB, "vb"), (VC, "vc"), (VD, "vd")] {
        builder.version(
            JOURNAL,
            stamp,
            &[("entry.txt", name), ("deep/er/file.txt", "deeper\n")],
        );
    }
    Setup::new(builder.finish())
}

/// The text of the answer to a refused command: stderr in text mode, the
/// message of the error object in JSON mode.
fn refusal(world: &World, outcome: &crate::support::scenario::world::Outcome) -> String {
    match world.mode() {
        Mode::Text => outcome.stderr.clone(),
        Mode::Json => {
            let mut message = String::new();
            outcome.json_only(|json| {
                message = json["error"]["message"]
                    .as_str()
                    .expect("an error message")
                    .to_string();
            });
            message
        }
    }
}

/// `unearth -n` of the given version: the stamp it names.
fn dry_run_names(world: &World, version: &str) -> String {
    let mut named = String::new();
    world.assert_unchanged(|| {
        let dry = world.run(&["unearth", "-n", JOURNAL, "--version", version]);
        dry.expect_code(0);
        dry.text_only(|out| {
            let first = out.lines().next().expect("a first line");
            let stamp = first
                .strip_prefix(&format!("Would unearth {JOURNAL} (version "))
                .and_then(|rest| rest.strip_suffix(')'))
                .unwrap_or_else(|| panic!("unexpected first line {first:?}"));
            named = stamp.to_string();
        });
        dry.json_only(|json| {
            named = json["actions"][0]["stamp"]
                .as_str()
                .expect("a stamp")
                .to_string();
        });
    });
    named
}

/// Unearths the version `version` of the journal, checks that `stamp` came
/// back and left the store alone, and removes the folder again so that the
/// next unearth finds its destination free.
fn take(world: &World, version: Option<&str>, stamp: &str, entry: &str) {
    let mut args = vec!["unearth", JOURNAL];
    if let Some(version) = version {
        args.extend(["--version", version]);
    }
    let before = world.collection().versions(JOURNAL);
    let outcome = world.run(&args);
    outcome.expect_code(0);
    assert_eq!(outcome.stderr, "", "{args:?}");
    outcome.keys(&[
        "format",
        "command",
        "unearthed",
        "piece",
        "stamp",
        "version",
        "path",
        "bytes",
        "repairs",
        "warnings",
    ]);
    outcome.text_only(|out| {
        assert_eq!(out, format!("Unearthed {JOURNAL} (version {stamp})\n"));
    });
    outcome.json_only(|json| {
        assert_eq!(json["unearthed"], true);
        assert_eq!(json["stamp"], stamp);
        assert_eq!(json["path"], JOURNAL);
        assert_eq!(json["version"], utc_of(stamp));
    });
    let folder = world.collection().path(JOURNAL);
    assert_eq!(
        fs::read_to_string(folder.join("entry.txt")).expect("the entry exists"),
        entry,
        "{args:?}"
    );
    assert!(folder.join("deep/er/file.txt").is_file());
    let after = world.collection().versions(JOURNAL);
    let expected: Vec<String> = before.into_iter().filter(|each| each != stamp).collect();
    assert_eq!(after, expected, "only {stamp} leaves the store");
    fs::remove_dir_all(&folder).expect("the folder goes");
}

#[test]
fn unearth_a_given_version_by_local_text_or_stamp_across_the_repeated_hour() {
    both_modes(journal_world, |world| {
        // Refusals first, while every version is still in the store.
        world.assert_unchanged(|| {
            let all = [V0, VA, VB, VC, VD];
            // Both repeated-hour versions render as 02:30:00 on 2026-04-05
            // without an offset, and as that date alone.
            for input in ["2026-04-05 02:30:00", "2026-04-05"] {
                let several = world.run(&["unearth", JOURNAL, "--version", input]);
                several.expect_code(1);
                several.keys(&["format", "command", "error"]);
                several.json_only(|_| assert_eq!(several.error_kind().as_deref(), Some("other")));
                let message = refusal(world, &several);
                for stamp in [VA, VB] {
                    assert!(message.contains(stamp), "{input}: {message:?}");
                }
                for stamp in [V0, VC, VD] {
                    assert!(!message.contains(stamp), "{input}: {message:?}");
                }
            }
            // The spelling decides how an input is matched, and the two are
            // never mixed: the UTC date of `VC` is no local date, and the
            // UTC time of `VA` is no local time.
            for input in ["2026-09-03", "2026-04-04 13:30:00", "1999"] {
                let none = world.run(&["unearth", JOURNAL, "--version", input]);
                none.expect_code(1);
                none.keys(&["format", "command", "error"]);
                let message = refusal(world, &none);
                for stamp in all {
                    assert!(message.contains(stamp), "{input}: {message:?}");
                }
            }
        });

        // Offsets tell the repeated hour apart, as a terminal shows them.
        assert_eq!(dry_run_names(world, "2026-04-05 02:30:00 +12:00"), VB);
        assert_eq!(dry_run_names(world, "2026-04-05 02:30:00 +13:00"), VA);
        assert_eq!(dry_run_names(world, VA), VA);
        world.assert_unchanged(|| {
            for (version, shown) in [
                ("2026-04-05 02:30:00 +12:00", "2026-04-05 02:30:00 +12:00"),
                ("2026-04-05 02:30:00 +13:00", "2026-04-05 02:30:00 +13:00"),
                ("2026-09-04", "2026-09-04 00:15:00"),
            ] {
                let tty = world.run_tty(&["unearth", "-n", JOURNAL, "--version", version]);
                tty.expect_code(0);
                let plain = tty.plain();
                assert_eq!(
                    plain.lines().next(),
                    Some(format!("Would unearth {JOURNAL} (version {shown})").as_str()),
                    "{version}"
                );
            }
        });

        // Without `--version` the newest version comes back.
        take(world, None, VD, "vd");
        // The offset names one of the two while both are in the store.
        take(world, Some("2026-04-05 02:30:00 +12:00"), VB, "vb");
        // Once `VB` is gone `VA` is alone in its hour and shows no offset;
        // its stamp names it.
        take(world, Some(VA), VA, "va");
        // A UTC prefix.
        take(world, Some("2026-02-10T08"), V0, "v0");
        // A local date, which is the day after the UTC date of the stamp.
        take(world, Some("2026-09-04"), VC, "vc");

        assert_eq!(world.collection().versions(JOURNAL), Vec::<String>::new());
        assert!(!world.collection().path(".antiq/pieces/notes").exists());
        no_staging_left(world);
    });
}

#[test]
fn an_unearthed_folder_that_is_archived_again_becomes_a_new_version() {
    both_modes(scanner_world, |world| {
        let original = folder(world, SCANNER);
        world.run(&["archive", SCANNER]).expect_code(0);
        world.run(&["unearth", "scanner"]).expect_code(0);
        original.assert_like(&unearthed(world, SCANNER));
        assert_eq!(world.collection().versions(SCANNER), Vec::<String>::new());

        // The idle clock starts again at the unearth: every file is forty
        // days old, and the piece is not due.
        world.assert_unchanged(|| {
            let dry = world.run(&["archive", "-n"]);
            dry.expect_code(0);
            dry.keys(&[
                "format", "command", "dry_run", "actions", "refused", "warnings",
            ]);
            dry.text_only(|out| assert_eq!(out, "Nothing to do.\n"));
            dry.json_only(|json| {
                assert_eq!(json["actions"], json!([]));
                assert_eq!(json["refused"], json!([]));
            });
            let bare = world.run(&["archive"]);
            bare.expect_code(0);
            bare.keys(&[
                "format", "command", "archived", "skipped", "refused", "repairs", "warnings",
            ]);
            bare.text_only(|out| assert_eq!(out, "Archived 0, skipped 0, refused 0.\n"));
            bare.json_only(|json| assert_eq!(json["archived"], json!([])));
            let status = world.run(&["status"]);
            status.expect_code(0);
            status.text_only(|out| assert!(!out.contains("scanner"), "{out:?}"));
            status.json_only(|json| assert_eq!(json["pieces"], json!([])));
        });

        // Work on the folder: an edit through one name of the hard link, a
        // new file in a new directory, a file removed. Then the folder is
        // left alone for forty days.
        let root = world.collection().path(SCANNER);
        fs::write(root.join("src/main.rs"), "fn main() { scan(); }\n").expect("the edit");
        fs::create_dir(root.join("notes")).expect("the directory is made");
        fs::write(root.join("notes/new.md"), "# new\n").expect("the file is written");
        fs::remove_file(root.join("old.txt")).expect("the file goes");
        world.collection().set_age(SCANNER, 40);
        let edited = unearthed(world, SCANNER);

        // Now it is due, and the next run archives the changed folder.
        let archived = world.run(&["archive"]);
        archived.expect_code(0);
        archived.keys(&[
            "format", "command", "archived", "skipped", "refused", "repairs", "warnings",
        ]);
        archived.text_only(|out| {
            assert_eq!(
                out,
                format!("archived {SCANNER}\nArchived 1, skipped 0, refused 0.\n")
            );
        });
        let stamps = world.collection().versions(SCANNER);
        assert_eq!(stamps.len(), 1, "{stamps:?}");
        let stored = store_files(world.collection(), SCANNER).remove(0);
        let archive = stored.archive.expect("the archive");
        archived.json_only(|json| {
            let entry = json["archived"][0].as_object().expect("an archived piece");
            let keys: Vec<&str> = entry.keys().map(String::as_str).collect();
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
            assert_eq!(entry["piece"], SCANNER);
            assert_eq!(entry["stamp"], stamps[0].as_str());
            assert_eq!(entry["version"], utc_of(&stamps[0]));
            assert_eq!(
                entry["archive_bytes"],
                fs::metadata(&archive).expect("the archive exists").len()
            );
            assert_eq!(entry["files"], edited.stored_files());
            assert_eq!(entry["bytes"], edited.unpacked_bytes());
        });
        assert!(!world.collection().path(SCANNER).exists());

        // The old version is gone and the new one holds the changes.
        let members = tar_members(&archive);
        for present in ["scanner/notes/new.md", "scanner/src/main-copy.rs"] {
            assert!(members.iter().any(|name| name == present), "{members:?}");
        }
        for absent in ["scanner/old.txt", "scanner/.antiq-restored"] {
            assert!(!members.iter().any(|name| name == absent), "{members:?}");
        }
        let checksum = stored.checksum.expect("the checksum file");
        shasum_check(checksum.parent().expect("a store folder"), &checksum);

        // Unearthing it again gives the changed folder back.
        world.run(&["unearth", "scanner"]).expect_code(0);
        edited.assert_like(&unearthed(world, SCANNER));
        for name in ["src/main.rs", "src/main-copy.rs"] {
            let text = fs::read_to_string(root.join(name)).expect("the file reads");
            assert_eq!(text, "fn main() { scan(); }\n", "{name}");
        }
        assert!(!root.join("old.txt").exists());
        assert_eq!(world.collection().versions(SCANNER), Vec::<String>::new());
        assert!(!world.collection().path(".antiq/pieces/projects").exists());
        no_staging_left(world);
    });
}

#[test]
fn unearth_without_c_resolves_a_path_from_the_current_directory() {
    both_modes(scanner_world, |world| {
        let root = world.collection().root();
        let projects = world.collection().path("projects");

        let from_root = world.run_in(&root, &["unearth", "-n", "tools/scan-old"]);
        from_root.expect_code(0);
        from_root.json_only(|json| assert_eq!(json["actions"][0]["piece"], "tools/scan-old"));

        let from_below = world.run_in(&projects, &["unearth", "-n", "../tools/scan-old"]);
        from_below.expect_code(0);
        from_below.json_only(|json| assert_eq!(json["actions"][0]["piece"], "tools/scan-old"));
    });
}
