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
use support::pty::{Options, Pty};
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

/// A snapshot without the clock file a real run touches on the remote.
fn without_clock(
    mut snapshot: std::collections::BTreeMap<String, Option<Vec<u8>>>,
) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    snapshot.remove(".antiq-clock.laptop");
    snapshot
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

// =========================================================
// Pruning
// =========================================================

impl World {
    /// A live piece that is gone from its place: no folder, no archive.
    fn gone(&self, number: u8, at: &str) {
        let known = metadata(number, at, Lifecycle::Live, COLLECTION_ID, Vec::new());
        put_meta(self.root(), &known);
        put_entry(self.root(), &entry(number, at, EntryState::Gone));
    }

    /// `pads` archived pieces that the remote holds, from number 1 on, so
    /// that the pieces a test drops are a known share of the set.
    fn padded(pads: u8) -> World {
        let world = world();
        for number in 1..=pads {
            world.archived(number, &format!("pad{number}"), Archive::Remote);
        }
        world
    }

    fn tombstones(&self) -> Value {
        match fs::read(self.root().join(".antiq/pruned.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).expect("json"),
            Err(_) => json!({"format": 1, "pruned": []}),
        }
    }

    fn tracked(&self, number: u8) -> bool {
        self.root()
            .join(".antiq/tracking")
            .join(format!("{}.json", id(number)))
            .exists()
    }

    /// Replaces the entry of `number` by one that `change` altered.
    fn alter_entry(
        &self,
        number: u8,
        at: &str,
        change: impl FnOnce(&mut antiq::tracking::TrackingEntry),
    ) {
        let mut tracked = entry(number, at, EntryState::Gone);
        change(&mut tracked);
        put_entry(self.root(), &tracked);
    }
}

#[test]
fn a_piece_whose_archive_is_gone_from_the_remote_is_dropped() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);
    fs::write(
        world
            .root()
            .join(".antiq/tracking")
            .join(format!("{}.manifest.json", id(10))),
        "{}",
    )
    .expect("a manifest");
    let remote_before = without_clock(snapshot(&world.remote_root()));

    let run = world.reconcile_json(&["--prune"]);

    succeeded(&run);
    assert_eq!(
        run.json()["pruned"],
        json!([{"id": id(10).to_string(), "path": "alpha"}])
    );
    assert!(!world.tracked(10));
    assert!(!world.local_dir(10).exists());
    assert!(
        !world
            .root()
            .join(".antiq/tracking")
            .join(format!("{}.manifest.json", id(10)))
            .exists()
    );
    assert!(
        !world
            .root()
            .join(".antiq/index")
            .join(format!("{}.json", id(10)))
            .exists()
    );
    assert_eq!(
        world.tombstones()["pruned"],
        json!([{"id": id(10).to_string(), "version": 1}])
    );
    assert_eq!(without_clock(snapshot(&world.remote_root())), remote_before);
}

#[test]
fn the_console_names_a_dropped_piece() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);

    let run = world.reconcile(&["--prune"]);

    succeeded(&run);
    assert!(run.stdout.contains("Dropped: alpha\n"), "{}", run.stdout);
}

#[test]
fn a_gone_piece_is_dropped_without_a_remote() {
    let temp = TempCollection::minimal();
    let world = World {
        temp,
        remotes: tempfile::tempdir().expect("a directory"),
    };
    for number in 1..=9 {
        world.gone(number + 1, &format!("pad{number}"));
        fs::write(world.local_dir(number + 1).join("meta.pending"), "{}").expect("pending");
    }
    world.gone(1, "alpha");

    let run = world.reconcile_json(&["--prune"]);

    succeeded(&run);
    assert_eq!(
        run.json()["pruned"],
        json!([{"id": id(1).to_string(), "path": "alpha"}])
    );
}

#[test]
fn an_away_remote_drops_gone_pieces_only() {
    let world = World::padded(8);
    world.archived(9, "marked", Archive::Nowhere);
    succeeded(&world.reconcile(&[]));
    assert!(world.missing(9));
    world.gone(10, "alpha");
    world.take_remote_away();

    let run = world.reconcile(&["--prune"]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
    assert!(!world.tracked(10));
    assert!(world.tracked(9));
}

#[test]
fn a_piece_that_waits_for_its_upload_survives() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Here);

    succeeded(&world.reconcile(&["--prune", "--force"]));

    assert!(world.tracked(10));
}

