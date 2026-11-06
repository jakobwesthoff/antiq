// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq forget` seen from outside the binary, on stores the scenario
//! harness writes by hand. The harness runs every command under
//! `TZ=Pacific/Auckland`, which is +13:00 in November 2026 and +12:00 in
//! April 2026, so local time and UTC never read alike.

mod support;

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use support::cmd::{hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};
use support::tree::Seen;

/// 22:02:11 on 2026-11-04 in Auckland.
const SCANNER_NEW: &str = "2026-11-04T090211";
/// Taken in the same second, one counter later.
const SCANNER_NEW_2: &str = "2026-11-04T090211-2";
/// 00:15:00 on 2026-09-04 in Auckland.
const SCANNER_OLD: &str = "2026-09-03T121500";
/// 00:30:00 on 2026-11-05 in Auckland, 11:30:00 on 2026-11-04 in UTC.
const SCANNER_LATE: &str = "2026-11-04T113000";
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

fn both(build: fn(&mut CollectionBuilder), script: impl Fn(&World)) {
    for mode in [Mode::Text, Mode::Json] {
        script(&world(mode, build));
    }
}

/// The collection as it is, without the lock file that every locked run
/// leaves behind.
fn state(world: &World) -> BTreeMap<String, Seen> {
    let mut seen = snapshot(&world.collection().root());
    seen.remove(".antiq/lock");
    seen
}

fn store(world: &World, relative: &str) -> PathBuf {
    world
        .collection()
        .path(&format!(".antiq/pieces/{relative}"))
}

/// The file names of a version in the order `forget` deletes them.
fn names(world: &World, piece: &str, stamp: &str) -> Vec<String> {
    let files = world.collection().version_files(piece, stamp);
    [files.archive, files.listing, files.checksum]
        .iter()
        .map(|path| {
            path.file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn exists(world: &World, piece: &str, stamp: &str) -> bool {
    world
        .collection()
        .version_files(piece, stamp)
        .archive
        .exists()
}

/// Sets the user immutable flag, which fails a delete of the entry for
/// anyone, and clears it when dropped, also after a panic.
#[cfg(target_os = "macos")]
struct Immutable(PathBuf);

#[cfg(target_os = "macos")]
impl Immutable {
    fn set(path: &Path) -> Immutable {
        use std::os::macos::fs::MetadataExt;
        let status = Command::new("chflags")
            .arg("uchg")
            .arg(path)
            .status()
            .expect("the chflags executable exists");
        assert!(status.success(), "chflags failed for {}", path.display());
        let flags = fs::symlink_metadata(path)
            .expect("the entry can be read")
            .st_flags();
        assert!(flags & 0x2 != 0, "the flag did not take");
        Immutable(path.to_path_buf())
    }

    fn clear(self) {}
}

#[cfg(target_os = "macos")]
impl Drop for Immutable {
    fn drop(&mut self) {
        let _ = Command::new("chflags").arg("nouchg").arg(&self.0).status();
    }
}

// =========================================================
// One version
// =========================================================

#[test]
fn a_version_goes_and_the_others_stay() {
    both(three_versions, |world| {
        let run = world.run(&["forget", "projects/scanner", SCANNER_NEW]);
        run.expect_code(0).keys(&[
            "format",
            "command",
            "forgotten",
            "failed",
            "repairs",
            "warnings",
        ]);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!("Deleted version {SCANNER_NEW} of projects/scanner\n")
            );
        });
        run.json_only(|json| {
            assert_eq!(json["command"], "forget");
            let gone = json["forgotten"].as_array().expect("a list");
            assert_eq!(gone.len(), 1);
            let keys: Vec<&str> = gone[0]
                .as_object()
                .expect("an object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, ["piece", "stamp", "version", "incomplete", "files"]);
            assert_eq!(gone[0]["piece"], "projects/scanner");
            assert_eq!(gone[0]["stamp"], SCANNER_NEW);
            assert_eq!(gone[0]["version"], "2026-11-04T09:02:11Z");
            assert_eq!(gone[0]["incomplete"], false);
            assert_eq!(
                gone[0]["files"],
                serde_json::json!(names(world, "projects/scanner", SCANNER_NEW))
            );
            assert_eq!(json["failed"], Value::Array(Vec::new()));
            assert_eq!(json["repairs"], Value::Array(Vec::new()));
            assert_eq!(json["warnings"], Value::Array(Vec::new()));
        });
        assert_eq!(run.stderr, "");
        assert!(!exists(world, "projects/scanner", SCANNER_NEW));
        assert!(
            !world
                .collection()
                .version_files("projects/scanner", SCANNER_NEW)
                .listing
                .exists()
        );
        assert!(
            !world
                .collection()
                .version_files("projects/scanner", SCANNER_NEW)
                .checksum
                .exists()
        );
        assert!(exists(world, "projects/scanner", SCANNER_OLD));
        assert!(exists(world, "tools/scan-old", SCAN_OLD));
    });
}

#[test]
fn a_terminal_gets_the_local_time_of_the_version() {
    let world = world(Mode::Text, three_versions);
    let run = world.run_tty(&["forget", "projects/scanner", SCANNER_NEW]);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "Deleted version 2026-11-04 22:02:11 of projects/scanner\n"
    );
}

