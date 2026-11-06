// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq unearth` seen from outside the binary. The archives are made with
//! the library, since the command that makes them is another command's
//! business; what the tests judge is what `unearth` does with them.

mod support;

use antiq::clock::SystemClock;
use antiq::fsx::tree::TreeRoot;
use antiq::id::ObjectId;
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
use support::tree::{COLLECTION_ID, TempCollection};

const OBJECT: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const ARCHIVED_AT: &str = "2026-01-02T03:04:05Z";
const FILE_TIME: i64 = 1_000_000_000;

fn object() -> ObjectId {
    OBJECT.parse().expect("an ID")
}

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

    fn object_dir(&self) -> PathBuf {
        self.root().join(".antiq/objects").join(OBJECT)
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
        (bytes, packed)
    }

    /// Archives the files as payload version `version` of the object at
    /// `piece`. The piece's folder is gone afterwards.
    fn archive(&mut self, piece: &str, version: u32, files: &[(&str, &str)]) -> FileName {
        let scratch = format!("scratch-{version}");
        let (bytes, packed) = self.pack(&scratch, files);
        fs::remove_dir_all(self.root().join(&scratch)).expect("the scratch folder goes");
        let sha = packed.sha256.clone();
        let payload_file =
            FileName::new(format!("{}-v{version}.tar.zst", &sha[..16])).expect("name");
        let listing_file = listing::file_name(&sha).expect("a listing name");
        let directory = self.object_dir();
        fs::create_dir_all(&directory).expect("the object directory");
        fs::write(directory.join(payload_file.as_str()), bytes).expect("the payload");
        listing::write(
            packed.listing.iter(),
            fs::File::create(directory.join(listing_file.as_str())).expect("a file"),
        )
        .expect("the listing");
        let archived_at = ARCHIVED_AT.parse().expect("a time");
        let payload = Payload {
            kind: PayloadKind::Archive,
            version,
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
        };
        let path = directory.join("meta.1");
        let metadata = match fs::read(&path) {
            Ok(bytes) => {
                let mut known = meta::parse(&bytes).expect("parses");
                known.payloads.push(payload);
                known
            }
            Err(_) => {
                let top = RelPath::parse(piece).expect("a path");
                let name = Name::from_bytes(top.components().last().expect("a name").to_vec())
                    .expect("a valid name");
                Metadata {
                    format: meta::FORMAT,
                    id: object(),
                    collection: COLLECTION_ID.parse().expect("an ID"),
                    path: top,
                    name,
                    created: archived_at,
                    created_source: CreatedSource::Birth,
                    lifecycle: Lifecycle::Archived,
                    payloads: vec![payload],
                    history: vec![HistoryEntry::new(
                        archived_at,
                        "laptop",
                        HistoryAction::Archive,
                    )],
                    extra: serde_json::Map::new(),
                }
            }
        };
        fs::write(&path, meta::to_bytes(&metadata).expect("serialises")).expect("metadata");

        let mut entry = TrackingEntry::new(
            object(),
            RelPath::parse(piece).expect("a path"),
            EntryState::Gone,
            VersionTakenOut::Committed(1),
            archived_at,
        );
        entry.local_payloads = metadata
            .payloads
            .iter()
            .map(|payload| LocalPayload {
                kind: PayloadKind::Archive,
                version: payload.version,
                file: payload.file.clone(),
            })
            .collect();
        fs::write(
            self.root().join(format!(".antiq/tracking/{OBJECT}.json")),
            serde_json::to_vec(&entry).expect("serialises"),
        )
        .expect("the entry");
        payload_file
    }

    fn unearth(&self, args: &[&str]) -> Run {
        Antiq::new()
            .arg("-C")
            .arg(self.root())
            .arg("unearth")
            .args(args)
            .run()
    }

    fn global(&self, flag: &str, args: &[&str]) -> Run {
        Antiq::new()
            .arg("-C")
            .arg(self.root())
            .arg(flag)
            .arg("unearth")
            .args(args)
            .run()
    }

    /// Every path below the collection with its content, directories as
    /// `None`, apart from the lock file.
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

#[test]
fn an_archived_piece_comes_back_to_its_path() {
    let mut world = world();
    world.archive(
        "alpha",
        1,
        &[("notes.txt", "hello"), ("src/main.rs", "fn main() {}")],
    );

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Unearthed alpha (archive version 1)");
    assert_eq!(run.stderr, "");
    assert_eq!(
        fs::read_to_string(world.root().join("alpha/notes.txt")).expect("reads"),
        "hello"
    );
    assert_eq!(
        fs::read_to_string(world.root().join("alpha/src/main.rs")).expect("reads"),
        "fn main() {}"
    );
}

