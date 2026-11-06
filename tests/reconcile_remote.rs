// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The remote part of `antiq reconcile` with a path remote, seen from
//! outside the binary. Objects are laid down with the library, since the
//! commands that upload them are other commands' business.

mod support;

use antiq::meta::{Lifecycle, PayloadKind};
use antiq::tracking::{EntryState, LocalPayload};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::objects::{archive_with_members, entry, id, metadata, put_entry, put_meta, snapshot};
use support::tree::{COLLECTION_ID, TempCollection};

/// The archive file the helpers of `support::objects` name for version 1.
const ARCHIVE: &str = "0000000000000001-payload.tar.zst";

struct World {
    temp: TempCollection,
    remotes: tempfile::TempDir,
}

/// Where an object's archive is.
#[derive(Clone, Copy, PartialEq)]
enum Archive {
    /// On the remote only.
    Remote,
    /// Held here and on the remote.
    Both,
    /// Held here only, as before its upload.
    Here,
    /// Nowhere.
    Nowhere,
}

fn world() -> World {
    let temp = TempCollection::minimal();
    fs::write(
        temp.root().join(".antiq/config.toml"),
        format!("[collection]\nid = \"{COLLECTION_ID}\"\n\n[machine]\nname = \"laptop\"\n"),
    )
    .expect("the config");
    let world = World {
        temp,
        remotes: tempfile::tempdir().expect("a directory for the remote"),
    };
    let root = world.remote_root();
    let run = world.antiq(&[
        "remote",
        "add",
        "--init",
        root.to_str().expect("a UTF-8 path"),
    ]);
    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    world
}

impl World {
    fn root(&self) -> &Path {
        self.temp.root()
    }

    fn remote_root(&self) -> PathBuf {
        self.remotes.path().join("antiq")
    }

    fn local_dir(&self, number: u8) -> PathBuf {
        self.root()
            .join(".antiq/objects")
            .join(id(number).to_string())
    }

    fn remote_dir(&self, number: u8) -> PathBuf {
        self.remote_root()
            .join("objects")
            .join(id(number).to_string())
    }

    fn antiq(&self, args: &[&str]) -> Run {
        Antiq::new().arg("-C").arg(self.root()).args(args).run()
    }

    fn reconcile(&self, args: &[&str]) -> Run {
        let mut all = vec!["reconcile"];
        all.extend_from_slice(args);
        self.antiq(&all)
    }

    fn reconcile_json(&self, args: &[&str]) -> Run {
        let mut all = vec!["--json", "reconcile"];
        all.extend_from_slice(args);
        self.antiq(&all)
    }

    /// An archived piece `at`, whose archive is where `archive` says.
    fn archived(&self, number: u8, at: &str, archive: Archive) {
        let payload = archive_with_members(self.root(), number, 1, 10, &["a.txt"]);
        let listing = payload.listing.clone().expect("a listing");
        let known = metadata(
            number,
            at,
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![payload],
        );
        put_meta(self.root(), &known);
        let mut tracked = entry(number, at, EntryState::Gone);
        if matches!(archive, Archive::Both | Archive::Here) {
            fs::write(self.local_dir(number).join(ARCHIVE), b"archive").expect("the archive");
            tracked.local_payloads = vec![LocalPayload {
                kind: PayloadKind::Archive,
                version: 1,
                file: antiq::meta::FileName::new(ARCHIVE).expect("a name"),
            }];
        }
        put_entry(self.root(), &tracked);

        let there = self.remote_dir(number);
        fs::create_dir_all(&there).expect("the remote object directory");
        fs::copy(self.local_dir(number).join("meta.1"), there.join("meta.1")).expect("meta");
        fs::copy(
            self.local_dir(number).join(listing.as_str()),
            there.join(listing.as_str()),
        )
        .expect("the listing");
        if matches!(archive, Archive::Remote | Archive::Both) {
            fs::write(there.join(ARCHIVE), b"archive").expect("the archive");
        }
    }

