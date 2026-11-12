// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq rebuild` seen from outside the binary, on stores the scenario
//! harness writes by hand. The harness runs every command under
//! `TZ=Pacific/Auckland`. Every test runs in text and in JSON mode.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use support::cmd::{hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Outcome, Setup, World};

const PIECE: &str = "projects/scanner";
const STAMP: &str = "2026-11-04T090211";
const OLD_STAMP: &str = "2026-09-03T121500";
/// What the checksum file of an archive holds when it is a different file.
const OTHER_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn world(mode: Mode, build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), mode)
}

fn one_version(builder: &mut CollectionBuilder) {
    builder.version(
        PIECE,
        STAMP,
        &[("src/main.rs", "fn main() {}\n"), ("README", "scan\n")],
    );
}

fn two_versions(builder: &mut CollectionBuilder) {
    one_version(builder);
    builder.version(PIECE, OLD_STAMP, &[("README", "old\n")]);
}

fn both(build: fn(&mut CollectionBuilder), script: impl Fn(&World)) {
    for mode in [Mode::Text, Mode::Json] {
        script(&world(mode, build));
    }
}

// =========================================================
// The files of the version under test
// =========================================================

struct Files {
    archive: PathBuf,
    listing: PathBuf,
    checksum: PathBuf,
}

fn files(world: &World, stamp: &str) -> Files {
    let found = world.collection().version_files(PIECE, stamp);
    Files {
        archive: found.archive,
        listing: found.listing,
        checksum: found.checksum,
    }
}

fn decode(path: &Path) -> String {
    let bytes = fs::read(path).expect("the listing exists");
    String::from_utf8(zstd::stream::decode_all(&bytes[..]).expect("the listing decodes"))
        .expect("the listing is UTF-8")
}

/// Replaces the listing by `text`, compressed the way listings are.
fn write_listing(path: &Path, text: &str) {
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).expect("the encoder starts");
    encoder
        .include_checksum(true)
        .expect("the checksum is switched on");
    encoder
        .write_all(text.as_bytes())
        .expect("the listing is written");
    fs::write(path, encoder.finish().expect("the frame ends")).expect("the listing is replaced");
}

fn edit_listing(path: &Path, change: impl FnOnce(String) -> String) {
    let text = decode(path);
    write_listing(path, &change(text));
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn remove(path: &Path) {
    fs::remove_file(path).expect("the file exists");
}

/// The collection as it is, without the lock file that every locked run
/// leaves behind.
fn state(world: &World) -> std::collections::BTreeMap<String, support::tree::Seen> {
    let mut seen = snapshot(&world.collection().root());
    seen.remove(".antiq/lock");
    seen
}

/// The text of the error: stderr in text mode, the message and the fix in
/// JSON mode.
fn error_text(run: &Outcome) -> String {
    match run.mode {
        Mode::Text => run.stderr.clone(),
        Mode::Json => {
            let json: Value = serde_json::from_str(&run.stdout).expect("a JSON envelope");
            let message = json["error"]["message"]
                .as_str()
                .expect("the error has a message");
            match json["error"]["fix"].as_str() {
                Some(fix) => format!("{message}\n{fix}"),
                None => message.to_string(),
            }
        }
    }
}

fn assert_all_in(text: &str, parts: &[&str]) {
    for part in parts {
        assert!(text.contains(part), "`{part}` is not in {text:?}");
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .expect("a file name")
        .to_string_lossy()
        .into_owned()
}

/// What a refusal of a damaged version names as the ways out.
fn assert_ways_out(text: &str) {
    assert_all_in(
        text,
        &[
            &format!("antiq forget {PIECE} {STAMP}"),
            "copying the version back from the target",
        ],
    );
}

/// A refusal changes nothing and is an error of no kind of its own.
fn assert_refused(world: &World, args: &[&str]) -> Outcome {
    let before = state(world);
    let run = world.run(args);
    run.expect_code(1);
    assert_eq!(state(world), before, "a refusal changed the collection");
    run
}

// =========================================================
// The three missing-file cases
// =========================================================

#[test]
fn a_missing_listing_is_written_from_the_archive_after_the_hash_matches() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        let original = decode(&version.listing);
        let checksum = fs::read(&version.checksum).expect("the checksum file exists");
        let archive = fs::read(&version.archive).expect("the archive exists");
        remove(&version.listing);

        let run = world.run(&["rebuild", PIECE, STAMP]);
        run.expect_code(0).keys(&[
            "format", "command", "piece", "stamp", "version", "basis", "written", "repairs",
            "warnings",
        ]);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!(
                    "Rebuilt version {STAMP} of {PIECE}: wrote {}\n",
                    name(&version.listing)
                )
            );
            assert_eq!(
                run.stderr,
                "note: the archive is the one checksummed when it was stored\n"
            );
        });
        run.json_only(|json| {
            assert_eq!(json["piece"], PIECE);
            assert_eq!(json["stamp"], STAMP);
            assert_eq!(json["version"], "2026-11-04T09:02:11Z");
            assert_eq!(json["basis"], "checksum_file");
            assert_eq!(json["written"], serde_json::json!([name(&version.listing)]));
        });

        let rebuilt = decode(&version.listing);
        let lines: Vec<&str> = rebuilt.lines().collect();
        let at = lines
            .iter()
            .position(|line| line.starts_with("# rebuilt "))
            .expect("the listing has a rebuilt line");
        assert!(lines[at - 1].starts_with("# bytes "), "{lines:?}");
        let time = lines[at]["# rebuilt       ".len()..]
            .parse::<jiff::Timestamp>()
            .expect("the rebuilt line holds a time");
        let age = jiff::Timestamp::now().as_second() - time.as_second();
        assert!((0..120).contains(&age), "rebuilt {age} seconds ago");
        let without: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != at)
            .map(|(_, line)| *line)
            .collect();
        assert_eq!(format!("{}\n", without.join("\n")), original);

        assert_eq!(
            fs::read(&version.checksum).expect("the checksum file stays"),
            checksum
        );
        assert_eq!(
            fs::read(&version.archive).expect("the archive stays"),
            archive
        );
        assert!(!world.collection().path(".antiq/tmp/rebuild").exists());
    });
}