#[test]
fn the_last_version_takes_the_store_folder_and_the_empty_parents() {
    both(
        |builder| {
            builder
                .version("a/b/c", SCANNER_NEW, &[("f", "x")])
                .version("a/sibling", SCANNER_NEW, &[("f", "x")]);
        },
        |world| {
            world.run(&["forget", "a/b/c", SCANNER_NEW]).expect_code(0);
            assert!(!store(world, "a/b").exists(), "the empty parent stays");
            assert!(store(world, "a/sibling").exists());
            assert!(store(world, "a").exists());
        },
    );
}

#[test]
fn the_version_is_named_by_local_date_stamp_prefix_or_exact_stamp() {
    let build = |builder: &mut CollectionBuilder| {
        builder
            .version("projects/scanner", SCANNER_OLD, &[("a", "1")])
            .version("projects/scanner", SCANNER_NEW, &[("a", "1")])
            .version("projects/scanner", SCANNER_NEW_2, &[("a", "1")])
            .version("projects/scanner", SCANNER_LATE, &[("a", "1")]);
    };
    for mode in [Mode::Text, Mode::Json] {
        // The local date 2026-11-05 is the late version alone, though its
        // UTC date is 2026-11-04.
        let world = world(mode, build);
        world
            .run(&["forget", "scanner", "2026-11-05"])
            .expect_code(0);
        assert!(!exists(&world, "projects/scanner", SCANNER_LATE));
        assert_eq!(world.collection().versions("projects/scanner").len(), 3);

        // A stamp prefix with a T that only one version has.
        world
            .run(&["forget", "scanner", "2026-09-03T12"])
            .expect_code(0);
        assert!(!exists(&world, "projects/scanner", SCANNER_OLD));

        // The exact stamp wins over the stamp it is a prefix of.
        world
            .run(&["forget", "scanner", SCANNER_NEW])
            .expect_code(0);
        assert!(!exists(&world, "projects/scanner", SCANNER_NEW));
        assert!(exists(&world, "projects/scanner", SCANNER_NEW_2));
    }
}

#[test]
fn several_matches_are_an_error_naming_each_stamp() {
    let build = |builder: &mut CollectionBuilder| {
        builder
            .version("projects/scanner", SCANNER_NEW, &[("a", "1")])
            .version("projects/scanner", SCANNER_LATE, &[("a", "1")]);
    };
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, build);
        let before = state(&world);
        let run = world.run(&["forget", "scanner", "2026-11-04T"]);
        run.expect_code(1);
        run.text_only(|_| {
            assert!(run.stderr.contains(SCANNER_NEW), "{:?}", run.stderr);
            assert!(run.stderr.contains(SCANNER_LATE), "{:?}", run.stderr);
        });
        run.json_only(|json| {
            let message = json["error"]["message"].as_str().expect("a text");
            assert!(message.contains(SCANNER_NEW) && message.contains(SCANNER_LATE));
        });
        assert_eq!(state(&world), before);
    }
}

#[test]
fn a_version_that_does_not_exist_is_an_error() {
    both(three_versions, |world| {
        let before = state(world);
        let run = world.run(&["forget", "projects/scanner", "2020-01-01"]);
        run.expect_code(1);
        run.text_only(|_| {
            assert!(
                run.stderr.contains("matches no version"),
                "{:?}",
                run.stderr
            );
        });
        assert_eq!(run.error_kind().as_deref().unwrap_or("other"), "other");
        assert_eq!(state(world), before);
    });
}

