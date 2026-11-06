// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq fork` seen from outside the binary, on stores the scenario
//! harness writes by hand. The harness runs every command under
//! `TZ=Pacific/Auckland`, which is +13:00 in November 2026.

mod support;

use std::fs;
use support::cmd::{hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};

const NEW: &str = "2026-11-04T090211";
const OLD: &str = "2026-09-03T121500";

fn world(mode: Mode, build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*", "tools/*", "work/*/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), mode)
}

fn scanner(builder: &mut CollectionBuilder) {
    builder
        .version(
            "projects/scanner",
            OLD,
            &[("src/main.rs", "fn main() {}\n")],
        )
        .version(
            "projects/scanner",
            NEW,
            &[("src/main.rs", "fn main() {}\n"), ("README", "scan\n")],
        );
}

fn only_new(builder: &mut CollectionBuilder) {
    builder.version("projects/scanner", NEW, &[("a.txt", "hello\n")]);
}

fn marker(world: &World, at: &str) -> String {
    fs::read_to_string(world.collection().path(at).join(".antiq-restored"))
        .expect("the marker exists")
}

fn store_names(world: &World) -> Vec<String> {
    let mut names: Vec<String> =
        fs::read_dir(world.collection().path(".antiq/pieces/projects/scanner"))
            .expect("the store folder")
            .map(|item| {
                item.expect("item")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
    names.sort();
    names
}

fn assert_tmp_has_nothing_of_the_run(world: &World) {
    let left = snapshot(&world.collection().path(".antiq/tmp/extract"));
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_fork_to_the_original_path_keeps_the_version_and_writes_a_marker_without_stamp() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let store = store_names(&world);
        let run = world.run(&["fork", "projects/scanner"]);
        run.expect_code(0)
            .keys(&[
                "format", "command", "forked", "piece", "stamp", "version", "path", "bytes",
                "repairs", "warnings",
            ])
            .text_only(|out| {
                assert_eq!(
                    out,
                    format!("Forked projects/scanner (version {NEW}) to projects/scanner\n")
                );
            })
            .json_only(|json| {
                assert_eq!(json["command"], "fork");
                assert_eq!(json["forked"], true);
                assert_eq!(json["stamp"], NEW);
                assert_eq!(json["version"], "2026-11-04T09:02:11Z");
                assert_eq!(json["path"], "projects/scanner");
                assert!(json["bytes"].is_u64(), "{json}");
            });
        assert!(world.collection().path("projects/scanner/README").exists());
        let text = marker(&world, "projects/scanner");
        assert!(
            text.starts_with("restored ") && !text.contains("version"),
            "{text}"
        );
        assert_eq!(store_names(&world), store);
    }
}

#[test]
fn a_name_gives_the_copy_another_path_and_leaves_the_original_free() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let run = world.run(&["fork", "projects/scanner", "--name", "tools/copy"]);
        run.expect_code(0)
            .text_only(|out| {
                assert_eq!(
                    out,
                    format!("Forked projects/scanner (version {NEW}) to tools/copy\n")
                );
            })
            .json_only(|json| {
                assert_eq!(json["piece"], "projects/scanner");
                assert_eq!(json["path"], "tools/copy");
            });
        assert!(world.collection().path("tools/copy/README").exists());
        assert!(!world.collection().path("projects").exists());
    }
}

#[test]
fn a_name_with_folders_above_it_creates_them() {
    let world = world(Mode::Text, scanner);
    world
        .run(&["fork", "projects/scanner", "--name", "work/a/copy"])
        .expect_code(0);
    assert!(world.collection().path("work/a/copy/README").exists());
}

#[test]
fn a_version_is_named_by_its_stamp_or_its_local_date() {
    let world = world(Mode::Text, scanner);
    world
        .run(&[
            "fork",
            "projects/scanner",
            "--version",
            OLD,
            "--name",
            "tools/old",
        ])
        .expect_code(0);
    assert!(!world.collection().path("tools/old/README").exists());
    // 09:02:11 UTC on November 4th is 22:02:11 at +13:00 that day.
    world
        .run(&[
            "fork",
            "projects/scanner",
            "--version",
            "2026-11-04",
            "--name",
            "tools/new",
        ])
        .expect_code(0);
    assert!(world.collection().path("tools/new/README").exists());
}