#[test]
fn a_missing_checksum_file_is_written_after_every_member_matches_the_listing() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        let checksum = fs::read(&version.checksum).expect("the checksum file exists");
        let listing = fs::read(&version.listing).expect("the listing exists");
        remove(&version.checksum);

        let run = world.run(&["rebuild", PIECE, STAMP]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!(
                    "Rebuilt version {STAMP} of {PIECE}: wrote {}\n",
                    name(&version.checksum)
                )
            );
            assert_eq!(
                run.stderr,
                "note: every member of the archive matches the listing\n"
            );
        });
        run.json_only(|json| {
            assert_eq!(json["basis"], "listing");
            assert_eq!(
                json["written"],
                serde_json::json!([name(&version.checksum)])
            );
        });
        assert_eq!(
            fs::read(&version.checksum).expect("the checksum file is written"),
            checksum
        );
        assert_eq!(
            fs::read(&version.listing).expect("the listing stays"),
            listing
        );
        assert!(!world.collection().path(".antiq/tmp/rebuild").exists());
    });
}

#[test]
fn with_both_missing_the_archive_is_read_and_both_files_are_written() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        let original = decode(&version.listing);
        let checksum = fs::read(&version.checksum).expect("the checksum file exists");
        remove(&version.listing);
        remove(&version.checksum);

        let run = world.run(&["rebuild", PIECE, STAMP]);
        run.expect_code(0);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!(
                    "Rebuilt version {STAMP} of {PIECE}: wrote {}, {}\n",
                    name(&version.listing),
                    name(&version.checksum)
                )
            );
            assert_eq!(
                run.stderr,
                "note: this shows only that the archive reads without error, not that it is unchanged\n"
            );
        });
        run.json_only(|json| {
            assert_eq!(json["basis"], "readable_only");
            assert_eq!(
                json["written"],
                serde_json::json!([name(&version.listing), name(&version.checksum)])
            );
        });
        assert_eq!(
            fs::read(&version.checksum).expect("the checksum file is written"),
            checksum
        );
        let rebuilt = decode(&version.listing);
        assert!(rebuilt.contains("\n# rebuilt "), "{rebuilt}");
        let without: String = rebuilt
            .split_inclusive('\n')
            .filter(|line| !line.starts_with("# rebuilt "))
            .collect();
        assert_eq!(without, original);
    });
}

#[test]
fn a_terminal_names_the_version_on_its_card_by_its_local_time() {
    let world = world(Mode::Text, one_version);
    remove(&files(&world, STAMP).listing);
    let run = world.run_tty(&["rebuild", PIECE, STAMP]);
    run.expect_code(0);
    // 09:02:11 UTC is 22:02:11 in Auckland. On a terminal the finished line
    // of the card names the version.
    assert!(
        run.plain().contains("version 2026-11-04 22:02:11"),
        "{:?}",
        run.stdout
    );
}

