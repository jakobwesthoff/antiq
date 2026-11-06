// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The record of pruned objects, `.antiq/pruned.json`.
//!
//! `reconcile --prune` drops the local copy of an object whose archive is
//! gone. The remote keeps the object's metadata, so without a record the
//! next pull would fetch it again. The record names each dropped object
//! with the highest committed metadata version this machine had. It stays
//! in force until a committed version above that number is stored for the
//! object: that version is new work on the remote and brings the object
//! back.

use crate::collection::Collection;
use crate::fsx::atomic::{flush_dir, write_atomic};
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::remote::KnownVersions;
use crate::store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;

const FORMAT: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum TombstoneError {
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error(transparent)]
    Fsx(#[from] crate::fsx::FsxError),
    #[error("{path} cannot be read: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("{path} has format {found}, which this version does not read")]
    UnknownFormat { path: String, found: u32 },
}

/// The contents of `pruned.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tombstones {
    pruned: BTreeMap<ObjectId, u32>,
}

#[derive(Serialize, Deserialize)]
struct Document {
    format: u32,
    pruned: Vec<Pruned>,
}

#[derive(Serialize, Deserialize)]
struct Pruned {
    id: ObjectId,
    version: u32,
}

impl Tombstones {
    /// Reads the record; a collection that never pruned has none.
    pub fn read(collection: &Collection) -> Result<Tombstones, TombstoneError> {
        let path = collection.pruned_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Tombstones::default());
            }
            Err(source) => {
                return Err(TombstoneError::Io {
                    action: "read",
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let document: Document =
            serde_json::from_slice(&bytes).map_err(|source| TombstoneError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        if document.format != FORMAT {
            return Err(TombstoneError::UnknownFormat {
                path: path.display().to_string(),
                found: document.format,
            });
        }
        let mut tombstones = Tombstones::default();
        for pruned in document.pruned {
            tombstones.record(pruned.id, pruned.version);
        }
        Ok(tombstones)
    }

    /// Writes the record atomically and makes it durable.
    pub fn write(&self, collection: &Collection, _lock: &RunLock) -> Result<(), TombstoneError> {
        let document = Document {
            format: FORMAT,
            pruned: self
                .iter()
                .map(|(id, version)| Pruned { id, version })
                .collect(),
        };
        let mut bytes = serde_json::to_vec(&document).expect("plain data always serialises");
        bytes.push(b'\n');
        write_atomic(&collection.pruned_path(), &bytes)?;
        flush_dir(&collection.antiq_dir())?;
        Ok(())
    }

    /// Records that `id` was pruned with `version` as its highest committed
    /// version. A lower number than the one recorded changes nothing.
    pub fn record(&mut self, id: ObjectId, version: u32) {
        let recorded = self.pruned.entry(id).or_insert(version);
        *recorded = (*recorded).max(version);
    }

    pub fn recorded(&self, id: ObjectId) -> Option<u32> {
        self.pruned.get(&id).copied()
    }

    /// Every pruned object with its recorded version, in ID order.
    pub fn iter(&self) -> impl Iterator<Item = (ObjectId, u32)> + '_ {
        self.pruned.iter().map(|(id, version)| (*id, *version))
    }

    /// Whether a committed version `number` of `id` is covered by the
    /// record, that is, is not newer than what was pruned.
    pub fn covers(&self, id: ObjectId, number: u32) -> bool {
        self.recorded(id).is_some_and(|recorded| number <= recorded)
    }
}

/// The pruned objects whose record is in force: no committed version above
/// the recorded one is stored.
pub fn active(collection: &Collection) -> Result<BTreeSet<ObjectId>, StoreError> {
    let tombstones = Tombstones::read(collection)?;
    let store = Store::new(collection);
    let mut active = BTreeSet::new();
    for (id, recorded) in tombstones.iter() {
        let highest = store.committed_versions(id)?.last().copied().unwrap_or(0);
        if highest <= recorded {
            active.insert(id);
        }
    }
    Ok(active)
}

/// The highest committed metadata version this machine has of each object,
/// for `Remote::pull_metadata`. A pruned object counts with the version
/// recorded for it, so that the pull brings only versions above it.
/// `sync` and `unearth` build their `known` through this function as well.
pub fn known_versions(collection: &Collection) -> Result<KnownVersions, StoreError> {
    let store = Store::new(collection);
    let mut known = KnownVersions::new();
    for id in store.objects()? {
        if let Some(highest) = store.committed_versions(id)?.last() {
            known.insert(id, *highest);
        }
    }
    for (id, recorded) in Tombstones::read(collection)?.iter() {
        known.insert(id, known.highest(id).max(recorded));
    }
    Ok(known)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::lock;
    use crate::store::Store;
    use crate::testing::tree::TempCollection;
    use std::fs;

    const ONE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const TWO: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a ULID")
    }

    struct Fixture {
        _temp: TempCollection,
        collection: Collection,
        lock: RunLock,
    }

    fn fixture() -> Fixture {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        let clock = FixedClock::new("2026-01-02T03:04:05Z".parse().expect("valid"));
        let lock = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("free");
        Fixture {
            _temp: temp,
            collection,
            lock,
        }
    }

    impl Fixture {
        fn put_version(&self, object: &str, number: u32) {
            let dir = self.collection.object_dir(id(object));
            fs::create_dir_all(&dir).expect("dirs");
            fs::write(dir.join(format!("meta.{number}")), b"x").expect("write");
        }

        fn prune(&self, object: &str, version: u32) {
            let mut tombstones = Tombstones::read(&self.collection).expect("reads");
            tombstones.record(id(object), version);
            tombstones
                .write(&self.collection, &self.lock)
                .expect("writes");
        }
    }

    #[test]
    fn a_collection_without_the_file_has_no_tombstones() {
        let fx = fixture();
        assert_eq!(
            Tombstones::read(&fx.collection)
                .expect("reads")
                .iter()
                .count(),
            0
        );
    }

    #[test]
    fn tombstones_are_written_as_the_format_says_and_read_back() {
        let fx = fixture();
        fx.prune(ONE, 3);
        assert_eq!(
            fs::read_to_string(fx.collection.pruned_path()).expect("the file"),
            format!("{{\"format\":1,\"pruned\":[{{\"id\":\"{ONE}\",\"version\":3}}]}}\n")
        );
        let read = Tombstones::read(&fx.collection).expect("reads");
        assert_eq!(read.recorded(id(ONE)), Some(3));
        assert_eq!(read.recorded(id(TWO)), None);
    }

    #[test]
    fn recording_again_keeps_the_higher_version() {
        let mut tombstones = Tombstones::default();
        tombstones.record(id(ONE), 3);
        tombstones.record(id(ONE), 2);
        assert_eq!(tombstones.recorded(id(ONE)), Some(3));
        tombstones.record(id(ONE), 5);
        assert_eq!(tombstones.recorded(id(ONE)), Some(5));
    }

    #[test]
    fn a_file_that_cannot_be_parsed_is_an_error() {
        let fx = fixture();
        fs::write(fx.collection.pruned_path(), b"{").expect("write");
        let error = Tombstones::read(&fx.collection).expect_err("not JSON");
        assert!(matches!(error, TombstoneError::Parse { .. }), "{error}");
    }

    #[test]
    fn a_file_of_another_format_is_an_error() {
        let fx = fixture();
        fs::write(fx.collection.pruned_path(), br#"{"format":2,"pruned":[]}"#).expect("write");
        let error = Tombstones::read(&fx.collection).expect_err("format 2");
        assert!(
            matches!(error, TombstoneError::UnknownFormat { found: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error() {
        let fx = fixture();
        fs::create_dir(fx.collection.pruned_path()).expect("a directory in its place");
        let error = Tombstones::read(&fx.collection).expect_err("a directory");
        assert!(matches!(error, TombstoneError::Io { .. }), "{error}");
    }

    #[test]
    fn a_tombstone_is_active_until_a_higher_version_is_stored() {
        let fx = fixture();
        fx.prune(ONE, 3);
        assert_eq!(
            active(&fx.collection).expect("lists"),
            BTreeSet::from([id(ONE)])
        );
        fx.put_version(ONE, 3);
        assert_eq!(
            active(&fx.collection).expect("lists"),
            BTreeSet::from([id(ONE)])
        );
        fx.put_version(ONE, 4);
        assert!(active(&fx.collection).expect("lists").is_empty());
    }

    #[test]
    fn known_versions_hold_the_recorded_version_of_a_pruned_object() {
        let fx = fixture();
        fx.put_version(TWO, 2);
        fx.prune(ONE, 3);
        let known = known_versions(&fx.collection).expect("lists");
        assert_eq!(known.highest(id(ONE)), 3);
        assert_eq!(known.highest(id(TWO)), 2);
    }

    #[test]
    fn known_versions_hold_the_local_version_of_a_lifted_tombstone() {
        let fx = fixture();
        fx.prune(ONE, 3);
        fx.put_version(ONE, 5);
        assert_eq!(
            known_versions(&fx.collection)
                .expect("lists")
                .highest(id(ONE)),
            5
        );
    }

    #[test]
    fn the_store_skips_a_tombstoned_object_and_stores_a_newer_version() {
        let fx = fixture();
        fx.prune(ONE, 3);
        let store = Store::new(&fx.collection);
        let number = |n: u32| std::num::NonZeroU32::new(n).expect("not zero");
        store
            .store_committed(&fx.lock, id(ONE), number(3), b"old")
            .expect("skipped");
        assert!(
            !fx.collection.object_dir(id(ONE)).exists(),
            "nothing is stored"
        );
        store
            .store_committed(&fx.lock, id(ONE), number(4), b"new")
            .expect("stored");
        assert_eq!(store.objects().expect("lists"), vec![id(ONE)]);
    }

    #[test]
    fn the_store_does_not_list_a_tombstoned_object_whose_directory_is_left() {
        let fx = fixture();
        fx.put_version(ONE, 3);
        fx.put_version(TWO, 1);
        fx.prune(ONE, 3);
        assert_eq!(
            Store::new(&fx.collection).objects().expect("lists"),
            vec![id(TWO)]
        );
    }
}
