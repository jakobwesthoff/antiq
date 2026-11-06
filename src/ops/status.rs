// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `status` command: every piece of the collection with its state.
//!
//! `status` only reads. It takes no lock and writes nothing below
//! `.antiq/`, so it can run while another run holds the collection.

#![allow(unused)]
use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::device::{self, FsKind};
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::idle::{self, IdleError, IdleVerdict};
use crate::lock;
use crate::meta::{HistoryAction, Lifecycle, Metadata, Payload, PayloadKind};
use crate::relpath::{Name, RelPath, escape};
use crate::remote::away;
use crate::remote::{self, RemoteState};
use crate::report::{CommandReport, Warning};
use crate::select::{Rules, SelectError, select};
use crate::store::{Current, Source, Store, StoreError};
use crate::tracking::{
    self, Answer, DecisionKind, EntryState, PieceState, TrackingEntry, TrackingError, derive_state,
};
use crate::tree::{PieceTree, Problem};
use crate::walk;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    #[error(transparent)]
    Walk(#[from] walk::WalkError),
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

impl Piece {
    fn new(id: Option<ObjectId>, path: RelPath, name: Name, state: PieceKind) -> Piece {
        Piece {
            id,
            path,
            name,
            state,
            flags: Vec::new(),
            remote_only_in_days: None,
            age_days: None,
            detail: Detail::default(),
            unearthed: None,
            diverged: false,
        }
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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
    /// The first entry that could not be read, relative to the collection
    /// root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable_first: Option<RelPath>,
    /// Where other devices are mounted inside the piece. A top directory on
    /// another device counts as a mount point at the piece's own path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mount_points: Vec<RelPath>,
    /// Exclude entries inside the piece.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_paths: Vec<RelPath>,
    /// Files with a modification time too far in the future to count.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub future_files: Vec<RelPath>,
    /// The time the idle clock counts from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_clock: Option<Timestamp>,
    /// Since when the live backup fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_since: Option<Timestamp>,
    /// The decisions waiting for an answer, with the answers that fit.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
}

/// A decision `resolve` can answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub kind: DecisionKind,
    pub options: Vec<Answer>,
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
    /// Whole days left until the archive becomes remote-only, for an
    /// archived piece whose archive is on the remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_only_in_days: Option<i64>,
    /// Whole days since the idle clock, where the piece has a verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_days: Option<i64>,
    #[serde(skip_serializing_if = "Detail::is_empty")]
    pub detail: Detail,
    /// The time the piece was last unearthed, for the idle clock.
    #[serde(skip)]
    unearthed: Option<Timestamp>,
    /// A diverged folder is neither archived nor synced.
    #[serde(skip)]
    diverged: bool,
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
    /// When the last `sync` finished, as `remote-state.json` records it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<Timestamp>,
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

    /// The filesystem calls the walk makes, so that a test can put a
    /// second device inside a piece.
    fn walk_hooks(&self) -> &dyn walk::Hooks;

    /// What kind of filesystem the collection lies on.
    fn fs_kind(&self, root: &Path) -> Result<FsKind, FsxError>;

    /// The warning about a remote that stays away, if there is one.
    fn away_warning(
        &self,
        collection: &Collection,
        state: RemoteState,
        pending_remote_work: bool,
        now: Timestamp,
    ) -> Option<Warning>;
}

struct RealProbes;

impl Probes for RealProbes {
    fn remote_state(&self, collection: &Collection) -> RemoteState {
        remote::probe_state(collection)
    }

    fn walk_hooks(&self) -> &dyn walk::Hooks {
        &walk::RealFs
    }

    fn fs_kind(&self, root: &Path) -> Result<FsKind, FsxError> {
        device::fs_kind(root)
    }

    fn away_warning(
        &self,
        collection: &Collection,
        state: RemoteState,
        pending_remote_work: bool,
        now: Timestamp,
    ) -> Option<Warning> {
        away::status_warning(collection, state, pending_remote_work, now)
    }
}

pub(crate) struct StatusRequest<'a> {
    pub collection: &'a Collection,
    pub clock: &'a dyn Clock,
    pub probes: &'a dyn Probes,
}

/// Shows every piece of `collection` as of `clock`'s now.
pub fn status(collection: &Collection, clock: &dyn Clock) -> Result<StatusReport, StatusError> {
    status_with(&StatusRequest {
        collection,
        clock,
        probes: &RealProbes,
    })
}