#[test]
fn an_unknown_or_ambiguous_piece_is_an_error() {
    both(three_versions, |world| {
        let run = world.run(&["forget", "zzz", SCANNER_NEW]);
        run.expect_code(1);
        assert_eq!(
            run.error_kind().as_deref().unwrap_or("unknown_reference"),
            "unknown_reference"
        );
        let run = world.run(&["forget", "scan", SCANNER_NEW]);
        run.expect_code(1);
        run.json_only(|json| {
            assert_eq!(json["error"]["kind"], "ambiguous_reference");
            let paths: Vec<&str> = json["error"]["candidates"]
                .as_array()
                .expect("a list")
                .iter()
                .map(|candidate| candidate["path"].as_str().expect("a text"))
                .collect();
            assert_eq!(paths, ["projects/scanner", "tools/scan-old"]);
        });
        assert!(exists(world, "projects/scanner", SCANNER_NEW));
    });
}

// =========================================================
// Incomplete versions, doubled slots, versions of a newer format
// =========================================================

#[test]
fn an_incomplete_version_is_taken_by_its_stamp_and_said_to_be_incomplete() {
    both(three_versions, |world| {
        world
            .collection()
            .damage_remove_archive("projects/scanner", SCANNER_OLD);
        // Its time does not name it: it has no archive.
        world
            .run(&["forget", "projects/scanner", "2026-09-04"])
            .expect_code(1);

        let run = world.run(&["forget", "projects/scanner", SCANNER_OLD]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!("Deleted incomplete version {SCANNER_OLD} of projects/scanner\n")
            );
        });
        run.json_only(|json| {
            assert_eq!(json["forgotten"][0]["incomplete"], true);
            assert_eq!(json["forgotten"][0]["stamp"], SCANNER_OLD);
        });
        let files = world
            .collection()
            .version_files("projects/scanner", SCANNER_OLD);
        assert!(!files.listing.exists() && !files.checksum.exists());
        assert!(exists(world, "projects/scanner", SCANNER_NEW));
    });
}

#[test]
fn an_incomplete_version_of_a_sidecar_only_folder_takes_the_folder() {
    both(three_versions, |world| {
        world
            .collection()
            .damage_remove_archive("tools/scan-old", SCAN_OLD);
        world
            .run(&["forget", "tools/scan-old", SCAN_OLD])
            .expect_code(0);
        assert!(!store(world, "tools").exists());
    });
}

#[test]
fn a_doubled_slot_is_removed_whole() {
    both(three_versions, |world| {
        let files = world
            .collection()
            .version_files("projects/scanner", SCANNER_NEW);
        let twin = store(
            world,
            &format!("projects/scanner/other-name--{SCANNER_NEW}.tar.zst"),
        );
        fs::copy(&files.archive, &twin).expect("the twin is written");
        world
            .run(&["forget", "projects/scanner", SCANNER_NEW])
            .expect_code(0);
        assert!(!twin.exists() && !files.archive.exists());
        assert!(!files.listing.exists() && !files.checksum.exists());
        assert!(exists(world, "projects/scanner", SCANNER_OLD));
    });
}

#[test]
fn a_listing_of_a_newer_format_does_not_keep_its_version() {
    both(three_versions, |world| {
        let files = world
            .collection()
            .version_files("projects/scanner", SCANNER_NEW);
        fs::write(&files.listing, "antiq-listing 99\n").expect("the listing is replaced");
        world
            .run(&["forget", "projects/scanner", SCANNER_NEW])
            .expect_code(0);
        assert!(!files.archive.exists() && !files.listing.exists());
    });
}

// =========================================================
// All versions
// =========================================================

#[test]
fn all_versions_goes_through_every_version_incomplete_ones_included() {
    both(three_versions, |world| {
        world
            .collection()
            .damage_remove_archive("projects/scanner", SCANNER_OLD);
        let run = world.run(&["forget", "projects/scanner", "--all-versions", "--force"]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!(
                    "Deleted incomplete version {SCANNER_OLD} of projects/scanner\n\
                     Deleted version {SCANNER_NEW} of projects/scanner\n"
                )
            );
        });
        run.json_only(|json| {
            let gone = json["forgotten"].as_array().expect("a list");
            let stamps: Vec<&str> = gone
                .iter()
                .map(|entry| entry["stamp"].as_str().expect("a text"))
                .collect();
            assert_eq!(stamps, [SCANNER_OLD, SCANNER_NEW]);
            assert_eq!(gone[0]["incomplete"], true);
            assert_eq!(gone[1]["incomplete"], false);
        });
        assert!(!store(world, "projects").exists(), "the piece folder stays");
        assert!(exists(world, "tools/scan-old", SCAN_OLD));
    });
}

