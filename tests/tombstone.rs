// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A pruned object seen from outside the binary: the commands leave it out
//! and the next run finishes a drop that stopped half way.

mod support;

use antiq::meta::Lifecycle;
use antiq::tombstone::Tombstones;
use antiq::tracking::EntryState;
use serde_json::json;
use std::fs;
use support::cmd::{Antiq, Run};
use support::objects::{
    archive_with_members, entry, hold_lock, id, metadata, put_entry, put_meta, snapshot,
};
use support::tree::{COLLECTION_ID, TempCollection};

fn antiq_in(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

/// Two archived objects, `alpha` (1) and `beta` (2), with their entries.
fn world() -> TempCollection {
    let temp = TempCollection::minimal();
    for (number, at) in [(1, "alpha"), (2, "beta")] {
        let archive = archive_with_members(temp.root(), number, 1, 10, &["src/main.rs"]);
        put_meta(
            temp.root(),
            &metadata(
                number,
                at,
                Lifecycle::Archived,
                COLLECTION_ID,
                vec![archive],
            ),
        );
        put_entry(temp.root(), &entry(number, at, EntryState::Gone));
    }
    temp
}

/// Records object `number` as pruned at metadata version 1.
fn prune(temp: &TempCollection, number: u8) {
    let collection = antiq::collection::Collection::find(temp.root()).expect("a collection");
    let lock = hold_lock(temp.root());
    let mut tombstones = Tombstones::read(&collection).expect("reads");
    tombstones.record(id(number), 1);
    tombstones.write(&collection, &lock).expect("writes");
}

fn object_dir(temp: &TempCollection, number: u8) -> std::path::PathBuf {
    temp.root().join(format!(".antiq/objects/{}", id(number)))
}

#[test]
fn status_leaves_a_pruned_object_out() {
    let temp = world();
    prune(&temp, 1);
    let before = snapshot(temp.root());
    let run = antiq_in(&temp, &["status"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(run.stdout.contains("0 live · 1 archived"), "{}", run.stdout);
    assert_eq!(snapshot(temp.root()), before, "status changes nothing");
}

#[test]
fn reconcile_finishes_a_drop_that_stopped_after_the_record() {
    let temp = world();
    prune(&temp, 1);
    fs::write(
        temp.root()
            .join(format!(".antiq/tracking/{}.manifest.json", id(1))),
        "{}",
    )
    .expect("a manifest");
    let run = antiq_in(&temp, &["--json", "reconcile"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(!object_dir(&temp, 1).exists());
    assert!(
        !temp
            .root()
            .join(format!(".antiq/tracking/{}.json", id(1)))
            .exists()
    );
    assert!(
        !temp
            .root()
            .join(format!(".antiq/tracking/{}.manifest.json", id(1)))
            .exists()
    );
    assert!(object_dir(&temp, 2).exists());
    assert_eq!(
        run.json()["index"],
        json!({"written": 1, "removed": 0, "skipped": []})
    );
    assert!(
        !temp
            .root()
            .join(format!(".antiq/index/{}.json", id(1)))
            .exists()
    );
}
