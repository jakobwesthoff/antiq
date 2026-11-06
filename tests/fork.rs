// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq fork` seen from outside the binary. The archive and the
//! unearthed folder are made with the library and `antiq unearth`, since
//! the commands that make them are other commands' business; what the tests
//! judge is what `fork` does with an unearthed piece.

mod support;

use antiq::clock::SystemClock;
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
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::objects;
use support::tree::{COLLECTION_ID, TempCollection};

const OBJECT: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const ARCHIVED_AT: &str = "2026-01-02T03:04:05Z";
const FILE_TIME: i64 = 1_000_000_000;

/// A collection that holds the archive of a piece and nothing else.
struct World {
    temp: TempCollection,
}

fn world() -> World {
    World {
        temp: TempCollection::minimal(),
    }
}

impl World {
    fn root(&self) -> &Path {
        self.temp.root()
    }

    fn object_dir(&self, object: &str) -> PathBuf {
        self.root().join(".antiq/objects").join(object)
    }

    /// The archive of `piece` as `archive` leaves it: the payload in the
    /// store, an entry `gone` that keeps it, and no folder.
    fn archive(&mut self, piece: &str, files: &[(&str, &str)]) {
        let scratch = "scratch";
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

        let sha = packed.sha256.clone();
        let payload_file = FileName::new(format!("{}-v1.tar.zst", &sha[..16])).expect("name");
        let listing_file = listing::file_name(&sha).expect("a listing name");
        let directory = self.object_dir(OBJECT);
        fs::create_dir_all(&directory).expect("the object directory");
        fs::write(directory.join(payload_file.as_str()), bytes).expect("the payload");
        listing::write(
            packed.listing.iter(),
            fs::File::create(directory.join(listing_file.as_str())).expect("a file"),
        )
        .expect("the listing");
        let archived_at = ARCHIVED_AT.parse().expect("a time");
        let top = RelPath::parse(piece).expect("a path");
        let name = Name::from_bytes(top.components().last().expect("a name").to_vec())
            .expect("a valid name");
        let known = Metadata {
            format: meta::FORMAT,
            id: OBJECT.parse().expect("an ID"),
            collection: COLLECTION_ID.parse().expect("an ID"),
            path: top.clone(),
            name,
            created: archived_at,
            created_source: CreatedSource::Birth,
            lifecycle: Lifecycle::Archived,
            payloads: vec![Payload {
                kind: PayloadKind::Archive,
                version: 1,
                file: payload_file.clone(),
                package_format: "tar+zstd/1".to_string(),
                size: packed.size,
                checksum: Checksum::sha256(sha),
                listing: Some(listing_file),
                machine: "laptop".to_string(),
                created: archived_at,
                verified: None,
                superseded: false,
                encoder: None,
                extra: serde_json::Map::new(),
            }],
            history: vec![HistoryEntry::new(
                archived_at,
                "laptop",
                HistoryAction::Archive,
            )],
            extra: serde_json::Map::new(),
        };
        fs::write(
            directory.join("meta.1"),
            meta::to_bytes(&known).expect("serialises"),
        )
        .expect("metadata");

        let mut entry = TrackingEntry::new(
            known.id,
            top,
            EntryState::Gone,
            VersionTakenOut::Committed(1),
            archived_at,
        );
        entry.local_payloads = vec![LocalPayload {
            kind: PayloadKind::Archive,
            version: 1,
            file: payload_file,
        }];
        objects::put_entry(self.root(), &entry);
    }

    /// An unearthed piece: archived, then brought back by `antiq unearth`.
    fn unearthed(&mut self, piece: &str) {
        self.archive(
            piece,
            &[("notes.txt", "hello"), ("src/main.rs", "fn main() {}")],
        );
        succeeded(&self.command(&["unearth", piece]));
    }

    fn command(&self, args: &[&str]) -> Run {
        Antiq::new().arg("-C").arg(self.root()).args(args).run()
    }

