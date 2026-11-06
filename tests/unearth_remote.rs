// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq unearth` with a path remote, seen from outside the binary. The
//! objects are laid down with the library, since the commands that upload
//! them are other commands' business; what the tests judge is what
//! `unearth` does with the remote.

mod support;

use antiq::fsx::tree::TreeRoot;
use antiq::listing;
use antiq::meta::{
    self, Checksum, CreatedSource, FileName, HistoryAction, HistoryEntry, Lifecycle, Metadata,
    Payload, PayloadKind,
};
use antiq::pack::zstd::ZstdSettings;
use antiq::pack::{PackSettings, Packager, TarZstd};
use antiq::relpath::{Name, RelPath};
use antiq::select::Rules;
use antiq::tracking::{EntryState, LocalPayload, TrackingEntry, VersionTakenOut};
use antiq::walk::walk_piece;
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::tree::{COLLECTION_ID, TempCollection};

const OBJECT: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const OTHER_OBJECT: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";
const OTHER_COLLECTION: &str = "01D39ZY06FGSCTVN4T2V9PKHFY";
const ARCHIVED_AT: &str = "2026-01-02T03:04:05Z";
const FILE_TIME: i64 = 1_000_000_000;

/// Who a piece belongs to and what the metadata says about it.
#[derive(Clone, Copy)]
enum Owner {
    Own,
    Foreign,
}

/// One object to lay down.
struct Object {
    id: &'static str,
    piece: &'static str,
    kind: PayloadKind,
    owner: Owner,
    lifecycle: Lifecycle,
    /// The payload and the listing are in the local store.
    held: bool,
    /// The payload (and listing) and the metadata are on the remote.
    on_remote: bool,
    /// The tracking entry this machine has.
    entry: Option<EntryState>,
}

impl Object {
    /// An archived piece of this collection whose folder is gone.
    fn archived(id: &'static str, piece: &'static str, held: bool) -> Object {
        Object {
            id,
            piece,
            kind: PayloadKind::Archive,
            owner: Owner::Own,
            lifecycle: Lifecycle::Archived,
            held,
            on_remote: true,
            entry: Some(EntryState::Gone),
        }
    }

    /// A live piece with a live package on the remote only.
    fn live(id: &'static str, piece: &'static str, owner: Owner) -> Object {
        Object {
            id,
            piece,
            kind: PayloadKind::Live,
            owner,
            lifecycle: Lifecycle::Live,
            held: false,
            on_remote: true,
            entry: match owner {
                Owner::Own => Some(EntryState::Live),
                Owner::Foreign => None,
            },
        }
    }
}

struct World {
    temp: TempCollection,
    remotes: tempfile::TempDir,
}

fn world() -> World {
    let temp = TempCollection::minimal();
    fs::write(
        temp.root().join(".antiq/config.toml"),
        format!("[collection]\nid = \"{COLLECTION_ID}\"\n\n[machine]\nname = \"laptop\"\n"),
    )
    .expect("the config");
    World {
        temp,
        remotes: tempfile::tempdir().expect("a directory for the remote"),
    }
}