#[test]
fn a_rebuilt_version_is_no_longer_damaged() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        remove(&version.listing);
        remove(&version.checksum);
        world.run(&["rebuild", PIECE, STAMP]).expect_code(0);

        let run = world.run(&["find", PIECE]);
        run.expect_code(0);
        run.json_only(|json| {
            assert_eq!(json["versions"][0]["damage"], serde_json::json!([]));
            assert_eq!(json["versions"][0]["state"], "ok");
        });
        run.text_only(|out| assert!(!out.contains("damaged"), "{out}"));
    });
}

// =========================================================
// Refusals
// =========================================================

#[test]
fn a_checksum_mismatch_refuses_and_names_both_hashes() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        let expected = fs::read_to_string(&version.checksum).expect("the checksum file exists");
        let expected = &expected[..64];
        let mut archive = fs::read(&version.archive).expect("the archive exists");
        let middle = archive.len() / 2;
        archive[middle] ^= 0xff;
        fs::write(&version.archive, &archive).expect("the archive is changed");
        let found = hex(&archive);
        remove(&version.listing);

        let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
        let text = error_text(&run);
        assert_all_in(&text, &[&found, expected]);
        assert_ways_out(&text);
        assert!(!version.listing.exists());
    });
}

#[test]
fn a_member_that_differs_from_the_surviving_listing_refuses() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        edit_listing(&version.listing, |text| {
            text.replace(&hex(b"scan\n"), OTHER_HASH)
        });
        remove(&version.checksum);

        let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
        let text = error_text(&run);
        assert_all_in(&text, &["differs from its listing", "README"]);
        assert_ways_out(&text);
        assert!(!version.checksum.exists());
    });
}

#[test]
fn an_archive_of_another_size_than_its_listing_records_cannot_be_repaired() {
    for remove_checksum in [false, true] {
        both(one_version, |world| {
            let version = files(world, STAMP);
            let mut archive = fs::read(&version.archive).expect("the archive exists");
            archive.push(0);
            fs::write(&version.archive, archive).expect("the archive is longer");
            if remove_checksum {
                remove(&version.checksum);
            }

            let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
            let text = error_text(&run);
            assert_all_in(&text, &["bytes, its listing records", "cannot repair"]);
            assert_ways_out(&text);
        });
    }
}

#[test]
fn a_corrupt_checksum_file_is_not_overwritten() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        fs::write(&version.checksum, "not a checksum line\n").expect("the file is corrupted");
        remove(&version.listing);

        let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
        let text = error_text(&run);
        assert_all_in(
            &text,
            &[&name(&version.checksum), "does not overwrite", "delete it"],
        );
        assert_ways_out(&text);
        assert_eq!(
            fs::read(&version.checksum).expect("the file stays"),
            b"not a checksum line\n"
        );
    });
}

#[test]
fn a_corrupt_listing_is_not_overwritten_with_the_checksum_file_present_or_missing() {
    type Corrupt = fn(&Path);
    let heads: Corrupt = |path| {
        edit_listing(path, |text| {
            text.lines()
                .filter(|line| !line.starts_with("# piece "))
                .map(|line| format!("{line}\n"))
                .collect()
        });
    };
    let bodies: Corrupt = |path| edit_listing(path, |text| format!("{text}not a line\n"));
    for corrupt in [heads, bodies] {
        for remove_checksum in [false, true] {
            both(one_version, |world| {
                let version = files(world, STAMP);
                corrupt(&version.listing);
                let damaged = fs::read(&version.listing).expect("the listing exists");
                if remove_checksum {
                    remove(&version.checksum);
                }

                let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
                let text = error_text(&run);
                assert_all_in(&text, &[&name(&version.listing), "does not overwrite"]);
                assert_ways_out(&text);
                assert_eq!(fs::read(&version.listing).expect("it stays"), damaged);
            });
        }
    }
}

#[test]
fn a_version_that_is_not_damaged_is_refused() {
    both(one_version, |world| {
        let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
        assert_all_in(&error_text(&run), &["is not damaged", "nothing to rebuild"]);
    });
}