pub(crate) fn status_with(request: &StatusRequest<'_>) -> Result<StatusReport, StatusError> {
    let collection = request.collection;
    let own = collection.id();
    let config = &collection.config().collection;
    let mut warnings = Vec::new();
    let mut unknown_keys = collection.config().unknown_collection_keys.clone();
    unknown_keys.sort();
    for key in &unknown_keys {
        warnings.push(Warning::new(format!(
            "the key `{key}` under [collection] is unknown and ignored"
        )));
    }
    match request.probes.fs_kind(collection.root()) {
        Ok(FsKind::Smb) => warnings.push(Warning::new("the collection is on an SMB mount")),
        Ok(FsKind::Nfs) => warnings.push(Warning::new("the collection is on an NFS mount")),
        Ok(FsKind::Local | FsKind::Other) => {}
        Err(error) => warnings.push(Warning::new(format!(
            "cannot tell what kind of filesystem the collection is on: {error}"
        ))),
    }
    // The lock is only read: taking it would make `status` a run of its own.
    match lock::holder(&collection.lock_path()) {
        Ok(Some(holder)) => warnings.push(Warning::new(format!(
            "another run holds the lock (pid {}, machine {}, started {})",
            holder.pid, holder.machine, holder.started
        ))),
        Ok(None) => {}
        Err(error) => warnings.push(Warning::new(format!("cannot read the lock file: {error}"))),
    }
    let last_sync = read_last_sync(collection, &mut warnings);

    // =====================================================
    // What the folders say
    // =====================================================

    let root = TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    let selection = select(&root, &rules)?;
    let selected: HashSet<RelPath> = selection.pieces().iter().cloned().collect();
    let mut unreadable_directories = selection.unreadable().to_vec();
    unreadable_directories.sort();
    for directory in &unreadable_directories {
        warnings.push(Warning::new(format!(
            "cannot read {directory}: pieces below it are neither new nor gone in this run"
        )));
    }

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
    for (object, current) in &metadata {
        for version in &current.unreadable {
            warnings.push(Warning::new(format!(
                "meta.{} of {object} cannot be read: {}",
                version.number, version.reason
            )));
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
                let mut piece = Piece::new(
                    Some(entry.id),
                    entry.path.clone(),
                    current.map_or_else(|| last_name(&entry.path), |m| m.name.clone()),
                    state.into(),
                );
                piece.unearthed = entry.unearthed;
                piece.diverged = entry.diverged;
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
            Ok(state) => {
                let mut piece = Piece::new(
                    Some(*object),
                    known.path.clone(),
                    known.name.clone(),
                    state.into(),
                );
                if state == PieceState::LiveOnAnotherMachine {
                    piece.detail.machine = machine_of(known);
                }
                pieces.push(piece);
            }
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
            pieces.push(Piece::new(
                None,
                new.clone(),
                last_name(new),
                PieceKind::New,
            ));
        }
    }

    // =====================================================
    // What the folders hold
    // =====================================================

    let now = request.clock.now();
    let entries: HashMap<ObjectId, &TrackingEntry> = tracked
        .entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect();
    let remote_attached = !collection.config().remotes.is_empty();
    for piece in &mut pieces {
        let current = piece
            .id
            .filter(|_| piece.state != PieceKind::Error)
            .and_then(|object| metadata.get(&object));
        if let Some(current) = current {
            let entry = piece.id.and_then(|object| entries.get(&object).copied());
            note_fields(piece, entry, current);
            if piece.state == PieceKind::Archived
                && remote_attached
                && !piece.flags.contains(&Flag::PendingUpload)
            {
                piece.remote_only_in_days = archive_commit_time(&current.metadata).map(|commit| {
                    let elapsed = (now.as_second() - commit.as_second()).max(0) / 86_400;
                    (i64::from(config.remote_only_days) - elapsed).max(0)
                });
            }
        }
    }
    let junk = junk_names(&config.junk, &mut warnings);
    let idle_settings = match idle::Settings::new(config.idle_days, &config.ignore) {
        Ok(settings) => Some(settings),
        Err(error) => {
            warnings.push(Warning::new(format!(
                "{error}, so no piece has an idle verdict"
            )));
            None
        }
    };
    let walk_hooks = request.probes.walk_hooks();
    for piece in &mut pieces {
        let has_folder = matches!(
            piece.state,
            PieceKind::New | PieceKind::Live | PieceKind::Unearthed
        );
        // A piece below a directory selection could not read is neither
        // walked nor judged.
        if has_folder && piece.detail.below_unreadable.is_none() {
            let tree = walk::walk_piece_with(&root, &piece.path, &junk, &rules, walk_hooks)?;
            judge(piece, &tree, &root, idle_settings.as_ref(), now)?;
        }
        piece.flags.sort();
        piece.flags.dedup();
    }
    pieces.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
    damaged.sort_by(|a, b| a.id.cmp(&b.id).then(a.path.cmp(&b.path)));

    // =====================================================
    // Around the pieces
    // =====================================================

    let recovered = recovered_names(collection, &mut warnings);
    let counts = count(&pieces);
    let remote_state = request.probes.remote_state(collection);
    let pending_remote_work = pieces.iter().any(|piece| {
        piece.flags.contains(&Flag::PendingUpload) || piece.flags.contains(&Flag::StaleBackup)
    });
    warnings.extend(request.probes.away_warning(
        collection,
        remote_state,
        pending_remote_work,
        now,
    ));
    Ok(StatusReport {
        collection: CollectionInfo {
            id: own,
            path: collection.root().display().to_string(),
        },
        remote: RemoteInfo {
            state: remote_state,
            url: collection
                .config()
                .remotes
                .first()
                .map(|remote| remote.url.clone()),
        },
        last_sync,
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

/// The junk list as names. An entry that is no file name cannot match
/// anything and gives a warning.
fn junk_names(junk: &[String], warnings: &mut Vec<Warning>) -> Vec<Name> {
    junk.iter()
        .filter_map(|entry| match Name::from_bytes(entry.clone().into_bytes()) {
            Ok(name) => Some(name),
            Err(_) => {
                warnings.push(Warning::new(format!(
                    "collection.junk entry `{entry}` is not a file name and is ignored"
                )));
                None
            }
        })
        .collect()
}

/// Sets the flags, details and age a walk and the idle check give a piece.
fn judge(
    piece: &mut Piece,
    tree: &PieceTree,
    root: &TreeRoot,
    settings: Option<&idle::Settings>,
    now: Timestamp,
) -> Result<(), StatusError> {
    // The walk reports paths relative to the piece's top, which is the
    // piece's own path for the top itself.
    for problem in tree.problems() {
        match problem {
            Problem::Unreadable { first } => {
                piece.flags.push(Flag::Unreadable);
                piece.detail.unreadable_first = Some(piece.path.concat(first));
            }
            Problem::MountPoint { other } => {
                piece.flags.push(Flag::MountPoint);
                piece.detail.mount_points.push(piece.path.concat(other));
            }
            Problem::Excluded { path } => {
                piece.flags.push(Flag::Excluded);
                piece.detail.excluded_paths.push(piece.path.concat(path));
            }
            // Until `status` has a row of its own for it, a collection
            // inside a piece shows as a mount point at its marker.
            Problem::NestedCollection { root } => {
                piece.flags.push(Flag::MountPoint);
                piece.detail.mount_points.push(piece.path.concat(root));
            }
        }
    }
    let Some(settings) = settings else {
        return Ok(());
    };
    let check = match idle::check(root, tree, piece.unearthed, settings, now) {
        Ok(check) => check,
        // A piece the walk could not read completely has no verdict.
        Err(IdleError::Problems) => return Ok(()),
        // Running out of file descriptors fails the run, as in a walk.
        Err(IdleError::IgnoreFile { path, source }) if walk::is_out_of_descriptors(&source) => {
            return Err(StatusError::Io {
                action: "read",
                path: path.to_string(),
                source,
            });
        }
        Err(error) => {
            piece.flags.push(Flag::Error);
            piece.detail.error = Some(error.to_string());
            return Ok(());
        }
    };
    if !check.future.is_empty() {
        piece.flags.push(Flag::FutureMtime);
        piece.detail.future_files = check
            .future
            .iter()
            .map(|file| piece.path.concat(file))
            .collect();
    }
    match check.verdict {
        IdleVerdict::Pinned => piece.flags.push(Flag::Pinned),
        IdleVerdict::Empty => piece.flags.push(Flag::Empty),
        IdleVerdict::OnlyFuture => {}
        IdleVerdict::Active { clock, .. } => note_age(piece, clock, now),
        IdleVerdict::Idle { clock, .. } => {
            note_age(piece, clock, now);
            // A diverged folder waits for a decision, not for archiving.
            if !piece.diverged {
                piece.flags.push(Flag::Idle);
            }
        }
    }
    Ok(())
}

fn note_age(piece: &mut Piece, clock: Timestamp, now: Timestamp) {
    let seconds = (now.as_second() - clock.as_second()).max(0);
    piece.age_days = Some(seconds / 86_400);
    piece.detail.idle_clock = Some(clock);
}

/// The history actions that bring a new current archive.
const ARCHIVING: [HistoryAction; 4] = [
    HistoryAction::Archive,
    HistoryAction::Import,
    HistoryAction::Resolve,
    HistoryAction::Fork,
];

/// The current archive of an object: its newest payload that is not
/// superseded.
fn current_archive(metadata: &Metadata) -> Option<&Payload> {
    metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Archive && !payload.superseded)
        .max_by_key(|payload| payload.version)
}

/// The time the current archive was committed: the earliest history entry,
/// from the creation of the archive on, that an archiving action wrote.
/// The remote-only tier counts from it.
pub(crate) fn archive_commit_time(metadata: &Metadata) -> Option<Timestamp> {
    let archive = current_archive(metadata)?;
    metadata
        .history
        .iter()
        .filter(|entry| entry.time >= archive.created)
        .filter(|entry| {
            ARCHIVING
                .iter()
                .any(|action| action.as_str() == entry.action)
        })
        .map(|entry| entry.time)
        .min()
}

/// The flags the entry and the metadata give a piece.
fn note_fields(piece: &mut Piece, entry: Option<&TrackingEntry>, current: &Current) {
    if let Some(entry) = entry {
        if let Some(since) = entry.stale_backup_since {
            piece.flags.push(Flag::StaleBackup);
            piece.detail.stale_since = Some(since);
        }
        if entry.diverged {
            piece.flags.push(Flag::Diverged);
        }
        if entry.missing_on_remote {
            piece.flags.push(Flag::MissingOnRemote);
        }
        if !entry.pending_decisions.is_empty() {
            piece.flags.push(Flag::NeedsDecision);
            piece.detail.decisions = entry
                .pending_decisions
                .iter()
                .map(|decision| Decision {
                    kind: decision.kind,
                    options: decision.options().to_vec(),
                })
                .collect();
        }
    }
    let metadata = &current.metadata;
    if metadata
        .payloads
        .iter()
        .any(|payload| payload.kind == PayloadKind::Archive && payload.superseded)
    {
        piece.flags.push(Flag::Superseded);
    }
    // An archive that is only here, or whose copy on the remote was never
    // checked, still has to go up; so has a change nobody committed yet.
    let archive = current_archive(metadata);
    let held = entry.zip(archive).is_some_and(|(entry, archive)| {
        entry
            .local_payloads
            .iter()
            .any(|held| held.kind == PayloadKind::Archive && held.file == archive.file)
    });
    let unconfirmed = archive.is_some_and(|archive| !matches!(archive.verified, Some(Some(_))));
    if current.source == Source::Pending || (held && unconfirmed) {
        piece.flags.push(Flag::PendingUpload);
    }
}

/// When the last `sync` finished, from `.antiq/remote-state.json`. The file
/// is written by runs, so a state that cannot be read is a warning and no
/// failure of `status`.
fn read_last_sync(collection: &Collection, warnings: &mut Vec<Warning>) -> Option<Timestamp> {
    #[derive(Deserialize)]
    struct StateFile {
        last_sync: Option<Timestamp>,
    }
    let file = collection.remote_state_path();
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            warnings.push(Warning::new(format!(
                "cannot read {}: {error}",
                relative_to_collection(collection, &file)
            )));
            return None;
        }
    };
    match serde_json::from_slice::<StateFile>(&bytes) {
        Ok(state) => state.last_sync.map(whole_seconds),
        Err(error) => {
            warnings.push(Warning::new(format!(
                "cannot read {}: {error}",
                relative_to_collection(collection, &file)
            )));
            None
        }
    }
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
    let mut piece = Piece::new(id, path.clone(), last_name(&path), PieceKind::Error);
    piece.flags.push(Flag::Error);
    piece.detail.error = Some(message);
    piece
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
    use crate::clock::FixedClock;
    use crate::fsx::FsxError;
    use crate::meta::{Checksum, CreatedSource, FileName, Payload, PayloadKind};
    use crate::meta::{HistoryAction, HistoryEntry};
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use crate::tracking::{DecisionKind, PendingDecision};
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

        fn walk_hooks(&self) -> &dyn walk::Hooks {
            &walk::RealFs
        }

        fn fs_kind(&self, _: &Path) -> Result<FsKind, FsxError> {
            Ok(FsKind::Local)
        }

        fn away_warning(
            &self,
            _: &Collection,
            _: RemoteState,
            _: bool,
            _: Timestamp,
        ) -> Option<Warning> {
            None
        }
    }

    /// Probes whose filesystem is `kind`, or cannot be told.
    struct Mounted(Option<FsKind>);

    impl Probes for Mounted {
        fn remote_state(&self, _: &Collection) -> RemoteState {
            RemoteState::None
        }

        fn walk_hooks(&self) -> &dyn walk::Hooks {
            &walk::RealFs
        }

        fn fs_kind(&self, root: &Path) -> Result<FsKind, FsxError> {
            self.0.ok_or_else(|| FsxError::Io {
                action: "stat",
                path: root.display().to_string(),
                source: io::Error::from_raw_os_error(5),
            })
        }

        fn away_warning(
            &self,
            _: &Collection,
            _: RemoteState,
            _: bool,
            _: Timestamp,
        ) -> Option<Warning> {
            None
        }
    }

    /// Probes that remember what the away warning was asked.
    struct Away(std::cell::RefCell<Vec<(RemoteState, bool)>>);

    impl Probes for Away {
        fn remote_state(&self, _: &Collection) -> RemoteState {
            RemoteState::Away
        }

        fn walk_hooks(&self) -> &dyn walk::Hooks {
            &walk::RealFs
        }

        fn fs_kind(&self, _: &Path) -> Result<FsKind, FsxError> {
            Ok(FsKind::Local)
        }

        fn away_warning(
            &self,
            _: &Collection,
            state: RemoteState,
            pending: bool,
            _: Timestamp,
        ) -> Option<Warning> {
            self.0.borrow_mut().push((state, pending));
            Some(Warning::new("the remote has been away"))
        }
    }

    /// Probes whose walk sees another device at some paths or fails.
    struct Hooked(Box<dyn walk::Hooks>);

    impl Probes for Hooked {
        fn remote_state(&self, _: &Collection) -> RemoteState {
            RemoteState::None
        }

        fn walk_hooks(&self) -> &dyn walk::Hooks {
            self.0.as_ref()
        }

        fn fs_kind(&self, _: &Path) -> Result<FsKind, FsxError> {
            Ok(FsKind::Local)
        }

        fn away_warning(
            &self,
            _: &Collection,
            _: RemoteState,
            _: bool,
            _: Timestamp,
        ) -> Option<Warning> {
            None
        }
    }

    /// A second device below the piece at `path`, relative to its top.
    struct OtherDevice(&'static str);

    impl walk::Hooks for OtherDevice {
        fn device(&self, path: &RelPath, reported: u64) -> u64 {
            if path.to_string() == self.0 {
                reported + 1
            } else {
                reported
            }
        }

        fn fault(&self, _: &RelPath, _: walk::Call) -> Option<io::Error> {
            None
        }
    }

    /// A walk that runs out of file descriptors.
    struct OutOfDescriptors;

    impl walk::Hooks for OutOfDescriptors {
        fn fault(&self, _: &RelPath, call: walk::Call) -> Option<io::Error> {
            (call == walk::Call::List).then(|| io::Error::from_raw_os_error(24))
        }
    }

    const NOW: &str = "2026-11-01T12:00:00Z";

    fn now() -> Timestamp {
        NOW.parse().expect("a time")
    }

    fn clock() -> FixedClock {
        FixedClock::new(now())
    }

    /// Seconds since the epoch, `days` days before now.
    fn days_ago(days: i64) -> i64 {
        now().as_second() - days * 86_400
    }

    fn run(collection: &Collection) -> StatusReport {
        run_with(collection, &Probed(RemoteState::None)).expect("status runs")
    }

    fn run_with(collection: &Collection, probes: &dyn Probes) -> Result<StatusReport, StatusError> {
        status_with(&StatusRequest {
            collection,
            clock: &clock(),
            probes,
        })
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
            run(&self.collection)
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
        let report = status(&world.collection, &clock()).expect("status runs");
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
        let report = run(&collection);
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
        let report = run(&collection);
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
        let report = run(&collection);
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
        let report = run(&collection);
        assert_eq!(states(&report), [("work".into(), PieceKind::Gone)]);
    }

    // =========================================================
    // The report as data
    // =========================================================

    #[test]
    fn a_collection_root_that_cannot_be_opened_fails_the_run() {
        let error =
            status(&Collection::for_tests(), &clock()).expect_err("the root does not exist");
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
                "state": "new", "flags": ["empty"],
            })
        );
        assert_eq!(json["remote"], serde_json::json!({"state": "none"}));
        assert_eq!(
            json["skipped"],
            serde_json::json!({"symlinks": 0, "dot_entries": 0})
        );
    }

    // =========================================================
    // What the folders hold
    // =========================================================

    impl World {
        /// Rewrites the config with extra `[collection]` lines.
        fn configure(&mut self, lines: &str) {
            fs::write(
                self.temp.root().join(".antiq/config.toml"),
                format!("[collection]\nid = \"{COLLECTION_ID}\"\n{lines}\n"),
            )
            .expect("a config can be written");
            self.collection = Collection::find(self.temp.root()).expect("a collection");
        }

        fn file(&mut self, relative: &str, age_days: i64) {
            self.temp
                .tree()
                .file_with(relative, "x", 0o644, days_ago(age_days));
        }

        fn only(&self) -> Piece {
            let report = self.report();
            assert_eq!(report.pieces.len(), 1, "{:?}", states(&report));
            report.pieces[0].clone()
        }

        fn live_entry_of(&self, at: &str, archive: bool) -> TrackingEntry {
            let lifecycle = Lifecycle::Live;
            let payloads = if archive {
                vec![archive_payload()]
            } else {
                vec![payload(PayloadKind::Live, 1, "0123456789abcdef-a.tar.zst")]
            };
            self.put_meta(&metadata(1, at, lifecycle, COLLECTION_ID, payloads));
            TrackingEntry::new(
                id(1),
                path(at),
                EntryState::Live,
                VersionTakenOut::Committed(1),
                when(),
            )
        }
    }

    #[test]
    fn a_piece_with_a_pin_is_pinned_and_has_no_age() {
        let mut world = world();
        world.file("p/.antiq-pin", 90);
        world.file("p/note.txt", 90);
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::Pinned]);
        assert_eq!(piece.age_days, None);
    }

    #[test]
    fn a_piece_without_counted_files_is_empty() {
        let mut world = world();
        world.dir("e/sub");
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::Empty]);
        assert_eq!(piece.age_days, None);
    }

    #[test]
    fn a_new_piece_older_than_the_idle_days_is_idle() {
        let mut world = world();
        world.file("old/note.txt", 30);
        let piece = world.only();
        assert_eq!(piece.state, PieceKind::New);
        assert_eq!(piece.flags, [Flag::Idle]);
        assert_eq!(piece.age_days, Some(30));
        assert_eq!(
            piece.detail.idle_clock,
            Some(Timestamp::from_second(days_ago(30)).expect("a time"))
        );
    }

    #[test]
    fn a_piece_younger_than_the_idle_days_only_has_an_age() {
        let mut world = world();
        world.file("busy/note.txt", 3);
        let piece = world.only();
        assert_eq!(piece.flags, []);
        assert_eq!(piece.age_days, Some(3));
    }

    #[test]
    fn an_idle_live_piece_is_idle() {
        let mut world = world();
        world.file("work/note.txt", 20);
        let entry = world.live_entry_of("work", false);
        world.write_entry(&entry);
        let piece = world.only();
        assert_eq!(piece.state, PieceKind::Live);
        assert_eq!(piece.flags, [Flag::Idle]);
    }

    #[test]
    fn an_unearthed_piece_counts_from_the_unearthing() {
        let mut world = world();
        world.file("work/note.txt", 20);
        let mut entry = world.live_entry_of("work", true);
        entry.unearthed = Some(Timestamp::from_second(days_ago(1)).expect("a time"));
        world.write_entry(&entry);
        let piece = world.only();
        assert_eq!(piece.state, PieceKind::Unearthed);
        assert_eq!(piece.flags, []);
        assert_eq!(piece.age_days, Some(1));
    }

    #[test]
    fn a_diverged_folder_is_not_called_idle_but_keeps_its_age() {
        let mut world = world();
        world.file("work/note.txt", 20);
        let mut entry = world.live_entry_of("work", true);
        entry.diverged = true;
        world.write_entry(&entry);
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::Diverged]);
        assert_eq!(piece.age_days, Some(20));
    }

    #[test]
    fn an_unreadable_entry_has_no_verdict_and_names_the_first_one() {
        let mut world = world();
        world.file("u/note.txt", 30);
        world.temp.tree().unreadable_dir("u/sub");
        let blocked = world.temp.root().join("u/sub");
        assert!(
            fs::read_dir(&blocked).is_err(),
            "the test needs a directory that cannot be read; it must not run as root"
        );
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::Unreadable]);
        assert_eq!(piece.age_days, None);
        assert_eq!(piece.detail.unreadable_first, Some(path("u/sub")));
        assert_eq!(piece.detail.error, None);
    }

    #[test]
    fn a_top_directory_on_another_device_is_a_mount_point_at_the_pieces_own_path() {
        let mut world = world();
        world.dir("m");
        let piece = run_with(&world.collection, &Hooked(Box::new(OtherDevice(""))))
            .expect("status runs")
            .pieces
            .remove(0);
        assert_eq!(piece.flags, [Flag::MountPoint]);
        assert_eq!(piece.detail.mount_points, [path("m")]);
    }

    #[test]
    fn a_mount_point_inside_a_piece_is_named_relative_to_the_collection() {
        let mut world = world();
        world.file("m/note.txt", 30);
        world.dir("m/sub");
        let piece = run_with(&world.collection, &Hooked(Box::new(OtherDevice("sub"))))
            .expect("status runs")
            .pieces
            .remove(0);
        assert_eq!(piece.detail.mount_points, [path("m/sub")]);
    }

    #[test]
    fn a_pinned_piece_with_a_mount_point_has_both_flags() {
        let mut world = world();
        world.file("m/.antiq-pin", 30);
        world.dir("m/sub");
        let piece = run_with(&world.collection, &Hooked(Box::new(OtherDevice("sub"))))
            .expect("status runs")
            .pieces
            .remove(0);
        assert_eq!(piece.flags, [Flag::Pinned, Flag::MountPoint]);
    }

    #[test]
    fn a_piece_holding_an_exclude_entry_names_its_path() {
        let mut world = world();
        world.configure("exclude = [\"x/skip\"]");
        world.file("x/note.txt", 30);
        world.dir("x/skip");
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::Excluded]);
        assert_eq!(piece.detail.excluded_paths, [path("x/skip")]);
    }

    #[test]
    fn files_too_far_ahead_are_listed_and_do_not_count() {
        let mut world = world();
        world.file("f/old.txt", 30);
        world.file("f/new.txt", -5);
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::FutureMtime, Flag::Idle]);
        assert_eq!(piece.detail.future_files, [path("f/new.txt")]);
    }

    #[test]
    fn a_piece_with_only_future_files_is_not_idle() {
        let mut world = world();
        world.file("f/new.txt", -5);
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::FutureMtime]);
        assert_eq!(piece.age_days, None);
    }

    #[test]
    fn broken_ignore_rules_of_a_piece_are_an_error_of_that_piece() {
        let mut world = world();
        world.file("b/note.txt", 30);
        world.temp.tree().file("b/.antiqignore", "fine\nbroken\\\n");
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::Error]);
        let message = piece.detail.error.expect("a message");
        assert!(message.contains(".antiqignore"), "{message}");
        assert_eq!(piece.age_days, None);
    }

    #[test]
    fn broken_ignore_rules_in_the_config_give_one_warning_and_no_verdicts() {
        let mut world = world();
        world.configure("ignore = [\"broken\\\\\"]");
        world.file("a/note.txt", 30);
        world.file("b/note.txt", 30);
        let report = world.report();
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].message.contains("collection.ignore"),
            "{:?}",
            report.warnings
        );
        for piece in &report.pieces {
            assert_eq!(piece.flags, []);
            assert_eq!(piece.age_days, None);
        }
    }

    #[test]
    fn a_junk_entry_that_is_no_file_name_is_ignored_with_a_warning() {
        let mut world = world();
        world.configure("junk = [\"a/b\"]");
        world.file("a/note.txt", 30);
        let report = world.report();
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].message.contains("collection.junk"));
        assert_eq!(report.pieces[0].flags, [Flag::Idle]);
    }

    #[test]
    fn running_out_of_file_descriptors_fails_the_run() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let error = run_with(&world.collection, &Hooked(Box::new(OutOfDescriptors)))
            .expect_err("no descriptors left");
        assert!(matches!(error, StatusError::Walk(_)), "{error:?}");
    }

    #[test]
    fn pieces_below_an_unreadable_directory_are_not_walked() {
        let mut world = world();
        world.dir("group/work");
        let entry = world.live_entry_of("group/work", false);
        world.write_entry(&entry);
        world.configure("patterns = [\"group/*\"]");
        let blocked = world.temp.root().join("group");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).expect("a mode");
        assert!(
            fs::read_dir(&blocked).is_err(),
            "the test needs a directory that cannot be read; it must not run as root"
        );
        let piece = world.only();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).expect("a mode");
        assert_eq!(piece.flags, []);
        assert_eq!(piece.age_days, None);
    }

    // =========================================================
    // What the entries and the metadata say
    // =========================================================

    const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

    impl World {
        fn configure_with_remote(&mut self) {
            fs::write(
                self.temp.root().join(".antiq/config.toml"),
                format!(
                    "[collection]\nid = \"{COLLECTION_ID}\"\n\n[[remote]]\nurl = \"ssh://nas/srv/antiq\"\nid = \"{REMOTE_ID}\"\n"
                ),
            )
            .expect("a config can be written");
            self.collection = Collection::find(self.temp.root()).expect("a collection");
        }

        /// A live piece with its folder, an entry and a hook to change both.
        fn live_piece(&mut self, change: impl FnOnce(&mut TrackingEntry)) -> Piece {
            self.file("work/note.txt", 1);
            let mut entry = self.live_entry_of("work", false);
            change(&mut entry);
            self.write_entry(&entry);
            self.only()
        }

        /// An archived piece held locally, with the archive payload and
        /// the history entry that `change` makes.
        fn archived_piece(
            &mut self,
            change: impl FnOnce(&mut Metadata, &mut TrackingEntry),
        ) -> Piece {
            let mut known = metadata(
                1,
                "old",
                Lifecycle::Archived,
                COLLECTION_ID,
                vec![archive_payload()],
            );
            let mut entry = TrackingEntry::new(
                id(1),
                path("old"),
                EntryState::Gone,
                VersionTakenOut::Committed(1),
                when(),
            );
            entry.local_payloads = vec![LocalPayload {
                kind: PayloadKind::Archive,
                version: 1,
                file: FileName::new("0123456789abcdef-a.tar.zst").expect("a plain name"),
            }];
            change(&mut known, &mut entry);
            self.put_meta(&known);
            self.write_entry(&entry);
            self.only()
        }
    }

    fn committed(known: &mut Metadata, action: HistoryAction, time: &str) {
        known.history.push(HistoryEntry::new(
            time.parse().expect("a time"),
            "laptop",
            action,
        ));
    }

    fn verified(known: &mut Metadata, time: Option<&str>) {
        known.payloads[0].verified = Some(time.map(|time| time.parse().expect("a time")));
    }

    #[test]
    fn a_stale_backup_names_since_when() {
        let mut world = world();
        let since: Timestamp = "2026-10-30T08:00:00Z".parse().expect("a time");
        let piece = world.live_piece(|entry| entry.stale_backup_since = Some(since));
        assert_eq!(piece.flags, [Flag::StaleBackup]);
        assert_eq!(piece.detail.stale_since, Some(since));
    }

    #[test]
    fn a_diverged_folder_is_flagged() {
        let mut world = world();
        let piece = world.live_piece(|entry| entry.diverged = true);
        assert_eq!(piece.flags, [Flag::Diverged]);
    }

    #[test]
    fn a_missing_archive_on_the_remote_is_flagged() {
        let mut world = world();
        let piece = world.live_piece(|entry| entry.missing_on_remote = true);
        assert_eq!(piece.flags, [Flag::MissingOnRemote]);
    }

    fn decision(kind: DecisionKind, options: &[Answer]) {
        let mut world = world();
        let piece = world.live_piece(|entry| {
            entry.pending_decisions = vec![PendingDecision { kind }];
        });
        assert!(
            piece.flags.contains(&Flag::NeedsDecision),
            "{:?}",
            piece.flags
        );
        assert_eq!(
            piece.detail.decisions,
            [Decision {
                kind,
                options: options.to_vec()
            }]
        );
    }

    #[test]
    fn a_version_conflict_waits_for_a_decision() {
        decision(
            DecisionKind::VersionConflict,
            &[Answer::Mine, Answer::Theirs, Answer::Fork],
        );
    }

    #[test]
    fn replacing_with_loss_waits_for_a_decision() {
        decision(
            DecisionKind::ReplacingWithLoss,
            &[Answer::Replace, Answer::Fork, Answer::Skip],
        );
    }

    #[test]
    fn a_diverged_folder_decision_waits_for_an_answer() {
        decision(
            DecisionKind::DivergedFolder,
            &[Answer::Theirs, Answer::Mine, Answer::Fork],
        );
    }

    #[test]
    fn a_superseded_archive_waits_for_a_decision() {
        decision(DecisionKind::SupersededArchive, &[Answer::DropSuperseded]);
    }

    #[test]
    fn a_superseded_archive_payload_is_flagged() {
        let mut world = world();
        let piece = world.archived_piece(|known, _| {
            known.payloads[0].superseded = true;
            verified(known, Some("2026-10-02T00:00:00Z"));
        });
        assert_eq!(piece.flags, [Flag::Superseded]);
    }

    #[test]
    fn a_recorded_change_that_is_not_committed_is_pending_upload() {
        let mut world = world();
        let mut known = metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        );
        verified(&mut known, Some("2026-10-02T00:00:00Z"));
        let directory = world.collection.object_dir(known.id);
        fs::create_dir_all(&directory).expect("a directory");
        fs::write(
            directory.join("meta.pending"),
            crate::meta::to_bytes(&known).expect("metadata serialises"),
        )
        .expect("a file");
        let piece = world.only();
        assert_eq!(piece.flags, [Flag::PendingUpload]);
    }

    #[test]
    fn a_local_archive_never_checked_on_the_remote_is_pending_upload() {
        let mut world = world();
        let piece = world.archived_piece(|known, _| verified(known, None));
        assert_eq!(piece.flags, [Flag::PendingUpload]);
    }

    #[test]
    fn a_local_archive_without_a_check_time_is_pending_upload() {
        let mut world = world();
        let piece = world.archived_piece(|_, _| {});
        assert_eq!(piece.flags, [Flag::PendingUpload]);
    }

    #[test]
    fn a_local_archive_checked_on_the_remote_is_not_pending() {
        let mut world = world();
        let piece = world.archived_piece(|known, _| verified(known, Some("2026-10-02T00:00:00Z")));
        assert_eq!(piece.flags, []);
    }

    #[test]
    fn an_archive_held_only_on_the_remote_is_not_pending() {
        let mut world = world();
        let piece = world.archived_piece(|_, entry| entry.local_payloads.clear());
        assert_eq!(piece.flags, []);
    }

    fn remote_only_days(change: impl FnOnce(&mut Metadata)) -> Option<i64> {
        let mut world = world();
        world.configure_with_remote();
        world
            .archived_piece(|known, _| {
                verified(known, Some("2026-10-02T00:00:00Z"));
                change(known);
            })
            .remote_only_in_days
    }

    #[test]
    fn days_until_remote_only_count_from_the_commit_of_the_archive() {
        let days = remote_only_days(|known| {
            committed(known, HistoryAction::Archive, "2026-10-02T12:00:00Z");
        });
        assert_eq!(days, Some(150));
    }

    #[test]
    fn days_until_remote_only_never_go_below_zero() {
        let days = remote_only_days(|known| {
            known.payloads[0].created = "2024-12-31T00:00:00Z".parse().expect("a time");
            committed(known, HistoryAction::Archive, "2025-01-01T00:00:00Z");
        });
        assert_eq!(days, Some(0));
    }

    #[test]
    fn an_archive_without_a_commit_time_has_no_days() {
        assert_eq!(remote_only_days(|_| {}), None);
    }

    #[test]
    fn without_a_remote_no_archive_becomes_remote_only() {
        let mut world = world();
        let piece = world.archived_piece(|known, _| {
            verified(known, Some("2026-10-02T00:00:00Z"));
            committed(known, HistoryAction::Archive, "2026-10-02T12:00:00Z");
        });
        assert_eq!(piece.remote_only_in_days, None);
    }

    #[test]
    fn a_pending_archive_has_no_days_yet() {
        let mut world = world();
        world.configure_with_remote();
        let piece = world.archived_piece(|known, _| {
            verified(known, None);
            committed(known, HistoryAction::Archive, "2026-10-02T12:00:00Z");
        });
        assert_eq!(piece.remote_only_in_days, None);
    }

    #[test]
    fn the_commit_time_of_an_archive_is_the_history_entry_that_brought_it() {
        let mut known = metadata(
            1,
            "a",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        );
        committed(&mut known, HistoryAction::Archive, "2026-10-02T00:00:00Z");
        assert_eq!(
            archive_commit_time(&known),
            Some("2026-10-02T00:00:00Z".parse().expect("a time"))
        );
    }

    #[test]
    fn an_import_brings_an_archive_too() {
        let mut known = metadata(
            1,
            "a",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        );
        committed(&mut known, HistoryAction::Import, "2026-10-03T00:00:00Z");
        assert_eq!(
            archive_commit_time(&known),
            Some("2026-10-03T00:00:00Z".parse().expect("a time"))
        );
    }

    #[test]
    fn a_resolve_after_an_older_archive_brings_the_newer_one() {
        let mut older = archive_payload();
        older.created = "2026-09-01T00:00:00Z".parse().expect("a time");
        let mut newer = payload(PayloadKind::Archive, 2, "fedcba9876543210-a.tar.zst");
        newer.created = "2026-10-05T00:00:00Z".parse().expect("a time");
        let mut known = metadata(
            1,
            "a",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![older, newer],
        );
        committed(&mut known, HistoryAction::Archive, "2026-09-02T00:00:00Z");
        committed(&mut known, HistoryAction::Resolve, "2026-10-06T00:00:00Z");
        assert_eq!(
            archive_commit_time(&known),
            Some("2026-10-06T00:00:00Z".parse().expect("a time"))
        );
    }

    #[test]
    fn a_fork_brings_the_archive_of_its_new_id() {
        let mut known = metadata(
            1,
            "a",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        );
        committed(&mut known, HistoryAction::Fork, "2026-10-04T00:00:00Z");
        assert_eq!(
            archive_commit_time(&known),
            Some("2026-10-04T00:00:00Z".parse().expect("a time"))
        );
    }

    #[test]
    fn other_actions_bring_no_archive() {
        let mut known = metadata(
            1,
            "a",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        );
        committed(&mut known, HistoryAction::Sync, "2026-10-04T00:00:00Z");
        assert_eq!(archive_commit_time(&known), None);
    }

    #[test]
    fn a_superseded_archive_is_not_the_current_one() {
        let mut only = archive_payload();
        only.superseded = true;
        let mut known = metadata(1, "a", Lifecycle::Archived, COLLECTION_ID, vec![only]);
        committed(&mut known, HistoryAction::Archive, "2026-10-04T00:00:00Z");
        assert_eq!(archive_commit_time(&known), None);
    }

    // =========================================================
    // The remote and the warnings
    // =========================================================

    #[test]
    fn the_remote_state_and_url_come_from_the_probe_and_the_config() {
        let mut world = world();
        world.configure_with_remote();
        let report = run_with(&world.collection, &Probed(RemoteState::Reachable)).expect("runs");
        assert_eq!(report.remote.state, RemoteState::Reachable);
        assert_eq!(report.remote.url.as_deref(), Some("ssh://nas/srv/antiq"));
    }

    fn last_sync_of(content: Option<&str>) -> StatusReport {
        let world = world();
        if let Some(content) = content {
            fs::write(world.collection.remote_state_path(), content).expect("a file");
        }
        world.report()
    }

    #[test]
    fn without_a_state_file_the_remote_was_never_synced() {
        let report = last_sync_of(None);
        assert_eq!(report.last_sync, None);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn the_last_sync_is_read_in_whole_seconds_and_other_keys_are_ignored() {
        let report = last_sync_of(Some(
            r#"{"last_reachable": "2026-10-31T00:00:00Z", "last_sync": "2026-11-01T09:30:15.750Z"}"#,
        ));
        assert_eq!(
            report.last_sync,
            Some("2026-11-01T09:30:15Z".parse().expect("a time"))
        );
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn a_state_file_without_the_key_has_no_last_sync() {
        let report = last_sync_of(Some(r#"{"last_reachable": "2026-10-31T00:00:00Z"}"#));
        assert_eq!(report.last_sync, None);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn a_state_file_that_cannot_be_read_gives_a_warning_and_no_failure() {
        let report = last_sync_of(Some("{not json"));
        assert_eq!(report.last_sync, None);
        assert!(
            report.warnings[0].message.contains("remote-state.json"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_state_file_that_is_a_directory_gives_a_warning() {
        let world = world();
        fs::create_dir(world.collection.remote_state_path()).expect("a directory");
        let report = world.report();
        assert_eq!(report.last_sync, None);
        assert!(report.warnings[0].message.contains("remote-state.json"));
    }

    #[test]
    fn the_away_warning_is_asked_with_pending_work() {
        let mut world = world();
        world.file("work/note.txt", 1);
        let mut entry = world.live_entry_of("work", false);
        entry.stale_backup_since = Some(when());
        world.write_entry(&entry);
        let away = Away(Default::default());
        let report = run_with(&world.collection, &away).expect("runs");
        assert_eq!(*away.0.borrow(), [(RemoteState::Away, true)]);
        assert_eq!(report.warnings[0].message, "the remote has been away");
    }

    #[test]
    fn the_away_warning_is_asked_without_pending_work() {
        let mut world = world();
        world.file("work/note.txt", 1);
        let mut entry = world.live_entry_of("work", false);
        entry.missing_on_remote = true;
        world.write_entry(&entry);
        let away = Away(Default::default());
        run_with(&world.collection, &away).expect("runs");
        assert_eq!(*away.0.borrow(), [(RemoteState::Away, false)]);
    }

    #[test]
    fn a_pending_upload_is_pending_remote_work() {
        let mut world = world();
        world.archived_piece(|_, _| {});
        let away = Away(Default::default());
        run_with(&world.collection, &away).expect("runs");
        assert_eq!(*away.0.borrow(), [(RemoteState::Away, true)]);
    }

    #[test]
    fn every_unknown_collection_key_is_a_warning_in_alphabetical_order() {
        let mut world = world();
        world.configure("idle_dyas = 3\nbogus = 1");
        let report = world.report();
        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
        assert!(report.warnings[0].message.contains("bogus"));
        assert!(report.warnings[1].message.contains("idle_dyas"));
    }

    #[test]
    fn a_collection_on_smb_or_nfs_gets_a_warning() {
        let world = world();
        let smb = run_with(&world.collection, &Mounted(Some(FsKind::Smb))).expect("runs");
        assert!(
            smb.warnings[0].message.contains("SMB"),
            "{:?}",
            smb.warnings
        );
    }

    #[test]
    fn a_collection_on_nfs_gets_a_warning() {
        let world = world();
        let nfs = run_with(&world.collection, &Mounted(Some(FsKind::Nfs))).expect("runs");
        assert!(
            nfs.warnings[0].message.contains("NFS"),
            "{:?}",
            nfs.warnings
        );
    }

    #[test]
    fn a_local_filesystem_gets_no_warning() {
        let world = world();
        let local = run_with(&world.collection, &Mounted(Some(FsKind::Local))).expect("runs");
        let other = run_with(&world.collection, &Mounted(Some(FsKind::Other))).expect("runs");
        assert!(local.warnings.is_empty());
        assert!(other.warnings.is_empty());
    }

    #[test]
    fn a_filesystem_that_cannot_be_told_gives_a_warning_and_no_failure() {
        let world = world();
        let report = run_with(&world.collection, &Mounted(None)).expect("runs");
        assert!(
            report.warnings[0].message.contains("filesystem"),
            "{:?}",
            report.warnings
        );
    }

    fn lock_note(content: Option<&str>) -> Vec<Warning> {
        let world = world();
        if let Some(content) = content {
            fs::write(world.collection.lock_path(), content).expect("a lock file");
        }
        let before = fs::read(world.collection.lock_path()).ok();
        let warnings = world.report().warnings;
        // Reading the note does not touch the file or take the lock.
        assert_eq!(fs::read(world.collection.lock_path()).ok(), before);
        if content.is_some() {
            let clock = clock();
            crate::lock::acquire(&world.collection.lock_path(), "laptop", &clock)
                .expect("status took no lock");
        }
        warnings
    }

    fn holder_json(pid: u32) -> String {
        format!(r#"{{"pid":{pid},"machine":"desktop","started":"2026-11-01T11:00:00Z"}}"#)
    }

    #[test]
    fn a_run_that_holds_the_lock_is_named_in_a_note() {
        let warnings = lock_note(Some(&holder_json(std::process::id())));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let message = &warnings[0].message;
        assert!(
            message.contains(&std::process::id().to_string())
                && message.contains("desktop")
                && message.contains("2026-11-01T11:00:00Z"),
            "{message}"
        );
    }

    #[test]
    fn an_empty_lock_file_has_no_note() {
        assert!(lock_note(Some("")).is_empty());
    }

    #[test]
    fn a_lock_file_naming_a_dead_process_has_no_note() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("the true executable runs");
        let pid = child.id();
        child.wait().expect("the child ends");
        assert!(lock_note(Some(&holder_json(pid))).is_empty());
    }

    #[test]
    fn an_unparsable_lock_file_has_no_note() {
        assert!(lock_note(Some("not json")).is_empty());
    }

    #[test]
    fn a_missing_lock_file_has_no_note() {
        assert!(lock_note(None).is_empty());
    }

    #[test]
    fn a_lock_file_that_cannot_be_read_gives_a_warning_and_no_failure() {
        let world = world();
        fs::create_dir(world.collection.lock_path()).expect("a directory");
        let report = world.report();
        assert!(
            report.warnings[0].message.contains("lock"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_version_above_the_one_in_use_that_cannot_be_read_is_reported() {
        let mut world = world();
        world.dir("old");
        world.put_meta(&metadata(
            1,
            "old",
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive_payload()],
        ));
        fs::write(
            world.collection.object_dir(id(1)).join("meta.2"),
            "not json",
        )
        .expect("a file");
        let report = world.report();
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        let message = &report.warnings[0].message;
        assert!(
            message.contains(&id(1).to_string()) && message.contains("meta.2"),
            "{message}"
        );
        assert!(
            report
                .pieces
                .iter()
                .any(|p| p.state == PieceKind::RemoteOnly)
        );
    }

    #[test]
    fn every_unreadable_directory_gets_a_warning() {
        let mut world = world();
        world.dir("group/work");
        world.configure("patterns = [\"group/*\"]");
        let blocked = world.temp.root().join("group");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).expect("a mode");
        assert!(
            fs::read_dir(&blocked).is_err(),
            "the test needs a directory that cannot be read; it must not run as root"
        );
        let report = world.report();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).expect("a mode");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].message.contains("group"));
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
        world.temp.tree().file("scratch/.antiq-pin", "");
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
                pinned: 1,
                empty: 2
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