/// A world whose collection has a path remote attached.
fn world_with_remote() -> World {
    let world = world();
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

    fn object_dir(&self, id: &str) -> PathBuf {
        self.root().join(".antiq/objects").join(id)
    }

    fn remote_object_dir(&self, id: &str) -> PathBuf {
        self.remote_root().join("objects").join(id)
    }

    fn antiq(&self, args: &[&str]) -> Run {
        Antiq::new().arg("-C").arg(self.root()).args(args).run()
    }

    fn unearth(&self, args: &[&str]) -> Run {
        let mut all = vec!["unearth"];
        all.extend_from_slice(args);
        self.antiq(&all)
    }

    fn unearth_json(&self, args: &[&str]) -> Run {
        let mut all = vec!["--json", "unearth"];
        all.extend_from_slice(args);
        self.antiq(&all)
    }

    fn pack(&mut self, scratch: &str, files: &[(&str, &str)]) -> (Vec<u8>, antiq::pack::Packed) {
        let tree = self.temp.tree();
        for (name, content) in files {
            tree.file_with(&format!("{scratch}/{name}"), content, 0o644, FILE_TIME);
        }
        let collection = TreeRoot::open(tree.root()).expect("the tree opens");
        let top = RelPath::parse(scratch).expect("a path");
        let rules = Rules::new(&[], &[]).expect("no patterns");
        let walked = walk_piece(&collection, &top, &[], &rules).expect("walks");
        let root = collection.sub(&top).expect("opens");
        let mut bytes = Vec::new();
        let settings = PackSettings {
            zstd: ZstdSettings::live(1),
        };
        let packed = TarZstd
            .pack(&root, &walked, &settings, &mut bytes)
            .expect("packs");
        fs::remove_dir_all(self.root().join(scratch)).expect("the scratch folder goes");
        (bytes, packed)
    }

    /// Lays the object down as `archive` or `sync` would have left it.
    fn put(&mut self, object: &Object) {
        let (bytes, packed) = self.pack("scratch", &[("notes.txt", "hello")]);
        let sha = packed.sha256.clone();
        let (payload_file, listing_file) = match object.kind {
            PayloadKind::Archive => (
                FileName::new(format!("{}-v1.tar.zst", &sha[..16])).expect("name"),
                Some(listing::file_name(&sha).expect("a listing name")),
            ),
            _ => (
                FileName::new("0000000000000001-l.tar.zst").expect("name"),
                None,
            ),
        };
        let archived_at: Timestamp = ARCHIVED_AT.parse().expect("a time");
        let payload = Payload {
            kind: object.kind,
            version: 1,
            file: payload_file.clone(),
            package_format: "tar+zstd/1".to_string(),
            size: packed.size,
            checksum: Checksum::sha256(sha),
            listing: listing_file.clone(),
            machine: "laptop".to_string(),
            created: archived_at,
            verified: None,
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        };
        let top = RelPath::parse(object.piece).expect("a path");
        let name = Name::from_bytes(top.components().last().expect("a name").to_vec())
            .expect("a valid name");
        let metadata = Metadata {
            format: meta::FORMAT,
            id: object.id.parse().expect("an ID"),
            collection: match object.owner {
                Owner::Own => COLLECTION_ID,
                Owner::Foreign => OTHER_COLLECTION,
            }
            .parse()
            .expect("an ID"),
            path: top.clone(),
            name,
            created: archived_at,
            created_source: CreatedSource::Birth,
            lifecycle: object.lifecycle,
            payloads: vec![payload],
            history: vec![HistoryEntry::new(
                archived_at,
                "laptop",
                HistoryAction::Archive,
            )],
            extra: serde_json::Map::new(),
        };
        let meta_bytes = meta::to_bytes(&metadata).expect("serialises");

        let local = self.object_dir(object.id);
        fs::create_dir_all(&local).expect("the object directory");
        fs::write(local.join("meta.1"), &meta_bytes).expect("metadata");
        let mut places = Vec::new();
        if object.held {
            places.push(local);
        }
        if object.on_remote {
            let remote = self.remote_object_dir(object.id);
            fs::create_dir_all(&remote).expect("the remote object directory");
            fs::write(remote.join("meta.1"), &meta_bytes).expect("metadata");
            places.push(remote);
        }
        for directory in places {
            fs::write(directory.join(payload_file.as_str()), &bytes).expect("the payload");
            if let Some(listing_file) = &listing_file {
                listing::write(
                    packed.listing.iter(),
                    fs::File::create(directory.join(listing_file.as_str())).expect("a file"),
                )
                .expect("the listing");
            }
        }

        if let Some(state) = object.entry {
            let mut entry = TrackingEntry::new(
                object.id.parse().expect("an ID"),
                top,
                state,
                VersionTakenOut::Committed(1),
                archived_at,
            );
            if object.held {
                entry.local_payloads = vec![LocalPayload {
                    kind: object.kind,
                    version: 1,
                    file: payload_file,
                }];
            }
            fs::write(
                self.root()
                    .join(format!(".antiq/tracking/{}.json", object.id)),
                serde_json::to_vec(&entry).expect("serialises"),
            )
            .expect("the entry");
        }
    }

    /// The remote has a version 2 of the object that moved it to `piece`.
    fn move_on_remote(&self, id: &str, piece: &str) {
        let there = self.remote_object_dir(id);
        let mut document: Value =
            serde_json::from_slice(&fs::read(there.join("meta.1")).expect("reads")).expect("json");
        document["path"] = Value::from(piece);
        document["name"] = Value::from(piece);
        fs::write(
            there.join("meta.2"),
            serde_json::to_vec(&document).expect("json"),
        )
        .expect("written");
    }

    fn state_file(&self) -> PathBuf {
        self.root().join(".antiq/remote-state.json")
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state_file()).expect("the state file")).expect("json")
    }

    /// The remote is out of reach, as an unmounted volume is.
    fn take_remote_away(&self) {
        fs::rename(self.remote_root(), self.remotes.path().join("elsewhere")).expect("moved");
    }

    /// The remote's marker is unreadable.
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

    /// Every path below the collection with its content, apart from the
    /// lock file.
    fn snapshot(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        fn walk(base: &Path, directory: &Path, seen: &mut BTreeMap<String, Option<Vec<u8>>>) {
            for item in fs::read_dir(directory).expect("lists") {
                let path = item.expect("an item").path();
                let name = path
                    .strip_prefix(base)
                    .expect("below")
                    .display()
                    .to_string();
                if name == ".antiq/lock" {
                    continue;
                }
                if path.is_dir() {
                    seen.insert(name, None);
                    walk(base, &path, seen);
                } else {
                    seen.insert(name, fs::read(&path).ok());
                }
            }
        }
        let mut seen = BTreeMap::new();
        walk(self.root(), self.root(), &mut seen);
        seen
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
// An archive only the remote holds
// =========================================================

#[test]
fn a_remote_only_archive_is_fetched_and_unearthed_and_stays_local() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", false));

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Unearthed alpha (archive version 1)");
    assert_eq!(run.stderr, "");
    assert_eq!(
        fs::read_to_string(world.root().join("alpha/notes.txt")).expect("reads"),
        "hello"
    );
    let held: Vec<String> = fs::read_dir(world.object_dir(OBJECT))
        .expect("lists")
        .map(|item| {
            item.expect("an item")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        held.iter().any(|name| name.ends_with("-v1.tar.zst")),
        "{held:?}"
    );
    assert_eq!(
        fs::read_dir(world.root().join(".antiq/tmp"))
            .expect("lists")
            .count(),
        0
    );
}

#[test]
fn a_real_run_records_that_the_remote_was_reachable() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", true));

    succeeded(&world.unearth(&["alpha"]));

    assert!(world.state()["last_reachable"].is_string());
}