#[test]
fn an_incomplete_version_is_refused_naming_forget() {
    both(two_versions, |world| {
        remove(&files(world, OLD_STAMP).archive);
        let run = assert_refused(world, &["rebuild", PIECE, OLD_STAMP]);
        assert_all_in(
            &error_text(&run),
            &[
                "has no archive",
                &format!("antiq forget {PIECE} {OLD_STAMP}"),
            ],
        );
    });
}

#[test]
fn a_piece_whose_only_version_is_incomplete_is_unknown_to_rebuild() {
    both(one_version, |world| {
        remove(&files(world, STAMP).archive);
        let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
        run.json_only(|_| assert_eq!(run.error_kind().as_deref(), Some("unknown_reference")));
        assert_all_in(
            &error_text(&run),
            &["holds only incomplete versions", "forget"],
        );
    });
}

#[test]
fn the_path_of_an_incomplete_piece_is_not_taken_for_a_piece_that_fits_it_loosely() {
    let build = |builder: &mut CollectionBuilder| {
        one_version(builder);
        builder.version("projects/scanner-next", STAMP, &[("README", "next\n")]);
    };
    both(build, |world| {
        remove(&files(world, STAMP).archive);
        let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
        run.json_only(|_| assert_eq!(run.error_kind().as_deref(), Some("unknown_reference")));
        assert_all_in(
            &error_text(&run),
            &["holds only incomplete versions", "forget"],
        );
    });
}

#[test]
fn a_listing_of_a_newer_format_is_refused_also_beside_a_missing_or_corrupt_checksum_file() {
    for checksum in ["intact", "missing", "corrupt"] {
        both(one_version, |world| {
            let version = files(world, STAMP);
            edit_listing(&version.listing, |text| {
                text.replacen("# antiq listing 1", "# antiq listing 2", 1)
            });
            match checksum {
                "missing" => remove(&version.checksum),
                "corrupt" => fs::write(&version.checksum, "garbage\n").expect("written"),
                _ => {
                    // The intact checksum file is the only damage-free case:
                    // the version is not damaged but of a newer format.
                }
            }
            let listing = fs::read(&version.listing).expect("the listing exists");

            let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
            run.json_only(|_| assert_eq!(run.error_kind().as_deref(), Some("format_too_new")));
            assert_all_in(&error_text(&run), &["newer format"]);
            assert_eq!(fs::read(&version.listing).expect("it stays"), listing);
        });
    }
}

#[test]
fn a_doubled_slot_is_refused_naming_the_files_to_remove_by_hand() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        let twin = version
            .listing
            .with_file_name(format!("other-name--{STAMP}.sha256.zst"));
        fs::copy(&version.listing, &twin).expect("the listing is copied");
        remove(&version.checksum);

        let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
        let text = error_text(&run);
        assert_all_in(&text, &[&name(&version.listing), &name(&twin), "by hand"]);
        assert_ways_out(&text);
        assert!(!version.checksum.exists());
    });
}

#[test]
fn an_archive_that_does_not_read_is_refused_although_its_checksum_matches() {
    for cut in [false, true] {
        both(one_version, |world| {
            let version = files(world, STAMP);
            let mut content = b"this is not a zstd stream".to_vec();
            if cut {
                content = fs::read(&version.archive).expect("the archive exists");
                content.truncate(content.len() - 8);
            }
            fs::write(&version.archive, &content).expect("the archive is replaced");
            fs::write(
                &version.checksum,
                format!("{}  {}\n", hex(&content), name(&version.archive)),
            )
            .expect("the checksum file is rewritten");
            remove(&version.listing);

            let run = assert_refused(world, &["rebuild", PIECE, STAMP]);
            let text = error_text(&run);
            assert_all_in(&text, &["cannot rebuild version", "damaged"]);
            assert_ways_out(&text);
            assert!(!version.listing.exists());
        });
    }
}

// =========================================================
// The command line
// =========================================================

#[test]
fn an_empty_piece_or_version_is_a_usage_error() {
    both(one_version, |world| {
        for args in [["rebuild", "", STAMP], ["rebuild", PIECE, ""]] {
            for dry in [false, true] {
                let mut args = args.to_vec();
                if dry {
                    args.push("-n");
                }
                let run = assert_refused_with(world, &args, 2);
                run.json_only(|_| assert_eq!(run.error_kind().as_deref(), Some("usage")));
            }
        }
    });
}

fn assert_refused_with(world: &World, args: &[&str], code: i32) -> Outcome {
    let before = state(world);
    let run = world.run(args);
    run.expect_code(code);
    assert_eq!(state(world), before, "the run changed the collection");
    run
}