    fn entry(&self, number: u8) -> Value {
        let path = self
            .root()
            .join(".antiq/tracking")
            .join(format!("{}.json", id(number)));
        serde_json::from_slice(&fs::read(path).expect("the entry")).expect("json")
    }

    fn missing(&self, number: u8) -> bool {
        self.entry(number)["missing_on_remote"]
            .as_bool()
            .unwrap_or(false)
    }

    fn state_file(&self) -> PathBuf {
        self.root().join(".antiq/remote-state.json")
    }

    fn take_remote_away(&self) {
        fs::rename(self.remote_root(), self.remotes.path().join("elsewhere")).expect("moved");
    }

    fn break_remote(&self) {
        fs::write(self.remote_root().join("antiq-remote.json"), "not json").expect("broken");
    }

    fn away_since(&self, days: i64) {
        let since = Timestamp::now() - SignedDuration::from_hours(24 * days);
        fs::write(
            self.state_file(),
            format!("{{\"last_reachable\": \"{since}\"}}"),
        )
        .expect("written");
    }

    /// A leftover temp file on the remote that is older than the grace.
    fn old_leftover(&self, number: u8, name: &str) {
        let path = self.remote_dir(number).join(name);
        fs::write(&path, b"half").expect("a leftover");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("opens")
            .set_modified(old)
            .expect("sets the time");
    }
}

fn succeeded(run: &Run) {
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
}

// =========================================================
// Marking pieces whose archive is gone from the remote
// =========================================================

#[test]
fn a_piece_whose_archive_left_the_remote_is_marked() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    world.archived(2, "beta", Archive::Remote);

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(
        run.json()["marked"],
        json!([{"id": id(1).to_string(), "path": "alpha"}])
    );
    assert!(world.missing(1));
    assert!(!world.missing(2));
}

#[test]
fn the_console_names_a_marked_piece() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);

    let run = world.reconcile(&[]);

    succeeded(&run);
    assert_eq!(
        run.stdout,
        "Rebuilt the index: 1 written, 0 removed\n\
         Remote: 0 pulled, 0 removed\n\
         Marked missing on the remote: alpha\n"
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn an_object_the_remote_lacks_altogether_is_marked() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    fs::remove_dir_all(world.remote_dir(1)).expect("gone");

    succeeded(&world.reconcile(&[]));

    assert!(world.missing(1));
}

#[test]
fn a_mark_is_cleared_when_the_archive_is_back() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    succeeded(&world.reconcile(&[]));
    fs::write(world.remote_dir(1).join(ARCHIVE), b"archive").expect("back");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(
        run.json()["unmarked"],
        json!([{"id": id(1).to_string(), "path": "alpha"}])
    );
    assert!(!world.missing(1));
}

#[test]
fn the_console_names_a_cleared_mark() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    succeeded(&world.reconcile(&[]));
    fs::write(world.remote_dir(1).join(ARCHIVE), b"archive").expect("back");

    let run = world.reconcile(&[]);

    succeeded(&run);
    assert!(
        run.stdout.contains("Archive back on the remote: alpha\n"),
        "{}",
        run.stdout
    );
}

#[test]
fn a_dry_run_lists_a_mark_to_clear() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    succeeded(&world.reconcile(&[]));
    fs::write(world.remote_dir(1).join(ARCHIVE), b"archive").expect("back");

    let run = world.reconcile(&["--dry-run"]);

    succeeded(&run);
    assert!(
        run.stdout.contains("clear the missing mark of alpha\n"),
        "{}",
        run.stdout
    );
    assert!(world.missing(1), "a dry run clears nothing");
}

#[test]
fn a_piece_that_holds_its_archive_is_never_marked() {
    let world = world();
    world.archived(1, "alpha", Archive::Both);
    fs::remove_file(world.remote_dir(1).join(ARCHIVE)).expect("gone from the remote");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["marked"], json!([]));
    assert!(!world.missing(1));
}

#[test]
fn a_recorded_change_that_no_sync_committed_is_not_marked() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    fs::copy(
        world.local_dir(1).join("meta.1"),
        world.local_dir(1).join("meta.pending"),
    )
    .expect("a recorded change");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["marked"], json!([]));
}