// =========================================================
// A failed deletion
// =========================================================

#[cfg(target_os = "macos")]
#[test]
fn a_version_is_deleted_archive_first_and_a_rerun_removes_the_rest() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        let files = world
            .collection()
            .version_files("projects/scanner", SCANNER_NEW);
        let guard = Immutable::set(&files.listing);

        let run = world.run(&["forget", "projects/scanner", SCANNER_NEW]);
        run.expect_code(3);
        assert!(!files.archive.exists(), "the archive goes first");
        assert!(files.listing.exists() && files.checksum.exists());
        run.text_only(|out| assert_eq!(out, ""));
        run.text_only(|_| {
            let line =
                format!("error: could not delete version {SCANNER_NEW} of projects/scanner: ");
            assert!(run.stderr.starts_with(&line), "{:?}", run.stderr);
        });
        run.json_only(|json| {
            assert_eq!(json["forgotten"], Value::Array(Vec::new()));
            assert_eq!(json["failed"][0]["piece"], "projects/scanner");
            assert_eq!(json["failed"][0]["stamp"], SCANNER_NEW);
            assert!(json["failed"][0]["reason"].is_string());
        });

        guard.clear();
        let rerun = world.run(&["forget", "projects/scanner", SCANNER_NEW]);
        rerun.expect_code(0);
        rerun.text_only(|out| {
            assert_eq!(
                out,
                format!("Deleted incomplete version {SCANNER_NEW} of projects/scanner\n")
            );
        });
        assert!(!files.listing.exists() && !files.checksum.exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn all_versions_goes_on_after_a_failed_version_and_exits_3() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        let stuck = world
            .collection()
            .version_files("projects/scanner", SCANNER_OLD);
        let _guard = Immutable::set(&stuck.listing);

        let run = world.run(&["forget", "projects/scanner", "--all-versions", "--force"]);
        run.expect_code(3);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!("Deleted version {SCANNER_NEW} of projects/scanner\n")
            );
        });
        run.json_only(|json| {
            assert_eq!(json["forgotten"].as_array().expect("a list").len(), 1);
            assert_eq!(json["failed"][0]["stamp"], SCANNER_OLD);
        });
        assert!(!exists(&world, "projects/scanner", SCANNER_NEW));
        assert!(stuck.listing.exists());
    }
}

// =========================================================
// The guard around the run
// =========================================================

#[test]
fn a_held_lock_fails_the_run_and_changes_nothing() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        let _lock = hold_lock(&world.collection().root());
        let before = state(&world);
        let run = world.run(&["forget", "projects/scanner", SCANNER_NEW]);
        run.expect_code(1);
        assert_eq!(
            run.error_kind().as_deref().unwrap_or("lock_held"),
            "lock_held"
        );
        run.json_only(|json| {
            assert_eq!(json["error"]["pid"], std::process::id());
            assert!(json["error"]["started"].is_string());
        });
        assert_eq!(state(&world), before);
    }
}

#[test]
fn the_repairs_run_first_and_are_reported() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        let leftover = world.collection().path(".antiq/tmp/leftover");
        fs::write(&leftover, "x").expect("the leftover is written");
        let run = world.run(&["forget", "projects/scanner", SCANNER_NEW]);
        run.expect_code(0);
        assert!(!leftover.exists());
        run.text_only(|_| {
            assert_eq!(run.stderr, "note: repaired: delete .antiq/tmp/leftover\n");
        });
        run.json_only(|json| {
            assert_eq!(
                json["repairs"],
                serde_json::json!([
                    {"kind": "repair", "repair": "delete_temporary", "entry": "leftover"}
                ])
            );
        });
    }
}