#[test]
fn an_unknown_and_an_ambiguous_version_and_an_unknown_piece_are_refused() {
    both(two_versions, |world| {
        let run = assert_refused(world, &["rebuild", PIECE, "2020"]);
        assert_all_in(
            &error_text(&run),
            &[
                "`2020` matches no version",
                &format!("there are {STAMP}, {OLD_STAMP}"),
            ],
        );

        let run = assert_refused(world, &["rebuild", PIECE, "2026"]);
        assert_all_in(
            &error_text(&run),
            &["names no single version", &format!("{STAMP}, {OLD_STAMP}")],
        );

        let run = assert_refused(world, &["rebuild", "zzzzzz", STAMP]);
        run.json_only(|_| assert_eq!(run.error_kind().as_deref(), Some("unknown_reference")));
    });
}

// =========================================================
// Dry run
// =========================================================

#[test]
fn a_dry_run_says_what_it_would_check_and_write_and_changes_nothing() {
    for (case, remove_listing, remove_checksum, would) in [
        (
            "checksum_file",
            true,
            false,
            "Would check {archive} against {checksum} and write {listing}",
        ),
        (
            "listing",
            false,
            true,
            "Would check every member of {archive} against {listing} and write {checksum}",
        ),
        (
            "readable_only",
            true,
            true,
            "Would read {archive} to its end and write {listing}, {checksum}",
        ),
    ] {
        both(one_version, |world| {
            let version = files(world, STAMP);
            if remove_listing {
                remove(&version.listing);
            }
            if remove_checksum {
                remove(&version.checksum);
            }
            let line = would
                .replace("{archive}", &name(&version.archive))
                .replace("{listing}", &name(&version.listing))
                .replace("{checksum}", &name(&version.checksum));
            let before = state(world);

            let run = world.run(&["rebuild", PIECE, STAMP, "-n"]);
            run.expect_code(0)
                .keys(&["format", "command", "dry_run", "actions", "warnings"]);
            run.text_only(|out| assert_eq!(out, format!("{line}\n")));
            run.json_only(|json| {
                assert_eq!(json["dry_run"], true);
                let action = &json["actions"][0];
                assert_eq!(action["kind"], "rebuild_version");
                assert_eq!(action["piece"], PIECE);
                assert_eq!(action["stamp"], STAMP);
                assert_eq!(action["basis"], case);
                assert_eq!(json["actions"].as_array().expect("a list").len(), 1);
            });
            assert_eq!(state(world), before);
        });
    }
}

#[test]
fn a_dry_run_does_not_hash_the_archive() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        let mut archive = fs::read(&version.archive).expect("the archive exists");
        let middle = archive.len() / 2;
        archive[middle] ^= 0xff;
        fs::write(&version.archive, archive).expect("the archive is changed");
        remove(&version.listing);

        let before = state(world);
        let run = world.run(&["rebuild", PIECE, STAMP, "-n"]);
        run.expect_code(0);
        run.text_only(|out| assert!(out.starts_with("Would check "), "{out}"));
        assert_eq!(state(world), before);
        world.run(&["rebuild", PIECE, STAMP]).expect_code(1);
    });
}

#[test]
fn a_dry_run_refuses_what_the_sidecars_and_sizes_show() {
    both(one_version, |world| {
        let run = assert_refused(world, &["rebuild", PIECE, STAMP, "-n"]);
        assert_all_in(&error_text(&run), &["is not damaged"]);

        let version = files(world, STAMP);
        fs::write(&version.checksum, "not a checksum line\n").expect("corrupted");
        let run = assert_refused(world, &["rebuild", PIECE, STAMP, "-n"]);
        assert_all_in(&error_text(&run), &["does not overwrite"]);
    });
}

// =========================================================
// The guard around the run
// =========================================================

#[test]
fn a_held_lock_fails_the_run_and_changes_nothing() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, one_version);
        remove(&files(&world, STAMP).listing);
        let _lock = hold_lock(&world.collection().root());
        let before = state(&world);
        let run = world.run(&["rebuild", PIECE, STAMP]);
        run.expect_code(1);
        run.json_only(|json| {
            assert_eq!(run.error_kind().as_deref(), Some("lock_held"));
            assert_eq!(json["error"]["pid"], std::process::id());
            assert!(json["error"]["started"].is_string());
        });
        assert_eq!(state(&world), before);
    }
}

