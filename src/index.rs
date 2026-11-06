// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The derived, rebuildable index and search.
//!
//! The index has two parts. The object table says which objects this
//! machine knows and where they are; it is computed from the local store
//! and the tracking entries on every call and never stored, so it cannot
//! be stale.

use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::id::ObjectId;
use crate::listing::ListingError;
use crate::meta::{FileName, Lifecycle, Metadata, Payload, PayloadKind};
use crate::relpath::{Name, RelPath};
use crate::store::{Store, StoreError};
use crate::tracking::{self, EntryState, TrackingError};
use jiff::Timestamp;
use std::io;

/// Errors of the index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
    #[error(transparent)]
    Fsx(#[from] FsxError),
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error("the listing of {id} cannot be read: {source}")]
    Listing { id: ObjectId, source: ListingError },
}

impl crate::error::HasKind for IndexError {}

/// An object with readable metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    pub id: ObjectId,
    /// The path in the metadata.
    pub path: RelPath,
    pub name: Name,
    /// The collection that owns the object.
    pub collection: ObjectId,
    pub created: Timestamp,
    pub lifecycle: Lifecycle,
    /// The size of the current payload.
    pub size: Option<u64>,
    /// The listing of the current payload.
    pub listing: Option<FileName>,
}

impl IndexEntry {
    fn of(metadata: &Metadata) -> IndexEntry {
        let payload = current_payload(metadata);
        IndexEntry {
            id: metadata.id,
            path: metadata.path.clone(),
            name: metadata.name.clone(),
            collection: metadata.collection,
            created: metadata.created,
            lifecycle: metadata.lifecycle,
            size: payload.map(|payload| payload.size),
            listing: payload.and_then(|payload| payload.listing.clone()),
        }
    }
}

/// The payload that stands for the object: the newest archive that did
/// not lose a conflict, else the newest live package. Its size and its
/// listing are the ones the index reports, so they describe one file.
fn current_payload(metadata: &Metadata) -> Option<&Payload> {
    let newest = |kind: PayloadKind, allow_superseded: bool| {
        metadata
            .payloads
            .iter()
            .filter(|payload| payload.kind == kind && (allow_superseded || !payload.superseded))
            .max_by_key(|payload| payload.version)
    };
    newest(PayloadKind::Archive, false).or_else(|| newest(PayloadKind::Live, true))
}

/// A tracking entry of this machine, or a tracking file that cannot be
/// used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedRef {
    pub id: ObjectId,
    /// `None` for a damaged file.
    pub path: Option<RelPath>,
    pub state: Option<EntryState>,
}

/// An object directory whose metadata cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub id: ObjectId,
    pub reason: String,
}

/// The objects at one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtPath {
    /// The object whose tracking entry says its folder is at the path or
    /// on its way in or out. The spec allows one per path.
    pub claimant: Option<ObjectId>,
    /// Every other object recorded at the path, such as archived ones.
    pub others: Vec<ObjectId>,
}

/// The objects this machine knows, sorted by ID.
#[derive(Debug, Clone, Default)]
pub struct Index {
    entries: Vec<IndexEntry>,
    tracked: Vec<TrackedRef>,
    unreadable: Vec<Unreadable>,
}