#[test]
fn a_taken_original_path_names_the_name_option() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        fs::create_dir_all(world.collection().path("projects/scanner")).expect("takes the place");
        let run = world.run(&["fork", "projects/scanner"]);
        run.expect_code(1);
        let shown = format!("{}{}", run.stdout, run.stderr);
        assert!(shown.contains("it is taken"), "{shown:?}");
        assert!(shown.contains("`--name`"), "{shown:?}");
        let dry = world.run(&["fork", "projects/scanner", "-n"]);
        dry.expect_code(1);
        let shown = format!("{}{}", dry.stdout, dry.stderr);
        assert!(shown.contains("`--name`"), "{shown:?}");
    }
}

#[test]
fn a_taken_name_is_refused_without_the_hint() {
    let world = world(Mode::Text, scanner);
    fs::create_dir_all(world.collection().path("tools/copy")).expect("takes the place");
    let run = world.run(&["fork", "projects/scanner", "--name", "tools/copy"]);
    run.expect_code(1);
    assert!(run.stderr.contains("it is taken"), "{:?}", run.stderr);
    assert!(!run.stderr.contains("--name"), "{:?}", run.stderr);
}

#[test]
fn an_empty_name_is_a_usage_error_and_a_name_outside_the_collection_is_not() {
    let world = world(Mode::Json, scanner);
    let run = world.run(&["fork", "projects/scanner", "--name", ""]);
    run.expect_code(2);
    assert_eq!(run.error_kind().as_deref(), Some("usage"));
    let run = world.run(&["fork", "projects/scanner", "--name", "/somewhere/else"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("outside_collection"));
    let run = world.run(&["fork", "projects/scanner", "--name", "."]);
    run.expect_code(1);
    run.json_only(|json| {
        let message = json["error"]["message"].as_str().expect("a message");
        assert!(message.contains("collection root"), "{message}");
    });
    let run = world.run(&["fork", ""]);
    run.expect_code(2);
    assert_eq!(run.error_kind().as_deref(), Some("usage"));
}

// ----- damage

#[test]
fn a_damaged_version_is_forked_with_a_warning() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, only_new);
        world
            .collection()
            .damage_corrupt_checksum("projects/scanner", NEW);
        let run = world.run(&["fork", "projects/scanner", "--name", "tools/copy"]);
        run.expect_code(0)
            .text_only(|out| {
                assert_eq!(
                    out,
                    format!("Forked projects/scanner (version {NEW}) to tools/copy\n")
                );
                assert!(run.stderr.starts_with("note: version "), "{:?}", run.stderr);
                assert!(run.stderr.contains("is damaged"), "{:?}", run.stderr);
            })
            .json_only(|json| {
                assert_eq!(json["forked"], true);
                let warnings = json["warnings"].as_array().expect("warnings");
                assert_eq!(warnings.len(), 1, "{json}");
                assert!(
                    warnings[0]["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("is damaged"))
                );
            });
        assert!(world.collection().path("tools/copy/a.txt").exists());
    }
}

#[test]
fn a_version_without_a_listing_is_forked_with_two_warnings_and_no_size() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, only_new);
        world
            .collection()
            .damage_remove_listing("projects/scanner", NEW);
        let run = world.run(&["fork", "projects/scanner"]);
        run.expect_code(0)
            .text_only(|_| {
                assert_eq!(run.stderr.matches("note: ").count(), 2, "{:?}", run.stderr);
                assert!(run.stderr.contains("free space is not checked"));
            })
            .json_only(|json| {
                assert!(json["bytes"].is_null(), "{json}");
                assert_eq!(json["warnings"].as_array().map(Vec::len), Some(2));
            });
        assert!(world.collection().path("projects/scanner/a.txt").exists());
    }
}

#[test]
fn a_damaged_archive_that_cannot_be_read_to_its_end_fails_and_leaves_nothing() {
    let world = world(Mode::Json, only_new);
    world
        .collection()
        .damage_truncate_archive("projects/scanner", NEW);
    let run = world.run(&["fork", "projects/scanner", "--name", "tools/copy"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("other"));
    assert!(!world.collection().path("tools").exists());
    assert_tmp_has_nothing_of_the_run(&world);
}

#[test]
fn a_dry_run_of_a_damaged_version_says_so_and_drops_the_checks_it_cannot_make() {
    let world = world(Mode::Text, only_new);
    world
        .collection()
        .damage_remove_listing("projects/scanner", NEW);
    let run = world.run(&["fork", "projects/scanner", "-n"]);
    run.expect_code(0);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!("Would fork projects/scanner (version {NEW}) to projects/scanner")
    );
    assert_eq!(
        lines[1],
        format!("  check the version {NEW} is damaged and is extracted with a warning")
    );
    assert!(
        lines[2].starts_with("  check the destination "),
        "{lines:?}"
    );
    assert!(lines[3].contains("member names"), "{lines:?}");
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(run.stderr.contains("note: version "), "{:?}", run.stderr);
}