#[test]
fn the_piece_can_be_named_by_its_id_or_a_prefix() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);

    let run = world.unearth(&[&OBJECT[..8]]);

    succeeded(&run);
    assert!(world.root().join("alpha/notes.txt").exists());
}

#[test]
fn json_names_the_piece_and_the_archive_it_came_from() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);

    let run = world.global("--json", &["alpha"]);

    succeeded(&run);
    let root = fs::canonicalize(world.root()).expect("the root exists");
    assert_eq!(
        run.json(),
        json!({
            "format": 1,
            "command": "unearth",
            "unearthed": true,
            "id": OBJECT,
            "path": "alpha",
            "abs_path": root.join("alpha").display().to_string(),
            "payload_version": 1,
            "warnings": [],
        })
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn what_the_start_of_the_run_found_is_reported_as_notes() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let stranger = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    fs::write(
        world
            .root()
            .join(format!(".antiq/tracking/{stranger}.json")),
        "not json",
    )
    .expect("a damaged entry");

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    assert!(
        run.stderr.starts_with(&format!(
            "note: the tracking entry of {stranger} cannot be used"
        )),
        "{}",
        run.stderr
    );
    let json = world.global("--json", &["alpha"]);
    succeeded(&json);
    assert_eq!(json.json()["warnings"].as_array().expect("a list").len(), 1);
}

#[test]
fn a_version_picks_that_archive() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "first")]);
    world.archive("alpha", 2, &[("notes.txt", "second")]);

    let run = world.unearth(&["--version", "1", "alpha"]);

    succeeded(&run);
    assert_eq!(
        fs::read_to_string(world.root().join("alpha/notes.txt")).expect("reads"),
        "first"
    );
}

#[test]
fn a_version_that_does_not_exist_is_an_error_that_names_the_versions() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "first")]);

    let run = world.global("--json", &["--version", "7", "alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
    assert_eq!(
        run.error_message(),
        "`alpha` has no archive of version 7; it has 1"
    );
}

#[test]
fn the_piece_can_go_to_another_path() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);

    let run = world.unearth(&["--to", "beta", "alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Unearthed beta (archive version 1)");
    assert!(world.root().join("beta/notes.txt").exists());
    assert!(!world.root().join("alpha").exists());
}