#[test]
fn a_recorded_change_survives() {
    let world = World::padded(9);
    world.gone(10, "alpha");
    fs::copy(
        world.local_dir(10).join("meta.1"),
        world.local_dir(10).join("meta.pending"),
    )
    .expect("a recorded change");

    succeeded(&world.reconcile(&["--prune", "--force"]));

    assert!(world.tracked(10));
}

#[test]
fn a_piece_with_a_pending_decision_survives() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);
    world.alter_entry(10, "alpha", |tracked| {
        tracked.decide(antiq::tracking::DecisionKind::ReplacingWithLoss);
    });

    succeeded(&world.reconcile(&["--prune", "--force"]));

    assert!(world.tracked(10));
}

#[test]
fn an_archive_file_in_the_object_directory_keeps_the_piece() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);
    fs::write(world.local_dir(10).join(ARCHIVE), b"archive").expect("an older archive");

    succeeded(&world.reconcile(&["--prune", "--force"]));

    assert!(world.tracked(10));
}

#[test]
fn an_unearthed_piece_with_its_folder_survives() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);
    world.alter_entry(10, "alpha", |tracked| {
        tracked.state = EntryState::Live;
    });
    fs::create_dir(world.root().join("alpha")).expect("the folder");

    succeeded(&world.reconcile(&["--prune", "--force"]));

    assert!(world.tracked(10));
}

#[test]
fn a_gone_piece_with_a_local_archive_survives() {
    let world = World::padded(9);
    world.gone(10, "alpha");
    world.alter_entry(10, "alpha", |tracked| {
        tracked.local_payloads = vec![LocalPayload {
            kind: PayloadKind::Archive,
            version: 1,
            file: antiq::meta::FileName::new(ARCHIVE).expect("a name"),
        }];
    });

    succeeded(&world.reconcile(&["--prune", "--force"]));

    assert!(world.tracked(10));
}

// ---------------------------------------------------------
// The brake
// ---------------------------------------------------------

fn three_of_ten() -> World {
    let world = World::padded(7);
    for number in 8..=10 {
        world.archived(number, &format!("gone{number}"), Archive::Nowhere);
    }
    world
}

#[test]
fn three_of_ten_are_refused_without_a_terminal_and_nothing_is_dropped() {
    let world = three_of_ten();

    let run = world.reconcile_json(&["--prune"]);

    assert_eq!(run.code(), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.error_kind(), "brake");
    assert_eq!(run.json()["error"]["planned"], 3);
    assert_eq!(run.json()["error"]["set_size"], 10);
    assert!(world.tracked(8) && world.tracked(9) && world.tracked(10));
    assert_eq!(world.tombstones()["pruned"], json!([]));
}

#[test]
fn force_goes_past_the_brake() {
    let world = three_of_ten();

    succeeded(&world.reconcile(&["--prune", "--force"]));

    assert!(!world.tracked(8) && !world.tracked(9) && !world.tracked(10));
}

#[test]
fn a_dry_run_says_where_the_brake_would_hold_and_drops_nothing() {
    let world = three_of_ten();

    let run = world.reconcile_json(&["--prune", "--dry-run"]);

    succeeded(&run);
    assert_eq!(run.json()["brake"]["planned"], 3);
    assert_eq!(run.json()["brake"]["set_size"], 10);
    let actions = run.json()["actions"].to_string();
    assert!(actions.contains("\"action\":\"drop\""), "{actions}");
    assert!(world.tracked(8));
    assert_eq!(world.tombstones()["pruned"], json!([]));
}

#[test]
fn a_dry_run_lists_a_drop_on_the_console() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);

    let run = world.reconcile(&["--prune", "--dry-run"]);

    succeeded(&run);
    assert!(run.stdout.contains("drop alpha\n"), "{}", run.stdout);
    assert!(world.tracked(10));
}

fn ask(world: &World) -> Pty {
    let mut pty = Pty::start(world.root(), &["reconcile", "--prune"], Options::piped());
    pty.wait_for("the question", |screen| screen.contains("Choose [1-2"));
    pty
}

#[test]
fn a_person_at_a_terminal_can_go_on() {
    let world = three_of_ten();
    let mut pty = ask(&world);
    assert!(
        pty.screen()
            .contains("Drop 3 of 10 pieces from this machine?"),
        "{}",
        pty.screen().text()
    );
    pty.send(b"2\r");
    let finished = pty.finish();
    assert_eq!(finished.code(), 0, "stderr: {}", finished.stderr);
    assert!(!world.tracked(8));
}