impl Index {
    /// Reads the store and the tracking entries. Nothing is written.
    pub fn compute(collection: &Collection) -> Result<Index, IndexError> {
        let store = Store::new(collection);
        let mut entries = Vec::new();
        let mut unreadable = Vec::new();
        for id in store.objects()? {
            // One object that cannot be read must not stop a search or a
            // reference to any other object, so it is listed instead.
            match store.current(id) {
                Ok(Some(current)) => entries.push(IndexEntry::of(&current.metadata)),
                Ok(None) => unreadable.push(Unreadable {
                    id,
                    reason: "no metadata".into(),
                }),
                Err(error) => unreadable.push(Unreadable {
                    id,
                    reason: error.to_string(),
                }),
            }
        }

        let set = tracking::read_all(collection)?;
        let mut tracked: Vec<TrackedRef> = set
            .entries
            .into_iter()
            .map(|entry| TrackedRef {
                id: entry.id,
                path: Some(entry.path),
                state: Some(entry.state),
            })
            .chain(set.damaged.into_iter().map(|damaged| TrackedRef {
                id: damaged.id,
                path: None,
                state: None,
            }))
            .collect();
        tracked.sort_by_key(|tracked| tracked.id);

        Ok(Index {
            entries,
            tracked,
            unreadable,
        })
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    pub fn tracked(&self) -> &[TrackedRef] {
        &self.tracked
    }

    pub fn unreadable(&self) -> &[Unreadable] {
        &self.unreadable
    }

    pub fn get(&self, id: ObjectId) -> Option<&IndexEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Every ID this machine knows: objects with metadata, object
    /// directories without usable metadata, and tracking files.
    pub fn known_ids(&self) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = self
            .entries
            .iter()
            .map(|entry| entry.id)
            .chain(self.unreadable.iter().map(|object| object.id))
            .chain(self.tracked.iter().map(|tracked| tracked.id))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// The path of an object: the one in its metadata, else the one of its
    /// tracking entry.
    pub fn path_of(&self, id: ObjectId) -> Option<&RelPath> {
        self.get(id).map(|entry| &entry.path).or_else(|| {
            self.tracked
                .iter()
                .find(|tracked| tracked.id == id)
                .and_then(|tracked| tracked.path.as_ref())
        })
    }

    /// The objects recorded at `path`, by metadata or by tracking entry.
    pub fn at_path(&self, path: &RelPath) -> AtPath {
        let mut ids: Vec<ObjectId> = self
            .entries
            .iter()
            .filter(|entry| &entry.path == path)
            .map(|entry| entry.id)
            .chain(
                self.tracked
                    .iter()
                    .filter(|tracked| tracked.path.as_ref() == Some(path))
                    .map(|tracked| tracked.id),
            )
            .collect();
        ids.sort();
        ids.dedup();

        let mut claimant = None;
        let mut others = Vec::new();
        for id in ids {
            let holds_the_folder = self.tracked.iter().any(|tracked| {
                tracked.id == id
                    && tracked.path.as_ref() == Some(path)
                    && matches!(
                        tracked.state,
                        Some(EntryState::Unearthing | EntryState::Live | EntryState::Removing)
                    )
            });
            if holds_the_folder && claimant.is_none() {
                claimant = Some(id);
            } else {
                others.push(id);
            }
        }
        AtPath { claimant, others }
    }

    /// The known IDs that start with `prefix`, compared in upper case.
    pub fn ids_with_prefix(&self, prefix: &str) -> Vec<ObjectId> {
        let prefix = prefix.to_ascii_uppercase();
        self.known_ids()
            .into_iter()
            .filter(|id| id.to_string().starts_with(&prefix))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::lock::{self, RunLock};
    use crate::testing::tree::TempCollection;
    use crate::tracking::{self, TrackingEntry, VersionTakenOut};
    use std::fs;
    use std::path::Path;

    const A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const B: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
    const C: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a ULID")
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    /// One payload as the metadata document spells it. The file names
    /// follow the remote layout, derived from the version.
    struct PayloadSpec {
        kind: &'static str,
        version: u32,
        size: u64,
        superseded: bool,
    }

    impl PayloadSpec {
        fn archive(version: u32, size: u64) -> PayloadSpec {
            PayloadSpec {
                kind: "archive",
                version,
                size,
                superseded: false,
            }
        }

        fn live(version: u32, size: u64) -> PayloadSpec {
            PayloadSpec {
                kind: "live",
                version,
                size,
                superseded: false,
            }
        }

        fn superseded(mut self) -> PayloadSpec {
            self.superseded = true;
            self
        }

        fn file_name(&self) -> String {
            format!("{:016x}-{}.tar.zst", self.version, self.kind)
        }

        fn listing_name(&self) -> String {
            format!("{:016x}.listing.zst", self.version)
        }

        fn json(&self) -> String {
            let listing = if self.kind == "archive" {
                format!(r#""listing":"{}","#, self.listing_name())
            } else {
                String::new()
            };
            let superseded = if self.superseded {
                r#","superseded":true"#
            } else {
                ""
            };
            format!(
                concat!(
                    r#"{{"kind":"{}","version":{},"file":"{}","package_format":"tar+zstd/1","#,
                    r#""size":{},"checksum":{{"algorithm":"sha256","value":"00"}},{}"#,
                    r#""machine":"imperius","created":"2026-01-02T03:04:05Z"{}}}"#
                ),
                self.kind,
                self.version,
                self.file_name(),
                self.size,
                listing,
                superseded
            )
        }
    }

    /// A collection in a temp directory with the run lock held.
    struct Fixture {
        temp: TempCollection,
        collection: Collection,
        lock: RunLock,
        clock: FixedClock,
    }

    impl Fixture {
        fn new() -> Fixture {
            let temp = TempCollection::minimal();
            let collection = Collection::find(temp.root()).expect("a collection");
            let clock = FixedClock::new("2026-01-02T03:04:05Z".parse().expect("valid"));
            let lock = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("free");
            Fixture {
                temp,
                collection,
                lock,
                clock,
            }
        }

        /// Writes `meta.1` of `object`.
        fn object(&mut self, object: &str, piece: &str, lifecycle: &str, payloads: &[PayloadSpec]) {
            let payloads: Vec<String> = payloads.iter().map(PayloadSpec::json).collect();
            let name = piece.rsplit('/').next().expect("a name");
            let document = format!(
                concat!(
                    r#"{{"format":1,"id":"{}","collection":"{}","path":"{}","name":"{}","#,
                    r#""created":"2020-09-13T12:26:40Z","created_source":"birth","#,
                    r#""lifecycle":"{}","payloads":[{}],"history":[]}}"#
                ),
                object,
                crate::testing::tree::COLLECTION_ID,
                piece,
                name,
                lifecycle,
                payloads.join(",")
            );
            self.temp
                .tree()
                .file(&format!(".antiq/objects/{object}/meta.1"), document);
        }

        fn track(&mut self, object: &str, piece: &str, state: EntryState) {
            let mut entry = TrackingEntry::new(
                id(object),
                path(piece),
                state,
                VersionTakenOut::Committed(1),
                self.clock.now(),
            );
            tracking::write(&self.collection, &self.lock, &self.clock, &mut entry)
                .expect("the entry is written");
        }

        fn compute(&self) -> Index {
            Index::compute(&self.collection).expect("computes")
        }

        fn index_dir(&self) -> std::path::PathBuf {
            self.collection.index_dir()
        }
    }

    use crate::clock::Clock;

    #[test]
    fn compute_lists_objects_with_their_fields() {
        let mut fx = Fixture::new();
        fx.object(
            B,
            "projects/beta",
            "archived",
            &[PayloadSpec::archive(1, 700)],
        );
        fx.object(A, "projects/alpha", "live", &[]);
        let index = fx.compute();
        let ids: Vec<ObjectId> = index.entries().iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec![id(A), id(B)], "sorted by ID");
        let beta = index.get(id(B)).expect("beta");
        assert_eq!(beta.path, path("projects/beta"));
        assert_eq!(beta.name.as_bytes(), b"beta");
        assert_eq!(beta.collection, id(crate::testing::tree::COLLECTION_ID));
        assert_eq!(beta.created, "2020-09-13T12:26:40Z".parse().expect("valid"));
        assert_eq!(beta.lifecycle, Lifecycle::Archived);
        assert_eq!(beta.size, Some(700));
        assert_eq!(
            beta.listing.as_ref().map(FileName::as_str),
            Some("0000000000000001.listing.zst")
        );
    }

    #[test]
    fn the_current_payload_is_the_newest_archive_that_is_not_superseded() {
        let mut fx = Fixture::new();
        fx.object(
            A,
            "a",
            "archived",
            &[
                PayloadSpec::archive(1, 100),
                PayloadSpec::archive(2, 200),
                PayloadSpec::archive(3, 300).superseded(),
                PayloadSpec::live(4, 400),
            ],
        );
        let entry = fx.compute().get(id(A)).cloned().expect("a");
        assert_eq!(entry.size, Some(200));
        assert_eq!(
            entry.listing.as_ref().map(FileName::as_str),
            Some("0000000000000002.listing.zst")
        );
    }

    #[test]
    fn an_object_with_only_live_packages_has_the_newest_ones_size_and_no_listing() {
        let mut fx = Fixture::new();
        fx.object(
            A,
            "a",
            "live",
            &[PayloadSpec::live(1, 10), PayloadSpec::live(2, 20)],
        );
        let entry = fx.compute().get(id(A)).cloned().expect("a");
        assert_eq!(entry.size, Some(20));
        assert_eq!(entry.listing, None);
    }

    #[test]
    fn an_object_without_payloads_has_no_size_and_no_listing() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "live", &[]);
        let entry = fx.compute().get(id(A)).cloned().expect("a");
        assert_eq!(entry.size, None);
        assert_eq!(entry.listing, None);
    }