#[test]
fn a_target_the_patterns_do_not_select_is_refused() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let before = world.snapshot();

    let run = world.global("--json", &["--to", "deep/er", "alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
    assert_eq!(
        run.error_message(),
        "cannot unearth into `deep/er`: the patterns do not select it"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn a_target_inside_the_antiq_directory_is_refused() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let before = world.snapshot();

    let run = world.global("--json", &["--to", ".antiq/x", "alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(
        run.error_message(),
        "cannot unearth into `.antiq/x`: it is inside `.antiq/`"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn the_collection_root_is_refused_as_a_target() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);

    let run = world.global("--json", &["--to", ".", "alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(
        run.error_message(),
        "cannot unearth into ``: it is the collection root"
    );
}

#[test]
fn a_target_outside_the_collection_is_refused_with_its_own_kind() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let outside = tempfile::tempdir().expect("a folder outside");

    let run = world.global(
        "--json",
        &["--to", outside.path().to_str().expect("UTF-8"), "alpha"],
    );

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "outside_collection");
    assert_eq!(fs::read_dir(outside.path()).expect("lists").count(), 0);
}

#[test]
fn a_taken_target_is_refused_and_nothing_changes() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    world.temp.tree().file("alpha/mine.txt", "my own work");
    let before = world.snapshot();

    let run = world.global("--json", &["alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(
        run.error_message(),
        "cannot unearth into `alpha`: it is taken"
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn unearthing_twice_says_where_the_piece_is() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    succeeded(&world.unearth(&["alpha"]));
    let before = world.snapshot();

    let run = world.unearth(&["alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(
        run.stdout.replace(OBJECT, "<id>"),
        @"<id> is already unearthed at alpha"
    );
    assert_eq!(world.snapshot(), before);

    let json = world.global("--json", &["alpha"]);
    succeeded(&json);
    assert_eq!(json.json()["unearthed"], false);
    assert_eq!(json.json()["path"], "alpha");
    assert!(json.json().get("payload_version").is_none());
}

#[test]
fn an_archive_that_is_not_on_this_machine_is_an_error() {
    let mut world = world();
    let payload = world.archive("alpha", 1, &[("notes.txt", "hello")]);
    fs::remove_file(world.object_dir().join(payload.as_str())).expect("gone");

    let run = world.global("--json", &["alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(
        run.error_message(),
        "the archive of `alpha` is not on this machine and cannot be fetched yet"
    );
}

#[test]
fn a_damaged_archive_is_an_error_and_moves_nothing() {
    let mut world = world();
    let payload = world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let file = world.object_dir().join(payload.as_str());
    let mut bytes = fs::read(&file).expect("reads");
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&file, bytes).expect("written");
    let before = world.snapshot();

    let run = world.global("--json", &["alpha"]);

    assert_eq!(run.code(), 1);
    assert!(
        run.error_message().contains("does not match its metadata"),
        "{}",
        run.error_message()
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn an_unknown_piece_is_an_error_of_its_own_kind() {
    let world = world();

    let run = world.global("--json", &["nowhere"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "unknown_reference");
}

#[test]
fn outside_a_collection_it_is_an_error() {
    let tree = support::tree::Tree::new();

    let run = Antiq::new()
        .arg("-C")
        .arg(tree.root())
        .args(["--json", "unearth", "alpha"])
        .run();

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "not_a_collection");
}

#[test]
fn a_dry_run_says_what_it_would_do_and_changes_nothing() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let before = world.snapshot();

    let run = world.unearth(&["--dry-run", "alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Would unearth alpha from archive version 1");
    assert_eq!(world.snapshot(), before);
    assert!(
        !world.root().join(".antiq/lock").exists(),
        "a dry run takes no lock"
    );
}

#[test]
fn a_dry_run_of_a_piece_that_is_unearthed_has_nothing_to_do() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    succeeded(&world.unearth(&["alpha"]));

    let run = world.unearth(&["--dry-run", "alpha"]);

    succeeded(&run);
    insta::assert_snapshot!(run.stdout, @"Nothing to do.");
}

#[test]
fn a_dry_run_in_json_is_the_plan() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);

    let run = world.global("--json", &["--dry-run", "alpha"]);

    succeeded(&run);
    assert_eq!(
        run.json(),
        json!({
            "format": 1,
            "command": "unearth",
            "dry_run": true,
            "actions": [{
                "action": "unearth",
                "id": OBJECT,
                "path": "alpha",
                "payload_version": 1,
            }],
        })
    );
}

#[test]
fn a_dry_run_while_another_run_holds_the_lock_prints_the_note() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let _held = antiq::lock::acquire(&world.root().join(".antiq/lock"), "laptop", &SystemClock)
        .expect("the lock is free");

    let run = world.unearth(&["--dry-run", "alpha"]);

    succeeded(&run);
    assert!(
        run.stderr
            .starts_with("note: another run holds the lock (pid "),
        "{}",
        run.stderr
    );
    insta::assert_snapshot!(run.stdout, @"Would unearth alpha from archive version 1");
}

#[test]
fn a_run_while_another_run_holds_the_lock_is_refused() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    let _held = antiq::lock::acquire(&world.root().join(".antiq/lock"), "laptop", &SystemClock)
        .expect("the lock is free");

    let run = world.global("--json", &["alpha"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "lock_held");
    assert!(!world.root().join("alpha").exists());
}

#[test]
fn the_start_of_the_run_finishes_what_an_earlier_unearth_left() {
    let mut world = world();
    world.archive("alpha", 1, &[("notes.txt", "hello")]);
    succeeded(&world.unearth(&["alpha"]));
    // The run died after the rename: the entry still says `unearthing`
    // and the change is not recorded.
    let object = world.object_dir();
    fs::remove_file(object.join("meta.pending")).expect("the recorded change");
    let entry_path = world.root().join(format!(".antiq/tracking/{OBJECT}.json"));
    let mut entry: Value =
        serde_json::from_slice(&fs::read(&entry_path).expect("reads")).expect("json");
    entry["state"] = json!("unearthing");
    fs::write(&entry_path, serde_json::to_vec(&entry).expect("json")).expect("written");

    let next = world.unearth(&["alpha"]);

    succeeded(&next);
    let entry: Value =
        serde_json::from_slice(&fs::read(&entry_path).expect("reads")).expect("json");
    assert_eq!(entry["state"], "live");
    assert!(
        object.join("meta.pending").exists(),
        "the change was recorded by the repair"
    );
}