#[test]
fn a_doubled_slot_is_refused_and_the_message_does_not_advise_fork() {
    let world = world(Mode::Text, only_new);
    let files = world.collection().version_files("projects/scanner", NEW);
    let twin = files
        .archive
        .with_file_name("other--2026-11-04T090211.tar.zst");
    fs::copy(&files.archive, &twin).expect("copies");
    let run = world.run(&["fork", "projects/scanner"]);
    run.expect_code(1);
    assert!(
        run.stderr.contains("several files in one slot"),
        "{:?}",
        run.stderr
    );
    assert!(!run.stderr.contains("`fork`"), "{:?}", run.stderr);
}

#[test]
fn a_listing_of_a_newer_format_is_refused_even_next_to_other_damage() {
    let world = world(Mode::Json, only_new);
    let files = world.collection().version_files("projects/scanner", NEW);
    let text =
        zstd::stream::decode_all(&fs::read(&files.listing).expect("reads")[..]).expect("decodes");
    let text = String::from_utf8(text).expect("text").replacen(
        "# antiq listing 1",
        "# antiq listing 2",
        1,
    );
    fs::write(
        &files.listing,
        zstd::stream::encode_all(text.as_bytes(), 3).expect("compresses"),
    )
    .expect("writes");
    world
        .collection()
        .damage_corrupt_checksum("projects/scanner", NEW);
    let run = world.run(&["fork", "projects/scanner"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("format_too_new"));
}

// ----- room

/// Makes the listing say that the file is far larger than any volume has
/// room for. Its totals agree, so only the free-space check refuses.
fn oversize_listing(world: &World) {
    let path = world
        .collection()
        .version_files("projects/scanner", NEW)
        .listing;
    let text = zstd::stream::decode_all(&fs::read(&path).expect("reads")[..]).expect("decodes");
    let text = String::from_utf8(text)
        .expect("text")
        .replace("# bytes         6", "# bytes         9000000000000000")
        .replace("                 6  a.txt", "  9000000000000000  a.txt");
    fs::write(
        &path,
        zstd::stream::encode_all(text.as_bytes(), 3).expect("compresses"),
    )
    .expect("writes");
}

#[test]
fn a_version_that_does_not_fit_is_skipped_with_exit_3_and_nothing_changes() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, only_new);
        oversize_listing(&world);
        let before = snapshot(&world.collection().root());
        let run = world.run(&["fork", "projects/scanner"]);
        let mut after = snapshot(&world.collection().root());
        after.remove(".antiq/lock");
        run.expect_code(3)
            .text_only(|out| {
                assert_eq!(out, "");
                assert!(
                    run.stderr
                        .starts_with("Skipped projects/scanner: 9000 TB needed, "),
                    "{:?}",
                    run.stderr
                );
            })
            .json_only(|json| {
                assert_eq!(json["forked"], false);
                assert_eq!(json["skipped"]["reason"], "no_space");
                assert_eq!(json["skipped"]["needed"], 9_000_000_000_000_000_u64);
            });
        assert_eq!(after, before);
    }
}

#[test]
fn a_damaged_version_with_a_listing_still_needs_room() {
    let world = world(Mode::Json, only_new);
    oversize_listing(&world);
    world
        .collection()
        .damage_corrupt_checksum("projects/scanner", NEW);
    let run = world.run(&["fork", "projects/scanner"]);
    run.expect_code(3).json_only(|json| {
        assert_eq!(json["forked"], false);
        assert_eq!(json["skipped"]["reason"], "no_space");
    });
}

#[test]
fn a_dry_run_that_finds_no_room_names_the_skip_and_exits_3() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, only_new);
        oversize_listing(&world);
        let run = world.run(&["fork", "projects/scanner", "-n"]);
        run.expect_code(3)
            .text_only(|out| {
                assert!(
                    out.starts_with(&format!(
                        "Would skip projects/scanner (version {NEW}): 9000 TB needed, "
                    )),
                    "{out:?}"
                );
            })
            .json_only(|json| {
                assert_eq!(json["actions"][0]["skipped"]["reason"], "no_space");
            });
    }
}