#[test]
fn sidecars_of_a_staged_version_are_not_a_piece_to_forget() {
    // Repair 1 deletes the sidecars of a version whose archive is staged,
    // so the folder is no candidate any more.
    both(three_versions, |world| {
        world
            .collection()
            .damage_remove_archive("tools/scan-old", SCAN_OLD);
        let staged = world
            .collection()
            .path(&format!(".antiq/tmp/pack/{SCAN_OLD}/tools/scan-old"));
        fs::create_dir_all(&staged).expect("the staging is made");
        fs::write(
            staged.join(format!("scan-old--{SCAN_OLD}.tar.zst")),
            "archive",
        )
        .expect("the staged archive is written");
        let run = world.run(&["forget", "tools/scan-old", SCAN_OLD]);
        run.expect_code(1);
        assert_eq!(
            run.error_kind().as_deref().unwrap_or("unknown_reference"),
            "unknown_reference"
        );
    });
}

/// Stages the archive of the incomplete version `stamp` of `piece` as an
/// `archive` that died while moving it into the store leaves it.
fn stage_archive(world: &World, piece: &str, stamp: &str) {
    let name = piece.rsplit('/').next().expect("a piece has a name");
    let staged = world
        .collection()
        .path(&format!(".antiq/tmp/pack/{stamp}/{piece}"));
    fs::create_dir_all(&staged).expect("the staging is made");
    fs::write(staged.join(format!("{name}--{stamp}.tar.zst")), "archive")
        .expect("the staged archive is written");
}

#[cfg(target_os = "macos")]
#[test]
fn sidecars_that_repair_one_could_not_delete_are_still_no_piece_to_forget() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        world
            .collection()
            .damage_remove_archive("tools/scan-old", SCAN_OLD);
        stage_archive(&world, "tools/scan-old", SCAN_OLD);
        let listing = world
            .collection()
            .version_files("tools/scan-old", SCAN_OLD)
            .listing;
        let _guard = Immutable::set(&listing);

        let run = world.run(&["forget", "tools/scan-old", SCAN_OLD]);

        assert_eq!(
            run.error_kind().as_deref().unwrap_or("unknown_reference"),
            "unknown_reference"
        );
        assert!(listing.exists());
    }
}

// =========================================================
// Usage
// =========================================================

fn refused_as_usage(args: &[&str]) {
    both(three_versions, |world| {
        let run = world.run(args);
        run.expect_code(2);
        assert_eq!(
            run.error_kind().as_deref().unwrap_or("usage"),
            "usage",
            "{args:?}"
        );
        assert!(exists(world, "projects/scanner", SCANNER_NEW));
    });
}

#[test]
fn an_empty_piece_is_a_usage_error() {
    refused_as_usage(&["forget", "", SCANNER_NEW]);
}

#[test]
fn an_empty_version_is_a_usage_error() {
    refused_as_usage(&["forget", "projects/scanner", ""]);
}

#[test]
fn the_command_line_refuses_a_missing_or_doubled_target() {
    both(three_versions, |world| {
        for args in [
            &["forget", "projects/scanner"][..],
            &["forget", "projects/scanner", "--all-versions"],
            &["forget", "projects/scanner", "--force"],
            &[
                "forget",
                "projects/scanner",
                SCANNER_NEW,
                "--all-versions",
                "--force",
            ],
        ] {
            world.run(args).expect_code(2);
        }
        assert!(exists(world, "projects/scanner", SCANNER_NEW));
    });
}

#[test]
fn all_versions_without_force_is_an_error_block_with_an_info_on_a_terminal() {
    let world = world(Mode::Text, three_versions);
    let run = world.run_tty_stderr(
        &["forget", "projects/scanner", "--all-versions"],
        &[("NO_COLOR", "1")],
    );
    run.expect_code(2);
    assert_eq!(
        run.stderr,
        " ✖ ERROR\n\
         ▌ projects/scanner: --all-versions needs --force\n\
         ▌\n\
         ▌ Why it happened\n\
         ▌   --all-versions deletes every stored version of the piece, damaged and\n\
         ▌   incomplete ones included, and that cannot be undone.\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   Add --force to confirm: antiq forget projects/scanner --all-versions\n\
         ▌   --force. Check the effect first with -n.\n\
         ▌\n\
         ▌ What antiq was doing\n\
         ▌   Attempting to forget all versions of projects/scanner.\n\
         \n \
         ℹ INFO  Nothing was deleted.\n"
    );
    assert!(exists(&world, "projects/scanner", SCANNER_NEW));
}