    fn fork(&self, args: &[&str]) -> Run {
        let mut all = vec!["fork"];
        all.extend_from_slice(args);
        self.command(&all)
    }

    fn json_fork(&self, args: &[&str]) -> Run {
        let mut all = vec!["--json", "fork"];
        all.extend_from_slice(args);
        self.command(&all)
    }

    fn metadata_of(&self, object: &str) -> Value {
        let directory = self.object_dir(object);
        let path = if directory.join("meta.pending").exists() {
            directory.join("meta.pending")
        } else {
            directory.join("meta.1")
        };
        serde_json::from_slice(&fs::read(path).expect("reads")).expect("json")
    }

    fn entry_of(&self, object: &str) -> Value {
        serde_json::from_slice(
            &fs::read(self.root().join(format!(".antiq/tracking/{object}.json"))).expect("reads"),
        )
        .expect("json")
    }

    /// The IDs of the objects other than the one the tests start with.
    fn others(&self) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(self.root().join(".antiq/objects"))
            .expect("lists")
            .map(|item| {
                item.expect("an item")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .filter(|name| name != OBJECT)
            .collect();
        found.sort();
        found
    }

    fn snapshot(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        let mut seen = objects::snapshot(self.root());
        seen.remove(".antiq/lock");
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

#[test]
fn an_unearthed_piece_gets_a_new_id_and_the_old_one_is_archived() {
    let mut world = world();
    world.unearthed("alpha");

    let run = world.fork(&["alpha"]);

    succeeded(&run);
    let [new] = world.others().try_into().expect("one new object");
    insta::assert_snapshot!(
        run.stdout.replace(&new, "<new>"),
        @"Forked alpha (new ID <new>)"
    );
    assert_eq!(run.stderr, "");

    let forked = world.metadata_of(&new);
    assert_eq!(forked["created_source"], "fork");
    assert_eq!(forked["lifecycle"], "live");
    assert_eq!(forked["path"], "alpha");
    assert_eq!(forked["payloads"], json!([]));
    assert_eq!(forked["collection"], COLLECTION_ID);
    let old = world.metadata_of(OBJECT);
    assert_eq!(old["lifecycle"], "archived");
    let history: Vec<&str> = old["history"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry["action"].as_str().expect("an action"))
        .collect();
    assert_eq!(history, ["archive", "unearth", "fork"]);

    let entry = world.entry_of(&new);
    assert_eq!(entry["state"], "live");
    assert_eq!(entry["path"], "alpha");
    assert!(entry["unearthed"].is_string());
    assert_eq!(world.entry_of(OBJECT)["state"], "gone");
    assert!(world.root().join("alpha/notes.txt").exists());
}

#[test]
fn json_names_both_ids_and_the_place_of_the_piece() {
    let mut world = world();
    world.unearthed("alpha");

    let run = world.json_fork(&["alpha"]);

    succeeded(&run);
    let [new] = world.others().try_into().expect("one new object");
    let root = fs::canonicalize(world.root()).expect("the root exists");
    assert_eq!(
        run.json(),
        json!({
            "format": 1,
            "command": "fork",
            "id": new,
            "from": OBJECT,
            "path": "alpha",
            "old_path": "alpha",
            "abs_path": root.join("alpha").display().to_string(),
            "warnings": [],
        })
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn the_piece_can_be_named_by_its_id() {
    let mut world = world();
    world.unearthed("alpha");

    let run = world.fork(&[&OBJECT[..8]]);

    succeeded(&run);
    assert_eq!(world.others().len(), 1);
}

#[test]
fn a_name_renames_the_folder_within_its_parent() {
    let mut world = world();
    world.unearthed("alpha");

    let run = world.fork(&["--name", "beta", "alpha"]);

    succeeded(&run);
    let [new] = world.others().try_into().expect("one new object");
    insta::assert_snapshot!(
        run.stdout.replace(&new, "<new>"),
        @"Forked alpha to beta (new ID <new>)"
    );
    assert!(!world.root().join("alpha").exists());
    assert!(world.root().join("beta/notes.txt").exists());
    assert_eq!(world.metadata_of(&new)["path"], "beta");
    assert_eq!(world.metadata_of(&new)["name"], "beta");
    assert_eq!(world.entry_of(&new)["path"], "beta");
    assert_eq!(world.metadata_of(OBJECT)["path"], "alpha");
}

#[test]
fn json_names_the_new_place_after_a_rename() {
    let mut world = world();
    world.unearthed("alpha");

    let run = world.json_fork(&["--name", "beta", "alpha"]);

    succeeded(&run);
    assert_eq!(run.json()["path"], "beta");
    assert_eq!(run.json()["old_path"], "alpha");
}

#[test]
fn a_taken_name_is_refused_and_nothing_changes() {
    let mut world = world();
    world.unearthed("alpha");
    world.temp.tree().file("beta/mine.txt", "my own work");
    let before = world.snapshot();

    let run = world.json_fork(&["--name", "beta", "alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
    assert_eq!(
        run.error_message(),
        "cannot rename the folder to `beta`: it is taken"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn a_name_that_is_not_a_name_is_a_usage_error() {
    let mut world = world();
    world.unearthed("alpha");
    let before = world.snapshot();

    let run = world.json_fork(&["--name", "a/b", "alpha"]);

    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert_eq!(
        run.error_message(),
        "`a/b` is not a name for a piece: a name cannot be `/` inside"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn a_name_inside_the_antiq_directory_is_refused() {
    let mut world = world();
    world.unearthed("alpha");

    let run = world.json_fork(&["--name", ".antiq", "alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(
        run.error_message(),
        "cannot rename the folder to `.antiq`: it is inside `.antiq/`"
    );
}

#[test]
fn a_piece_that_is_not_unearthed_is_refused_with_the_next_step() {
    let mut world = world();
    world.archive("alpha", &[("notes.txt", "hello")]);
    let before = world.snapshot();

    let run = world.json_fork(&["alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
    assert_eq!(
        run.error_message(),
        "cannot fork `alpha`: there is no folder; `unearth` brings it back"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn a_live_piece_that_was_never_archived_is_refused() {
    let world = world();
    fs::create_dir(world.root().join("alpha")).expect("a folder");
    fs::write(world.root().join("alpha/notes.txt"), "hello").expect("a file");
    objects::put_meta(
        world.root(),
        &objects::metadata(1, "alpha", Lifecycle::Live, COLLECTION_ID, Vec::new()),
    );
    objects::put_entry(world.root(), &objects::entry(1, "alpha", EntryState::Live));
    let before = world.snapshot();

    let run = world.json_fork(&["alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(
        run.error_message(),
        "cannot fork `alpha`: it is not unearthed: nothing was archived from it, so there is nothing to fork from"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn an_unknown_piece_is_an_error_of_its_own_kind() {
    let world = world();

    let run = world.json_fork(&["nowhere"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "unknown_reference");
}

#[test]
fn a_piece_outside_the_collection_is_an_error_of_its_own_kind() {
    let world = world();
    let outside = tempfile::tempdir().expect("a folder outside");

    let run = world.json_fork(&[outside.path().to_str().expect("UTF-8")]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "outside_collection");
}

#[test]
fn outside_a_collection_it_is_an_error() {
    let tree = support::tree::Tree::new();

    let run = Antiq::new()
        .arg("-C")
        .arg(tree.root())
        .args(["--json", "fork", "alpha"])
        .run();

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "not_a_collection");
}

#[test]
fn a_dry_run_says_what_it_would_do_and_changes_nothing() {
    let mut world = world();
    world.unearthed("alpha");
    let before = world.snapshot();
    let lock = fs::read(world.root().join(".antiq/lock")).ok();

    let run = world.fork(&["--dry-run", "alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Would fork alpha");
    assert_eq!(world.snapshot(), before);
    assert_eq!(
        fs::read(world.root().join(".antiq/lock")).ok(),
        lock,
        "a dry run takes no lock"
    );
}

#[test]
fn a_dry_run_with_a_name_says_where_the_folder_goes() {
    let mut world = world();
    world.unearthed("alpha");
    let before = world.snapshot();

    let run = world.fork(&["--dry-run", "--name", "beta", "alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Would fork alpha and rename it to beta");
    assert_eq!(world.snapshot(), before);
}

#[test]
fn a_dry_run_in_json_is_the_plan() {
    let mut world = world();
    world.unearthed("alpha");

    let run = world.json_fork(&["--dry-run", "--name", "beta", "alpha"]);

    succeeded(&run);
    assert_eq!(
        run.json(),
        json!({
            "format": 1,
            "command": "fork",
            "dry_run": true,
            "actions": [{
                "action": "fork",
                "from": OBJECT,
                "path": "beta",
                "old_path": "alpha",
            }],
        })
    );
}

#[test]
fn a_dry_run_refuses_what_a_run_would_refuse() {
    let mut world = world();
    world.unearthed("alpha");
    world.temp.tree().file("beta/mine.txt", "my own work");

    let run = world.json_fork(&["--dry-run", "--name", "beta", "alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(
        run.error_message(),
        "cannot rename the folder to `beta`: it is taken"
    );
}

#[test]
fn a_dry_run_while_another_run_holds_the_lock_prints_the_note() {
    let mut world = world();
    world.unearthed("alpha");
    let _held = antiq::lock::acquire(&world.root().join(".antiq/lock"), "laptop", &SystemClock)
        .expect("the lock is free");

    let run = world.fork(&["--dry-run", "alpha"]);

    succeeded(&run);
    assert!(
        run.stderr
            .starts_with("note: another run holds the lock (pid "),
        "{}",
        run.stderr
    );
    insta::assert_snapshot!(run.stdout, @"Would fork alpha");
}

#[test]
fn a_run_while_another_run_holds_the_lock_is_refused() {
    let mut world = world();
    world.unearthed("alpha");
    let _held = antiq::lock::acquire(&world.root().join(".antiq/lock"), "laptop", &SystemClock)
        .expect("the lock is free");

    let run = world.json_fork(&["alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "lock_held");
    assert!(world.others().is_empty());
}

#[test]
fn the_old_id_needs_another_path_to_be_unearthed() {
    let mut world = world();
    world.unearthed("alpha");
    succeeded(&world.fork(&["alpha"]));
    let before = world.snapshot();

    let refused = world.command(&["--json", "unearth", OBJECT]);

    assert_eq!(refused.code(), 1);
    assert_eq!(
        refused.error_message(),
        "cannot unearth into `alpha`: it is taken"
    );
    assert_eq!(world.snapshot(), before);

    let elsewhere = world.command(&["unearth", "--to", "beta", OBJECT]);
    succeeded(&elsewhere);
    assert!(world.root().join("beta/notes.txt").exists());
    assert!(world.root().join("alpha/notes.txt").exists());
    let [new] = world.others().try_into().expect("one new object");
    assert_eq!(world.metadata_of(&new)["path"], "alpha");
    assert_eq!(world.metadata_of(OBJECT)["path"], "beta");
}

#[test]
fn what_the_start_of_the_run_found_is_reported_as_notes() {
    let mut world = world();
    world.unearthed("alpha");
    let stranger = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    fs::write(
        world
            .root()
            .join(format!(".antiq/tracking/{stranger}.json")),
        "not json",
    )
    .expect("a damaged entry");

    let run = world.fork(&["alpha"]);

    succeeded(&run);
    assert!(
        run.stderr.starts_with(&format!(
            "note: the tracking entry of {stranger} cannot be used"
        )),
        "{}",
        run.stderr
    );
}