#[test]
fn a_dry_run_that_finds_the_lock_held_names_the_holder_and_lists_no_repairs() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, one_version);
        remove(&files(&world, STAMP).listing);
        fs::write(world.collection().path(".antiq/tmp/leftover"), "x").expect("written");
        let _lock = hold_lock(&world.collection().root());
        let before = snapshot(&world.collection().root());

        let run = world.run(&["rebuild", PIECE, STAMP, "-n"]);
        run.expect_code(0)
            .keys(&["format", "command", "dry_run", "lock_held", "actions"]);
        run.text_only(|out| {
            let line = format!("The lock is held by pid {}, started ", std::process::id());
            assert!(out.starts_with(&line), "{out:?}");
            assert!(out.ends_with(". No repairs are listed.\n"), "{out:?}");
            assert_eq!(out.lines().count(), 1);
        });
        run.json_only(|json| {
            assert_eq!(json["lock_held"]["pid"], std::process::id());
            assert_eq!(json["actions"], Value::Array(Vec::new()));
        });
        assert_eq!(run.stderr, "");
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

#[test]
fn the_repairs_run_first_and_a_dry_run_lists_them_before_the_rebuild() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, one_version);
        remove(&files(&world, STAMP).listing);
        let leftover = world.collection().path(".antiq/tmp/leftover");
        fs::write(&leftover, "x").expect("the leftover is written");

        let plan = world.run(&["rebuild", PIECE, STAMP, "-n"]);
        plan.expect_code(0);
        plan.text_only(|out| {
            assert!(
                out.starts_with("Would delete .antiq/tmp/leftover\n"),
                "{out}"
            );
            assert!(out.contains("\nWould check "), "{out}");
        });
        plan.json_only(|json| {
            assert_eq!(json["actions"][0]["kind"], "repair");
            assert_eq!(json["actions"][1]["kind"], "rebuild_version");
        });
        assert!(leftover.exists());

        let run = world.run(&["rebuild", PIECE, STAMP]);
        run.expect_code(0);
        assert!(!leftover.exists());
        run.text_only(|_| {
            assert!(
                run.stderr
                    .starts_with("note: repaired: delete .antiq/tmp/leftover\n"),
                "{}",
                run.stderr
            );
        });
        run.json_only(|json| assert_eq!(json["repairs"][0]["repair"], "delete_temporary"));
    }
}

// =========================================================
// A failing write
// =========================================================

/// Takes the write permission from a folder and gives it back when
/// dropped, also after a panic.
struct ReadOnly {
    path: PathBuf,
}

impl ReadOnly {
    fn set(path: &Path) -> ReadOnly {
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).expect("the mode is set");
        let probe = path.join("probe");
        assert!(
            fs::write(&probe, "x").is_err(),
            "the folder is still writable: do not run the tests as root"
        );
        ReadOnly {
            path: path.to_path_buf(),
        }
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_write_that_fails_leaves_the_store_as_it_was_and_no_staging_behind() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        remove(&version.listing);
        let folder = version
            .archive
            .parent()
            .expect("a store folder")
            .to_path_buf();
        let guard = ReadOnly::set(&folder);
        let before = state(world);

        let run = world.run(&["rebuild", PIECE, STAMP]);
        run.expect_code(1);
        assert_all_in(&error_text(&run), &["cannot"]);
        assert_eq!(state(world), before);
        assert!(!world.collection().path(".antiq/tmp/rebuild").exists());
        drop(guard);
        assert!(!version.listing.exists());
    });
}

#[test]
fn a_staging_folder_that_cannot_be_made_fails_before_the_store_is_touched() {
    both(one_version, |world| {
        let version = files(world, STAMP);
        remove(&version.listing);
        let guard = ReadOnly::set(&world.collection().path(".antiq/tmp"));
        let before = state(world);

        let run = world.run(&["rebuild", PIECE, STAMP]);
        run.expect_code(1);
        assert_all_in(&error_text(&run), &["cannot create"]);
        assert_eq!(state(world), before);
        drop(guard);
        assert!(!version.listing.exists());
    });
}