#[test]
fn a_person_at_a_terminal_can_stop() {
    let world = three_of_ten();
    let mut pty = ask(&world);
    pty.send(b"1\r");
    let finished = pty.finish();
    assert_eq!(finished.code(), 1, "stderr: {}", finished.stderr);
    assert!(world.tracked(8));
}

// ---------------------------------------------------------
// The record of pruned objects
// ---------------------------------------------------------

#[test]
fn a_pruned_piece_does_not_come_back_with_the_next_pull() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);
    succeeded(&world.reconcile(&["--prune"]));

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["remote"]["pulled"], 0);
    assert!(!world.local_dir(10).exists());
}

#[test]
fn a_newer_version_on_the_remote_brings_a_pruned_piece_back() {
    let world = World::padded(9);
    world.archived(10, "alpha", Archive::Nowhere);
    succeeded(&world.reconcile(&["--prune"]));
    fs::copy(
        world.remote_dir(10).join("meta.1"),
        world.remote_dir(10).join("meta.2"),
    )
    .expect("a newer version");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["remote"]["pulled"], 1);
    assert!(world.local_dir(10).join("meta.2").exists());
}

#[test]
fn a_gone_piece_is_not_marked() {
    let world = World::padded(2);
    world.gone(3, "alpha");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["marked"], json!([]));
}

#[test]
fn a_recorded_change_does_not_hide_a_version_another_machine_wrote() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    version_by(&world, 1, "studio");
    fs::copy(
        world.local_dir(1).join("meta.1"),
        world.local_dir(1).join("meta.pending"),
    )
    .expect("a recorded change");
    let mut tracked = entry(1, "alpha", EntryState::Gone);
    tracked.version_taken_out = antiq::tracking::VersionTakenOut::Pending;
    put_entry(world.root(), &tracked);

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["changed_elsewhere"][0]["version"], 2);
    assert_eq!(run.json()["changed_elsewhere"][0]["machine"], "studio");
}

#[test]
fn only_a_recorded_change_names_no_version_elsewhere() {
    let world = world();
    world.gone(1, "alpha");
    fs::copy(
        world.local_dir(1).join("meta.1"),
        world.local_dir(1).join("meta.pending"),
    )
    .expect("a recorded change");
    fs::remove_file(world.local_dir(1).join("meta.1")).expect("removed");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(run.json()["changed_elsewhere"], json!([]));
}

// ---------------------------------------------------------
// A folder that cannot be looked at
// ---------------------------------------------------------

#[test]
fn a_piece_whose_folder_cannot_be_looked_at_is_kept_with_a_note() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::padded(9);
    world.gone(10, "dir/alpha");
    fs::create_dir_all(world.root().join("dir/alpha")).expect("the folder");
    let parent = world.root().join("dir");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).expect("chmod");
    let blocked =
        fs::symlink_metadata(parent.join("alpha")).expect_err("a parent nobody may enter");
    assert_eq!(blocked.kind(), std::io::ErrorKind::PermissionDenied);

    let run = world.reconcile_json(&["--prune", "--force"]);

    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("chmod back");
    succeeded(&run);
    assert!(
        world.tracked(10),
        "dropped a piece whose folder is in place"
    );
    assert_eq!(run.json()["pruned"], json!([]));
    let warnings = run.json()["warnings"].to_string();
    assert!(warnings.contains("dir/alpha"), "{warnings}");
}

// ---------------------------------------------------------
// A partial local part and a remote that is away
// ---------------------------------------------------------

#[test]
fn if_reachable_with_a_fresh_away_remote_keeps_the_exit_code_of_a_partial_index() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    for item in fs::read_dir(world.local_dir(1)).expect("the object directory") {
        let item = item.expect("an item");
        if item.file_name().to_string_lossy().contains("listing") {
            fs::write(item.path(), b"not a listing").expect("a damaged listing");
        }
    }
    world.take_remote_away();
    world.away_since(1);

    let run = world.reconcile(&["--if-reachable"]);

    assert_eq!(run.code(), 3, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stderr.contains("while attempting to use the remote"),
        "{}",
        run.stderr
    );
}

// ---------------------------------------------------------
// The upload of a live package that waits
// ---------------------------------------------------------

