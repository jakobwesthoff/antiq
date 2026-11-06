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
use crate::fsx::atomic::{flush_dir, write_atomic};
use crate::id::ObjectId;
use crate::listing::{self, ListingError};
use crate::lock::RunLock;
use crate::meta::{FileName, Lifecycle, Metadata, Payload, PayloadKind};
use crate::relpath::{Name, RelPath};
use crate::store::{Store, StoreError};
use crate::tracking::{self, EntryState, TrackingError};
use jiff::Timestamp;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32String};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::Path;

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

/// What [`update`] and [`rebuild`] did to the member cache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Updated {
    /// Cache files written.
    pub written: usize,
    /// Items removed from the cache directory.
    pub removed: usize,
    /// Objects whose listing could not be read, with the reason. They get
    /// no cache file.
    pub skipped: Vec<(ObjectId, String)>,
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

    /// The member paths of the object's current listing, in listing order.
    /// `None` when the object has no listing or its listing is not stored
    /// on this machine.
    ///
    /// The cache file is used when it names the object's current listing.
    /// Otherwise the listing itself is read; nothing is written.
    pub fn members(
        &self,
        collection: &Collection,
        id: ObjectId,
    ) -> Result<Option<Vec<RelPath>>, IndexError> {
        let Some(listing) = self.get(id).and_then(|entry| entry.listing.as_ref()) else {
            return Ok(None);
        };
        if let Some(members) = read_cache(collection, id, listing) {
            return Ok(Some(members));
        }
        read_listing(collection, id, listing)
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

/// Brings the member cache in line with the store: a file for every
/// object whose current listing is stored here and readable, and nothing
/// else.
///
/// ```compile_fail,E0061
/// use antiq::collection::Collection;
/// use antiq::index::update;
///
/// // The lock proof is a parameter, and `RunLock` cannot be built outside
/// // the `lock` module (see its own doctest, E0451), so an update without
/// // a lock does not compile.
/// fn refresh(collection: &Collection) {
///     let _ = update(collection);
/// }
/// ```
pub fn update(collection: &Collection, _lock: &RunLock) -> Result<Updated, IndexError> {
    let index = Index::compute(collection)?;
    let dir = collection.index_dir();
    let created = !dir.is_dir();
    fs::create_dir_all(&dir).map_err(io_error("create", &dir))?;
    if created {
        flush_dir(&collection.antiq_dir())?;
    }

    let store = Store::new(collection);
    let mut updated = Updated::default();
    let mut wanted = BTreeSet::new();
    for entry in index.entries() {
        let Some(listing) = &entry.listing else {
            continue;
        };
        // A listing that is not stored here gives no members. A cache file
        // for it would answer for a listing the store cannot show.
        let Some(file) = open_listing(&store, entry.id, listing)? else {
            continue;
        };
        let name = cache_name(entry.id);
        if read_cache(collection, entry.id, listing).is_some() {
            wanted.insert(name);
            continue;
        }
        match read_members(entry.id, file) {
            Ok(members) => {
                let file = CacheFile {
                    format: CACHE_FORMAT,
                    listing: listing.as_str().to_string(),
                    members,
                };
                let mut bytes =
                    serde_json::to_vec(&file).expect("paths and names are plain strings");
                bytes.push(b'\n');
                write_atomic(&dir.join(&name), &bytes)?;
                wanted.insert(name);
                updated.written += 1;
            }
            Err(error) => updated.skipped.push((entry.id, error.to_string())),
        }
    }

    // Removing comes after writing, so an interrupted run leaves every
    // file a correct state would have, and possibly some more.
    for item in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
        let item = item.map_err(io_error("list", &dir))?;
        if item
            .file_name()
            .to_str()
            .is_some_and(|name| wanted.contains(name))
        {
            continue;
        }
        remove_item(&item.path())?;
        updated.removed += 1;
    }
    flush_dir(&dir)?;
    Ok(updated)
}

