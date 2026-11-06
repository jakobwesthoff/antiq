// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `status` command: every piece of the collection with its state.
//!
//! `status` only reads. It takes no lock and writes nothing below
//! `.antiq/`, so it can run while another run holds the collection.

#![allow(unused)]
use crate::collection::Collection;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::meta::{Lifecycle, Metadata};
use crate::relpath::{Name, RelPath, escape};
use crate::remote::{self, RemoteState};
use crate::report::{CommandReport, Warning};
use crate::select::{Rules, SelectError, select};
use crate::store::{Current, Store, StoreError};
use crate::tracking::{self, EntryState, PieceState, TrackingEntry, TrackingError, derive_state};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Errors of the `status` command.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
}

impl crate::error::HasKind for StatusError {}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> StatusError {
    let path = path.display().to_string();
    move |source| StatusError::Io {
        action,
        path,
        source,
    }
}

// =========================================================
// The report
// =========================================================

/// What a piece is, as `status` shows it: the states of spec "States", or
/// an error of that one piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PieceKind {
    New,
    Live,
    Unearthed,
    Gone,
    Archived,
    RemoteOnly,
    LiveOnAnotherMachine,
    Error,
}

impl From<PieceState> for PieceKind {
    fn from(state: PieceState) -> PieceKind {
        match state {
            PieceState::New => PieceKind::New,
            PieceState::Live => PieceKind::Live,
            PieceState::Unearthed => PieceKind::Unearthed,
            PieceState::Gone => PieceKind::Gone,
            PieceState::Archived => PieceKind::Archived,
            PieceState::RemoteOnly => PieceKind::RemoteOnly,
            PieceState::LiveOnAnotherMachine => PieceKind::LiveOnAnotherMachine,
        }
    }
}

/// The conditions `status` shows on a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    Pinned,
    Empty,
    StaleBackup,
    Diverged,
    NeedsDecision,
    Superseded,
    PendingUpload,
    MissingOnRemote,
    Unreadable,
    MountPoint,
    Excluded,
    FutureMtime,
    Idle,
    Error,
}

/// What a flag or a state needs to say beyond its name. A key that does
/// not apply is left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Detail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A `removing` entry whose folder is still in place.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub removal_unfinished: bool,
    /// The unreadable directory above a piece that is neither walked nor
    /// judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub below_unreadable: Option<RelPath>,
    /// The machine that backs up a live piece of another collection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

impl Detail {
    fn is_empty(&self) -> bool {
        *self == Detail::default()
    }
}

/// One piece of the collection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Piece {
    /// `null` for a piece that has no ID yet.
    pub id: Option<ObjectId>,
    pub path: RelPath,
    pub name: Name,
    pub state: PieceKind,
    pub flags: Vec<Flag>,
    #[serde(skip_serializing_if = "Detail::is_empty")]
    pub detail: Detail,
}

/// A tracking file or an object that cannot be used.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Damaged {
    pub id: ObjectId,
    /// Relative to the collection root.
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CollectionInfo {
    pub id: ObjectId,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemoteInfo {
    pub state: RemoteState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// What selection left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub symlinks: usize,
    pub dot_entries: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub live: usize,
    pub archived: usize,
    pub remote_only: usize,
    pub pinned: usize,
    pub empty: usize,
}

/// What `status` found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusReport {
    pub collection: CollectionInfo,
    pub remote: RemoteInfo,
    pub pieces: Vec<Piece>,
    /// Names below `.antiq/recovered/`.
    pub recovered: Vec<String>,
    pub damaged: Vec<Damaged>,
    /// Directories selection could not read, in the escaped form.
    pub unreadable_directories: Vec<RelPath>,
    pub skipped: Skipped,
    pub counts: Counts,
    pub warnings: Vec<Warning>,
}

impl CommandReport for StatusReport {
    /// `status` exits 0 whatever it shows.
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

// =========================================================
// Entry point
// =========================================================

/// What `status` asks the world, so that tests can answer for the parts an
/// unprivileged test cannot create.
pub(crate) trait Probes {
    fn remote_state(&self, collection: &Collection) -> RemoteState;
}

struct RealProbes;

impl Probes for RealProbes {
    fn remote_state(&self, collection: &Collection) -> RemoteState {
        remote::probe_state(collection)
    }
}

pub(crate) struct StatusRequest<'a> {
    pub collection: &'a Collection,
    pub probes: &'a dyn Probes,
}

/// Shows every piece of `collection`.
pub fn status(collection: &Collection) -> Result<StatusReport, StatusError> {
    status_with(&StatusRequest {
        collection,
        probes: &RealProbes,
    })
}