#[test]
fn the_notes_on_the_settings_come_with_a_real_run_and_a_dry_run() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, one_version);
        remove(&files(&world, STAMP).listing);
        let config = world.collection().path(".antiq/config.toml");
        let mut text = fs::read_to_string(&config).expect("the config exists");
        text.push_str("colour = \"red\"\n");
        fs::write(&config, text).expect("the config is written");
        let note = "unknown setting `colour` under [collection], which antiq ignores";

        for dry in [true, false] {
            let args: &[&str] = if dry {
                &["rebuild", PIECE, STAMP, "-n"]
            } else {
                &["rebuild", PIECE, STAMP]
            };
            let run = world.run(args);
            run.expect_code(0);
            run.text_only(|_| assert!(run.stderr.contains(&format!("warning: {note}\n"))));
            run.json_only(|json| {
                assert_eq!(
                    json["warnings"],
                    serde_json::json!([{
                        "code": "antiq::config::unknown_key",
                        "message": note,
                        "fix": "Correct the name or remove the line in `.antiq/config.toml`."
                    }])
                );
            });
        }
    }
}

#[test]
fn a_search_word_that_fits_one_piece_still_names_the_piece_to_rebuild() {
    let world = world(Mode::Text, one_version);
    remove(&files(&world, STAMP).listing);
    world
        .run(&["rebuild", "scanner", STAMP, "-n"])
        .expect_code(0);
}

// =========================================================
// An archive name that is not UTF-8
// =========================================================

// APFS refuses a file name with such bytes, so the version can only exist
// where the filesystem takes arbitrary bytes.
#[cfg(target_os = "linux")]
mod raw_name {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use support::scenario::outsider::{listing_check, shasum_check, tar_extract};

    /// The stem of the version's files, with a byte that is not UTF-8.
    fn stem() -> Vec<u8> {
        [b"a\xffb--".as_slice(), STAMP.as_bytes()].concat()
    }

    fn named(suffix: &str) -> OsString {
        OsString::from_vec([stem().as_slice(), suffix.as_bytes()].concat())
    }

    /// The name as text, with U+FFFD for the byte that is not UTF-8.
    fn lossy(suffix: &str) -> String {
        named(suffix).to_string_lossy().into_owned()
    }

    /// The name as the console shows it.
    fn shown(suffix: &str) -> String {
        lossy(suffix).replace('\u{fffd}', "%FF")
    }

    fn replace_bytes(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        let at = bytes
            .windows(from.len())
            .position(|window| window == from)
            .expect("the bytes hold the name");
        [&bytes[..at], to, &bytes[at + from.len()..]].concat()
    }

    /// The files of the version under their new names.
    struct Raw {
        folder: PathBuf,
        archive: PathBuf,
        listing: PathBuf,
        checksum: PathBuf,
    }

    /// Renames the three files of the version to names that are not UTF-8
    /// and rewrites the checksum file to name the new archive, since the
    /// harness writes the old name into it.
    fn rename_to_raw(world: &World) -> Raw {
        let old = files(world, STAMP);
        let folder = old.archive.parent().expect("a store folder").to_path_buf();
        let raw = Raw {
            archive: folder.join(named(".tar.zst")),
            listing: folder.join(named(".sha256.zst")),
            checksum: folder.join(named(".tar.zst.sha256")),
            folder,
        };
        let content = replace_bytes(
            &fs::read(&old.checksum).expect("the checksum file exists"),
            name(&old.archive).as_bytes(),
            named(".tar.zst").as_encoded_bytes(),
        );
        fs::rename(&old.archive, &raw.archive).expect("the archive moves");
        fs::rename(&old.listing, &raw.listing).expect("the listing moves");
        fs::rename(&old.checksum, &raw.checksum).expect("the checksum file moves");
        fs::write(&raw.checksum, content).expect("the checksum file is written");
        raw
    }

    fn names_in(folder: &Path) -> Vec<OsString> {
        let mut names: Vec<OsString> = fs::read_dir(folder)
            .expect("the folder lists")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        names.sort();
        names
    }