// =========================================================
// Dry run
// =========================================================

fn unchanged(world: &World, script: impl FnOnce()) {
    let before = snapshot(&world.collection().root());
    script();
    assert_eq!(snapshot(&world.collection().root()), before);
}

#[test]
fn a_dry_run_names_the_files_of_the_version_and_changes_nothing() {
    both(three_versions, |world| {
        unchanged(world, || {
            let run = world.run(&["forget", "projects/scanner", SCANNER_NEW, "-n"]);
            run.expect_code(0)
                .keys(&["format", "command", "dry_run", "actions", "warnings"]);
            let files = names(world, "projects/scanner", SCANNER_NEW);
            run.text_only(|out| {
                assert_eq!(
                    out,
                    format!(
                        "Would delete version {SCANNER_NEW} of projects/scanner: {}\n",
                        files.join(", ")
                    )
                );
            });
            run.json_only(|json| {
                assert_eq!(json["dry_run"], true);
                let actions = json["actions"].as_array().expect("a list");
                assert_eq!(actions.len(), 1);
                let keys: Vec<&str> = actions[0]
                    .as_object()
                    .expect("an object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    keys,
                    ["kind", "piece", "stamp", "version", "incomplete", "files"]
                );
                assert_eq!(actions[0]["kind"], "forget_version");
                assert_eq!(actions[0]["stamp"], SCANNER_NEW);
                assert_eq!(actions[0]["version"], "2026-11-04T09:02:11Z");
                assert_eq!(actions[0]["files"], serde_json::json!(files));
                assert_eq!(json["warnings"], Value::Array(Vec::new()));
            });
            assert_eq!(run.stderr, "");
        });
    });
}

#[test]
fn a_dry_run_on_a_terminal_names_the_version_by_its_local_time() {
    let world = world(Mode::Text, three_versions);
    let run = world.run_tty(&["forget", "projects/scanner", SCANNER_NEW, "-n"]);
    run.expect_code(0);
    assert!(
        run.stdout
            .starts_with("Would delete version 2026-11-04 22:02:11 of projects/scanner: "),
        "{:?}",
        run.stdout
    );
}

#[test]
fn a_dry_run_of_all_versions_lists_each_and_says_which_are_incomplete() {
    both(three_versions, |world| {
        world
            .collection()
            .damage_remove_archive("projects/scanner", SCANNER_OLD);
        unchanged(world, || {
            let run = world.run(&[
                "forget",
                "projects/scanner",
                "--all-versions",
                "--force",
                "-n",
            ]);
            run.expect_code(0);
            run.text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(lines.len(), 2, "{out:?}");
                assert!(
                    lines[0].starts_with(&format!(
                        "Would delete incomplete version {SCANNER_OLD} of projects/scanner: "
                    )),
                    "{:?}",
                    lines[0]
                );
                assert!(
                    lines[1].starts_with(&format!(
                        "Would delete version {SCANNER_NEW} of projects/scanner: "
                    )),
                    "{:?}",
                    lines[1]
                );
            });
            run.json_only(|json| {
                let actions = json["actions"].as_array().expect("a list");
                assert_eq!(actions[0]["incomplete"], true);
                assert_eq!(actions[1]["incomplete"], false);
            });
        });
    });
}

#[test]
fn a_dry_run_lists_the_repairs_first() {
    both(three_versions, |world| {
        let leftover = world.collection().path(".antiq/tmp/leftover");
        fs::write(&leftover, "x").expect("the leftover is written");
        unchanged(world, || {
            let run = world.run(&["forget", "projects/scanner", SCANNER_NEW, "-n"]);
            run.expect_code(0);
            run.text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(lines[0], "Would delete .antiq/tmp/leftover");
                assert!(lines[1].starts_with("Would delete version"), "{out:?}");
            });
            run.json_only(|json| {
                assert_eq!(
                    json["actions"][0],
                    serde_json::json!(
                        {"kind": "repair", "repair": "delete_temporary", "entry": "leftover"}
                    )
                );
                assert_eq!(json["actions"][1]["kind"], "forget_version");
            });
        });
        assert!(leftover.exists());
    });
}