#[test]
fn a_piece_that_is_already_marked_is_not_reported_again() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    succeeded(&world.reconcile(&[]));

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["marked"], json!([]));
    assert!(world.missing(1));
}

// =========================================================
// The remote's state
// =========================================================

#[test]
fn a_reachable_remote_is_recorded() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);

    succeeded(&world.reconcile(&[]));

    let state: Value =
        serde_json::from_slice(&fs::read(world.state_file()).expect("state")).expect("json");
    assert!(state["last_reachable"].is_string(), "{state}");
}

#[test]
fn an_away_remote_ends_the_run_with_four_after_the_local_part() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    fs::remove_dir_all(world.root().join(".antiq/index")).expect("the index is deleted");
    world.take_remote_away();

    let run = world.reconcile(&[]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout
            .starts_with("Rebuilt the index: 1 written, 0 removed\n"),
        "{}",
        run.stdout
    );
    assert!(
        world
            .root()
            .join(format!(".antiq/index/{}.json", id(1)))
            .exists()
    );
}

#[test]
fn if_reachable_turns_a_fresh_away_into_zero() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.take_remote_away();
    world.away_since(1);

    let run = world.reconcile(&["--if-reachable"]);

    succeeded(&run);
}

#[test]
fn an_overdue_away_remote_stays_at_four_whatever_the_flag() {
    let world = world();
    world.archived(1, "alpha", Archive::Here);
    world.take_remote_away();
    world.away_since(8);

    let run = world.reconcile(&["--if-reachable"]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
}

#[test]
fn under_json_an_away_remote_prints_the_error_envelope_only() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.take_remote_away();

    let run = world.reconcile_json(&[]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.error_kind(), "remote_away");
    assert_eq!(run.stdout.lines().count(), 1, "{}", run.stdout);
}

#[test]
fn an_away_remote_is_recorded_as_away() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.take_remote_away();

    world.reconcile(&[]);

    let state: Value =
        serde_json::from_slice(&fs::read(world.state_file()).expect("state")).expect("json");
    assert!(state["away_first_seen"].is_string(), "{state}");
}

#[test]
fn a_broken_remote_ends_the_run_with_its_kind() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.break_remote();

    let run = world.reconcile_json(&[]);

    assert_eq!(run.code(), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.error_kind(), "remote_broken");
}

#[test]
fn a_broken_remote_leaves_marks_alone() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    world.break_remote();

    world.reconcile(&[]);

    assert!(!world.missing(1));
}

// =========================================================
// Pulling metadata and cleaning up
// =========================================================

#[test]
fn metadata_only_the_remote_has_is_pulled() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    fs::remove_file(world.local_dir(1).join("meta.1")).expect("removed");
    fs::copy(
        world.remote_dir(1).join("meta.1"),
        world.remote_dir(1).join("meta.2"),
    )
    .expect("a newer version");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["remote"]["pulled"], 2);
    assert!(world.local_dir(1).join("meta.2").exists());
}

#[test]
fn an_old_leftover_on_the_remote_is_removed() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.old_leftover(1, ".meta.2.studio.part");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["remote"]["removed"], 1);
    assert!(!world.remote_dir(1).join(".meta.2.studio.part").exists());
}

#[test]
fn the_temp_file_of_a_pending_upload_of_this_machine_is_kept() {
    let world = world();
    world.archived(1, "alpha", Archive::Here);
    let temp = format!(".{ARCHIVE}.laptop.part");
    world.old_leftover(1, &temp);
    world.old_leftover(1, &format!(".{ARCHIVE}.studio.part"));

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert!(world.remote_dir(1).join(&temp).exists());
    assert!(
        !world
            .remote_dir(1)
            .join(format!(".{ARCHIVE}.studio.part"))
            .exists()
    );
}

// =========================================================
// Dry runs
// =========================================================