// ----- the dry run

#[test]
fn a_dry_run_names_the_version_the_destination_and_the_checks_and_changes_nothing() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let before = snapshot(&world.collection().root());
        let run = world.run(&["fork", "projects/scanner", "--name", "tools/copy", "-n"]);
        run.expect_code(0)
            .keys(&["format", "command", "dry_run", "actions", "warnings"])
            .text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(
                    lines[0],
                    format!("Would fork projects/scanner (version {NEW}) to tools/copy")
                );
                assert_eq!(lines[1], format!("  check the version {NEW} is sound"));
                assert_eq!(lines[2], "  check the destination tools/copy is free");
                assert!(
                    lines[3].starts_with("  check there is room for "),
                    "{out:?}"
                );
                assert!(lines[4].contains("(not run by -n)"), "{out:?}");
                assert!(lines[5].contains("(not run by -n)"), "{out:?}");
                assert_eq!(lines.len(), 6, "{out:?}");
            })
            .json_only(|json| {
                let action = &json["actions"][0];
                assert_eq!(action["kind"], "fork");
                assert_eq!(action["stamp"], NEW);
                assert_eq!(action["path"], "tools/copy");
                assert_eq!(
                    action["checks"],
                    serde_json::json!([
                        "version",
                        "destination",
                        "free_space",
                        "member_names",
                        "extracted_tree"
                    ])
                );
            });
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

#[test]
fn the_repairs_run_first_and_a_real_fork_reports_them() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let leftover = world.collection().path(".antiq/tmp/leftover");
        fs::write(&leftover, "x").expect("the leftover");

        let run = world.run(&["fork", "projects/scanner"]);

        run.expect_code(0);
        assert!(!leftover.exists());
        assert!(world.collection().path("projects/scanner/README").exists());
        run.text_only(|_| {
            assert_eq!(run.stderr, "note: repaired: delete .antiq/tmp/leftover\n");
        });
        run.json_only(|json| {
            let repairs = json["repairs"].as_array().expect("a list of repairs");
            assert_eq!(repairs.len(), 1, "{json}");
            assert_eq!(repairs[0]["kind"], "repair");
            assert_eq!(repairs[0]["repair"], "delete_temporary");
            assert_eq!(repairs[0]["entry"], "leftover");
        });
    }
}

// ----- the lock

#[test]
fn a_held_lock_stops_a_real_run_and_is_named_by_a_dry_run() {
    let world = world(Mode::Json, scanner);
    let _lock = hold_lock(&world.collection().root());
    let run = world.run(&["fork", "projects/scanner"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("lock_held"));
    run.json_only(|json| {
        assert!(json["error"]["pid"].is_u64(), "{json}");
        assert!(json["error"]["started"].is_string(), "{json}");
    });
    assert!(!world.collection().path("projects/scanner").exists());
}

#[test]
fn a_dry_run_with_the_lock_held_names_the_holder_and_lists_no_repairs() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let _lock = hold_lock(&world.collection().root());
        let before = snapshot(&world.collection().root());
        let run = world.run(&["fork", "projects/scanner", "-n"]);
        run.expect_code(0)
            .keys(&["format", "command", "dry_run", "lock_held", "actions"]);
        run.text_only(|out| {
            let line = format!("The lock is held by pid {}, started ", std::process::id());
            assert!(out.starts_with(&line), "{out:?}");
            assert_eq!(out.lines().count(), 1);
        });
        run.json_only(|json| {
            assert_eq!(json["lock_held"]["pid"], std::process::id());
            assert_eq!(json["actions"], serde_json::json!([]));
        });
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

// ----- the console

#[test]
fn a_terminal_names_the_version_by_its_local_time() {
    let world = world(Mode::Text, scanner);
    let run = world.run_tty(&["fork", "projects/scanner", "--name", "tools/copy"]);
    run.expect_code(0);
    // 09:02:11 UTC is 22:02:11 at +13:00.
    assert!(
        run.stdout
            .contains("Forked projects/scanner (version 2026-11-04 22:02:11) to tools/copy"),
        "{:?}",
        run.stdout
    );
}