#[test]
fn a_dry_run_assumes_repair_one_was_done() {
    both(three_versions, |world| {
        world
            .collection()
            .damage_remove_archive("tools/scan-old", SCAN_OLD);
        stage_archive(world, "tools/scan-old", SCAN_OLD);
        unchanged(world, || {
            let run = world.run(&["forget", "tools/scan-old", SCAN_OLD, "-n"]);
            run.expect_code(1);
            assert_eq!(
                run.error_kind().as_deref().unwrap_or("unknown_reference"),
                "unknown_reference"
            );
        });
    });
}

#[test]
fn a_dry_run_assumes_repair_five_was_done() {
    let build = |builder: &mut CollectionBuilder| {
        three_versions(builder);
        builder.patterns(&["projects/*"]);
        builder.piece("projects/scanner", |piece| {
            piece
                .age_days(1)
                .file("src/main.rs", "fn main() {}\n")
                .restored(1, Some(SCANNER_NEW));
        });
    };
    both(build, |world| {
        unchanged(world, || {
            let gone = world.run(&["forget", "projects/scanner", SCANNER_NEW, "-n"]);
            gone.expect_code(1);
            gone.text_only(|_| {
                assert!(
                    gone.stderr.contains("matches no version"),
                    "{:?}",
                    gone.stderr
                );
            });

            let kept = world.run(&["forget", "projects/scanner", SCANNER_OLD, "-n"]);
            kept.expect_code(0);
            kept.text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(
                    lines[0],
                    format!(
                        "Would delete version {SCANNER_NEW} of projects/scanner that an unearth left"
                    )
                );
                assert!(lines[1].starts_with("Would delete version"), "{out:?}");
            });
            kept.json_only(|json| {
                assert_eq!(json["actions"][0]["repair"], "delete_restored_version");
                assert_eq!(json["actions"][1]["stamp"], SCANNER_OLD);
            });
        });
    });
}

#[test]
fn a_dry_run_that_finds_the_lock_held_names_the_holder_and_lists_no_repairs() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        fs::write(world.collection().path(".antiq/tmp/leftover"), "x").expect("written");
        let _lock = hold_lock(&world.collection().root());
        let before = snapshot(&world.collection().root());

        let run = world.run(&["forget", "projects/scanner", SCANNER_NEW, "-n"]);
        run.expect_code(0)
            .keys(&["format", "command", "dry_run", "lock_held", "actions"]);
        run.text_only(|out| {
            let line = format!("The lock is held by pid {}, started ", std::process::id());
            assert!(out.starts_with(&line), "{out:?}");
            assert!(out.ends_with(". No repairs are listed.\n"), "{out:?}");
            assert_eq!(out.lines().count(), 1);
        });
        run.json_only(|json| {
            assert_eq!(json["dry_run"], true);
            assert_eq!(json["lock_held"]["pid"], std::process::id());
            assert!(json["lock_held"]["started"].is_string());
            assert_eq!(json["actions"], Value::Array(Vec::new()));
        });
        assert_eq!(run.stderr, "");
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

#[test]
fn a_dry_run_reports_what_a_real_run_would_refuse() {
    both(three_versions, |world| {
        unchanged(world, || {
            let run = world.run(&["forget", "projects/scanner", "2020-01-01", "-n"]);
            run.expect_code(1);
            let run = world.run(&["forget", "", SCANNER_NEW, "-n"]);
            run.expect_code(2);
            let run = world.run(&["forget", "projects/scanner", "", "-n"]);
            run.expect_code(2);
        });
    });
}

#[test]
fn the_notes_on_the_settings_come_with_a_real_run_and_a_dry_run() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_versions);
        let config = world.collection().path(".antiq/config.toml");
        let mut text = fs::read_to_string(&config).expect("the config exists");
        text.push_str("colour = \"red\"\n");
        fs::write(&config, text).expect("the config is written");
        let note = "the key `colour` under [collection] is unknown and ignored";

        for args in [
            &["forget", "projects/scanner", SCANNER_NEW, "-n"][..],
            &["forget", "projects/scanner", SCANNER_NEW][..],
        ] {
            let run = world.run(args);
            run.expect_code(0);
            run.text_only(|_| assert_eq!(run.stderr, format!("note: {note}\n")));
            run.json_only(|json| {
                assert_eq!(json["warnings"], serde_json::json!([{"message": note}]));
            });
        }
    }
}