pub(crate) fn status_with(request: &StatusRequest<'_>) -> Result<StatusReport, StatusError> {
    let collection = request.collection;
    let own = collection.id();
    let config = &collection.config().collection;
    let mut warnings = Vec::new();

    // =====================================================
    // What the folders say
    // =====================================================

    let root = TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    let selection = select(&root, &rules)?;
    let selected: HashSet<RelPath> = selection.pieces().iter().cloned().collect();
    let mut unreadable_directories = selection.unreadable().to_vec();
    unreadable_directories.sort();

    // =====================================================
    // What the store and the tracking entries say
    // =====================================================

    let store = Store::new(collection);
    let mut metadata: BTreeMap<ObjectId, Current> = BTreeMap::new();
    let mut damaged = Vec::new();
    let mut damaged_objects: BTreeMap<ObjectId, String> = BTreeMap::new();
    for object in store.objects()? {
        match store.current(object) {
            Ok(Some(current)) => {
                metadata.insert(object, current);
            }
            Ok(None) => {}
            // One object that cannot be read does not stop the others from
            // showing; the object is listed as damaged.
            Err(error) => {
                damaged_objects.insert(object, error.to_string());
                damaged.push(Damaged {
                    id: object,
                    path: relative_to_collection(collection, &collection.object_dir(object)),
                    error: error.to_string(),
                });
            }
        }
    }
    let tracked = tracking::read_all(collection)?;

    // =====================================================
    // Joining them into pieces
    // =====================================================

    // A path an object or an entry speaks for. A folder there is that
    // piece's, never a new one.
    let mut claimed: HashSet<RelPath> = HashSet::new();
    let mut pieces = Vec::new();
    let find_exact = |at: &RelPath| exists_exact(&root, at);

    for file in &tracked.damaged {
        let file_path = relative_to_collection(collection, &file.path);
        damaged.push(Damaged {
            id: file.id,
            path: file_path.clone(),
            error: file.error.to_string(),
        });
        // The entry's own path is unknown, so the metadata's path stands
        // in for it, and no new ID is minted there.
        let known = metadata.get(&file.id).map(|current| &current.metadata.path);
        if let Some(at) = known {
            claimed.insert(at.clone());
        }
        let at = known
            .cloned()
            .unwrap_or_else(|| RelPath::parse(&file_path).expect("a tracking file path is valid"));
        pieces.push(error_piece(Some(file.id), at, file.error.to_string()));
    }

    for entry in &tracked.entries {
        if let Some(reason) = damaged_objects.get(&entry.id) {
            claimed.insert(entry.path.clone());
            pieces.push(error_piece(
                Some(entry.id),
                entry.path.clone(),
                reason.clone(),
            ));
            continue;
        }
        let current = metadata.get(&entry.id).map(|current| &current.metadata);
        let presence = presence(
            &entry.path,
            &selected,
            &unreadable_directories,
            &rules,
            &find_exact,
        );
        let present = presence != Presence::Missing;
        match derive_state(Some(entry), current, present, own) {
            Ok(state) => {
                if claims(entry, current, own) {
                    claimed.insert(entry.path.clone());
                }
                let mut piece = Piece {
                    id: Some(entry.id),
                    path: entry.path.clone(),
                    name: current.map_or_else(|| last_name(&entry.path), |m| m.name.clone()),
                    state: state.into(),
                    flags: Vec::new(),
                    detail: Detail::default(),
                };
                piece.detail.removal_unfinished =
                    entry.state == EntryState::Removing && presence == Presence::Present;
                if let Presence::Below(directory) = presence {
                    piece.detail.below_unreadable = Some(directory);
                }
                if state == PieceState::LiveOnAnotherMachine {
                    piece.detail.machine = current.and_then(machine_of);
                }
                pieces.push(piece);
            }
            Err(error) => {
                claimed.insert(entry.path.clone());
                pieces.push(error_piece(
                    Some(entry.id),
                    entry.path.clone(),
                    error.to_string(),
                ));
            }
        }
    }

    let tracked_ids: HashSet<ObjectId> = tracked
        .entries
        .iter()
        .map(|entry| entry.id)
        .chain(tracked.damaged.iter().map(|file| file.id))
        .collect();
    for (object, current) in &metadata {
        if tracked_ids.contains(object) {
            continue;
        }
        let known = &current.metadata;
        match derive_state(None, Some(known), false, own) {
            Ok(state) => pieces.push(Piece {
                id: Some(*object),
                path: known.path.clone(),
                name: known.name.clone(),
                state: state.into(),
                flags: Vec::new(),
                detail: Detail {
                    machine: (state == PieceState::LiveOnAnotherMachine)
                        .then(|| machine_of(known))
                        .flatten(),
                    ..Detail::default()
                },
            }),
            Err(error) => {
                claimed.insert(known.path.clone());
                pieces.push(error_piece(
                    Some(*object),
                    known.path.clone(),
                    error.to_string(),
                ));
            }
        }
    }

    for new in selection.pieces() {
        if !claimed.contains(new) {
            pieces.push(Piece {
                id: None,
                path: new.clone(),
                name: last_name(new),
                state: PieceKind::New,
                flags: Vec::new(),
                detail: Detail::default(),
            });
        }
    }
    pieces.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
    damaged.sort_by(|a, b| a.id.cmp(&b.id).then(a.path.cmp(&b.path)));

    // =====================================================
    // Around the pieces
    // =====================================================

    let recovered = recovered_names(collection, &mut warnings);
    let counts = count(&pieces);
    Ok(StatusReport {
        collection: CollectionInfo {
            id: own,
            path: collection.root().display().to_string(),
        },
        remote: RemoteInfo {
            state: request.probes.remote_state(collection),
            url: collection
                .config()
                .remotes
                .first()
                .map(|remote| remote.url.clone()),
        },
        pieces,
        recovered,
        damaged,
        unreadable_directories,
        skipped: Skipped {
            symlinks: selection.skipped_symlinks(),
            dot_entries: selection.skipped_dot_entries(),
        },
        counts,
        warnings,
    })
}