#[test]
fn the_temp_file_of_a_live_package_that_waits_for_its_upload_is_kept() {
    let world = world();
    put_meta(
        world.root(),
        &metadata(1, "alpha", Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    put_entry(world.root(), &entry(1, "alpha", EntryState::Live));
    fs::create_dir_all(world.root().join("alpha")).expect("the folder");
    fs::write(world.root().join("alpha/a.txt"), b"x").expect("a file");
    let there = world.remote_dir(1);
    fs::create_dir_all(&there).expect("the object directory");
    fs::copy(world.local_dir(1).join("meta.1"), there.join("meta.1")).expect("the version");
    let live = support::objects::payload(PayloadKind::Live, 2, 10);
    let name = live.file.as_str().to_string();
    let waiting = metadata(1, "alpha", Lifecycle::Live, COLLECTION_ID, vec![live]);
    fs::write(
        world.local_dir(1).join("meta.pending"),
        antiq::meta::to_bytes(&waiting).expect("bytes"),
    )
    .expect("the recorded change");
    fs::write(world.local_dir(1).join(&name), b"live package").expect("the package");
    let temp = format!(".{name}.laptop.part");
    world.old_leftover(1, &temp);

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert!(
        world.remote_dir(1).join(&temp).exists(),
        "the temp file of a waiting live-package upload was removed"
    );
}

// ---------------------------------------------------------
// A dry run judges against the versions the real run pulls
// ---------------------------------------------------------

#[test]
fn a_dry_run_lists_no_drop_that_a_version_from_the_remote_prevents() {
    let world = World::padded(9);
    world.gone(10, "alpha");
    let there = world.remote_dir(10);
    fs::create_dir_all(&there).expect("the object directory");
    fs::copy(world.local_dir(10).join("meta.1"), there.join("meta.1")).expect("the version");
    let mut elsewhere = metadata(
        10,
        "alpha",
        Lifecycle::Live,
        "01BX5ZZKBKACTAV9WEVGEMMVRY",
        Vec::new(),
    );
    elsewhere.history = Vec::new();
    fs::write(
        there.join("meta.2"),
        antiq::meta::to_bytes(&elsewhere).expect("bytes"),
    )
    .expect("the version of another machine");

    let dry = world.reconcile(&["--prune", "--dry-run"]);
    let real = world.reconcile_json(&["--prune"]);

    succeeded(&dry);
    succeeded(&real);
    assert_eq!(real.json()["pruned"], json!([]), "{}", real.stdout);
    assert!(!dry.stdout.contains("drop alpha"), "{}", dry.stdout);
}

#[test]
fn a_dry_run_with_force_reports_no_hold_of_the_brake() {
    let world = three_of_ten();

    let run = world.reconcile_json(&["--prune", "--dry-run", "--force"]);

    succeeded(&run);
    assert!(run.json().get("brake").is_none(), "{}", run.stdout);
    let actions = run.json()["actions"].to_string();
    assert!(actions.contains("\"action\":\"drop\""), "{actions}");
}

// ---------------------------------------------------------
// Unreadable versions of objects without an entry
// ---------------------------------------------------------

#[test]
fn a_version_that_cannot_be_read_is_listed_for_an_object_without_an_entry() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    fs::remove_file(
        world
            .root()
            .join(".antiq/tracking")
            .join(format!("{}.json", id(1))),
    )
    .expect("no entry");
    fs::write(world.remote_dir(1).join("meta.2"), b"{ not json").expect("a damaged version");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    assert_eq!(
        run.json()["unreadable_versions"][0]["version"],
        2,
        "{}",
        run.stdout
    );
}

impl World {
    /// Takes the entry of `number` away, so that its object has none.
    fn untrack(&self, number: u8) {
        fs::remove_file(
            self.root()
                .join(".antiq/tracking")
                .join(format!("{}.json", id(number))),
        )
        .expect("no entry");
    }
}

#[test]
fn an_object_without_an_entry_and_with_nothing_readable_is_listed() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.untrack(1);
    fs::write(world.local_dir(1).join("meta.1"), b"{ not json").expect("damaged");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    let listed = run.json()["unreadable_versions"].clone();
    assert_eq!(listed[0]["id"], id(1).to_string(), "{}", run.stdout);
    assert_eq!(listed[0]["version"], 1);
}

#[test]
fn a_recorded_change_that_cannot_be_read_is_a_warning_for_an_object_without_an_entry() {
    let world = world();
    world.archived(1, "alpha", Archive::Remote);
    world.untrack(1);
    fs::write(world.local_dir(1).join("meta.pending"), b"{ not json").expect("damaged");

    let run = world.reconcile_json(&[]);

    succeeded(&run);
    let warnings = run.json()["warnings"].to_string();
    assert!(
        warnings.contains(&format!("the metadata of {} cannot be read", id(1))),
        "{warnings}"
    );
}