#[test]
fn a_dry_run_records_nothing() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", true));
    let before = world.snapshot();

    let run = world.antiq(&["unearth", "--dry-run", "alpha"]);

    succeeded(&run);
    assert_eq!(world.snapshot(), before);
    assert!(!world.state_file().exists());
}

// =========================================================
// Newer metadata on the remote
// =========================================================

#[test]
fn a_newer_version_of_the_remote_is_stored_and_reported() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", true));
    world.move_on_remote(OBJECT, "beta");

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Unearthed beta (archive version 1)");
    insta::assert_snapshot!(
        run.stderr,
        @"note: the remote has metadata version 2 of `beta`, which this run uses"
    );
    assert!(world.object_dir(OBJECT).join("meta.2").exists());
    assert!(world.root().join("beta/notes.txt").exists());
}

#[test]
fn a_recorded_change_is_kept_over_a_newer_remote_version() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", true));
    world.move_on_remote(OBJECT, "beta");
    fs::copy(
        world.object_dir(OBJECT).join("meta.1"),
        world.object_dir(OBJECT).join("meta.pending"),
    )
    .expect("a recorded change");

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(
        run.stderr,
        @"note: the remote has metadata version 2 of `alpha`; this machine's recorded change is used and `sync` applies it on top"
    );
    assert!(world.root().join("alpha/notes.txt").exists());
}