/// Empties the member cache and builds it again.
pub fn rebuild(collection: &Collection, lock: &RunLock) -> Result<Updated, IndexError> {
    let dir = collection.index_dir();
    let mut cleared = 0;
    if dir.is_dir() {
        for item in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
            let item = item.map_err(io_error("list", &dir))?;
            remove_item(&item.path())?;
            cleared += 1;
        }
    }
    let mut updated = update(collection, lock)?;
    updated.removed += cleared;
    Ok(updated)
}

// =========================================================
// The member cache
// =========================================================

const CACHE_FORMAT: u32 = 1;

/// The members of one listing, as `.antiq/index/<ULID>.json` holds them.
/// A listing's file name follows from its payload's checksum and never
/// refers to other content, so a file that names the object's current
/// listing needs no further check.
#[derive(Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    listing: String,
    members: Vec<RelPath>,
}

fn cache_name(id: ObjectId) -> String {
    format!("{id}.json")
}

/// The members from the cache file, if it exists, parses, has the format
/// this version writes and names `listing`.
fn read_cache(collection: &Collection, id: ObjectId, listing: &FileName) -> Option<Vec<RelPath>> {
    let bytes = fs::read(collection.index_dir().join(cache_name(id))).ok()?;
    let file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    (file.format == CACHE_FORMAT && file.listing == listing.as_str()).then_some(file.members)
}

/// The paths of the stored listing; `None` when this machine does not
/// have the file.
fn read_listing(
    collection: &Collection,
    id: ObjectId,
    listing: &FileName,
) -> Result<Option<Vec<RelPath>>, IndexError> {
    match open_listing(&Store::new(collection), id, listing)? {
        Some(file) => read_members(id, file).map(Some),
        None => Ok(None),
    }
}

fn open_listing(
    store: &Store<'_>,
    id: ObjectId,
    listing: &FileName,
) -> Result<Option<File>, IndexError> {
    let path = store.file_path(id, listing);
    match File::open(&path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("open", &path)(error)),
    }
}

fn read_members(id: ObjectId, file: File) -> Result<Vec<RelPath>, IndexError> {
    let entries = listing::read_all(file).map_err(|source| IndexError::Listing { id, source })?;
    Ok(entries.into_iter().map(|entry| entry.path).collect())
}

fn remove_item(path: &Path) -> Result<(), IndexError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error("stat", path))?;
    let removed = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    removed.map_err(io_error("remove", path))
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> IndexError {
    let path = path.display().to_string();
    move |source| IndexError::Io {
        action,
        path,
        source,
    }
}

// =========================================================
// Search
// =========================================================

/// A fuzzy query. It is the one place antiq scores text against what the
/// user typed, so `find` and the picker rank alike.
///
/// Matching follows ntropy's picker: smart case (a capital makes the match
/// case sensitive), smart normalisation, and several words that must all
/// match the same text.
pub struct Query {
    pattern: Pattern,
    matcher: Matcher,
    empty: bool,
}

impl Query {
    pub fn parse(text: &str) -> Query {
        let text = text.trim();
        Query {
            pattern: Pattern::parse(text, CaseMatching::Smart, Normalization::Smart),
            matcher: Matcher::new(Config::DEFAULT),
            empty: text.is_empty(),
        }
    }

    /// A query without atoms matches everything.
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// The score of `haystack`, or `None` when it does not match.
    pub fn score(&mut self, haystack: &str) -> Option<u32> {
        if self.empty {
            return Some(0);
        }
        let haystack = Utf32String::from(haystack);
        self.pattern.score(haystack.slice(..), &mut self.matcher)
    }
}

/// An object that matched a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub id: ObjectId,
    pub score: u32,
    /// The members that matched, in listing order. Empty when the piece's
    /// own path matched, and for an empty query.
    pub members: Vec<RelPath>,
}

/// The result of [`search`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Search {
    pub matches: Vec<Match>,
    /// Objects whose members could not be looked at, with the reason.
    pub skipped: Vec<(ObjectId, String)>,
}