/// A path below the collection root, in the escaped form.
fn relative_to_collection(collection: &Collection, path: &Path) -> String {
    let relative = path.strip_prefix(collection.root()).unwrap_or(path);
    escape(relative.as_os_str().as_bytes())
}

fn last_name(path: &RelPath) -> Name {
    let last = path
        .components()
        .last()
        .expect("a piece path has a name")
        .to_vec();
    Name::from_bytes(last).expect("a component of a path is a valid name")
}

fn error_piece(id: Option<ObjectId>, path: RelPath, message: String) -> Piece {
    Piece {
        id,
        name: last_name(&path),
        path,
        state: PieceKind::Error,
        flags: vec![Flag::Error],
        detail: Detail {
            error: Some(message),
            ..Detail::default()
        },
    }
}

/// Whether the folder at the entry's path is this entry's piece, so that
/// no new piece is minted there. A `gone` entry gets its folder back only
/// when the metadata is live and owned by this collection (spec "States").
fn claims(entry: &TrackingEntry, metadata: Option<&Metadata>, own: ObjectId) -> bool {
    match entry.state {
        EntryState::Unearthing | EntryState::Live | EntryState::Removing => true,
        EntryState::Gone => {
            metadata.is_some_and(|m| m.lifecycle == Lifecycle::Live && m.collection == own)
        }
    }
}

/// The machine that backs up a live piece: the writer of its newest live
/// payload, else the machine of its newest history entry.
fn machine_of(metadata: &Metadata) -> Option<String> {
    let live = metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == crate::meta::PayloadKind::Live)
        .max_by_key(|payload| payload.version)
        .map(|payload| payload.machine.clone());
    live.or_else(|| metadata.history.last().map(|entry| entry.machine.clone()))
}

fn count(pieces: &[Piece]) -> Counts {
    let mut counts = Counts::default();
    for piece in pieces {
        match piece.state {
            PieceKind::New | PieceKind::Live | PieceKind::Unearthed => counts.live += 1,
            PieceKind::Archived => counts.archived += 1,
            PieceKind::RemoteOnly => {
                counts.archived += 1;
                counts.remote_only += 1;
            }
            PieceKind::Gone | PieceKind::LiveOnAnotherMachine | PieceKind::Error => {}
        }
        counts.pinned += usize::from(piece.flags.contains(&Flag::Pinned));
        counts.empty += usize::from(piece.flags.contains(&Flag::Empty));
    }
    counts
}

/// The names below `.antiq/recovered/`. A directory that is missing has
/// none; one that cannot be listed gives a warning.
fn recovered_names(collection: &Collection, warnings: &mut Vec<Warning>) -> Vec<String> {
    let directory = collection.recovered_dir();
    let listing = match std::fs::read_dir(&directory) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(Warning::new(format!(
                "cannot list {}: {error}",
                relative_to_collection(collection, &directory)
            )));
            return Vec::new();
        }
    };
    let mut names: Vec<String> = listing
        .filter_map(Result::ok)
        .map(|item| escape(item.file_name().as_bytes()))
        .collect();
    names.sort();
    names
}

// =========================================================
// Whether a folder is there
// =========================================================

/// Whether the folder of an entry is there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Presence {
    Present,
    Missing,
    /// Below a directory selection could not read: unknown, taken as there.
    Below(RelPath),
}

/// Decides from what selection found, so that a folder renamed only in
/// case is not seen twice on a volume that ignores case: `lstat` of the
/// old spelling would still find it. Only a path the patterns no longer
/// select is looked up on disk, with the exact name checked in the
/// listing of its parent.
fn presence(
    path: &RelPath,
    selected: &HashSet<RelPath>,
    unreadable: &[RelPath],
    rules: &Rules,
    exists_exact: &dyn Fn(&RelPath) -> bool,
) -> Presence {
    if selected.contains(path) {
        return Presence::Present;
    }
    if let Some(directory) = unreadable
        .iter()
        .find(|directory| is_at_or_below(path, directory))
    {
        return Presence::Below(directory.clone());
    }
    if rules.matches(path) {
        return Presence::Missing;
    }
    if exists_exact(path) {
        Presence::Present
    } else {
        Presence::Missing
    }
}