#[test]
fn a_dry_run_plans_against_the_newer_version_and_stores_nothing() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", false));
    world.move_on_remote(OBJECT, "beta");
    let before = world.snapshot();

    let run = world.antiq(&["unearth", "--dry-run", "alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(
        run.stdout,
        @"Would fetch archive version 1 and unearth beta"
    );
    insta::assert_snapshot!(
        run.stderr,
        @"note: the remote has metadata version 2 of `beta`, which this run uses"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn a_dry_run_with_json_prints_the_plan_only() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", false));
    world.move_on_remote(OBJECT, "beta");

    let run = world.antiq(&["--json", "unearth", "--dry-run", "alpha"]);

    succeeded(&run);
    assert_eq!(run.stderr, "");
    assert_eq!(
        run.json()["actions"][0],
        serde_json::json!({
            "action": "unearth",
            "id": OBJECT,
            "path": "beta",
            "source": "remote_archive",
            "payload_version": 1,
        })
    );
}

// =========================================================
// A remote that is away
// =========================================================

#[test]
fn an_away_remote_does_not_stop_a_local_archive() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", true));
    world.take_remote_away();

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Unearthed alpha (archive version 1)");
    insta::assert_snapshot!(
        run.stderr,
        @"note: the remote is away; unearthing from local data"
    );
    assert!(world.state()["away_first_seen"].is_string());
}

#[test]
fn an_away_remote_stops_an_archive_only_the_remote_holds() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", false));
    world.take_remote_away();
    let before = world.snapshot();

    let run = world.unearth_json(&["alpha"]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.error_kind(), "remote_away");
    assert!(!world.root().join("alpha").exists());
    let mut after = world.snapshot();
    after.remove(".antiq/remote-state.json");
    assert_eq!(after, before);
}

/// A world whose remote is away and which has the remote-only piece the
/// run needs the remote for, and a held archive that waits for its upload.
fn away_with_pending_work() -> World {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", false));
    world.put(&Object::archived(OTHER_OBJECT, "gamma", true));
    world.take_remote_away();
    world
}