    #[test]
    fn a_tracking_entry_without_metadata_is_known_and_no_entry() {
        let mut fx = Fixture::new();
        fx.track(A, "lost", EntryState::Gone);
        let index = fx.compute();
        assert!(index.entries().is_empty());
        assert_eq!(index.known_ids(), vec![id(A)]);
        assert_eq!(index.path_of(id(A)), Some(&path("lost")));
        assert_eq!(
            index.tracked(),
            &[TrackedRef {
                id: id(A),
                path: Some(path("lost")),
                state: Some(EntryState::Gone),
            }]
        );
    }

    #[test]
    fn a_damaged_tracking_file_is_known_without_a_path() {
        let mut fx = Fixture::new();
        fx.temp
            .tree()
            .file(&format!(".antiq/tracking/{A}.json"), "not json");
        let index = fx.compute();
        assert_eq!(index.known_ids(), vec![id(A)]);
        assert_eq!(index.path_of(id(A)), None);
        assert_eq!(index.tracked()[0].path, None);
    }

    #[test]
    fn metadata_that_cannot_be_read_is_listed_and_known() {
        let mut fx = Fixture::new();
        fx.temp
            .tree()
            .file(&format!(".antiq/objects/{A}/meta.1"), "garbage");
        let index = fx.compute();
        assert!(index.entries().is_empty());
        assert_eq!(index.known_ids(), vec![id(A)]);
        assert_eq!(index.unreadable().len(), 1);
        assert_eq!(index.unreadable()[0].id, id(A));
    }