fn is_at_or_below(path: &RelPath, directory: &RelPath) -> bool {
    let (path, directory) = (path.as_bytes(), directory.as_bytes());
    path == directory || (path.starts_with(directory) && path[directory.len()] == b'/')
}

/// Whether a directory is at `path` under exactly that spelling.
fn exists_exact(root: &TreeRoot, path: &RelPath) -> bool {
    let is_directory = root
        .lstat(path)
        .is_ok_and(|entry| entry.kind == crate::tree::EntryKind::Directory);
    if !is_directory {
        return false;
    }
    let bytes = path.as_bytes();
    let (parent, name) = match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (&bytes[..slash], &bytes[slash + 1..]),
        None => (&bytes[..0], bytes),
    };
    let parent = RelPath::from_bytes(parent.to_vec()).expect("the parent of a valid path is valid");
    root.read_dir(&parent)
        .is_ok_and(|entries| entries.iter().any(|entry| entry.name.as_bytes() == name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{Checksum, CreatedSource, FileName, Payload, PayloadKind};
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use crate::tracking::{LocalPayload, VersionTakenOut};
    use jiff::Timestamp;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const OTHER: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    const WHEN: &str = "2026-10-01T10:00:00Z";

    fn id(number: u8) -> ObjectId {
        ObjectId::from_ulid(ulid::Ulid::from_parts(
            1_600_000_000_000 + u64::from(number),
            u128::from(number),
        ))
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    fn when() -> Timestamp {
        WHEN.parse().expect("a time")
    }

    struct Probed(RemoteState);

    impl Probes for Probed {
        fn remote_state(&self, _: &Collection) -> RemoteState {
            self.0
        }
    }

    /// A collection with folders, objects and entries written the way the
    /// store and the tracking module keep them.
    struct World {
        temp: TempCollection,
        collection: Collection,
    }

    fn world() -> World {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        World { temp, collection }
    }

    fn payload(kind: PayloadKind, version: u32, file: &str) -> Payload {
        Payload {
            kind,
            version,
            file: FileName::new(file).expect("a plain file name"),
            package_format: "tar+zstd/1".into(),
            size: 1,
            checksum: Checksum::sha256("00"),
            listing: None,
            machine: "laptop".into(),
            created: when(),
            verified: None,
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        }
    }

    fn metadata(
        number: u8,
        at: &str,
        lifecycle: Lifecycle,
        owner: &str,
        payloads: Vec<Payload>,
    ) -> Metadata {
        let path = path(at);
        let name = Name::from_bytes(path.components().last().expect("a name").to_vec())
            .expect("a valid name");
        Metadata {
            format: 1,
            id: id(number),
            collection: owner.parse().expect("an ID"),
            path,
            name,
            created: when(),
            created_source: CreatedSource::Birth,
            lifecycle,
            payloads,
            history: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    fn archive_payload() -> Payload {
        payload(PayloadKind::Archive, 1, "0123456789abcdef-a.tar.zst")
    }

    impl World {
        fn dir(&mut self, relative: &str) {
            self.temp.tree().dir(relative);
        }

        fn put_meta(&self, metadata: &Metadata) {
            let directory = self.collection.object_dir(metadata.id);
            fs::create_dir_all(&directory).expect("the object directory can be made");
            fs::write(
                directory.join("meta.1"),
                crate::meta::to_bytes(metadata).expect("metadata serialises"),
            )
            .expect("metadata can be written");
        }

        fn put_entry(&self, number: u8, at: &str, state: EntryState, local_archive: bool) {
            let mut entry = TrackingEntry::new(
                id(number),
                path(at),
                state,
                VersionTakenOut::Committed(1),
                when(),
            );
            if local_archive {
                entry.local_payloads = vec![LocalPayload {
                    kind: PayloadKind::Archive,
                    version: 1,
                    file: FileName::new("0123456789abcdef-a.tar.zst").expect("a plain name"),
                }];
            }
            self.write_entry(&entry);
        }

        fn write_entry(&self, entry: &TrackingEntry) {
            fs::write(
                self.collection.tracking_path(entry.id),
                serde_json::to_vec(entry).expect("an entry serialises"),
            )
            .expect("an entry can be written");
        }

        fn report(&self) -> StatusReport {
            status_with(&StatusRequest {
                collection: &self.collection,
                probes: &Probed(RemoteState::None),
            })
            .expect("status runs")
        }
    }

    /// The pieces as `(path, state)`, in report order.
    fn states(report: &StatusReport) -> Vec<(String, PieceKind)> {
        report
            .pieces
            .iter()
            .map(|piece| (piece.path.to_string(), piece.state))
            .collect()
    }

    // =========================================================
    // States
    // =========================================================

    #[test]
    fn a_folder_no_entry_claims_is_a_new_piece_without_an_id() {
        let mut world = world();
        world.dir("scratch");
        let report = world.report();
        assert_eq!(states(&report), [("scratch".into(), PieceKind::New)]);
        assert_eq!(report.pieces[0].id, None);
        assert_eq!(report.pieces[0].name.to_string(), "scratch");
    }

    #[test]
    fn status_asks_the_real_remote_and_reports_the_same_pieces() {
        let mut world = world();
        world.dir("scratch");
        let report = status(&world.collection).expect("status runs");
        assert_eq!(report, world.report());
    }

    #[test]
    fn the_report_names_the_collection_and_the_remote_state() {
        let world = world();
        let report = world.report();
        assert_eq!(report.collection.id.to_string(), COLLECTION_ID);
        assert_eq!(
            report.collection.path,
            world.collection.root().display().to_string()
        );
        assert_eq!(report.remote.state, RemoteState::None);
    }

    #[test]
    fn an_archived_piece_with_a_local_archive_is_archived() {
        let world = world();
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(1, "old", EntryState::Gone, true);
        let report = world.report();
        assert_eq!(states(&report), [("old".into(), PieceKind::Archived)]);
        assert_eq!(report.pieces[0].id, Some(id(1)));
    }

    #[test]
    fn an_archived_piece_without_a_local_archive_is_remote_only() {
        let world = world();
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(1, "old", EntryState::Gone, false);
        assert_eq!(
            states(&world.report()),
            [("old".into(), PieceKind::RemoteOnly)]
        );
    }

    #[test]
    fn metadata_without_an_entry_is_remote_only() {
        let world = world();
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            OTHER,
            vec![archive_payload()],
        ));
        assert_eq!(
            states(&world.report()),
            [("old".into(), PieceKind::RemoteOnly)]
        );
    }

    #[test]
    fn a_live_entry_whose_folder_is_missing_is_gone() {
        let world = world();
        world.put_meta(&metadata(
            1,
            "work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        world.put_entry(1, "work", EntryState::Live, false);
        assert_eq!(states(&world.report()), [("work".into(), PieceKind::Gone)]);
    }

    #[test]
    fn a_live_piece_with_its_folder_is_live() {
        let mut world = world();
        world.dir("work");
        world.put_meta(&metadata(
            1,
            "work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        world.put_entry(1, "work", EntryState::Live, false);
        assert_eq!(states(&world.report()), [("work".into(), PieceKind::Live)]);
    }

    #[test]
    fn a_live_piece_with_an_archive_payload_and_its_folder_is_unearthed() {
        let mut world = world();
        world.dir("work");
        world.put_meta(&metadata(
            1,
            "work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(1, "work", EntryState::Live, true);
        assert_eq!(
            states(&world.report()),
            [("work".into(), PieceKind::Unearthed)]
        );
    }

    #[test]
    fn a_live_piece_of_another_collection_is_live_on_another_machine() {
        let world = world();
        let mut other = metadata(
            1,
            "theirs",
            Lifecycle::Live,
            OTHER,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-theirs.tar.zst",
            )],
        );
        other.history.push(crate::meta::HistoryEntry::new(
            when(),
            "desktop",
            crate::meta::HistoryAction::Sync,
        ));
        world.put_meta(&other);
        let report = world.report();
        assert_eq!(
            states(&report),
            [("theirs".into(), PieceKind::LiveOnAnotherMachine)]
        );
        // The payload's writer wins over the newest history entry.
        assert_eq!(report.pieces[0].detail.machine.as_deref(), Some("laptop"));
    }

    #[test]
    fn the_machine_of_a_live_piece_falls_back_to_its_history() {
        let world = world();
        let mut other = metadata(1, "theirs", Lifecycle::Live, OTHER, Vec::new());
        other.history.push(crate::meta::HistoryEntry::new(
            when(),
            "desktop",
            crate::meta::HistoryAction::Sync,
        ));
        world.put_meta(&other);
        assert_eq!(
            world.report().pieces[0].detail.machine.as_deref(),
            Some("desktop")
        );
    }

    #[test]
    fn a_new_folder_shows_next_to_an_archived_piece_of_the_same_path() {
        let mut world = world();
        world.dir("old");
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(1, "old", EntryState::Gone, true);
        assert_eq!(
            states(&world.report()),
            [
                ("old".into(), PieceKind::New),
                ("old".into(), PieceKind::Archived)
            ]
        );
    }

    #[test]
    fn a_folder_at_the_path_of_a_piece_another_collection_took_over_is_new() {
        let mut world = world();
        world.dir("moved");
        world.put_meta(&metadata(
            1,
            "moved",
            Lifecycle::Live,
            OTHER,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-moved.tar.zst",
            )],
        ));
        world.put_entry(1, "moved", EntryState::Gone, false);
        assert_eq!(
            states(&world.report()),
            [
                ("moved".into(), PieceKind::New),
                ("moved".into(), PieceKind::LiveOnAnotherMachine)
            ]
        );
    }

    #[test]
    fn the_folder_of_a_removing_entry_is_no_new_piece() {
        let mut world = world();
        world.dir("old");
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(1, "old", EntryState::Removing, true);
        let report = world.report();
        assert_eq!(states(&report), [("old".into(), PieceKind::Archived)]);
        assert!(report.pieces[0].detail.removal_unfinished);
    }

    #[test]
    fn a_removing_entry_without_its_folder_is_finished() {
        let world = world();
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(1, "old", EntryState::Removing, true);
        assert!(!world.report().pieces[0].detail.removal_unfinished);
    }

    // =========================================================
    // Pieces with something wrong
    // =========================================================

    #[test]
    fn an_entry_without_metadata_is_an_error_of_that_piece() {
        let mut world = world();
        world.dir("lost");
        world.put_entry(1, "lost", EntryState::Live, false);
        let report = world.report();
        assert_eq!(states(&report), [("lost".into(), PieceKind::Error)]);
        assert!(report.pieces[0].flags.contains(&Flag::Error));
        assert!(report.pieces[0].detail.error.is_some());
    }

    #[test]
    fn own_live_metadata_without_an_entry_is_an_error_and_claims_its_folder() {
        let mut world = world();
        world.dir("work");
        world.put_meta(&metadata(
            1,
            "work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        assert_eq!(states(&world.report()), [("work".into(), PieceKind::Error)]);
    }

    #[test]
    fn a_damaged_entry_file_is_one_error_piece_and_listed() {
        let mut world = world();
        world.dir("old");
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        fs::write(world.collection.tracking_path(id(1)), "not json").expect("a file");
        let report = world.report();
        assert_eq!(states(&report), [("old".into(), PieceKind::Error)]);
        assert_eq!(report.damaged.len(), 1);
        assert_eq!(report.damaged[0].id, id(1));
        assert_eq!(
            report.damaged[0].path,
            format!(".antiq/tracking/{}.json", id(1))
        );
    }

    #[test]
    fn a_damaged_entry_file_without_readable_metadata_is_named_after_its_file() {
        let world = world();
        fs::write(world.collection.tracking_path(id(1)), "not json").expect("a file");
        let report = world.report();
        assert_eq!(
            states(&report),
            [(format!(".antiq/tracking/{}.json", id(1)), PieceKind::Error)]
        );
    }

    #[test]
    fn an_object_that_cannot_be_read_is_damaged_and_the_others_still_show() {
        let mut world = world();
        world.dir("fine");
        world.dir("old");
        world.put_meta(&metadata(
            2,
            "fine",
            Lifecycle::Archived,
            OTHER,
            vec![archive_payload()],
        ));
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(1, "old", EntryState::Gone, true);
        let unreadable = world.collection.object_dir(id(1)).join("meta.1");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("a mode");
        assert!(
            fs::read(&unreadable).is_err(),
            "the test needs a file that cannot be read; it must not run as root"
        );
        let report = world.report();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).expect("a mode");

        assert_eq!(report.damaged.len(), 1);
        assert_eq!(report.damaged[0].id, id(1));
        assert_eq!(report.damaged[0].path, format!(".antiq/objects/{}", id(1)));
        // The entry names the damaged object, so its folder is no new piece.
        assert_eq!(
            states(&report),
            [
                ("fine".into(), PieceKind::New),
                ("fine".into(), PieceKind::RemoteOnly),
                ("old".into(), PieceKind::Error)
            ]
        );
    }

    #[test]
    fn an_object_without_any_readable_version_is_damaged() {
        let world = world();
        let directory = world.collection.object_dir(id(1));
        fs::create_dir_all(&directory).expect("a directory");
        fs::write(directory.join("meta.1"), "not json").expect("a file");
        let report = world.report();
        assert_eq!(report.damaged.len(), 1);
        assert!(report.pieces.is_empty());
    }

    #[test]
    fn an_object_directory_without_files_is_nothing() {
        let world = world();
        fs::create_dir_all(world.collection.object_dir(id(1))).expect("a directory");
        let report = world.report();
        assert!(report.damaged.is_empty());
        assert!(report.pieces.is_empty());
    }

    // =========================================================
    // Selection
    // =========================================================

    #[test]
    fn an_entry_below_an_unreadable_directory_keeps_its_state_and_is_not_new() {
        let mut world = world();
        world.dir("group/work");
        world.put_meta(&metadata(
            1,
            "group/work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        world.put_entry(1, "group/work", EntryState::Live, false);
        let blocked = world.temp.root().join("group");
        fs::write(
            world.temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\npatterns = [\"group/*\"]\n"),
        )
        .expect("a config");
        let collection = Collection::find(world.temp.root()).expect("a collection");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).expect("a mode");
        assert!(
            fs::read_dir(&blocked).is_err(),
            "the test needs a directory that cannot be read; it must not run as root"
        );
        let report = status_with(&StatusRequest {
            collection: &collection,
            probes: &Probed(RemoteState::None),
        })
        .expect("status runs");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).expect("a mode");

        assert_eq!(states(&report), [("group/work".into(), PieceKind::Live)]);
        assert_eq!(
            report.pieces[0].detail.below_unreadable,
            Some(path("group"))
        );
        assert_eq!(report.unreadable_directories, [path("group")]);
    }

    #[test]
    fn selection_skips_are_counted() {
        let mut world = world();
        world.dir(".hidden");
        world.temp.tree().symlink("link", "elsewhere");
        let report = world.report();
        assert_eq!(report.skipped.symlinks, 1);
        assert_eq!(report.skipped.dot_entries, 1);
        assert!(report.pieces.is_empty());
    }

    #[test]
    fn the_rule_for_a_folder_is_the_selection_not_the_spelling() {
        // On a volume that ignores case, `lstat("Foo")` finds the folder
        // that selection lists as `foo`.
        let selected: HashSet<RelPath> = [path("foo")].into_iter().collect();
        let rules = Rules::new(&["*".to_string()], &[]).expect("a rule");
        let exact = |_: &RelPath| true;
        assert_eq!(
            presence(&path("Foo"), &selected, &[], &rules, &exact),
            Presence::Missing
        );
        assert_eq!(
            presence(&path("foo"), &selected, &[], &rules, &exact),
            Presence::Present
        );
    }

    #[test]
    fn a_path_the_patterns_no_longer_select_is_checked_on_disk() {
        let selected: HashSet<RelPath> = HashSet::new();
        let rules = Rules::new(&["other/*".to_string()], &[]).expect("a rule");
        assert_eq!(
            presence(&path("gone"), &selected, &[], &rules, &|_| true),
            Presence::Present
        );
        assert_eq!(
            presence(&path("gone"), &selected, &[], &rules, &|_| false),
            Presence::Missing
        );
    }

    #[test]
    fn a_path_the_patterns_do_not_select_is_found_on_disk() {
        let mut world = world();
        world.dir("outside/work");
        world.put_meta(&metadata(
            1,
            "outside/work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        world.put_entry(1, "outside/work", EntryState::Live, false);
        fs::write(
            world.temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\npatterns = [\"nothing/*\"]\n"),
        )
        .expect("a config");
        let collection = Collection::find(world.temp.root()).expect("a collection");
        let report = status_with(&StatusRequest {
            collection: &collection,
            probes: &Probed(RemoteState::None),
        })
        .expect("status runs");
        assert_eq!(states(&report), [("outside/work".into(), PieceKind::Live)]);
    }

    #[test]
    fn a_top_level_folder_the_patterns_do_not_select_is_found_on_disk() {
        let mut world = world();
        world.dir("work");
        world.put_meta(&metadata(
            1,
            "work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        world.put_entry(1, "work", EntryState::Live, false);
        fs::write(
            world.temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\npatterns = [\"nothing/*\"]\n"),
        )
        .expect("a config");
        let collection = Collection::find(world.temp.root()).expect("a collection");
        let report = status_with(&StatusRequest {
            collection: &collection,
            probes: &Probed(RemoteState::None),
        })
        .expect("status runs");
        assert_eq!(states(&report), [("work".into(), PieceKind::Live)]);
    }

    #[test]
    fn a_file_at_the_path_of_an_entry_is_no_folder() {
        let mut world = world();
        world.temp.tree().file("work", "not a folder");
        world.put_meta(&metadata(
            1,
            "work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        world.put_entry(1, "work", EntryState::Live, false);
        fs::write(
            world.temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\npatterns = [\"nothing/*\"]\n"),
        )
        .expect("a config");
        let collection = Collection::find(world.temp.root()).expect("a collection");
        let report = status_with(&StatusRequest {
            collection: &collection,
            probes: &Probed(RemoteState::None),
        })
        .expect("status runs");
        assert_eq!(states(&report), [("work".into(), PieceKind::Gone)]);
    }

    // =========================================================
    // The report as data
    // =========================================================

    #[test]
    fn a_collection_root_that_cannot_be_opened_fails_the_run() {
        let error = status(&Collection::for_tests()).expect_err("the root does not exist");
        assert!(
            matches!(error, StatusError::Io { action: "open", .. }),
            "{error:?}"
        );
    }

    #[test]
    fn every_state_of_the_derivation_has_a_kind() {
        let kinds = [
            (PieceState::New, PieceKind::New),
            (PieceState::Live, PieceKind::Live),
            (PieceState::Unearthed, PieceKind::Unearthed),
            (PieceState::Gone, PieceKind::Gone),
            (PieceState::Archived, PieceKind::Archived),
            (PieceState::RemoteOnly, PieceKind::RemoteOnly),
            (
                PieceState::LiveOnAnotherMachine,
                PieceKind::LiveOnAnotherMachine,
            ),
        ];
        for (state, kind) in kinds {
            assert_eq!(PieceKind::from(state), kind);
        }
    }

    #[test]
    fn status_never_makes_a_run_partial() {
        let report = world().report();
        assert!(!report.partial());
        assert!(report.warnings().is_empty());
    }

    #[test]
    fn a_piece_serialises_with_a_null_id_and_only_the_detail_that_applies() {
        let mut world = world();
        world.dir("scratch");
        world.put_entry(1, "lost", EntryState::Live, false);
        let json = serde_json::to_value(world.report()).expect("a report serialises");
        let pieces = json["pieces"].as_array().expect("pieces");
        assert_eq!(
            pieces[0],
            serde_json::json!({
                "id": id(1).to_string(),
                "path": "lost",
                "name": "lost",
                "state": "error",
                "flags": ["error"],
                "detail": {"error": format!(
                    "the tracking entry of {} has no metadata",
                    id(1)
                )},
            })
        );
        assert_eq!(
            pieces[1],
            serde_json::json!({
                "id": null, "path": "scratch", "name": "scratch",
                "state": "new", "flags": [],
            })
        );
        assert_eq!(json["remote"], serde_json::json!({"state": "none"}));
        assert_eq!(
            json["skipped"],
            serde_json::json!({"symlinks": 0, "dot_entries": 0})
        );
    }

    // =========================================================
    // Around the pieces
    // =========================================================

    #[test]
    fn recovered_trees_are_listed_by_name() {
        let mut world = world();
        world.dir(".antiq/recovered/01ARZ3NDEKTSV4RRFFQ69G5FAV-2026-11-01T10-00-00Z");
        let report = world.report();
        assert_eq!(
            report.recovered,
            ["01ARZ3NDEKTSV4RRFFQ69G5FAV-2026-11-01T10-00-00Z"]
        );
    }

    #[test]
    fn a_recovered_directory_that_cannot_be_listed_gives_a_warning() {
        let world = world();
        let recovered = world.collection.recovered_dir();
        fs::set_permissions(&recovered, fs::Permissions::from_mode(0o000)).expect("a mode");
        assert!(
            fs::read_dir(&recovered).is_err(),
            "the test needs a directory that cannot be read; it must not run as root"
        );
        let report = world.report();
        fs::set_permissions(&recovered, fs::Permissions::from_mode(0o755)).expect("a mode");
        assert!(report.recovered.is_empty());
        assert!(
            report.warnings[0].message.contains("recovered"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_missing_recovered_directory_is_an_empty_list() {
        let world = world();
        fs::remove_dir(world.collection.recovered_dir()).expect("an empty directory");
        let report = world.report();
        assert!(report.recovered.is_empty());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn the_counts_follow_the_states() {
        let mut world = world();
        world.dir("scratch");
        world.dir("work");
        world.dir("unearthed");
        world.put_meta(&metadata(
            1,
            "work",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-work.tar.zst",
            )],
        ));
        world.put_entry(1, "work", EntryState::Live, false);
        world.put_meta(&metadata(
            2,
            "unearthed",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(2, "unearthed", EntryState::Live, true);
        world.put_meta(&metadata(
            3,
            "kept",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        world.put_entry(3, "kept", EntryState::Gone, true);
        world.put_meta(&metadata(
            4,
            "far",
            Lifecycle::Archived,
            OTHER,
            vec![archive_payload()],
        ));
        world.put_meta(&metadata(
            5,
            "missing",
            Lifecycle::Live,
            COLLECTION_ID,
            vec![payload(
                PayloadKind::Live,
                1,
                "0123456789abcdef-missing.tar.zst",
            )],
        ));
        world.put_entry(5, "missing", EntryState::Live, false);
        world.put_meta(&metadata(6, "theirs", Lifecycle::Live, OTHER, Vec::new()));
        let counts = world.report().counts;
        // Live, unearthed and new pieces are live; archived and
        // remote-only are archived; gone, other machines' and error pieces
        // count nowhere.
        assert_eq!(
            counts,
            Counts {
                live: 3,
                archived: 2,
                remote_only: 1,
                pinned: 0,
                empty: 0
            }
        );
    }

    #[test]
    fn the_pieces_are_sorted_by_path_then_id_with_new_pieces_first() {
        let mut world = world();
        world.dir("b");
        world.dir("a");
        world.put_meta(&metadata(
            2,
            "a",
            Lifecycle::Archived,
            OTHER,
            vec![archive_payload()],
        ));
        let paths: Vec<_> = states(&world.report()).into_iter().map(|p| p.0).collect();
        assert_eq!(paths, ["a", "a", "b"]);
    }

    #[test]
    fn status_writes_nothing_below_the_antiq_directory() {
        let mut world = world();
        world.dir("work");
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        let before = listing(world.collection.antiq_dir().as_path());
        world.report();
        assert_eq!(listing(world.collection.antiq_dir().as_path()), before);
    }

    /// Every path below `root` with its size and modification time.
    fn listing(root: &Path) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for item in fs::read_dir(&directory).expect("a directory") {
                let item = item.expect("an entry");
                let metadata = item.metadata().expect("metadata");
                if metadata.is_dir() {
                    pending.push(item.path());
                }
                found.push((item.path(), metadata.len(), metadata.modified().ok()));
            }
        }
        found.sort();
        found
    }
}