#[test]
fn if_reachable_ends_a_run_after_one_day_away_with_zero() {
    let world = away_with_pending_work();
    world.away_since(1);

    let run = world.unearth(&["--if-reachable", "alpha"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
}

#[test]
fn if_reachable_ends_a_run_after_eight_days_away_with_pending_work_with_four() {
    let world = away_with_pending_work();
    world.away_since(8);

    let run = world.unearth(&["--if-reachable", "alpha"]);

    assert_eq!(run.code(), 4, "{}{}", run.stdout, run.stderr);
}

#[test]
fn a_dry_run_gets_the_exit_code_of_the_real_run() {
    let world = away_with_pending_work();
    world.away_since(1);
    let fresh = world.antiq(&["unearth", "--dry-run", "--if-reachable", "alpha"]);
    world.away_since(8);
    let overdue = world.antiq(&["unearth", "--dry-run", "--if-reachable", "alpha"]);

    assert_eq!(fresh.code(), 0, "{}{}", fresh.stdout, fresh.stderr);
    assert_eq!(overdue.code(), 4, "{}{}", overdue.stdout, overdue.stderr);
}

#[test]
fn eight_days_away_without_pending_work_is_not_overdue() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", false));
    world.take_remote_away();
    world.away_since(8);

    let run = world.unearth(&["--if-reachable", "alpha"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
}

// =========================================================
// A remote that is broken or missing
// =========================================================

#[test]
fn a_broken_remote_does_not_stop_a_local_archive() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", true));
    world.break_remote();

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    assert!(
        run.stderr.starts_with("note: the remote is broken ("),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .trim_end()
            .ends_with("); unearthing from local data, its metadata was not pulled"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_broken_remote_stops_an_archive_only_the_remote_holds_whatever_the_flag() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", false));
    world.break_remote();

    let run = world.unearth_json(&["--if-reachable", "alpha"]);

    assert_eq!(run.code(), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.error_kind(), "remote_broken");
}

#[test]
fn without_a_remote_an_archive_only_the_remote_holds_cannot_be_unearthed() {
    let mut world = world();
    world.put(&Object::archived(OBJECT, "alpha", false));

    let run = world.unearth_json(&["alpha"]);

    assert_eq!(run.code(), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.error_kind(), "no_remote");
}

#[test]
fn without_a_remote_a_local_archive_unearths_without_a_note() {
    let mut world = world();
    world.put(&Object::archived(OBJECT, "alpha", true));

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    assert_eq!(run.stderr, "");
}

#[test]
fn a_state_file_that_cannot_be_read_stops_the_run() {
    let mut world = world_with_remote();
    world.put(&Object::archived(OBJECT, "alpha", true));
    fs::create_dir(world.state_file()).expect("a directory in its place");

    let run = world.unearth_json(&["alpha"]);

    assert_eq!(run.code(), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.error_kind(), "other");
    assert!(!world.root().join("alpha").exists());
}

// =========================================================
// A live package
// =========================================================

#[test]
fn a_gone_piece_without_archive_comes_back_from_its_live_package() {
    let mut world = world_with_remote();
    world.put(&Object::live(OBJECT, "alpha", Owner::Own));

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Unearthed alpha (from its live package)");
    assert_eq!(
        fs::read_to_string(world.root().join("alpha/notes.txt")).expect("reads"),
        "hello"
    );
}

#[test]
fn a_live_piece_of_another_collection_becomes_a_fork_with_a_new_id() {
    let mut world = world_with_remote();
    world.put(&Object::live(OTHER_OBJECT, "alpha", Owner::Foreign));
    let foreign_before: BTreeMap<_, _> = world
        .snapshot()
        .into_iter()
        .filter(|(name, _)| name.contains(OTHER_OBJECT))
        .collect();

    let run = world.unearth_json(&["alpha"]);

    succeeded(&run);
    let report = run.json();
    assert_eq!(report["unearthed"], true);
    assert_eq!(report["from"], OTHER_OBJECT);
    assert_ne!(report["id"], OTHER_OBJECT);
    assert!(report.get("payload_version").is_none());
    assert_eq!(
        fs::read_to_string(world.root().join("alpha/notes.txt")).expect("reads"),
        "hello"
    );
    let foreign_after: BTreeMap<_, _> = world
        .snapshot()
        .into_iter()
        .filter(|(name, _)| name.contains(OTHER_OBJECT))
        .collect();
    assert_eq!(foreign_after, foreign_before);
}

#[test]
fn a_fork_prints_the_piece_it_came_from() {
    let mut world = world_with_remote();
    world.put(&Object::live(OTHER_OBJECT, "alpha", Owner::Foreign));

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    assert!(
        run.stdout.starts_with(&format!(
            "Unearthed alpha as a fork of {OTHER_OBJECT} (new ID "
        )),
        "{}",
        run.stdout
    );
}

#[test]
fn a_live_piece_of_another_collection_without_a_package_has_nothing_to_unearth_from() {
    let mut world = world_with_remote();
    let mut piece = Object::live(OTHER_OBJECT, "alpha", Owner::Foreign);
    piece.kind = PayloadKind::Archive;
    world.put(&piece);

    let run = world.unearth_json(&["alpha"]);

    assert_eq!(run.code(), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(
        run.error_message(),
        "`alpha` has no live package to unearth from"
    );
}