/// The objects that match `query`, best first.
///
/// The piece is matched on its path. A member is matched on its path below
/// the piece's own, so a query can name the piece and the member together.
/// Members are looked at only for pieces whose path does not match, since
/// every member would match through the path of a piece that does.
///
/// A piece match ranks above a member-only match; within each, a better
/// score first, then the newer object.
pub fn search(collection: &Collection, index: &Index, text: &str) -> Result<Search, IndexError> {
    let mut query = Query::parse(text);
    let mut result = Search::default();

    if query.is_empty() {
        let mut entries: Vec<&IndexEntry> = index.entries().iter().collect();
        entries.sort_by_key(|entry| (Reverse(entry.created), entry.id));
        result.matches = entries
            .into_iter()
            .map(|entry| Match {
                id: entry.id,
                score: 0,
                members: Vec::new(),
            })
            .collect();
        return Ok(result);
    }

    // (member-only, score, created) sorts the way the result is ordered.
    let mut ranked: Vec<(bool, u32, Timestamp, Match)> = Vec::new();
    for entry in index.entries() {
        let piece = entry.path.to_string();
        if let Some(score) = query.score(&piece) {
            let found = Match {
                id: entry.id,
                score,
                members: Vec::new(),
            };
            ranked.push((false, score, entry.created, found));
            continue;
        }

        let members = match index.members(collection, entry.id) {
            Ok(Some(members)) => members,
            Ok(None) => continue,
            Err(error) => {
                result.skipped.push((entry.id, error.to_string()));
                continue;
            }
        };
        let mut best = None;
        let mut hits = Vec::new();
        for member in members {
            if let Some(score) = query.score(&format!("{piece}/{member}")) {
                best = best.max(Some(score));
                hits.push(member);
            }
        }
        if let Some(score) = best {
            let found = Match {
                id: entry.id,
                score,
                members: hits,
            };
            ranked.push((true, score, entry.created, found));
        }
    }
    ranked.sort_by_key(|(member_only, score, created, found)| {
        (*member_only, Reverse(*score), Reverse(*created), found.id)
    });
    result.matches = ranked.into_iter().map(|(.., found)| found).collect();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::listing::{self, ListingEntry, ListingType};
    use crate::lock::{self, RunLock};
    use crate::testing::tree::TempCollection;
    use crate::tracking::{self, TrackingEntry, VersionTakenOut};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
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
        /// The ID time of the objects written next.
        created: &'static str,
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
                created: "2020-09-13T12:26:40Z",
            }
        }

        /// Writes `meta.1` of `object`.
        fn object(&mut self, object: &str, piece: &str, lifecycle: &str, payloads: &[PayloadSpec]) {
            let payloads: Vec<String> = payloads.iter().map(PayloadSpec::json).collect();
            let name = piece.rsplit('/').next().expect("a name");
            let document = format!(
                concat!(
                    r#"{{"format":1,"id":"{}","collection":"{}","path":"{}","name":"{}","#,
                    r#""created":"{}","created_source":"birth","#,
                    r#""lifecycle":"{}","payloads":[{}],"history":[]}}"#
                ),
                object,
                crate::testing::tree::COLLECTION_ID,
                piece,
                name,
                self.created,
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

    // ---------------------------------------------------------
    // The member cache
    // ---------------------------------------------------------

    impl Fixture {
        /// Stores the listing of archive `version` of `object`, with one
        /// file entry for every path.
        fn listing(&mut self, object: &str, version: u32, paths: &[&[u8]]) {
            let entries: Vec<ListingEntry> = paths
                .iter()
                .map(|bytes| ListingEntry {
                    path: RelPath::from_bytes(bytes.to_vec()).expect("a path"),
                    kind: ListingType::File,
                    size: 1,
                    mtime: 0,
                    mode: 0o644,
                    link: None,
                    sha256: Some("0".repeat(64)),
                })
                .collect();
            let name = format!("{version:016x}.listing.zst");
            let target = self.collection.object_dir(id(object)).join(name);
            fs::create_dir_all(target.parent().expect("in an object directory")).expect("dir");
            let file = fs::File::create(target).expect("creates");
            listing::write(&entries, file).expect("writes");
        }

        fn update(&self) -> Updated {
            update(&self.collection, &self.lock).expect("updates")
        }

        fn rebuild(&self) -> Updated {
            rebuild(&self.collection, &self.lock).expect("rebuilds")
        }

        fn cache_file(&self, object: &str) -> std::path::PathBuf {
            self.index_dir().join(format!("{object}.json"))
        }

        fn cache(&self, object: &str) -> serde_json::Value {
            let bytes = fs::read(self.cache_file(object)).expect("the cache file exists");
            serde_json::from_slice(&bytes).expect("the cache file is JSON")
        }

        fn members(&self, object: &str) -> Option<Vec<String>> {
            self.compute()
                .members(&self.collection, id(object))
                .expect("members")
                .map(|paths| paths.iter().map(RelPath::to_string).collect())
        }

        /// The content of every file of the index directory.
        fn content(&self) -> BTreeMap<String, Vec<u8>> {
            let mut files = BTreeMap::new();
            for entry in fs::read_dir(self.index_dir()).expect("lists") {
                let entry = entry.expect("entry");
                files.insert(
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).expect("reads"),
                );
            }
            files
        }

        fn content_names(&self) -> Vec<String> {
            self.content().into_keys().collect()
        }

        /// Puts `files` into the index directory, which is emptied first.
        fn restore(&self, files: &BTreeMap<String, Vec<u8>>) {
            let dir = self.index_dir();
            for entry in fs::read_dir(&dir).expect("lists") {
                fs::remove_file(entry.expect("entry").path()).expect("removes");
            }
            for (name, bytes) in files {
                fs::write(dir.join(name), bytes).expect("writes");
            }
        }
    }

    #[test]
    fn update_writes_a_member_file_per_listing_in_listing_order() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"src", b"src/main.rs", b"d\xff"]);
        let updated = fx.update();
        assert_eq!(updated.written, 1);
        assert_eq!(
            fx.cache(A),
            json!({
                "format": 1,
                "listing": "0000000000000001.listing.zst",
                "members": ["src", "src/main.rs", "d%FF"],
            })
        );
        assert_eq!(fx.content().len(), 1, "one file and nothing else");
    }

    #[test]
    fn members_come_from_the_cache_file_when_it_names_the_current_listing() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"from-the-listing"]);
        fx.update();
        fs::write(
            fx.cache_file(A),
            r#"{"format":1,"listing":"0000000000000001.listing.zst","members":["from-the-cache"]}"#,
        )
        .expect("writes");
        assert_eq!(fx.members(A), Some(vec!["from-the-cache".to_string()]));
    }

    #[test]
    fn members_fall_back_to_the_listing_when_there_is_no_cache_file() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"from-the-listing"]);
        assert_eq!(fx.members(A), Some(vec!["from-the-listing".to_string()]));
        assert!(fx.content().is_empty(), "reading must not write the cache");
    }

    #[test]
    fn members_fall_back_to_the_listing_when_the_cache_names_another_listing() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(2, 5)]);
        fx.listing(A, 2, &[b"from-the-listing"]);
        fs::write(
            fx.cache_file(A),
            r#"{"format":1,"listing":"0000000000000001.listing.zst","members":["old"]}"#,
        )
        .expect("writes");
        assert_eq!(fx.members(A), Some(vec!["from-the-listing".to_string()]));
    }

    #[test]
    fn members_fall_back_to_the_listing_when_the_cache_file_is_not_json() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"from-the-listing"]);
        fs::write(fx.cache_file(A), "not json").expect("writes");
        assert_eq!(fx.members(A), Some(vec!["from-the-listing".to_string()]));
    }

    #[test]
    fn members_fall_back_to_the_listing_when_the_cache_has_another_format() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"from-the-listing"]);
        fs::write(
            fx.cache_file(A),
            r#"{"format":2,"listing":"0000000000000001.listing.zst","members":["new"]}"#,
        )
        .expect("writes");
        assert_eq!(fx.members(A), Some(vec!["from-the-listing".to_string()]));
    }

    #[test]
    fn an_object_without_a_listing_has_no_members() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "live", &[PayloadSpec::live(1, 5)]);
        assert_eq!(fx.members(A), None);
        assert_eq!(fx.update(), Updated::default());
    }

    #[test]
    fn a_listing_that_is_not_stored_here_gives_no_members_and_no_file() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        assert_eq!(fx.members(A), None);
        assert_eq!(fx.update(), Updated::default());
        assert!(fx.content().is_empty());
    }

    #[test]
    fn a_listing_that_cannot_be_read_is_skipped_and_reported() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.temp.tree().file(
            &format!(".antiq/objects/{A}/0000000000000001.listing.zst"),
            "garbage",
        );
        let updated = fx.update();
        assert_eq!(updated.written, 0);
        assert_eq!(updated.skipped.len(), 1);
        assert_eq!(updated.skipped[0].0, id(A));
        let error = fx
            .compute()
            .members(&fx.collection, id(A))
            .expect_err("garbage is not a listing");
        assert!(matches!(error, IndexError::Listing { .. }), "{error:?}");
        assert!(error.to_string().contains(A), "{error}");
    }

    #[test]
    fn update_replaces_a_file_whose_listing_changed() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one"]);
        fx.update();
        fx.object(
            A,
            "a",
            "archived",
            &[PayloadSpec::archive(1, 5), PayloadSpec::archive(2, 6)],
        );
        fx.listing(A, 2, &[b"two"]);
        let updated = fx.update();
        assert_eq!(updated.written, 1);
        assert_eq!(
            fx.cache(A)["listing"],
            json!("0000000000000002.listing.zst")
        );
        assert_eq!(fx.cache(A)["members"], json!(["two"]));
    }

    #[test]
    fn update_leaves_a_current_file_untouched() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one"]);
        fx.update();
        let before = fs::metadata(fx.cache_file(A)).expect("exists");
        let updated = fx.update();
        assert_eq!(updated, Updated::default());
        let after = fs::metadata(fx.cache_file(A)).expect("exists");
        assert_eq!(before.ino(), after.ino(), "the file was written again");
    }

    #[test]
    fn update_removes_files_of_vanished_objects_and_stray_items() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one"]);
        fx.temp.tree().file(&format!(".antiq/index/{B}.json"), "{}");
        fx.temp.tree().file(".antiq/index/.leftover.1.2.tmp", "x");
        fx.temp.tree().file(".antiq/index/sub/inner", "x");
        let updated = fx.update();
        assert_eq!(updated.written, 1);
        assert_eq!(updated.removed, 3);
        assert_eq!(fx.content_names(), vec![format!("{A}.json")]);
    }

    #[test]
    fn update_removes_a_file_whose_listing_is_no_longer_stored() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one"]);
        fx.update();
        fs::remove_file(
            fx.collection
                .object_dir(id(A))
                .join("0000000000000001.listing.zst"),
        )
        .expect("removes");
        let updated = fx.update();
        assert_eq!(updated.removed, 1);
        assert!(!fx.cache_file(A).exists());
    }

    #[test]
    fn update_creates_the_index_directory() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one"]);
        fs::remove_dir(fx.index_dir()).expect("the fixture's index directory is empty");
        assert!(!fx.index_dir().exists(), "the precondition: no directory");
        fx.update();
        assert!(fx.cache_file(A).exists());
    }

    #[test]
    fn update_without_objects_and_without_a_directory_creates_an_empty_one() {
        let fx = Fixture::new();
        fs::remove_dir(fx.index_dir()).expect("the fixture's index directory is empty");
        assert_eq!(fx.update(), Updated::default());
        assert!(fx.index_dir().is_dir());
    }

    #[test]
    fn rebuild_repairs_a_cache_file_with_the_right_name_and_wrong_members() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"right"]);
        fx.update();
        fs::write(
            fx.cache_file(A),
            r#"{"format":1,"listing":"0000000000000001.listing.zst","members":["wrong"]}"#,
        )
        .expect("writes");
        assert_eq!(fx.update(), Updated::default(), "update trusts the name");
        let rebuilt = fx.rebuild();
        assert_eq!(rebuilt.written, 1);
        assert_eq!(fx.cache(A)["members"], json!(["right"]));
    }

    #[test]
    fn a_rebuilt_index_equals_the_one_updated_step_by_step() {
        let mut fx = Fixture::new();
        // After every step the updated directory is compared with a
        // rebuild, which is then undone so the next step still starts from
        // the incrementally maintained directory.
        let check = |fx: &Fixture, step: &str| {
            let updated = fx.content();
            fx.rebuild();
            let rebuilt = fx.content();
            assert_eq!(updated, rebuilt, "after: {step}");
            fx.restore(&updated);
        };

        fx.object(A, "alpha", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one", b"two"]);
        fx.update();
        check(&fx, "an archived object");

        fx.object(B, "beta", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(B, 1, &[b"three"]);
        fx.update();
        check(&fx, "a second archived object");

        fx.object(
            A,
            "alpha",
            "archived",
            &[PayloadSpec::archive(1, 5), PayloadSpec::archive(2, 6)],
        );
        fx.listing(A, 2, &[b"one", b"two", b"four"]);
        fx.update();
        check(&fx, "a newer archive of the first object");

        fs::remove_dir_all(fx.collection.object_dir(id(B))).expect("removes");
        fx.update();
        check(&fx, "the second object is gone");

        fx.object(C, "gamma", "live", &[PayloadSpec::live(1, 7)]);
        fx.update();
        check(&fx, "a live object without a listing");

        fs::remove_file(
            fx.collection
                .object_dir(id(A))
                .join("0000000000000002.listing.zst"),
        )
        .expect("removes");
        fx.update();
        check(&fx, "the listing of the first object is deleted");

        fx.object(B, "beta", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(B, 1, &[b"three"]);
        fx.update();
        check(&fx, "the second object is back");
        assert_eq!(fx.content_names(), vec![format!("{B}.json")]);
    }

    #[test]
    fn rebuild_clears_items_that_are_not_cache_files() {
        let mut fx = Fixture::new();
        fx.temp.tree().file(".antiq/index/stray", "x");
        let rebuilt = fx.rebuild();
        assert_eq!(rebuilt.removed, 1);
        assert!(fx.content().is_empty());
    }

    // ---------------------------------------------------------
    // Search
    // ---------------------------------------------------------

    impl Fixture {
        fn search(&self, query: &str) -> Search {
            search(&self.collection, &self.compute(), query).expect("searches")
        }

        /// An archived object with a stored listing of `members`.
        fn archived(&mut self, object: &str, piece: &str, members: &[&str]) {
            self.object(object, piece, "archived", &[PayloadSpec::archive(1, 5)]);
            let paths: Vec<&[u8]> = members.iter().map(|member| member.as_bytes()).collect();
            self.listing(object, 1, &paths);
        }
    }

    fn found(search: &Search) -> Vec<ObjectId> {
        search.matches.iter().map(|found| found.id).collect()
    }

    #[test]
    fn an_empty_query_lists_every_object_newest_first() {
        let mut fx = Fixture::new();
        fx.created = "2020-01-01T00:00:00Z";
        fx.object(A, "a", "live", &[]);
        fx.created = "2022-01-01T00:00:00Z";
        fx.object(B, "b", "live", &[]);
        fx.created = "2021-01-01T00:00:00Z";
        fx.object(C, "c", "live", &[]);
        fx.track("01BX5ZZKBKACTAV9WEVGEMMVRA", "lost", EntryState::Gone);
        fx.temp.tree().file(
            ".antiq/objects/01BX5ZZKBKACTAV9WEVGEMMVRB/meta.1",
            "garbage",
        );
        for query in ["", "   "] {
            let result = fx.search(query);
            assert_eq!(found(&result), vec![id(B), id(C), id(A)], "query {query:?}");
            assert!(result.matches.iter().all(|found| found.members.is_empty()));
            assert!(result.skipped.is_empty());
        }
    }

    #[test]
    fn equal_scores_are_ordered_newest_first() {
        let mut fx = Fixture::new();
        fx.created = "2020-01-01T00:00:00Z";
        fx.object(A, "p/x", "live", &[]);
        fx.created = "2022-01-01T00:00:00Z";
        fx.object(B, "q/x", "live", &[]);
        let result = fx.search("x");
        assert_eq!(result.matches[0].score, result.matches[1].score);
        assert_eq!(found(&result), vec![id(B), id(A)]);
    }

    #[test]
    fn a_query_matches_the_piece_path_fuzzily() {
        let mut fx = Fixture::new();
        fx.object(A, "projects/alpha", "live", &[]);
        fx.object(B, "elsewhere/zed", "live", &[]);
        let result = fx.search("prj");
        assert_eq!(found(&result), vec![id(A)]);
        assert!(result.matches[0].score > 0);
        assert!(result.matches[0].members.is_empty());
        assert_eq!(found(&fx.search("qqq")), vec![]);
    }

    #[test]
    fn a_query_matches_member_paths_when_the_piece_path_does_not() {
        let mut fx = Fixture::new();
        fx.archived(A, "projects/alpha", &["src", "src/main.rs", "README.md"]);
        fx.archived(B, "projects/beta", &["notes.txt"]);
        let result = fx.search("readme");
        assert_eq!(found(&result), vec![id(A)]);
        assert_eq!(result.matches[0].members, vec![path("README.md")]);
    }

    #[test]
    fn member_hits_come_in_listing_order() {
        let mut fx = Fixture::new();
        fx.archived(A, "p", &["b.rs", "a.rs", "c.txt", "d.rs"]);
        let result = fx.search("rs");
        assert_eq!(
            result.matches[0].members,
            vec![path("b.rs"), path("a.rs"), path("d.rs")]
        );
    }

    #[test]
    fn a_query_can_span_the_piece_and_a_member() {
        let mut fx = Fixture::new();
        fx.archived(A, "projects/alpha", &["src/main.rs"]);
        fx.archived(B, "projects/beta", &["src/main.rs"]);
        let result = fx.search("alpha main.rs");
        assert_eq!(found(&result), vec![id(A)]);
        assert_eq!(result.matches[0].members, vec![path("src/main.rs")]);
    }

    #[test]
    fn a_piece_match_lists_no_member_hits() {
        let mut fx = Fixture::new();
        fx.archived(A, "projects/alpha", &["src/main.rs", "README.md"]);
        let result = fx.search("alpha");
        assert_eq!(found(&result), vec![id(A)]);
        assert!(result.matches[0].members.is_empty());
    }

    #[test]
    fn piece_matches_rank_above_member_only_matches() {
        let mut fx = Fixture::new();
        // The member matches far better than the piece path does.
        fx.archived(A, "r/e/a/d/m/e", &[]);
        fx.archived(B, "other/b", &["README.md"]);
        let result = fx.search("readme");
        assert_eq!(found(&result), vec![id(A), id(B)]);
        assert!(result.matches[1].score > result.matches[0].score);
    }

    #[test]
    fn case_is_ignored_unless_the_query_has_a_capital() {
        let mut fx = Fixture::new();
        fx.object(A, "Projects/Alpha", "live", &[]);
        fx.object(B, "projects/alpha", "live", &[]);
        assert_eq!(found(&fx.search("alpha")), vec![id(A), id(B)]);
        assert_eq!(found(&fx.search("Alpha")), vec![id(A)]);
    }

    #[test]
    fn only_objects_with_metadata_are_matches() {
        let mut fx = Fixture::new();
        fx.object(A, "lost-and-found", "live", &[]);
        fx.track(B, "lost", EntryState::Gone);
        fx.temp
            .tree()
            .file(&format!(".antiq/objects/{C}/meta.1"), "garbage");
        assert_eq!(found(&fx.search("lost")), vec![id(A)]);
    }

    #[test]
    fn an_unreadable_listing_is_reported_and_the_other_objects_still_match() {
        let mut fx = Fixture::new();
        fx.object(A, "plain", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.temp.tree().file(
            &format!(".antiq/objects/{A}/0000000000000001.listing.zst"),
            "garbage",
        );
        fx.archived(B, "projects/needle", &[]);
        let result = fx.search("needle");
        assert_eq!(found(&result), vec![id(B)]);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].0, id(A));
    }

    #[test]
    fn search_works_without_an_index_directory_and_writes_nothing() {
        let mut fx = Fixture::new();
        fx.archived(A, "projects/alpha", &["README.md"]);
        fs::remove_dir(fx.index_dir()).expect("the fixture's index directory is empty");
        assert!(!fx.index_dir().exists(), "the precondition: no directory");
        let result = fx.search("readme");
        assert_eq!(found(&result), vec![id(A)]);
        assert!(!fx.index_dir().exists(), "search made the index directory");
    }

    #[test]
    fn a_query_scores_text_and_is_empty_without_atoms() {
        let mut query = Query::parse("abc");
        assert!(!query.is_empty());
        assert!(query.score("a-b-c").is_some());
        assert_eq!(query.score("xyz"), None);
        assert!(Query::parse("  ").is_empty());
        assert_eq!(Query::parse("").score("anything"), Some(0));
    }

    #[test]
    fn rebuild_creates_the_directory_when_there_is_none() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one"]);
        fs::remove_dir(fx.index_dir()).expect("the fixture's index directory is empty");
        let rebuilt = fx.rebuild();
        assert_eq!(rebuilt.written, 1);
        assert!(fx.cache_file(A).exists());
    }

    /// Takes away all access to `path` until the guard drops, and checks
    /// that this holds for the running user.
    fn deny(path: &Path) -> crate::fsx::RestoreModes {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("chmod");
        let guard = crate::fsx::RestoreModes(vec![path.to_path_buf()]);
        assert!(
            fs::File::open(path).is_err(),
            "the precondition: a 0000 path cannot be opened, so run as a non-root user"
        );
        guard
    }

    #[test]
    fn a_listing_that_cannot_be_opened_is_an_error_naming_it() {
        let mut fx = Fixture::new();
        fx.object(A, "a", "archived", &[PayloadSpec::archive(1, 5)]);
        fx.listing(A, 1, &[b"one"]);
        let listing = fx
            .collection
            .object_dir(id(A))
            .join("0000000000000001.listing.zst");
        let _restore = deny(&listing);
        let error = update(&fx.collection, &fx.lock).expect_err("not readable");
        assert!(
            matches!(error, IndexError::Io { action: "open", .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("listing.zst"), "{error}");
    }

    #[test]
    fn a_cache_directory_that_cannot_be_listed_is_an_error_naming_it() {
        let fx = Fixture::new();
        let _restore = deny(&fx.index_dir());
        let error = update(&fx.collection, &fx.lock).expect_err("not listable");
        assert!(
            matches!(error, IndexError::Io { action: "list", .. }),
            "{error:?}"
        );
        let error = rebuild(&fx.collection, &fx.lock).expect_err("not listable");
        assert!(
            matches!(error, IndexError::Io { action: "list", .. }),
            "{error:?}"
        );
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