#[test]
fn a_dry_run_lists_marks_and_leftovers_and_changes_nothing() {
    let world = world();
    world.archived(1, "alpha", Archive::Nowhere);
    world.old_leftover(1, ".meta.2.studio.part");
    let here = snapshot(world.root());
    let there = snapshot(&world.remote_root());

    let run = world.reconcile(&["--dry-run"]);

    succeeded(&run);
    assert!(
        run.stdout.contains("mark alpha as missing on the remote"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!(
            "remove temp file .meta.2.studio.part of {} (age unknown in a dry run)",
            id(1)
        )),
        "{}",
        run.stdout
    );
    let mut after = snapshot(world.root());
    let mut before = here;
    after.remove(".antiq/lock");
    before.remove(".antiq/lock");
    assert_eq!(after, before);
    assert_eq!(snapshot(&world.remote_root()), there);
    assert!(!world.state_file().exists());
}

#[test]
fn a_dry_run_with_an_away_remote_lists_the_local_part_and_ends_with_four() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.take_remote_away();

    let run = world.reconcile(&["--dry-run"]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("rebuild the index"), "{}", run.stdout);
}

#[test]
fn a_dry_run_gets_the_exit_code_of_the_real_run_when_overdue() {
    let world = world();
    world.archived(1, "alpha", Archive::Here);
    world.take_remote_away();
    world.away_since(8);

    let run = world.reconcile(&["--dry-run", "--if-reachable"]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
}

// =========================================================
// Versions another machine wrote
// =========================================================

/// Version 2 of object `number`, written by `machine`.
fn version_by(world: &World, number: u8, machine: &str) {
    let mut document: Value =
        serde_json::from_slice(&fs::read(world.local_dir(number).join("meta.1")).expect("meta.1"))
            .expect("json");
    document["history"] = json!([{
        "time": "2026-02-01T00:00:00Z",
        "machine": machine,
        "action": "rename"
    }]);
    let bytes = serde_json::to_vec(&document).expect("json");
    fs::write(world.remote_dir(number).join("meta.2"), bytes).expect("written");
}

#[test]
fn a_version_another_machine_wrote_is_reported_with_the_machine() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    version_by(&world, 1, "studio");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(
        run.json()["changed_elsewhere"],
        json!([{"id": id(1).to_string(), "path": "alpha", "version": 2, "machine": "studio"}])
    );
}

#[test]
fn the_console_notes_a_version_another_machine_wrote() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    version_by(&world, 1, "studio");

    let run = world.reconcile(&[]);

    succeeded(&run);
    assert_eq!(
        run.stderr,
        "note: `alpha` has version 2 from studio, which this machine has not taken\n"
    );
}

#[test]
fn a_version_this_machine_took_out_is_not_reported() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    version_by(&world, 1, "studio");
    let mut tracked = entry(1, "alpha", EntryState::Gone);
    tracked.version_taken_out = antiq::tracking::VersionTakenOut::Committed(2);
    put_entry(world.root(), &tracked);

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["changed_elsewhere"], json!([]));
}

#[test]
fn a_version_the_entry_lists_as_committed_is_not_reported() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    version_by(&world, 1, "studio");
    let mut tracked = entry(1, "alpha", EntryState::Gone);
    tracked.committed_versions = vec![1, 2];
    put_entry(world.root(), &tracked);

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["changed_elsewhere"], json!([]));
}

#[test]
fn a_version_that_cannot_be_read_is_listed_and_the_run_exits_zero() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    fs::write(world.remote_dir(1).join("meta.2"), b"{ not json").expect("damaged");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    let listed = run.json()["unreadable_versions"].clone();
    assert_eq!(listed[0]["id"], id(1).to_string());
    assert_eq!(listed[0]["version"], 2);
    assert_eq!(run.json()["changed_elsewhere"], json!([]));
}

#[test]
fn an_object_with_nothing_readable_is_listed_and_the_run_goes_on() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.archived(2, "beta", Archive::Remote);
    fs::write(world.remote_dir(1).join("meta.1"), b"{ not json").expect("damaged");
    fs::write(world.local_dir(1).join("meta.1"), b"{ not json").expect("damaged");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    let listed = run.json()["unreadable_versions"].clone();
    assert_eq!(listed[0]["id"], id(1).to_string());
    assert_eq!(listed[0]["version"], 1);
}