    #[test]
    fn an_object_directory_without_metadata_is_unreadable_with_that_reason() {
        let mut fx = Fixture::new();
        fx.temp.tree().file(
            &format!(".antiq/objects/{A}/0000000000000001.listing.zst"),
            "x",
        );
        let index = fx.compute();
        assert_eq!(
            index.unreadable(),
            &[Unreadable {
                id: id(A),
                reason: "no metadata".into()
            }]
        );
        assert_eq!(index.known_ids(), vec![id(A)]);
    }

    #[test]
    fn at_path_finds_objects_by_metadata_path_and_by_tracking_path() {
        let mut fx = Fixture::new();
        fx.object(A, "projects/alpha", "archived", &[]);
        fx.object(B, "projects/beta", "archived", &[]);
        fx.track(B, "elsewhere/beta", EntryState::Gone);
        let index = fx.compute();
        let by_metadata = index.at_path(&path("projects/alpha"));
        assert_eq!(by_metadata.claimant, None);
        assert_eq!(by_metadata.others, vec![id(A)]);
        let by_tracking = index.at_path(&path("elsewhere/beta"));
        assert_eq!(by_tracking.others, vec![id(B)]);
        assert_eq!(index.at_path(&path("projects/gamma")).others, vec![]);
    }

    #[test]
    fn at_path_names_the_object_whose_folder_is_there_as_the_claimant() {
        // After a fork the old ID stays archived at the path and the new ID
        // is live there.
        let mut fx = Fixture::new();
        fx.object(A, "proj", "archived", &[]);
        fx.track(A, "proj", EntryState::Gone);
        fx.object(B, "proj", "live", &[]);
        fx.track(B, "proj", EntryState::Live);
        let at = fx.compute().at_path(&path("proj"));
        assert_eq!(at.claimant, Some(id(B)));
        assert_eq!(at.others, vec![id(A)]);
    }

    #[test]
    fn a_tracking_entry_that_moved_does_not_claim_the_metadata_path() {
        let mut fx = Fixture::new();
        fx.object(A, "proj", "live", &[]);
        fx.track(A, "moved", EntryState::Live);
        let index = fx.compute();
        let at = index.at_path(&path("proj"));
        assert_eq!(at.claimant, None);
        assert_eq!(at.others, vec![id(A)]);
        assert_eq!(index.at_path(&path("moved")).claimant, Some(id(A)));
    }

    #[test]
    fn prefixes_match_in_either_case_and_count_every_known_id() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "live", &[]);
        fx.track(C, "c", EntryState::Gone);
        fx.temp
            .tree()
            .file(&format!(".antiq/objects/{B}/meta.1"), "garbage");
        let index = fx.compute();
        assert_eq!(index.ids_with_prefix("01arz3"), vec![id(A)]);
        assert_eq!(index.ids_with_prefix("01BX5Z"), vec![id(B)]);
        assert_eq!(index.ids_with_prefix("01d39z"), vec![id(C)]);
        assert_eq!(index.ids_with_prefix("01"), vec![id(A), id(B), id(C)]);
        assert_eq!(index.ids_with_prefix("02"), vec![]);
    }

    #[test]
    fn compute_writes_nothing() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "live", &[PayloadSpec::archive(1, 1)]);
        let index_dir = fx.index_dir();
        fs::remove_dir(&index_dir).expect("the fixture's index directory is empty");
        assert!(!index_dir.exists(), "the precondition: no index directory");
        let _ = fx.compute();
        assert!(!index_dir.exists(), "compute made the index directory");
    }

    #[test]
    fn compute_leaves_an_existing_index_alone() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "live", &[]);
        fx.temp.tree().file(".antiq/index/stray", "x");
        let before = snapshot(&fx.index_dir());
        let _ = fx.compute();
        assert_eq!(snapshot(&fx.index_dir()), before);
    }

    /// Names, sizes and modification times of the files in `dir`.
    fn snapshot(dir: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .expect("lists")
            .map(|entry| {
                let entry = entry.expect("entry");
                let metadata = entry.metadata().expect("metadata");
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    metadata.len(),
                    metadata.modified().expect("mtime"),
                )
            })
            .collect();
        files.sort();
        files
    }
}