    /// Removes the sidecars `missing` names, rebuilds, and checks what the
    /// store holds and what `shasum` and the listing make of it.
    fn rebuild_with_missing(missing: &[&str]) {
        both(one_version, |world| {
            let raw = rename_to_raw(world);
            let mut written = Vec::new();
            for (suffix, path) in [
                (".sha256.zst", &raw.listing),
                (".tar.zst.sha256", &raw.checksum),
            ] {
                if missing.contains(&suffix) {
                    remove(path);
                    written.push(suffix);
                }
            }
            let archive = fs::read(&raw.archive).expect("the archive exists");
            let expected_checksum = [
                format!("{}  ", hex(&archive)).as_bytes(),
                stem().as_slice(),
                b".tar.zst\n",
            ]
            .concat();

            let run = world.run(&["rebuild", PIECE, STAMP]);
            run.expect_code(0);
            run.text_only(|out| {
                let wrote: Vec<String> = written.iter().map(|suffix| shown(suffix)).collect();
                assert_eq!(
                    out,
                    format!(
                        "Rebuilt version {STAMP} of {PIECE}: wrote {}\n",
                        wrote.join(", ")
                    )
                );
            });
            run.json_only(|json| {
                let wrote: Vec<String> = written.iter().map(|suffix| lossy(suffix)).collect();
                assert_eq!(json["written"], serde_json::json!(wrote));
            });

            let mut expected = vec![
                named(".tar.zst"),
                named(".sha256.zst"),
                named(".tar.zst.sha256"),
            ];
            expected.sort();
            assert_eq!(
                names_in(&raw.folder),
                expected,
                "the sidecars sit beside the archive under its name"
            );
            assert_eq!(
                fs::read(&raw.checksum).expect("the checksum file exists"),
                expected_checksum
            );
            if written.contains(&".sha256.zst") {
                let decoded = zstd::stream::decode_all(
                    &fs::read(&raw.listing).expect("the listing exists")[..],
                )
                .expect("the listing decodes");
                let line = [b"# archive       ".as_slice(), &stem(), b".tar.zst\n"].concat();
                assert!(
                    decoded
                        .windows(line.len())
                        .any(|window| window == line.as_slice()),
                    "the head names the archive by its bytes"
                );
            }

            // The checks of the spec's hand verification, with the tools
            // of the machine.
            shasum_check(&raw.folder, &raw.checksum);
            let scratch = tempfile::tempdir().expect("a scratch directory");
            let unpacked = scratch.path().join("unpacked");
            tar_extract(&raw.archive, &unpacked);
            listing_check(
                &raw.listing,
                &unpacked.join("scanner"),
                &scratch.path().join("listing"),
            );

            let again = assert_refused(world, &["rebuild", PIECE, STAMP]);
            assert_all_in(&error_text(&again), &["is not damaged"]);
        });
    }

    #[test]
    fn a_missing_listing_is_written_under_the_name_of_the_archive() {
        rebuild_with_missing(&[".sha256.zst"]);
    }

    #[test]
    fn a_missing_checksum_file_is_written_under_the_name_of_the_archive() {
        rebuild_with_missing(&[".tar.zst.sha256"]);
    }

    #[test]
    fn two_missing_sidecars_are_both_written_under_the_name_of_the_archive() {
        rebuild_with_missing(&[".sha256.zst", ".tar.zst.sha256"]);
    }

    #[test]
    fn a_dry_run_shows_the_name_escaped_and_names_it_by_its_text_in_json() {
        both(one_version, |world| {
            let raw = rename_to_raw(world);
            remove(&raw.listing);
            remove(&raw.checksum);
            let before = state(world);

            let run = world.run(&["rebuild", PIECE, STAMP, "-n"]);
            run.expect_code(0);
            run.text_only(|out| {
                assert_eq!(
                    out,
                    format!(
                        "Would read {} to its end and write {}, {}\n",
                        shown(".tar.zst"),
                        shown(".sha256.zst"),
                        shown(".tar.zst.sha256")
                    )
                );
            });
            run.json_only(|json| {
                let action = &json["actions"][0];
                assert_eq!(action["archive"], lossy(".tar.zst"));
                assert_eq!(
                    action["write"],
                    serde_json::json!([lossy(".sha256.zst"), lossy(".tar.zst.sha256")])
                );
            });
            assert_eq!(state(world), before);
        });
    }

    /// What a rebuild of 1.4.0 left: a checksum file under the name with
    /// U+FFFD, holding that name.
    #[test]
    fn a_checksum_file_of_the_lossy_name_is_damaged_and_replaced_after_it_is_deleted() {
        both(one_version, |world| {
            let raw = rename_to_raw(world);
            remove(&raw.checksum);
            let archive = fs::read(&raw.archive).expect("the archive exists");
            let stale = raw.folder.join(lossy(".tar.zst.sha256"));
            fs::write(
                &stale,
                format!("{}  {}\n", hex(&archive), lossy(".tar.zst")),
            )
            .expect("the stale file is written");

            let refused = assert_refused(world, &["rebuild", PIECE, STAMP]);
            assert_all_in(&error_text(&refused), &["rebuild does not overwrite"]);

            remove(&stale);
            world.run(&["rebuild", PIECE, STAMP]).expect_code(0);
            shasum_check(&raw.folder, &raw.checksum);
        });
    }
}
