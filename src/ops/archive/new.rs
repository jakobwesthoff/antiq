// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archiving new and live pieces.
//!
//! One piece goes through these steps, and every step that leaves
//! something behind is ordered so that a process that dies between two of
//! them leaves a state the start-of-run repairs or the next run can finish
//! (spec "archive", "States"):
//!
//! 0. walk the piece and judge it idle again;
//! 1. adopt what a crash left behind, or mint an ID;
//! 2. pack into `tmp/`;
//! 3. verify the package against the folder;
//! 4. store payload and listing;
//! 5. record the metadata change as `meta.pending`;
//! 6. write the tracking entry `live`, taken out at `pending`;
//! 7. set the entry to `removing`, naming the payload;
//! 8. rename the folder into the trash;
//! 9. walk the trashed tree again and compare it with the verified folder;
//! 10. delete the tree and set the entry to `gone`, or give a different tree
//!     back (see `finish`).

mod finish;

use super::durable::{self, Matched};
use super::{
    ArchiveError, Archived, Candidate, Kept, Seams, SkipReason, Skipped, Step, apparent_size,
    size_text,
};
use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::fsx::tree::TreeRoot;
use crate::id::{EntryTimes, IdSource, IdTime, ObjectId, id_time};
use crate::idle::{self, IdleError, IdleVerdict};
use crate::listing;
use crate::lock::RunLock;
use crate::meta::{
    Checksum, CreatedSource, FileName, HistoryAction, HistoryEntry, Lifecycle, Metadata, Payload,
    PayloadKind,
};
use crate::pack::names::{MemberKind, MemberName, portability_warnings};
use crate::pack::pool::{
    ARCHIVE_FIXED_BYTES, ARCHIVE_JOB_BYTES, ARCHIVE_WORKER_BYTES, Pool, workers_for_piece,
};
use crate::pack::tar_write::sparse_warning;
use crate::pack::zstd::ZstdSettings;
use crate::pack::{PackError, PackSettings, Packed, WalkRules};
use crate::relpath::{Name, RelPath, name_part};
use crate::report::Warning;
use crate::select::Rules;
use crate::store::{Put, Source, Store};
use crate::tracking::{self, EntryState, LocalPayload, TrackingEntry, VersionTakenOut};
use crate::tree::{EntryKind, PieceTree, Problem};
use crate::walk;
use jiff::Timestamp;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// What one run shares between its pieces.
pub(super) struct Context<'a> {
    pub collection: &'a Collection,
    pub lock: &'a RunLock,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdSource,
    pub pool: &'a Pool,
    pub seams: &'a dyn Seams,
    pub junk: Vec<Name>,
    pub rules: Rules,
    pub idle: idle::Settings,
    pub root: TreeRoot,
    pub machine: String,
    /// Objects a crashed run left behind, for new pieces to adopt.
    pub orphans: Vec<Orphan>,
}

/// An object with archived metadata of this collection and no tracking
/// entry: what a run that died between the metadata and the entry leaves.
pub(super) struct Orphan {
    id: ObjectId,
    metadata: Metadata,
    source: Source,
}

/// The objects that no entry tracks and no damaged entry may track, newest
/// first.
pub(super) fn find_orphans(collection: &Collection) -> Result<Vec<Orphan>, ArchiveError> {
    let tracked = tracking::read_all(collection)?;
    let known: HashSet<ObjectId> = tracked
        .entries
        .iter()
        .map(|entry| entry.id)
        .chain(tracked.damaged.iter().map(|damaged| damaged.id))
        .collect();
    let store = Store::new(collection);
    let mut orphans = Vec::new();
    for id in store.objects()? {
        if known.contains(&id) {
            continue;
        }
        // An object whose metadata cannot be read is for `status` to list.
        let Ok(Some(current)) = store.current(id) else {
            continue;
        };
        if current.metadata.collection == collection.id()
            && current.metadata.lifecycle == Lifecycle::Archived
        {
            orphans.push(Orphan {
                id,
                metadata: current.metadata,
                source: current.source,
            });
        }
    }
    orphans.sort_by_key(|orphan| std::cmp::Reverse((orphan.metadata.created, orphan.id)));
    Ok(orphans)
}

/// How archiving one piece ended.
pub(super) enum PieceOutcome {
    /// The piece is not idle any more; there is nothing to report.
    Dropped,
    Skipped(Skipped),
    Archived(Archived),
    Kept(Kept),
}

pub(super) struct PieceResult {
    pub outcome: PieceOutcome,
    pub warnings: Vec<Warning>,
    /// A listing reached the object store, so the index has to learn of it.
    pub listing_stored: bool,
}

/// Why a piece stops before its end.
enum Stop {
    Dropped,
    Skip {
        reason: SkipReason,
        message: String,
    },
    /// Only what no piece can go on after: out of file descriptors.
    Fail(ArchiveError),
}

type Flow<T> = Result<T, Stop>;

impl Stop {
    fn error(message: impl Into<String>) -> Stop {
        Stop::Skip {
            reason: SkipReason::Error,
            message: message.into(),
        }
    }

    /// A failed call: out of space is told apart, as it frees the space the
    /// next piece can use.
    fn from_error(error: &(dyn Error + 'static)) -> Stop {
        let reason = if is_out_of_space(error) {
            SkipReason::NoSpace
        } else {
            SkipReason::Error
        };
        Stop::Skip {
            reason,
            message: chain(error),
        }
    }

    fn from_pack(error: PackError) -> Stop {
        match error {
            PackError::Walk(error) => Stop::Fail(error.into()),
            PackError::Differs(_) => Stop::Skip {
                reason: SkipReason::Differs,
                message: chain(&error),
            },
            other => Stop::from_error(&other),
        }
    }
}

/// The error and its causes on one line.
fn chain(error: &(dyn Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Whether the error or one of its causes is the OS saying the volume is
/// full.
fn is_out_of_space(error: &(dyn Error + 'static)) -> bool {
    let mut current: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(error) = current {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(libc::ENOSPC))
        {
            return true;
        }
        current = error.source();
    }
    false
}

pub(super) fn archive_piece(
    cx: &Context<'_>,
    candidate: &Candidate,
) -> Result<PieceResult, ArchiveError> {
    let mut piece = Piece {
        cx,
        path: &candidate.path,
        known: candidate.id,
        warnings: Vec::new(),
        listing_stored: false,
    };
    let outcome = match piece.run() {
        Ok(outcome) => outcome,
        Err(Stop::Dropped) => PieceOutcome::Dropped,
        Err(Stop::Skip { reason, message }) => PieceOutcome::Skipped(Skipped {
            id: candidate.id,
            path: candidate.path.clone(),
            reason,
            message,
        }),
        Err(Stop::Fail(error)) => return Err(error),
    };
    Ok(PieceResult {
        outcome,
        warnings: piece.warnings,
        listing_stored: piece.listing_stored,
    })
}

/// The payload and listing of a piece in the object store, and the
/// metadata that names them.
struct Stored {
    id: ObjectId,
    payload: Payload,
    source: Source,
    /// The verify of the stored package against the folder.
    matched: Matched,
}

struct Piece<'a, 'c> {
    cx: &'a Context<'c>,
    path: &'a RelPath,
    /// The ID the piece had before the run.
    known: Option<ObjectId>,
    warnings: Vec<Warning>,
    listing_stored: bool,
}

impl Piece<'_, '_> {
    fn at(&self, step: Step) -> Flow<()> {
        self.cx
            .seams
            .at(step, self.path)
            .map_err(|error| Stop::from_error(&error))
    }

    fn store(&self) -> Store<'_> {
        Store::new(self.cx.collection)
    }

    fn run(&mut self) -> Flow<PieceOutcome> {
        let tree = self.walk_and_judge()?;
        let size = apparent_size(&tree);
        let sub = self
            .cx
            .root
            .sub(self.path)
            .map_err(|error| Stop::from_error(&error))?;
        self.warnings.extend(tree_warnings(&tree));
        let stored = match self.adopt(&sub)? {
            Some(stored) => stored,
            None => self.pack_and_store(&tree, &sub, size)?,
        };
        let mut entry = self.write_entry(&stored)?;
        self.mark_removing(&mut entry, &stored)?;
        let durable = durable::seal(self.cx.lock, stored.matched, &stored.payload, &entry)
            .expect("the entry was just marked removing for this payload");
        Ok(match self.remove_folder(&durable, entry)? {
            finish::Finished::Removed => PieceOutcome::Archived(Archived {
                id: durable.id(),
                path: self.path.clone(),
                size,
                archive_size: stored.payload.size,
                payload_version: durable.payload_version(),
            }),
            finish::Finished::Kept(kept) => PieceOutcome::Kept(kept),
        })
    }

    // =====================================================
    // Step 0: the piece is judged again
    // =====================================================

    /// The piece may have changed since `status` judged it: it is walked
    /// with the rules of the run and must still be idle.
    fn walk_and_judge(&self) -> Flow<PieceTree> {
        self.at(Step::Walk)?;
        let cx = self.cx;
        let tree = walk::walk_piece_with(
            &cx.root,
            self.path,
            &cx.junk,
            &cx.rules,
            cx.seams.walk_hooks(),
        )
        .map_err(|error| Stop::Fail(error.into()))?;
        match idle::check(&cx.root, &tree, None, &cx.idle, cx.clock.now()) {
            Ok(check) => match check.verdict {
                IdleVerdict::Idle { .. } => Ok(tree),
                _ => Err(Stop::Dropped),
            },
            Err(IdleError::Problems) => Err(self.refused_by(&tree)),
            Err(IdleError::IgnoreFile { path, source }) if walk::is_out_of_descriptors(&source) => {
                Err(Stop::Fail(ArchiveError::Io {
                    action: "read",
                    path: path.to_string(),
                    source,
                }))
            }
            Err(error) => Err(Stop::from_error(&error)),
        }
    }

    /// The skip a walk problem gives; the first one names the reason.
    fn refused_by(&self, tree: &PieceTree) -> Stop {
        let first = tree
            .problems()
            .first()
            .expect("the idle check refuses a tree only when the walk found a problem");
        let (reason, what) = match first {
            Problem::Unreadable { first } => (
                SkipReason::Unreadable,
                format!("`{}` cannot be read", self.path.concat(first)),
            ),
            Problem::MountPoint { other } => (
                SkipReason::MountPoint,
                format!(
                    "another filesystem is mounted at `{}`",
                    self.path.concat(other)
                ),
            ),
            Problem::Excluded { path } => (
                SkipReason::Excluded,
                format!("`{}` is on the exclude list", self.path.concat(path)),
            ),
            Problem::NestedCollection { root } => (
                SkipReason::NestedCollection,
                format!("`{}` is a collection of its own", self.path.concat(root)),
            ),
        };
        Stop::Skip {
            reason,
            message: format!("{what} since it was last judged"),
        }
    }

    // =====================================================
    // Step 1: an object a crash left behind
    // =====================================================

    /// A new piece takes over the object of a run that died after the
    /// metadata, when the folder still equals that object's archive.
    /// Minting a second ID would leave two objects for one folder.
    fn adopt(&mut self, sub: &TreeRoot) -> Flow<Option<Stored>> {
        if self.known.is_some() {
            return Ok(None);
        }
        let cx = self.cx;
        for orphan in cx
            .orphans
            .iter()
            .filter(|orphan| orphan.metadata.path == *self.path)
        {
            let Some(payload) = orphan
                .metadata
                .payloads
                .iter()
                .filter(|payload| {
                    payload.kind == PayloadKind::Archive
                        && !payload.superseded
                        && payload.machine == cx.machine
                })
                .max_by_key(|payload| payload.version)
            else {
                continue;
            };
            if let Some(matched) = self.matches_folder(orphan.id, payload, sub) {
                self.listing_stored = true;
                return Ok(Some(Stored {
                    id: orphan.id,
                    payload: payload.clone(),
                    source: orphan.source,
                    matched,
                }));
            }
        }
        Ok(None)
    }

    /// The verify of the stored payload against the folder, when the
    /// payload is the one the metadata describes, its listing is what the
    /// payload holds, and the folder equals it.
    fn matches_folder(&self, id: ObjectId, payload: &Payload, sub: &TreeRoot) -> Option<Matched> {
        let store = self.store();
        let payload_path = store.file_path(id, &payload.file);
        let Some(listing_name) = &payload.listing else {
            return None;
        };
        if payload.checksum.algorithm != "sha256"
            || file_sha256(&payload_path).ok().as_deref() != Some(payload.checksum.value.as_str())
        {
            return None;
        }
        let stored = File::open(store.file_path(id, listing_name))
            .ok()
            .and_then(|file| listing::read_all(file).ok())?;
        let rules = WalkRules {
            junk: self.cx.junk.clone(),
            staged: false,
        };
        let verified = self
            .cx
            .seams
            .packager()
            .verify_strict(&payload_path, sub, &rules)
            .ok()?;
        Matched::of_stored(verified, &stored).ok()
    }

    // =====================================================
    // Steps 2 to 5: pack, verify, store, record
    // =====================================================

    fn pack_and_store(&mut self, tree: &PieceTree, sub: &TreeRoot, apparent: u64) -> Flow<Stored> {
        let cx = self.cx;
        let (id, created) = match self.known {
            Some(id) => (id, None),
            None => {
                let found = id_time_of(tree, cx.clock.now());
                self.warnings.extend(found.warning.clone());
                (cx.ids.mint(found.time), Some(found))
            }
        };

        let tmp_dir = cx.collection.tmp_dir();
        self.at(Step::Pack)?;
        let free = cx
            .seams
            .free_space(self.path, &tmp_dir)
            .map_err(|error| Stop::from_error(&error))?;
        if apparent > free {
            return Err(Stop::Skip {
                reason: SkipReason::NoSpace,
                message: format!(
                    "the piece is {} and {} are free",
                    size_text(apparent),
                    size_text(free)
                ),
            });
        }
        let tmp = tmp_dir.join(format!("{id}.tar.zst"));
        let packed = self.pack(tree, sub, apparent, &tmp)?;

        let outcome = self.verify_and_store(id, &tmp, &packed, sub);
        if outcome.is_err() {
            remove_quietly(&tmp);
        }
        let (payload_name, listing_name, placed, matched) = outcome?;

        let name = piece_name(self.path)?;
        let basis = match (&created, self.store().current(id)) {
            (Some(found), _) => Basis::New {
                created: whole_seconds(found.time),
                source: found.source,
            },
            (None, Ok(Some(current))) => Basis::Live(Box::new(current.metadata)),
            (None, Ok(None)) => {
                discard(&placed);
                return Err(Stop::error(format!("the object {id} has no metadata")));
            }
            (None, Err(error)) => {
                discard(&placed);
                return Err(Stop::from_error(&error));
            }
        };
        let payload = Payload {
            kind: PayloadKind::Archive,
            version: match &basis {
                Basis::New { .. } => 1,
                Basis::Live(metadata) => next_archive_version(metadata),
            },
            file: payload_name,
            package_format: cx.seams.packager().format().as_str().to_string(),
            size: packed.size,
            checksum: Checksum::sha256(packed.sha256.clone()),
            listing: Some(listing_name),
            machine: cx.machine.clone(),
            created: whole_seconds(cx.clock.now()),
            verified: None,
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        };
        let metadata = archived_metadata(id, self.path, name, basis, payload.clone(), cx);
        self.record(&metadata, &placed)?;
        Ok(Stored {
            id,
            payload,
            source: Source::Pending,
            matched,
        })
    }

    /// Step 2: the package is written to `tmp/` and hashed again from the
    /// disk.
    fn pack(
        &mut self,
        tree: &PieceTree,
        sub: &TreeRoot,
        apparent: u64,
        tmp: &Path,
    ) -> Flow<Packed> {
        let cx = self.cx;
        let lease = cx.pool.lease(
            workers_for_piece(apparent, ARCHIVE_JOB_BYTES),
            ARCHIVE_FIXED_BYTES,
            ARCHIVE_WORKER_BYTES,
        );
        if let Some(warning) = lease.warning() {
            self.warnings.push(warning.clone());
        }
        let settings = PackSettings {
            zstd: ZstdSettings::archive(
                u32::try_from(lease.workers()).expect("a worker count fits 32 bits"),
            ),
        };
        let written = write_package(cx.seams, sub, tree, &settings, tmp);
        drop(lease);

        let packed = written.inspect_err(|_| remove_quietly(tmp))?;
        self.warnings.extend(
            packed
                .notes
                .iter()
                .map(|note| Warning::new(note.to_string())),
        );
        match file_sha256(tmp) {
            Ok(found) if found == packed.sha256 => Ok(packed),
            Ok(_) => {
                remove_quietly(tmp);
                Err(Stop::error(
                    "the hash of the package file differs from the one computed while writing it",
                ))
            }
            Err(error) => {
                remove_quietly(tmp);
                Err(Stop::from_error(&error))
            }
        }
    }

    /// Steps 3 and 4. The files this run put in the store come back, so a
    /// later failure can take them out again.
    fn verify_and_store(
        &mut self,
        id: ObjectId,
        tmp: &Path,
        packed: &Packed,
        sub: &TreeRoot,
    ) -> Flow<(FileName, FileName, Vec<PathBuf>, Matched)> {
        let cx = self.cx;
        self.at(Step::Verify)?;
        let rules = WalkRules {
            junk: cx.junk.clone(),
            staged: false,
        };
        let verified = cx
            .seams
            .packager()
            .verify_strict(tmp, sub, &rules)
            .map_err(Stop::from_pack)?;
        let matched = Matched::of_packing(verified, packed)
            .map_err(|error| Stop::error(error.to_string()))?;

        let payload_name = FileName::new(format!(
            "{}-{}.tar.zst",
            &packed.sha256[..16],
            name_part(&piece_name(self.path)?)
        ))
        .expect("a hash prefix and a name part make a plain file name");
        let listing_name =
            listing::file_name(&packed.sha256).expect("a SHA-256 makes a listing name");
        let store = self.store();
        let mut placed = Vec::new();

        self.at(Step::StorePayload)?;
        match store.put_file(cx.lock, id, &payload_name, tmp, &packed.sha256) {
            Ok(Put::Stored) => placed.push(store.file_path(id, &payload_name)),
            Ok(Put::AlreadyPresent) => remove_quietly(tmp),
            Err(error) => return Err(Stop::from_error(&error)),
        }

        let stored = self.store_listing(id, &listing_name, matched.listing(), &mut placed);
        if let Err(stop) = stored {
            discard(&placed);
            return Err(stop);
        }
        Ok((payload_name, listing_name, placed, matched))
    }

    fn store_listing(
        &mut self,
        id: ObjectId,
        name: &FileName,
        entries: &[listing::ListingEntry],
        placed: &mut Vec<PathBuf>,
    ) -> Flow<()> {
        let cx = self.cx;
        self.at(Step::StoreListing)?;
        let tmp = cx.collection.tmp_dir().join(format!("{id}.listing.zst"));
        let written = File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|error| Stop::from_error(&error))
            .and_then(|file| {
                let written = listing::write(entries.iter(), &file)
                    .map_err(|error| Stop::from_error(&error))?;
                file.sync_all().map_err(|error| Stop::from_error(&error))?;
                Ok(written)
            })
            .inspect_err(|_| remove_quietly(&tmp))?;
        let store = self.store();
        match store.put_file(cx.lock, id, name, &tmp, &written.sha256) {
            Ok(Put::Stored) => placed.push(store.file_path(id, name)),
            Ok(Put::AlreadyPresent) => remove_quietly(&tmp),
            Err(error) => {
                remove_quietly(&tmp);
                return Err(Stop::from_error(&error));
            }
        }
        self.listing_stored = true;
        Ok(())
    }

    /// Step 5. A write that failed after the document reached the disk
    /// leaves everything in place for the repairs; otherwise the files of
    /// this run go.
    fn record(&self, metadata: &Metadata, placed: &[PathBuf]) -> Flow<()> {
        let outcome = self.at(Step::WriteMetadata).and_then(|()| {
            self.store()
                .write_pending(self.cx.lock, metadata)
                .map_err(|error| Stop::from_error(&error))
        });
        if outcome.is_err() && !is_recorded(&self.store(), metadata) {
            discard(placed);
        }
        outcome
    }

    // =====================================================
    // Steps 6 and 7: the tracking entry
    // =====================================================

    fn write_entry(&self, stored: &Stored) -> Flow<TrackingEntry> {
        let cx = self.cx;
        self.at(Step::WriteEntry)?;
        let local = LocalPayload {
            kind: PayloadKind::Archive,
            version: stored.payload.version,
            file: stored.payload.file.clone(),
        };
        let mut entry = match self.known {
            Some(id) => tracking::read(cx.collection, id)
                .map_err(|error| Stop::from_error(&error))?
                .ok_or_else(|| Stop::error(format!("{id} has no tracking entry")))?,
            None => TrackingEntry::new(
                stored.id,
                self.path.clone(),
                EntryState::Live,
                VersionTakenOut::Pending,
                cx.clock.now(),
            ),
        };
        entry.version_taken_out = match stored.source {
            Source::Pending => VersionTakenOut::Pending,
            Source::Committed(number) => VersionTakenOut::Committed(number),
        };
        entry.local_payloads.push(local);
        tracking::write(cx.collection, cx.lock, cx.clock, &mut entry)
            .map_err(|error| Stop::from_error(&error))?;
        Ok(entry)
    }

    fn mark_removing(&self, entry: &mut TrackingEntry, stored: &Stored) -> Flow<()> {
        let cx = self.cx;
        // The folder moves to this name in the next step, which refuses to
        // replace anything. Finding something here now means the refusal
        // could only be about this leftover.
        let trash = cx.collection.trash_path(stored.id);
        match fs::symlink_metadata(&trash) {
            Ok(_) => {
                return Err(Stop::error(format!(
                    "{} is in the way of the trash",
                    trash.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Stop::from_error(&error)),
        }
        self.at(Step::MarkRemoving)?;
        entry.state = EntryState::Removing;
        entry.removing_payload = Some(stored.payload.file.clone());
        tracking::write(cx.collection, cx.lock, cx.clock, entry)
            .map_err(|error| Stop::from_error(&error))
    }
}

// =========================================================
// Helpers
// =========================================================

/// Whether `meta.pending` holds exactly `metadata`.
fn is_recorded(store: &Store<'_>, metadata: &Metadata) -> bool {
    matches!(
        store.current(metadata.id),
        Ok(Some(current)) if current.source == Source::Pending && current.metadata == *metadata
    )
}

/// Where a new document for the object comes from.
enum Basis {
    New {
        created: Timestamp,
        source: CreatedSource,
    },
    /// The object's metadata as it is, which gains the archive.
    Live(Box<Metadata>),
}

fn piece_name(path: &RelPath) -> Flow<Name> {
    let last = path
        .components()
        .last()
        .expect("a piece path is not the root");
    Name::from_bytes(last.to_vec()).map_err(|error| Stop::from_error(&error))
}

/// The metadata with the archive payload added: the live piece's own
/// document, or a new one for a new piece.
fn archived_metadata(
    id: ObjectId,
    path: &RelPath,
    name: Name,
    basis: Basis,
    payload: Payload,
    cx: &Context<'_>,
) -> Metadata {
    let now = whole_seconds(cx.clock.now());
    let history = HistoryEntry::new(now, cx.machine.clone(), HistoryAction::Archive);
    match basis {
        Basis::New { created, source } => Metadata {
            format: crate::meta::FORMAT,
            id,
            collection: cx.collection.id(),
            path: path.clone(),
            name,
            created,
            created_source: source,
            lifecycle: Lifecycle::Archived,
            payloads: vec![payload],
            history: vec![history],
            extra: serde_json::Map::new(),
        },
        Basis::Live(mut metadata) => {
            metadata.lifecycle = Lifecycle::Archived;
            metadata.payloads.push(payload);
            metadata.history.push(history);
            *metadata
        }
    }
}

/// The archive version after the highest one the metadata has.
fn next_archive_version(metadata: &Metadata) -> u32 {
    metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Archive)
        .map(|payload| payload.version)
        .max()
        .unwrap_or(0)
        + 1
}

/// Packs into a new file at `tmp` and makes it durable. The file stays when
/// this fails; the caller removes it.
fn write_package(
    seams: &dyn Seams,
    sub: &TreeRoot,
    tree: &PieceTree,
    settings: &PackSettings,
    tmp: &Path,
) -> Flow<Packed> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|error| Stop::from_error(&error))?;
    let packed = seams
        .packager()
        .pack(sub, tree, settings, &mut file)
        .map_err(Stop::from_pack)?;
    file.flush().map_err(|error| Stop::from_error(&error))?;
    file.sync_all().map_err(|error| Stop::from_error(&error))?;
    Ok(packed)
}

fn remove_quietly(path: &Path) {
    // Whatever stays is a leftover the start-of-run repairs clear.
    let _ = fs::remove_file(path);
}

fn discard(files: &[PathBuf]) {
    for file in files {
        remove_quietly(file);
    }
}

fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        match file.read(&mut chunk)? {
            0 => return Ok(crate::pack::hashing::hex(&hasher.finalize())),
            read => hasher.update(&chunk[..read]),
        }
    }
}

/// The time an object's ID is minted for: the oldest time inside the
/// folder.
fn id_time_of(tree: &PieceTree, minted_at: Timestamp) -> IdTime {
    // A time a `Timestamp` cannot hold counts as absent. The epoch stands
    // in for a modification time, since `id_time` ignores everything
    // before 1990.
    let instant = |ns: i128| Timestamp::from_nanosecond(ns).ok();
    let top = tree
        .entries()
        .iter()
        .find(|entry| entry.path.is_root())
        .and_then(|entry| entry.birth_ns)
        .and_then(instant);
    let inside = tree
        .entries()
        .iter()
        .filter(|entry| !entry.path.is_root())
        .map(|entry| EntryTimes {
            birth: entry.birth_ns.and_then(instant),
            modified: instant(entry.mtime_ns).unwrap_or(Timestamp::UNIX_EPOCH),
        });
    id_time(tree.top(), top, inside, minted_at)
}

/// What the name rules of macOS and Linux and the size of the piece say
/// about it.
fn tree_warnings(tree: &PieceTree) -> Vec<Warning> {
    let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
    let members: Vec<MemberName> = tree
        .entries()
        .iter()
        .filter(|entry| !entry.path.is_root())
        .filter_map(|entry| {
            let kind = match entry.kind {
                EntryKind::File if entry.nlink > 1 => {
                    if seen.insert((entry.device, entry.inode), ()).is_some() {
                        MemberKind::HardLink
                    } else {
                        MemberKind::Plain
                    }
                }
                EntryKind::File | EntryKind::Directory => MemberKind::Plain,
                EntryKind::Symlink => MemberKind::Symlink {
                    target_len: entry.link_target.as_ref().map_or(0, Vec::len),
                },
                _ => return None,
            };
            Some(MemberName {
                path: entry.path.clone(),
                kind,
            })
        })
        .collect();
    let top = tree.top();
    let mut warnings: Vec<Warning> = portability_warnings(&members)
        .into_iter()
        .map(|warning| Warning::new(format!("in `{top}`, {warning}")))
        .collect();
    if let Some(sparse) = sparse_warning(tree) {
        warnings.push(Warning::new(format!(
            "`{top}` is {} but takes {} on disk; sparse files are stored in full",
            size_text(sparse.apparent),
            size_text(sparse.allocated)
        )));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{ObjectId, SeqIds};
    use crate::listing::{self, ListingType};
    use crate::meta::{CreatedSource, Lifecycle, PayloadKind};
    use crate::ops::archive::tests::{NOW, World, clock, days_ago, live_piece, world};
    use crate::ops::archive::{ArchiveReport, ArchiveRequest, KeptHow, Step, archive_with};
    use crate::pack::verify::LstatRecord;
    use crate::pack::{PackError, PackSettings, Packager, Packed, TarZstd};
    use crate::relpath::RelPath;
    use crate::repair::{Effects, RealEffects};
    use crate::store::{Source, Store};
    use crate::testing::shared_pool;
    use crate::tracking::{self, EntryState, LocalPayload, TrackingEntry, VersionTakenOut};
    use crate::tree::{EntryKind, TreeEntry};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // =========================================================
    // Seams and helpers
    // =========================================================

    type Hook = Box<dyn Fn(Step, &RelPath) -> io::Result<()> + Send + Sync>;

    /// Sees a second device at `mnt` once the first flag is set, and has no
    /// file descriptor left once the second one is.
    struct ArmedMount(Arc<AtomicBool>, Arc<AtomicBool>);

    impl crate::walk::Hooks for ArmedMount {
        fn device(&self, path: &RelPath, reported: u64) -> u64 {
            if self.0.load(Ordering::SeqCst) && path.to_string() == "mnt" {
                reported + 1
            } else {
                reported
            }
        }

        fn fault(&self, _: &RelPath, call: crate::walk::Call) -> Option<io::Error> {
            (self.1.load(Ordering::SeqCst) && call == crate::walk::Call::Open)
                .then(|| io::Error::from_raw_os_error(libc::EMFILE))
        }
    }

    struct Scripted {
        on: Hook,
        free: Option<u64>,
        packager: Option<Box<dyn Packager + Sync>>,
        effects: Option<Box<dyn Effects + Sync>>,
        mount: ArmedMount,
    }

    impl Scripted {
        fn real() -> Scripted {
            Scripted::on(|_, _| Ok(()))
        }

        fn on(hook: impl Fn(Step, &RelPath) -> io::Result<()> + Send + Sync + 'static) -> Scripted {
            Scripted {
                on: Box::new(hook),
                free: None,
                packager: None,
                effects: None,
                mount: ArmedMount(
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                ),
            }
        }

        /// Runs `action` right before `step`.
        fn before(step: Step, action: impl Fn() + Send + Sync + 'static) -> Scripted {
            Scripted::on(move |at, _| {
                if at == step {
                    action();
                }
                Ok(())
            })
        }

        /// Calls `hook` with the collection root before each step and uses
        /// `effects` for the changes to the trash and the folder.
        fn with_effects(
            hook: impl Fn(Step, &Path) + Send + Sync + 'static,
            root: &Path,
            effects: impl Effects + Sync + 'static,
        ) -> Scripted {
            let root = root.to_path_buf();
            let mut seams = Scripted::on(move |step, _| {
                hook(step, &root);
                Ok(())
            });
            seams.effects = Some(Box::new(effects));
            seams
        }

        /// Also fails `step` with the OS error `errno`.
        fn failing_at(mut self, step: Step, errno: i32) -> Scripted {
            let before = self.on;
            self.on = Box::new(move |at, piece| {
                before(at, piece)?;
                if at == step {
                    Err(io::Error::from_raw_os_error(errno))
                } else {
                    Ok(())
                }
            });
            self
        }

        /// Fails `step` with the OS error `errno`.
        fn failing(step: Step, errno: i32) -> Scripted {
            Scripted::on(move |at, _| {
                if at == step {
                    Err(io::Error::from_raw_os_error(errno))
                } else {
                    Ok(())
                }
            })
        }
    }

    impl Seams for Scripted {
        fn walk_hooks(&self) -> &(dyn crate::walk::Hooks + Sync) {
            &self.mount
        }

        fn free_space(
            &self,
            _piece: &RelPath,
            dir: &std::path::Path,
        ) -> Result<u64, crate::fsx::FsxError> {
            match self.free {
                Some(free) => Ok(free),
                None => crate::fsx::space::available_space(dir),
            }
        }

        fn packager(&self) -> &(dyn Packager + Sync) {
            match &self.packager {
                Some(packager) => packager.as_ref(),
                None => &TarZstd,
            }
        }

        fn effects(&self) -> &dyn Effects {
            match &self.effects {
                Some(effects) => effects.as_ref(),
                None => &RealEffects,
            }
        }

        fn at(&self, step: Step, piece: &RelPath) -> io::Result<()> {
            (self.on)(step, piece)
        }
    }

    /// A packager that does what `TarZstd` does and then lets `tamper`
    /// change the outcome.
    type Tamper = Box<dyn Fn(&mut Packed, &mut dyn Write) -> Result<(), PackError> + Sync + Send>;

    struct Tampering {
        tamper: Tamper,
    }

    impl Packager for Tampering {
        fn format(&self) -> crate::pack::PackageFormat {
            TarZstd.format()
        }

        fn pack(
            &self,
            root: &crate::fsx::tree::TreeRoot,
            tree: &PieceTree,
            settings: &PackSettings,
            out: &mut dyn Write,
        ) -> Result<Packed, PackError> {
            let mut packed = TarZstd.pack(root, tree, settings, out)?;
            (self.tamper)(&mut packed, out)?;
            Ok(packed)
        }

        fn verify_strict(
            &self,
            package: &std::path::Path,
            root: &crate::fsx::tree::TreeRoot,
            rules: &crate::pack::WalkRules,
        ) -> Result<crate::pack::Verified, PackError> {
            TarZstd.verify_strict(package, root, rules)
        }

        fn list(&self, package: &std::path::Path) -> Result<Vec<listing::ListingEntry>, PackError> {
            TarZstd.list(package)
        }

        fn unpack(
            &self,
            package: &std::path::Path,
            staging: &crate::fsx::tree::TreeRoot,
            rules: &crate::pack::TargetRules,
        ) -> Result<crate::pack::Unpacked, PackError> {
            TarZstd.unpack(package, staging, rules)
        }
    }

    fn tampering(
        tamper: impl Fn(&mut Packed, &mut dyn Write) -> Result<(), PackError> + Sync + Send + 'static,
    ) -> Scripted {
        let mut seams = Scripted::real();
        seams.packager = Some(Box::new(Tampering {
            tamper: Box::new(tamper),
        }));
        seams
    }

    fn run_from(world: &World, seams: &dyn Seams, first_id: u64) -> ArchiveReport {
        let ids = SeqIds::new(first_id);
        archive_with(
            &ArchiveRequest {
                collection: &world.collection,
                clock: &clock(),
                ids: &ids,
                pool: shared_pool(),
                dry_run: false,
            },
            seams,
        )
        .expect("the run finishes")
    }

    fn run(world: &World, seams: &dyn Seams) -> ArchiveReport {
        run_from(world, seams, 1)
    }

    /// Runs with a seam that panics before `step`, as a process that dies
    /// there, and checks that it did.
    fn crash_before(world: &World, step: Step, first_id: u64) {
        let fired = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&fired);
        let seams = Scripted::on(move |at, _| {
            if at == step {
                seen.store(true, Ordering::SeqCst);
                panic!("the process dies before {at:?}");
            }
            Ok(())
        });
        let result = catch_unwind(AssertUnwindSafe(|| run_from(world, &seams, first_id)));
        assert!(result.is_err(), "the run was cut short");
        assert!(
            fired.load(Ordering::SeqCst),
            "the seam fired before {step:?}"
        );
    }

    fn archived(report: &ArchiveReport) -> &[crate::ops::archive::Archived] {
        report
            .archived
            .as_deref()
            .expect("a real run lists what it archived")
    }

    fn skipped(report: &ArchiveReport) -> &[Skipped] {
        report
            .skipped
            .as_deref()
            .expect("a real run lists what it skipped")
    }

    fn store(world: &World) -> Store<'_> {
        Store::new(&world.collection)
    }

    fn objects(world: &World) -> Vec<ObjectId> {
        store(world).objects().expect("the store lists")
    }

    fn object_files(world: &World, id: ObjectId) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(world.collection.object_dir(id))
            .expect("the object directory exists")
            .map(|item| {
                item.expect("an item")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn entry(world: &World, id: ObjectId) -> Option<TrackingEntry> {
        tracking::read(&world.collection, id).expect("the entry reads")
    }

    fn pending(world: &World, id: ObjectId) -> Metadata {
        let current = store(world)
            .current(id)
            .expect("the metadata reads")
            .expect("the object has metadata");
        assert_eq!(current.source, Source::Pending);
        current.metadata
    }

    fn tmp_files(world: &World) -> Vec<String> {
        fs::read_dir(world.collection.tmp_dir())
            .expect("tmp exists")
            .map(|item| {
                item.expect("an item")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn sha256_of(path: &std::path::Path) -> String {
        crate::pack::hashing::hex(&Sha256::digest(fs::read(path).expect("the file reads")))
    }

    fn piece_path(world: &World, relative: &str) -> PathBuf {
        world.temp.root().join(relative)
    }

    /// Lays a file back with the time it had, as an editor that saves
    /// without anyone noticing would not, but a process that writes and
    /// resets the time does.
    fn rewrite(path: &std::path::Path, content: &str) {
        let before = fs::metadata(path).expect("the file exists");
        fs::write(path, content).expect("the file is written");
        let time = filetime::FileTime::from_last_modification_time(&before);
        filetime::set_file_mtime(path, time).expect("the time is set");
    }

    fn lone_piece(at: &str) -> World {
        let mut world = world();
        world.file(&format!("{at}/note.txt"), 30);
        world
    }

    fn tree_entry(path: &str, kind: EntryKind) -> TreeEntry {
        TreeEntry {
            path: RelPath::parse(path).expect("valid path"),
            kind,
            size: 0,
            allocated: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            inode: 1,
            device: 1,
            mode: 0o644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
            mount_root: false,
        }
    }

    fn tree_of(entries: Vec<TreeEntry>) -> PieceTree {
        PieceTree::new(
            RelPath::parse("projects/a").expect("valid path"),
            entries,
            vec![],
        )
    }

    // =========================================================
    // What a stored piece looks like
    // =========================================================

    #[test]
    fn an_idle_new_piece_is_stored_with_pending_metadata() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::real());

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_eq!(done.path.to_string(), "a");
        assert_eq!(done.size, 1);
        assert_eq!(done.payload_version, 1);
        assert!(skipped(&report).is_empty());

        let metadata = pending(&world, done.id);
        assert_eq!(metadata.format, 1);
        assert_eq!(metadata.collection, world.collection.id());
        assert_eq!(metadata.path.to_string(), "a");
        assert_eq!(metadata.name.as_bytes(), b"a");
        assert_eq!(metadata.lifecycle, Lifecycle::Archived);
        assert_eq!(metadata.created_source, CreatedSource::OldestMtime);
        assert_eq!(
            metadata.created,
            Timestamp::from_second(days_ago(30)).expect("a time")
        );
        let [payload] = metadata.payloads.as_slice() else {
            panic!("one payload: {metadata:?}");
        };
        let sha = payload.checksum.value.clone();
        assert_eq!(payload.kind, PayloadKind::Archive);
        assert_eq!(payload.version, 1);
        assert_eq!(payload.file.as_str(), format!("{}-a.tar.zst", &sha[..16]));
        assert_eq!(payload.package_format, "tar+zstd/1");
        assert_eq!(payload.checksum.algorithm, "sha256");
        assert_eq!(
            payload.listing,
            Some(listing::file_name(&sha).expect("a name"))
        );
        assert_eq!(
            payload.machine,
            world.collection.machine_name().expect("a name")
        );
        assert_eq!(payload.created, NOW.parse::<Timestamp>().expect("a time"));
        assert_eq!(payload.verified, None);
        assert!(!payload.superseded);
        let payload_path = store(&world).file_path(done.id, &payload.file);
        assert_eq!(
            payload.size,
            fs::metadata(&payload_path).expect("stored").len()
        );
        assert_eq!(done.archive_size, payload.size);
        assert_eq!(sha256_of(&payload_path), sha);
        let [history] = metadata.history.as_slice() else {
            panic!("one history entry: {metadata:?}");
        };
        assert_eq!(history.action, "archive");
        assert_eq!(history.machine, payload.machine);
        assert_eq!(history.time, payload.created);

        let listing_path =
            store(&world).file_path(done.id, payload.listing.as_ref().expect("a listing"));
        let stored = listing::read_all(fs::File::open(listing_path).expect("stored"))
            .expect("the listing reads");
        assert_eq!(
            TarZstd.list(&payload_path).expect("the payload lists"),
            stored
        );
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].kind, ListingType::File);

        let tracked = entry(&world, done.id).expect("an entry");
        assert_eq!(tracked.state, EntryState::Gone);
        assert_eq!(tracked.removing_payload, None);
        assert_eq!(tracked.version_taken_out, VersionTakenOut::Pending);
        assert_eq!(
            tracked.local_payloads,
            [LocalPayload {
                kind: PayloadKind::Archive,
                version: 1,
                file: payload.file.clone()
            }]
        );
        assert!(!piece_path(&world, "a").exists());
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_live_piece_keeps_its_id_and_its_live_payload() {
        let mut world = world();
        world.file("b/note.txt", 30);
        live_piece(&world, 1, "b", crate::testing::tree::COLLECTION_ID, |_| {});
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1));
        let live = crate::meta::Payload {
            kind: PayloadKind::Live,
            version: 1,
            file: crate::meta::FileName::new("0123456789abcdef-b.tar.zst").expect("a name"),
            package_format: "tar+zstd/1".into(),
            size: 5,
            checksum: crate::meta::Checksum::sha256("0".repeat(64)),
            listing: None,
            machine: "elsewhere".into(),
            created: NOW.parse().expect("a time"),
            verified: None,
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        };
        let path = world.collection.object_dir(id).join("meta.1");
        let mut metadata = crate::meta::parse(&fs::read(&path).expect("reads")).expect("parses");
        metadata.payloads.push(live.clone());
        fs::write(&path, crate::meta::to_bytes(&metadata).expect("serialises")).expect("writes");

        let report = run(&world, &Scripted::real());

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_eq!(done.id, id);
        let current = store(&world).current(id).expect("reads").expect("exists");
        assert_eq!(current.source, Source::Pending);
        assert_eq!(current.metadata.lifecycle, Lifecycle::Archived);
        assert_eq!(current.metadata.created, metadata.created);
        assert_eq!(current.metadata.payloads.len(), 2);
        assert_eq!(current.metadata.payloads[0], live);
        assert_eq!(current.metadata.payloads[1].kind, PayloadKind::Archive);
        assert_eq!(current.metadata.payloads[1].version, 1);
        assert_eq!(current.metadata.history.len(), 1);
        let tracked = entry(&world, id).expect("an entry");
        assert_eq!(tracked.state, EntryState::Gone);
        assert_eq!(tracked.version_taken_out, VersionTakenOut::Pending);
        assert_eq!(tracked.local_payloads.len(), 1);
    }

    #[test]
    fn the_next_archive_version_follows_the_highest_one() {
        let mut world = world();
        world.file("b/note.txt", 30);
        live_piece(&world, 1, "b", crate::testing::tree::COLLECTION_ID, |_| {});
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1));
        let path = world.collection.object_dir(id).join("meta.1");
        let mut metadata = crate::meta::parse(&fs::read(&path).expect("reads")).expect("parses");
        assert_eq!(next_archive_version(&metadata), 1, "no archive yet");
        for version in [1, 2] {
            metadata.payloads.push(crate::meta::Payload {
                kind: PayloadKind::Archive,
                version,
                file: crate::meta::FileName::new(format!("{version:016x}-b.tar.zst"))
                    .expect("a name"),
                package_format: "tar+zstd/1".into(),
                size: 5,
                checksum: crate::meta::Checksum::sha256("0".repeat(64)),
                listing: None,
                machine: "m".into(),
                created: NOW.parse().expect("a time"),
                verified: None,
                superseded: false,
                encoder: None,
                extra: serde_json::Map::new(),
            });
        }
        assert_eq!(next_archive_version(&metadata), 3);
    }

    // =========================================================
    // The time an ID is minted for
    // =========================================================

    #[test]
    fn an_id_is_minted_for_the_oldest_time_of_the_piece() {
        let mut world = world();
        world.file("a/old.txt", 40);
        world.file("a/older.txt", 31);
        let report = run(&world, &Scripted::real());
        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_eq!(done.id.timestamp_ms() / 1000, days_ago(40) as u64);
    }

    #[test]
    fn the_id_time_comes_from_the_entries_below_the_top() {
        let mut old = tree_entry("old", EntryKind::File);
        old.mtime_ns = 700_000_000_000_000_000;
        old.birth_ns = Some(900_000_000_000_000_000);
        let mut newer = tree_entry("new", EntryKind::File);
        newer.mtime_ns = 800_000_000_000_000_000;
        let found = id_time_of(
            &tree_of(vec![tree_entry("", EntryKind::Directory), old, newer]),
            "2026-11-01T12:00:00Z".parse().expect("a time"),
        );
        assert_eq!(found.time.as_second(), 700_000_000);
        assert_eq!(found.source, CreatedSource::OldestMtime);
        assert_eq!(found.warning, None);
    }

    #[test]
    fn a_piece_without_a_usable_time_gets_a_minted_id_and_a_warning() {
        let mut unfit = tree_entry("odd", EntryKind::File);
        unfit.mtime_ns = i128::MAX;
        unfit.birth_ns = Some(i128::MIN);
        let minted_at: Timestamp = "2026-11-01T12:00:00Z".parse().expect("a time");
        let found = id_time_of(
            &tree_of(vec![tree_entry("", EntryKind::Directory), unfit]),
            minted_at,
        );
        assert_eq!(found.time, minted_at);
        assert_eq!(found.source, CreatedSource::Minted);
        assert!(found.warning.is_some());
    }

    // =========================================================
    // Step 0: the piece is judged again
    // =========================================================

    #[test]
    fn a_piece_that_became_active_is_dropped_without_a_skip() {
        let world = lone_piece("a");
        let touched = piece_path(&world, "a/fresh.txt");
        let seams = Scripted::before(Step::Walk, move || {
            fs::write(&touched, "new").expect("a file is written");
            let recent = filetime::FileTime::from_unix_time(days_ago(1), 0);
            filetime::set_file_mtime(&touched, recent).expect("the time is set");
        });
        let report = run(&world, &seams);
        assert!(archived(&report).is_empty());
        assert!(skipped(&report).is_empty());
        assert!(!report.partial_for_test());
        assert!(objects(&world).is_empty());
    }

    /// The piece `a` gets something at step 0 that status did not see; the
    /// run skips it with `reason` and writes nothing.
    fn refused_at_step_0(
        prepare: impl Fn(&std::path::Path) + Send + Sync + 'static,
        reason: SkipReason,
    ) {
        let world = lone_piece("a");
        let root = world.temp.root().to_path_buf();
        let seams = Scripted::before(Step::Walk, move || prepare(&root));
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, reason);
        assert_eq!(row.path.to_string(), "a");
        assert_eq!(row.id, None);
        assert!(archived(&report).is_empty());
        assert!(objects(&world).is_empty());
        assert!(piece_path(&world, "a/note.txt").exists());
    }

    #[test]
    fn unreadable_after_status() {
        refused_at_step_0(
            |root| {
                let locked = root.join("a/locked");
                fs::create_dir(&locked).expect("a directory");
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("a mode");
                assert!(
                    fs::read_dir(&locked).is_err(),
                    "the test needs a user that mode 000 locks out"
                );
            },
            SkipReason::Unreadable,
        );
    }

    #[test]
    fn nested_collection_after_status() {
        refused_at_step_0(
            |root| fs::create_dir_all(root.join("a/inner/.antiq")).expect("a marker"),
            SkipReason::NestedCollection,
        );
    }

    #[test]
    fn excluded_after_status() {
        let mut world = world();
        world.configure("exclude = [\"a/skip\"]");
        world.file("a/note.txt", 30);
        let root = world.temp.root().to_path_buf();
        let seams = Scripted::before(Step::Walk, move || {
            fs::create_dir(root.join("a/skip")).expect("a directory");
        });
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Excluded);
        assert!(objects(&world).is_empty());
    }

    #[test]
    fn mount_point_after_status() {
        let world = lone_piece("a");
        let root = world.temp.root().to_path_buf();
        let mut seams = Scripted::on(|_, _| Ok(()));
        let armed = Arc::clone(&seams.mount.0);
        seams.on = Box::new(move |step, _| {
            if step == Step::Walk {
                fs::create_dir(root.join("a/mnt")).expect("a directory");
                armed.store(true, Ordering::SeqCst);
            }
            Ok(())
        });
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::MountPoint);
        assert!(objects(&world).is_empty());
    }

    // =========================================================
    // Steps 2 and 3: pack and verify
    // =========================================================

    #[test]
    fn a_difference_found_by_the_verify_keeps_the_folder() {
        let world = lone_piece("a");
        let file = piece_path(&world, "a/note.txt");
        let seams = Scripted::before(Step::Verify, move || rewrite(&file, "y"));
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Differs);
        assert!(archived(&report).is_empty());
        assert!(objects(&world).is_empty());
        assert_eq!(tmp_files(&world), Vec::<String>::new());
        assert_eq!(
            fs::read_to_string(piece_path(&world, "a/note.txt")).expect("reads"),
            "y"
        );
    }

    #[test]
    fn a_listing_mismatch_stores_nothing() {
        let world = lone_piece("a");
        let seams = tampering(|packed, _| {
            packed.listing.pop();
            Ok(())
        });
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert!(row.message.contains("listing"), "{}", row.message);
        assert!(objects(&world).is_empty());
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_tmp_file_with_the_wrong_hash_is_refused() {
        let world = lone_piece("a");
        let seams = tampering(|_, out| out.write_all(b"x").map_err(PackError::Write));
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert!(row.message.contains("hash"), "{}", row.message);
        assert!(objects(&world).is_empty());
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_stale_trash_tree_skips_the_piece() {
        let mut world = world();
        world.file("b/note.txt", 30);
        live_piece(&world, 1, "b", crate::testing::tree::COLLECTION_ID, |_| {});
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1));
        let trash = world.collection.trash_path(id);
        let leftover = trash.clone();
        let seams = Scripted::before(Step::WriteEntry, move || {
            fs::create_dir(&leftover).expect("a leftover");
        });
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert_eq!(row.id, Some(id));
        assert!(
            row.message.contains(&trash.display().to_string()),
            "{}",
            row.message
        );
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Live);
        assert!(trash.exists(), "the leftover is not touched");
    }

    // =========================================================
    // Space and failures
    // =========================================================

    #[test]
    fn too_little_space_skips() {
        let world = lone_piece("a");
        let mut seams = Scripted::real();
        seams.free = Some(0);
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::NoSpace);
        assert!(objects(&world).is_empty());
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn enospc_while_packing() {
        let world = lone_piece("a");
        let seams =
            tampering(|_, _| Err(PackError::Write(io::Error::from_raw_os_error(libc::ENOSPC))));
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::NoSpace);
        assert_eq!(tmp_files(&world), Vec::<String>::new());
        assert!(objects(&world).is_empty());
    }

    #[test]
    fn enospc_in_store_removes_what_was_put() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::failing(Step::StoreListing, libc::ENOSPC));
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::NoSpace);
        for id in objects(&world) {
            assert_eq!(object_files(&world, id), Vec::<String>::new());
        }
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_failed_metadata_write_removes_the_files() {
        let world = lone_piece("a");
        let collection_dir = world.collection.objects_dir();
        let seams = Scripted::before(Step::WriteMetadata, move || {
            // A directory where `meta.pending` goes makes the write fail.
            let object = fs::read_dir(&collection_dir)
                .expect("lists")
                .next()
                .expect("the object directory exists")
                .expect("an item")
                .path();
            fs::create_dir(object.join("meta.pending")).expect("a directory");
        });
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        for id in objects(&world) {
            assert_eq!(
                object_files(&world, id),
                ["meta.pending"],
                "only the blocker is left"
            );
        }
    }

    #[test]
    fn a_failed_entry_write_keeps_the_files_and_the_metadata() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::failing(Step::WriteEntry, libc::EIO));
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        let [id] = objects(&world)[..] else {
            panic!("one object");
        };
        assert_eq!(
            object_files(&world, id).len(),
            3,
            "payload, listing and meta.pending"
        );
        assert_eq!(entry(&world, id), None);
    }

    #[test]
    fn eio_at_mark_removing_does_not_rename() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::failing(Step::MarkRemoving, libc::EIO));
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        let [id] = objects(&world)[..] else {
            panic!("one object");
        };
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Live);
        assert!(piece_path(&world, "a/note.txt").exists());
        assert!(!world.collection.trash_path(id).exists());
    }

    #[test]
    fn a_change_counts_as_recorded_only_when_meta_pending_holds_it() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::real());
        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        let store = store(&world);
        let metadata = pending(&world, done.id);
        assert!(is_recorded(&store, &metadata));
        let mut other = metadata.clone();
        other.history.clear();
        assert!(!is_recorded(&store, &other));
        fs::remove_file(world.collection.meta_pending_path(done.id)).expect("removes");
        assert!(!is_recorded(&store, &metadata));
    }

    #[test]
    fn running_out_of_space_is_found_through_the_error_chain() {
        let space = io::Error::from_raw_os_error(libc::ENOSPC);
        assert!(is_out_of_space(&space));
        assert!(is_out_of_space(&PackError::Write(
            io::Error::from_raw_os_error(libc::ENOSPC)
        )));
        assert!(is_out_of_space(&crate::store::StoreError::Io {
            action: "move",
            path: "x".into(),
            source: io::Error::from_raw_os_error(libc::ENOSPC),
        }));
        assert!(!is_out_of_space(&io::Error::from_raw_os_error(libc::EIO)));
    }

    // =========================================================
    // Warnings
    // =========================================================

    #[test]
    fn names_that_collide_on_macos_give_a_warning() {
        let warnings = tree_warnings(&tree_of(vec![
            tree_entry("", EntryKind::Directory),
            tree_entry("README", EntryKind::File),
            tree_entry("Readme", EntryKind::File),
        ]));
        let [warning] = warnings.as_slice() else {
            panic!("one warning: {warnings:?}");
        };
        assert!(warning.message.contains("on macOS"), "{}", warning.message);
        assert!(
            warning.message.contains("become one name"),
            "{}",
            warning.message
        );
        assert!(
            warning.message.contains("projects/a"),
            "{}",
            warning.message
        );
    }

    #[test]
    fn a_sparse_piece_gives_a_warning() {
        let mut hole = tree_entry("hole", EntryKind::File);
        hole.size = 3 << 30;
        hole.allocated = 0;
        let warnings = tree_warnings(&tree_of(vec![tree_entry("", EntryKind::Directory), hole]));
        let [warning] = warnings.as_slice() else {
            panic!("one warning: {warnings:?}");
        };
        assert!(warning.message.contains("sparse"), "{}", warning.message);
    }

    #[test]
    fn a_fifo_is_noted_and_the_piece_is_archived() {
        let mut world = lone_piece("a");
        world.temp.tree().fifo("a/queue");
        let past = filetime::FileTime::from_unix_time(days_ago(30), 0);
        filetime::set_symlink_file_times(piece_path(&world, "a/queue"), past, past)
            .expect("a time");
        let report = run(&world, &Scripted::real());
        assert_eq!(archived(&report).len(), 1, "{report:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("is a fifo")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_time_before_1970_is_noted_and_the_piece_is_archived() {
        let world = lone_piece("a");
        let file = piece_path(&world, "a/note.txt");
        let past = filetime::FileTime::from_unix_time(-86_400, 0);
        filetime::set_file_mtime(&file, past).expect("a time");
        let report = run(&world, &Scripted::real());
        assert_eq!(archived(&report).len(), 1, "{report:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("before 1970")),
            "{:?}",
            report.warnings
        );
    }

    // =========================================================
    // The index
    // =========================================================

    #[test]
    fn index_names_the_stored_listing_after_a_run() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::real());
        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        let listing_name = pending(&world, done.id).payloads[0]
            .listing
            .clone()
            .expect("a listing");
        let cache: Vec<String> = fs::read_dir(world.collection.index_dir())
            .expect("the index directory exists")
            .map(|item| {
                fs::read_to_string(item.expect("an item").path()).expect("a cache file reads")
            })
            .collect();
        assert_eq!(cache.len(), 1, "one cache file for the stored listing");
        assert!(cache[0].contains(listing_name.as_str()), "{}", cache[0]);
    }

    // =========================================================
    // Adopting what a crash left behind
    // =========================================================

    #[test]
    fn a_crashed_object_is_adopted_instead_of_minting_a_second_id() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("the crash left one object");
        };
        assert_eq!(entry(&world, first), None);

        let report = run_from(&world, &Scripted::real(), 100);

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_eq!(done.id, first);
        assert_eq!(objects(&world), [first], "no second object");
        assert_eq!(
            entry(&world, first).expect("an entry").state,
            EntryState::Gone
        );
    }

    #[test]
    fn a_crashed_object_that_does_not_match_is_not_adopted() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        rewrite(&piece_path(&world, "a/note.txt"), "y");

        let report = run_from(&world, &Scripted::real(), 100);

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_eq!(objects(&world).len(), 2);
        assert_eq!(
            entry(&world, done.id).expect("an entry").state,
            EntryState::Gone
        );
    }

    #[test]
    fn two_crashed_objects_the_matching_one_is_adopted() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("one object");
        };
        rewrite(&piece_path(&world, "a/note.txt"), "y");
        crash_before(&world, Step::WriteEntry, 100);
        let second = *objects(&world)
            .iter()
            .find(|id| **id != first)
            .expect("a second object");

        let report = run_from(&world, &Scripted::real(), 200);

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_eq!(done.id, second);
        assert_eq!(objects(&world).len(), 2);
    }

    #[test]
    fn a_crashed_object_with_a_changed_payload_is_not_adopted() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("one object");
        };
        let payload = pending(&world, first).payloads[0].file.clone();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(store(&world).file_path(first, &payload))
            .expect("opens");
        file.write_all(b"x").expect("appends");
        drop(file);

        let report = run_from(&world, &Scripted::real(), 100);

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_ne!(done.id, first);
    }

    #[test]
    fn a_crashed_object_with_another_valid_package_is_not_adopted() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("one object");
        };
        // A different package of the same folder: valid and equal to it, but
        // not the bytes the metadata has the checksum of.
        let payload = pending(&world, first).payloads[0].file.clone();
        let path = store(&world).file_path(first, &payload);
        let root = TreeRoot::open(world.temp.root()).expect("opens");
        let sub = root
            .sub(&RelPath::parse("a").expect("valid"))
            .expect("opens");
        let walked = walk::walk_piece(
            &root,
            &RelPath::parse("a").expect("valid"),
            &[],
            &Rules::new(&["*".to_string()], &[]).expect("rules"),
        )
        .expect("walks");
        let settings = PackSettings {
            zstd: ZstdSettings::live(1),
        };
        let mut file = fs::File::create(&path).expect("creates");
        TarZstd
            .pack(&sub, &walked, &settings, &mut file)
            .expect("packs");
        drop(file);

        let report = run_from(&world, &Scripted::real(), 100);

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_ne!(done.id, first);
    }

    #[test]
    fn a_crashed_object_with_another_listing_is_not_adopted() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("one object");
        };
        let name = pending(&world, first).payloads[0]
            .listing
            .clone()
            .expect("a listing");
        let path = store(&world).file_path(first, &name);
        let mut entries = listing::read_all(fs::File::open(&path).expect("opens")).expect("reads");
        entries[0].mtime += 1;
        listing::write(entries.iter(), fs::File::create(&path).expect("creates")).expect("writes");

        let report = run_from(&world, &Scripted::real(), 100);

        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert_ne!(done.id, first);
    }

    // =========================================================
    // A process that dies before a step
    // =========================================================

    /// The disk after a crash before `step` and after the next run's
    /// repairs: the folder is untouched either way.
    fn after_crash(step: Step) -> World {
        let world = lone_piece("a");
        crash_before(&world, step, 1);
        world
    }

    fn repaired(world: &World) {
        let started =
            crate::run::start(&world.collection, &clock(), false).expect("the repairs run");
        drop(started);
    }

    #[test]
    fn a_crash_before_pack_leaves_nothing() {
        let world = after_crash(Step::Pack);
        assert!(objects(&world).is_empty());
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_crash_before_verify_leaves_a_tmp_payload_that_the_repair_removes() {
        let world = after_crash(Step::Verify);
        assert_eq!(tmp_files(&world).len(), 1);
        assert!(objects(&world).is_empty());
        repaired(&world);
        assert_eq!(tmp_files(&world), Vec::<String>::new());
        assert!(piece_path(&world, "a/note.txt").exists());
    }

    #[test]
    fn a_crash_before_the_payload_is_stored_leaves_a_tmp_payload() {
        let world = after_crash(Step::StorePayload);
        assert_eq!(tmp_files(&world).len(), 1);
        assert!(objects(&world).is_empty());
        repaired(&world);
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_crash_before_the_listing_is_stored_leaves_the_payload_alone_in_the_object() {
        let world = after_crash(Step::StoreListing);
        let [id] = objects(&world)[..] else {
            panic!("one object");
        };
        let names = object_files(&world, id);
        assert_eq!(names.len(), 1);
        assert!(names[0].ends_with(".tar.zst"));
        repaired(&world);
        assert_eq!(object_files(&world, id), names, "the orphan payload stays");
        assert!(piece_path(&world, "a/note.txt").exists());
    }

    #[test]
    fn a_crash_before_the_metadata_leaves_payload_and_listing() {
        let world = after_crash(Step::WriteMetadata);
        let [id] = objects(&world)[..] else {
            panic!("one object");
        };
        assert_eq!(object_files(&world, id).len(), 2);
        repaired(&world);
        assert_eq!(object_files(&world, id).len(), 2);
    }

    #[test]
    fn a_crash_before_the_entry_leaves_the_object_for_the_next_run_to_adopt() {
        let world = after_crash(Step::WriteEntry);
        let [id] = objects(&world)[..] else {
            panic!("one object");
        };
        assert_eq!(object_files(&world, id).len(), 3);
        assert_eq!(entry(&world, id), None);
        repaired(&world);
        assert_eq!(
            entry(&world, id),
            None,
            "the repairs leave an entry-less object alone"
        );
    }

    #[test]
    fn a_crash_before_removing_leaves_a_live_entry_that_the_repair_turns_into_unearthed() {
        let world = after_crash(Step::MarkRemoving);
        let [id] = objects(&world)[..] else {
            panic!("one object");
        };
        let before = entry(&world, id).expect("an entry");
        assert_eq!(before.state, EntryState::Live);
        assert_eq!(before.version_taken_out, VersionTakenOut::Pending);
        repaired(&world);
        let after = entry(&world, id).expect("an entry");
        assert_eq!(after.state, EntryState::Live);
        assert_eq!(after.unearthed_payload_version, Some(1));
        assert!(after.unearthed.is_some());
        assert!(piece_path(&world, "a/note.txt").exists());
    }

    // =========================================================
    // The entry points and the rest of the paths
    // =========================================================

    fn live_b(world: &mut World) -> ObjectId {
        world.file("b/note.txt", 30);
        live_piece(world, 1, "b", crate::testing::tree::COLLECTION_ID, |_| {});
        ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1))
    }

    fn request<'a>(
        world: &'a World,
        ids: &'a SeqIds,
        clock: &'a crate::clock::FixedClock,
        pool: &'a Pool,
        dry_run: bool,
    ) -> ArchiveRequest<'a> {
        ArchiveRequest {
            collection: &world.collection,
            clock,
            ids,
            pool,
            dry_run,
        }
    }

    #[test]
    fn the_public_entry_point_archives_and_plans() {
        let world = lone_piece("a");
        let (ids, now) = (SeqIds::new(1), clock());
        let planned =
            crate::ops::archive::archive(&request(&world, &ids, &now, shared_pool(), true))
                .expect("the plan is made");
        assert!(planned.dry_run);
        assert!(objects(&world).is_empty());

        let report =
            crate::ops::archive::archive(&request(&world, &ids, &now, shared_pool(), false))
                .expect("the run finishes");
        assert_eq!(archived(&report).len(), 1);
    }

    #[test]
    fn a_held_lock_is_refused_with_its_kind() {
        use crate::error::{ErrorKind, HasKind};
        let world = lone_piece("a");
        let _holder =
            crate::run::start(&world.collection, &clock(), false).expect("the lock is free");
        let (ids, now) = (SeqIds::new(1), clock());
        let error =
            crate::ops::archive::archive(&request(&world, &ids, &now, shared_pool(), false))
                .expect_err("the lock is held");
        assert_eq!(error.kind(), ErrorKind::LockHeld);
    }

    #[test]
    fn the_other_errors_keep_the_kind_of_their_cause() {
        use crate::error::{ErrorKind, HasKind};
        use crate::ops::archive::ArchiveError;
        let too_new = crate::store::StoreError::NothingReadable {
            unreadable: vec![crate::store::Unreadable {
                number: 2,
                format: Some(99),
                reason: "newer".into(),
            }],
        };
        assert_eq!(ArchiveError::Store(too_new).kind(), ErrorKind::FormatTooNew);
        let machine = crate::machine::MachineError::Unavailable { reason: "x".into() };
        assert_eq!(ArchiveError::Machine(machine).kind(), ErrorKind::Other);
        let tracking = crate::tracking::TrackingError::NothingTracked;
        assert_eq!(ArchiveError::Tracking(tracking).kind(), ErrorKind::Other);
        let io = ArchiveError::Io {
            action: "open",
            path: "x".into(),
            source: io::Error::from_raw_os_error(libc::EIO),
        };
        assert_eq!(io.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_tracked_object_is_not_taken_over_by_a_new_piece() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let live = live_b(&mut world);
        let report = run(&world, &Scripted::real());
        let mut ids: Vec<ObjectId> = archived(&report).iter().map(|done| done.id).collect();
        ids.sort();
        assert_eq!(ids.len(), 2, "{report:?}");
        assert!(ids.contains(&live));
    }

    /// A run that died before the entry left an object; `change` edits its
    /// pending metadata before the next run looks at it.
    fn crashed_object_edited(
        change: impl FnOnce(&mut Metadata),
    ) -> (World, ObjectId, ArchiveReport) {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("one object");
        };
        let mut metadata = pending(&world, first);
        change(&mut metadata);
        fs::write(
            world.collection.meta_pending_path(first),
            crate::meta::to_bytes(&metadata).expect("serialises"),
        )
        .expect("writes");
        let report = run_from(&world, &Scripted::real(), 100);
        (world, first, report)
    }

    #[test]
    fn an_object_made_by_another_machine_is_not_adopted() {
        let (_world, first, report) =
            crashed_object_edited(|metadata| metadata.payloads[0].machine = "elsewhere".into());
        assert_ne!(archived(&report)[0].id, first);
    }

    #[test]
    fn an_object_whose_payload_names_no_listing_is_not_adopted() {
        let (_world, first, report) =
            crashed_object_edited(|metadata| metadata.payloads[0].listing = None);
        assert_ne!(archived(&report)[0].id, first);
    }

    #[test]
    fn an_object_whose_listing_is_gone_is_not_adopted() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("one object");
        };
        let name = pending(&world, first).payloads[0]
            .listing
            .clone()
            .expect("a listing");
        fs::remove_file(store(&world).file_path(first, &name)).expect("removes");
        let report = run_from(&world, &Scripted::real(), 100);
        assert_ne!(archived(&report)[0].id, first);
    }

    #[test]
    fn an_adopted_object_with_committed_metadata_is_taken_out_at_that_version() {
        let world = lone_piece("a");
        crash_before(&world, Step::WriteEntry, 1);
        let [first] = objects(&world)[..] else {
            panic!("one object");
        };
        fs::rename(
            world.collection.meta_pending_path(first),
            store(&world).meta_path(first, 1),
        )
        .expect("renames");
        let report = run_from(&world, &Scripted::real(), 100);
        assert_eq!(archived(&report)[0].id, first);
        assert_eq!(
            entry(&world, first).expect("an entry").version_taken_out,
            VersionTakenOut::Committed(1)
        );
    }

    #[test]
    fn a_live_piece_whose_metadata_became_unreadable_is_skipped_and_its_files_removed() {
        let mut world = world();
        let id = live_b(&mut world);
        let meta = store(&world).meta_path(id, 1);
        let seams = Scripted::before(Step::StoreListing, move || {
            fs::write(&meta, "not json").expect("overwrites");
        });
        let report = run(&world, &seams);
        assert_eq!(skipped(&report)[0].reason, SkipReason::Error);
        assert_eq!(object_files(&world, id), ["meta.1"]);
    }

    #[test]
    fn a_live_piece_whose_metadata_vanished_is_skipped_and_its_files_removed() {
        let mut world = world();
        let id = live_b(&mut world);
        let meta = store(&world).meta_path(id, 1);
        let seams = Scripted::before(Step::StoreListing, move || {
            fs::remove_file(&meta).expect("removes");
        });
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert!(row.message.contains("has no metadata"), "{}", row.message);
        assert_eq!(object_files(&world, id), Vec::<String>::new());
    }

    #[test]
    fn a_pool_that_cannot_hold_a_worker_warns() {
        let world = lone_piece("a");
        let (ids, now) = (SeqIds::new(1), clock());
        let tiny = Pool::new(1, 1);
        let report = archive_with(
            &request(&world, &ids, &now, &tiny, false),
            &Scripted::real(),
        )
        .expect("the run finishes");
        assert_eq!(archived(&report).len(), 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("memory")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_tmp_file_that_cannot_be_hashed_is_skipped() {
        let world = lone_piece("a");
        let tmp = world.collection.tmp_dir();
        let seams = tampering(move |_, _| {
            for item in fs::read_dir(&tmp).expect("lists") {
                fs::remove_file(item.expect("an item").path()).expect("removes");
            }
            Ok(())
        });
        let report = run(&world, &seams);
        assert_eq!(skipped(&report)[0].reason, SkipReason::Error);
        assert!(objects(&world).is_empty());
    }

    #[test]
    fn a_live_piece_stores_again_after_a_crash_before_its_metadata() {
        let mut world = world();
        let id = live_b(&mut world);
        crash_before(&world, Step::WriteMetadata, 1);
        assert_eq!(
            object_files(&world, id).len(),
            3,
            "meta.1, payload and listing"
        );

        let report = run(&world, &Scripted::real());

        assert_eq!(archived(&report)[0].id, id);
        assert_eq!(
            object_files(&world, id).len(),
            4,
            "the same two files and meta.pending"
        );
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_trash_directory_that_cannot_be_looked_into_skips_the_piece() {
        let mut world = world();
        live_b(&mut world);
        let trash = world.collection.trash_dir();
        let locked = trash.clone();
        let seams = Scripted::before(Step::WriteEntry, move || {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("a mode");
        });
        let restore = trash.clone();
        let report = run(&world, &seams);
        fs::set_permissions(&restore, fs::Permissions::from_mode(0o755)).expect("a mode");
        let [row] = skipped(&report) else {
            panic!("one piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
    }

    #[test]
    fn symlink_targets_and_hard_links_are_checked_by_name_rules() {
        let mut long = tree_entry("link", EntryKind::Symlink);
        long.link_target = Some(vec![b'x'; 1100]);
        let mut first = tree_entry("one", EntryKind::File);
        first.nlink = 2;
        let mut second = tree_entry("two", EntryKind::File);
        second.nlink = 2;
        let warnings = tree_warnings(&tree_of(vec![
            tree_entry("", EntryKind::Directory),
            long,
            first,
            second,
            tree_entry("queue", EntryKind::Fifo),
        ]));
        let [warning] = warnings.as_slice() else {
            panic!("one warning: {warnings:?}");
        };
        assert!(warning.message.contains("on macOS"), "{}", warning.message);
        assert!(
            warning.message.contains("1100 bytes"),
            "{}",
            warning.message
        );
    }

    #[test]
    fn a_failing_index_update_is_a_warning() {
        let world = lone_piece("a");
        let index = world.collection.index_dir();
        fs::remove_dir_all(&index).expect("removes");
        fs::write(&index, "in the way").expect("writes");
        let report = run(&world, &Scripted::real());
        assert_eq!(archived(&report).len(), 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("cannot update the index")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn running_out_of_descriptors_fails_the_run() {
        let world = lone_piece("a");
        let mut seams = Scripted::real();
        let armed = Arc::clone(&seams.mount.1);
        seams.on = Box::new(move |step, _| {
            if step == Step::Walk {
                armed.store(true, Ordering::SeqCst);
            }
            Ok(())
        });
        let (ids, now) = (SeqIds::new(1), clock());
        let error = archive_with(&request(&world, &ids, &now, shared_pool(), false), &seams)
            .expect_err("the run fails");
        assert!(matches!(error, ArchiveError::Walk(_)), "{error}");
        assert!(objects(&world).is_empty());
    }

    #[test]
    fn an_object_with_unreadable_metadata_does_not_stop_a_new_piece() {
        let world = lone_piece("a");
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_000, 7));
        let dir = world.collection.object_dir(id);
        fs::create_dir_all(&dir).expect("creates");
        fs::write(dir.join("meta.1"), "not json").expect("writes");
        let report = run(&world, &Scripted::real());
        assert_eq!(archived(&report).len(), 1, "{report:?}");
    }

    /// Makes the object directory of the live piece read-only before
    /// `step`, so that placing a file in it fails.
    fn placing_fails_at(step: Step) -> (World, ArchiveReport, PathBuf) {
        let mut world = world();
        let id = live_b(&mut world);
        let dir = world.collection.object_dir(id);
        let locked = dir.clone();
        let seams = Scripted::before(step, move || {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("a mode");
        });
        let report = run(&world, &seams);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("a mode");
        (world, report, dir)
    }

    #[test]
    fn a_payload_that_cannot_be_placed_skips_the_piece() {
        let (world, report, _dir) = placing_fails_at(Step::StorePayload);
        assert_eq!(skipped(&report)[0].reason, SkipReason::Error);
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_listing_that_cannot_be_placed_skips_the_piece_and_clears_tmp() {
        let (world, report, _dir) = placing_fails_at(Step::StoreListing);
        assert_eq!(skipped(&report)[0].reason, SkipReason::Error);
        assert_eq!(tmp_files(&world), Vec::<String>::new());
    }

    // =========================================================
    // Removing the folder
    // =========================================================

    /// Real effects that fail on request.
    #[derive(Default)]
    struct Failing {
        /// Every flush from the n-th on fails.
        flush_from: Option<usize>,
        flushes: AtomicUsize,
        remove_tree: bool,
        remove_tree_at: bool,
    }

    impl Effects for Failing {
        fn remove_tree_at(&self, path: &Path) -> Result<(), crate::fsx::FsxError> {
            if self.remove_tree_at {
                return Err(crate::fsx::FsxError::Io {
                    action: "remove",
                    path: path.display().to_string(),
                    source: io::Error::from_raw_os_error(libc::EIO),
                });
            }
            RealEffects.remove_tree_at(path)
        }

        fn remove_tree(&self, root: &TreeRoot, path: &RelPath) -> Result<(), crate::fsx::FsxError> {
            if self.remove_tree {
                return Err(crate::fsx::FsxError::Io {
                    action: "remove",
                    path: path.to_string(),
                    source: io::Error::from_raw_os_error(libc::EIO),
                });
            }
            RealEffects.remove_tree(root, path)
        }

        fn move_dir(
            &self,
            from_root: &TreeRoot,
            from: &RelPath,
            to_root: &TreeRoot,
            to: &RelPath,
        ) -> Result<crate::fsx::moves::Moved, crate::fsx::FsxError> {
            RealEffects.move_dir(from_root, from, to_root, to)
        }

        fn flush_dir(&self, dir: &Path) -> Result<(), crate::fsx::FsxError> {
            let number = self.flushes.fetch_add(1, Ordering::SeqCst);
            if self.flush_from.is_some_and(|from| number >= from) {
                return Err(crate::fsx::FsxError::Io {
                    action: "flush directory",
                    path: dir.display().to_string(),
                    source: io::Error::from_raw_os_error(libc::EIO),
                });
            }
            RealEffects.flush_dir(dir)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            RealEffects.remove_file(path)
        }

        fn write_entry(
            &self,
            collection: &Collection,
            lock: &RunLock,
            clock: &dyn Clock,
            entry: &mut TrackingEntry,
        ) -> Result<(), crate::tracking::TrackingError> {
            RealEffects.write_entry(collection, lock, clock, entry)
        }

        fn remove_entry(
            &self,
            collection: &Collection,
            lock: &RunLock,
            id: ObjectId,
        ) -> Result<(), crate::tracking::TrackingError> {
            RealEffects.remove_entry(collection, lock, id)
        }

        fn record_metadata(
            &self,
            collection: &Collection,
            lock: &RunLock,
            metadata: &crate::meta::Metadata,
        ) -> Result<(), crate::store::StoreError> {
            RealEffects.record_metadata(collection, lock, metadata)
        }
    }

    /// The piece `a` with a subdirectory, as the tests of the removal want
    /// it.
    fn piece_with_subdirectory() -> World {
        let mut world = world();
        world.file("a/top.txt", 30);
        world.file("a/sub/note.txt", 30);
        world
    }

    fn names_in(dir: PathBuf) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("the directory lists")
            .map(|item| {
                item.expect("an item")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn trash_names(world: &World) -> Vec<String> {
        names_in(world.collection.trash_dir())
    }

    fn recovered_names(world: &World) -> Vec<String> {
        names_in(world.collection.recovered_dir())
    }

    fn mode_of(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("a mode");
    }

    fn set_mtime(path: &Path, seconds: i64) {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 0))
            .expect("the time is set");
    }

    fn state_of(world: &World, path: &str) -> crate::ops::status::PieceKind {
        crate::ops::status::status(&world.collection, &clock())
            .expect("status runs")
            .pieces
            .iter()
            .find(|piece| piece.path.to_string() == path)
            .unwrap_or_else(|| panic!("status lists {path}"))
            .state
    }

    /// Runs with `edit` applied to the collection root right before the
    /// folder is moved: a change after the verify.
    fn changed_after_verify(
        world: &World,
        edit: impl Fn(&Path) + Send + Sync + 'static,
    ) -> ArchiveReport {
        let root = world.temp.root().to_path_buf();
        run(
            world,
            &Scripted::before(Step::MoveToTrash, move || edit(&root)),
        )
    }

    /// The folder is back at its path, the entry says unearthed, and the
    /// report keeps the piece.
    fn assert_given_back(world: &World, report: &ArchiveReport, path: &str) {
        let kept = report.kept.as_deref().expect("a real run lists kept");
        let [row] = kept else {
            panic!("one piece is kept: {report:?}");
        };
        assert_eq!(row.path.to_string(), path);
        assert_eq!(row.how, KeptHow::Unearthed);
        assert_eq!(row.recovered_as, None);
        assert!(archived(report).is_empty(), "{report:?}");
        assert!(skipped(report).is_empty(), "{report:?}");
        assert!(report.warnings.iter().any(|w| w.message.contains(path)));
        assert!(piece_path(world, path).is_dir());
        assert_eq!(trash_names(world), Vec::<String>::new());
        let tracked = entry(world, row.id).expect("an entry");
        assert_eq!(tracked.state, EntryState::Live);
        assert_eq!(tracked.unearthed_payload_version, Some(1));
        assert_eq!(tracked.unearthed, Some(NOW.parse().expect("a time")));
        assert_eq!(tracked.removing_payload, None);
        assert_eq!(
            state_of(world, path),
            crate::ops::status::PieceKind::Unearthed
        );
    }

    #[test]
    fn an_archived_piece_is_gone_and_its_entry_is_gone_with_the_payload() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::real());
        let [done] = archived(&report) else {
            panic!("one piece is archived: {report:?}");
        };
        assert!(skipped(&report).is_empty());
        assert_eq!(report.kept.as_deref(), Some(&[][..]));
        assert!(!piece_path(&world, "a").exists());
        assert_eq!(trash_names(&world), Vec::<String>::new());
        let tracked = entry(&world, done.id).expect("an entry");
        assert_eq!(tracked.state, EntryState::Gone);
        assert_eq!(tracked.removing_payload, None);
        assert_eq!(tracked.local_payloads.len(), 1, "the payload is kept");
        assert_eq!(
            state_of(&world, "a"),
            crate::ops::status::PieceKind::Archived
        );
        let status = crate::ops::status::status(&world.collection, &clock()).expect("status");
        let piece = status
            .pieces
            .iter()
            .find(|piece| piece.path.to_string() == "a")
            .expect("listed");
        assert!(
            piece
                .flags
                .contains(&crate::ops::status::Flag::PendingUpload)
        );
    }

    macro_rules! given_back {
        ($name:ident, $edit:expr) => {
            #[test]
            fn $name() {
                let world = piece_with_subdirectory();
                let report = changed_after_verify(&world, $edit);
                assert_given_back(&world, &report, "a");
            }
        };
    }

    given_back!(a_changed_file_is_given_back, |root| {
        fs::write(root.join("a/top.txt"), "changed content").expect("written");
    });
    given_back!(an_added_file_is_given_back, |root| {
        fs::write(root.join("a/sub/added.txt"), "x").expect("written");
    });
    given_back!(a_removed_file_is_given_back, |root| {
        fs::remove_file(root.join("a/top.txt")).expect("removed");
    });
    given_back!(a_file_with_only_its_time_changed_is_given_back, |root| {
        set_mtime(&root.join("a/top.txt"), 1_700_000_000);
    });
    given_back!(
        a_replaced_file_with_the_same_size_and_time_is_given_back,
        |root| {
            let file = root.join("a/top.txt");
            let before = fs::metadata(&file).expect("exists");
            let directory = fs::metadata(root.join("a")).expect("exists");
            fs::write(root.join("a/other"), "x").expect("written");
            fs::rename(root.join("a/other"), &file).expect("replaced");
            filetime::set_file_mtime(
                &file,
                filetime::FileTime::from_last_modification_time(&before),
            )
            .expect("the time is set");
            filetime::set_file_mtime(
                root.join("a"),
                filetime::FileTime::from_last_modification_time(&directory),
            )
            .expect("the time is set");
        }
    );
    given_back!(a_touched_directory_is_given_back, |root| {
        set_mtime(&root.join("a/sub"), 1_700_000_000);
    });
    given_back!(a_replaced_top_directory_is_given_back, |root| {
        fs::rename(root.join("a"), root.join("a.old")).expect("moved aside");
        fs::create_dir_all(root.join("a/sub")).expect("made");
        fs::write(root.join("a/top.txt"), "x").expect("written");
        fs::write(root.join("a/sub/note.txt"), "x").expect("written");
    });
    given_back!(a_touched_top_directory_is_given_back, |root| {
        set_mtime(&root.join("a"), 1_700_000_000);
    });
    given_back!(a_nested_collection_that_appeared_is_given_back, |root| {
        fs::create_dir_all(root.join("a/sub/.antiq")).expect("made");
    });

    #[test]
    fn a_directory_that_became_unreadable_is_given_back() {
        let world = piece_with_subdirectory();
        let sub = world.temp.root().join("a/sub");
        let report = changed_after_verify(&world, |root| mode_of(&root.join("a/sub"), 0o000));
        mode_of(&sub, 0o755);
        assert_given_back(&world, &report, "a");
    }

    #[test]
    fn a_mount_point_that_appeared_is_given_back() {
        let world = piece_with_subdirectory();
        fs::create_dir(world.temp.root().join("a/mnt")).expect("made");
        let mut seams = Scripted::real();
        let armed = Arc::clone(&seams.mount.0);
        seams.on = Box::new(move |step, _| {
            if step == Step::MoveToTrash {
                armed.store(true, Ordering::SeqCst);
            }
            Ok(())
        });
        let report = run(&world, &seams);
        assert_given_back(&world, &report, "a");
    }

    fn lstat_of(path: &str, kind: EntryKind, mtime_ns: i128, ctime_ns: i128) -> LstatRecord {
        LstatRecord {
            path: RelPath::parse(path).expect("valid path"),
            kind,
            size: 0,
            mtime_ns,
            ctime_ns,
            inode: 1,
        }
    }

    fn timed_entry(path: &str, kind: EntryKind, mtime_ns: i128, ctime_ns: i128) -> TreeEntry {
        TreeEntry {
            mtime_ns,
            ctime_ns,
            ..tree_entry(path, kind)
        }
    }

    #[test]
    fn the_moves_own_ctime_change_is_no_difference() {
        let record = [
            lstat_of("", EntryKind::Directory, 5, 10),
            lstat_of("f", EntryKind::File, 5, 10),
        ];
        let moved = tree_of(vec![
            timed_entry("", EntryKind::Directory, 5, 99),
            timed_entry("f", EntryKind::File, 5, 10),
        ]);
        assert_eq!(finish::differs(&record, &moved), None);
    }

    #[test]
    fn the_ctime_of_an_entry_below_the_top_is_a_difference() {
        let record = [
            lstat_of("", EntryKind::Directory, 5, 10),
            lstat_of("f", EntryKind::File, 5, 10),
        ];
        let touched = tree_of(vec![
            timed_entry("", EntryKind::Directory, 5, 10),
            timed_entry("f", EntryKind::File, 5, 11),
        ]);
        assert!(finish::differs(&record, &touched).is_some_and(|text| text.contains("`f`")));
    }

    #[test]
    fn the_top_changing_its_modification_time_is_a_difference() {
        let record = [lstat_of("", EntryKind::Directory, 5, 10)];
        let touched = tree_of(vec![timed_entry("", EntryKind::Directory, 6, 10)]);
        assert!(finish::differs(&record, &touched).is_some());
    }

    #[test]
    fn a_path_in_one_side_only_is_a_difference() {
        let record = [
            lstat_of("", EntryKind::Directory, 5, 10),
            lstat_of("gone", EntryKind::File, 5, 10),
        ];
        let tree = tree_of(vec![
            timed_entry("", EntryKind::Directory, 5, 10),
            timed_entry("new", EntryKind::File, 5, 10),
        ]);
        let text = finish::differs(&record, &tree).expect("the first difference");
        assert!(text.contains("`gone`") || text.contains("`new`"), "{text}");
        let only_new = tree_of(vec![
            timed_entry("", EntryKind::Directory, 5, 10),
            timed_entry("gone", EntryKind::File, 5, 10),
            timed_entry("new", EntryKind::File, 5, 10),
        ]);
        assert!(finish::differs(&record, &only_new).is_some_and(|text| text.contains("`new`")));
    }

    // ---- Where a different tree goes ----

    /// A change after the verify, then `then` right before the tree goes
    /// back to its path.
    fn different_then(
        world: &World,
        then_at: Step,
        then: impl Fn(&Path) + Send + Sync + 'static,
    ) -> ArchiveReport {
        let root = world.temp.root().to_path_buf();
        run(
            world,
            &Scripted::on(move |step, _| {
                if step == Step::MoveToTrash {
                    fs::write(root.join("a/top.txt"), "changed content").expect("written");
                }
                if step == then_at {
                    then(&root);
                }
                Ok(())
            }),
        )
    }

    fn assert_recovered(world: &World, report: &ArchiveReport) -> String {
        let [row] = report.kept.as_deref().expect("a real run lists kept") else {
            panic!("one piece is kept: {report:?}");
        };
        assert_eq!(row.how, KeptHow::Recovered);
        let name = row.recovered_as.clone().expect("the recovered name");
        assert_eq!(name, format!("{}-20261101T120000Z", row.id));
        assert_eq!(recovered_names(world), std::slice::from_ref(&name));
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains(&format!(".antiq/recovered/{name}"))),
            "{report:?}"
        );
        assert_eq!(trash_names(world), Vec::<String>::new());
        assert_eq!(
            fs::read_to_string(world.collection.recovered_dir().join(&name).join("top.txt"))
                .expect("the tree is kept"),
            "changed content"
        );
        let tracked = entry(world, row.id).expect("an entry");
        assert_eq!(tracked.state, EntryState::Gone);
        assert_eq!(tracked.removing_payload, None);
        name
    }

    #[test]
    fn a_different_tree_whose_path_was_taken_goes_to_recovered() {
        let world = piece_with_subdirectory();
        let report = different_then(&world, Step::GiveBackMove, |root| {
            fs::create_dir_all(root.join("a")).expect("a stranger's folder");
            fs::write(root.join("a/stranger.txt"), "s").expect("written");
        });
        assert_recovered(&world, &report);
        assert_eq!(
            fs::read_to_string(piece_path(&world, "a/stranger.txt")).expect("untouched"),
            "s"
        );
    }

    #[test]
    fn a_move_back_the_filesystem_refuses_goes_to_recovered() {
        let world = piece_with_subdirectory();
        let report = different_then(&world, Step::GiveBackMove, |root| mode_of(root, 0o555));
        mode_of(world.temp.root(), 0o755);
        assert_recovered(&world, &report);
    }

    #[test]
    fn two_refused_moves_keep_the_tree_in_the_trash() {
        let world = piece_with_subdirectory();
        let recovered = world.collection.recovered_dir();
        let locked = recovered.clone();
        let report = different_then(&world, Step::GiveBackMove, move |root| {
            mode_of(root, 0o555);
            mode_of(&locked, 0o555);
        });
        mode_of(world.temp.root(), 0o755);
        mode_of(&recovered, 0o755);

        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert!(report.kept.as_deref().is_some_and(<[Kept]>::is_empty));
        assert_eq!(trash_names(&world).len(), 1, "the tree stays in the trash");
        assert!(report.warnings.iter().any(|w| w.message.contains("trash")));
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );

        repaired(&world);
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Live);
        assert!(piece_path(&world, "a/top.txt").exists(), "back at its path");
    }

    #[test]
    fn a_flush_failure_after_the_move_back_leaves_the_entry_for_the_repair() {
        let world = piece_with_subdirectory();
        let seams = Scripted::with_effects(
            |step, root| {
                if step == Step::MoveToTrash {
                    fs::write(root.join("a/top.txt"), "changed content").expect("written");
                }
            },
            world.temp.root(),
            Failing {
                // Two flushes for the rename to the trash, then the move back.
                flush_from: Some(2),
                ..Failing::default()
            },
        );
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert!(piece_path(&world, "a/top.txt").exists());
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        let after = entry(&world, id).expect("an entry");
        assert_eq!(after.state, EntryState::Live);
        assert_eq!(after.unearthed_payload_version, Some(1));
    }

    #[test]
    fn a_failing_entry_write_after_the_move_back_skips_the_piece() {
        let world = piece_with_subdirectory();
        let root = world.temp.root().to_path_buf();
        let report = run(
            &world,
            &Scripted::on(move |step, _| {
                match step {
                    Step::MoveToTrash => {
                        fs::write(root.join("a/top.txt"), "changed content").expect("written");
                    }
                    Step::GiveBackEntry => return Err(io::Error::from_raw_os_error(libc::EIO)),
                    _ => {}
                }
                Ok(())
            }),
        );
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert!(piece_path(&world, "a/top.txt").exists());
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert_eq!(
            state_of(&world, "a"),
            crate::ops::status::PieceKind::Unearthed
        );
    }

    #[test]
    fn a_failing_entry_write_after_the_move_to_recovered_skips_the_piece() {
        let world = piece_with_subdirectory();
        let root = world.temp.root().to_path_buf();
        let report = run(
            &world,
            &Scripted::on(move |step, _| {
                match step {
                    Step::MoveToTrash => {
                        fs::write(root.join("a/top.txt"), "changed content").expect("written");
                    }
                    Step::GiveBackMove => {
                        fs::create_dir(root.join("a")).expect("a stranger's folder");
                    }
                    Step::RecoveredEntry => return Err(io::Error::from_raw_os_error(libc::EIO)),
                    _ => {}
                }
                Ok(())
            }),
        );
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert_eq!(recovered_names(&world).len(), 1);
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
        assert!(piece_path(&world, "a").is_dir(), "the stranger's folder");
    }

    #[test]
    fn a_failing_gone_write_still_reports_the_piece_as_archived() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::failing(Step::MarkGone, libc::EIO));
        let [done] = archived(&report) else {
            panic!("the piece is archived: {report:?}");
        };
        assert!(skipped(&report).is_empty());
        assert!(!piece_path(&world, "a").exists());
        assert!(report.warnings.iter().any(|w| w.message.contains("gone")));
        assert_eq!(
            entry(&world, done.id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert_eq!(
            entry(&world, done.id).expect("an entry").state,
            EntryState::Gone
        );
    }

    // ---- The rename is refused ----

    #[test]
    fn a_refused_rename_deletes_the_unchanged_folder_in_place() {
        let world = lone_piece("a");
        let report = run(&world, &Scripted::failing(Step::MoveToTrash, libc::EXDEV));
        let [done] = archived(&report) else {
            panic!("the piece is archived: {report:?}");
        };
        assert!(!piece_path(&world, "a").exists());
        assert_eq!(
            entry(&world, done.id).expect("an entry").state,
            EntryState::Gone
        );
    }

    #[test]
    fn a_refused_rename_and_a_changed_folder_keeps_the_folder() {
        let world = lone_piece("a");
        let root = world.temp.root().to_path_buf();
        let report = run(
            &world,
            &Scripted::on(move |step, _| {
                if step == Step::MoveToTrash {
                    fs::write(root.join("a/note.txt"), "changed content").expect("written");
                    return Err(io::Error::from_raw_os_error(libc::EXDEV));
                }
                Ok(())
            }),
        );
        assert_given_back(&world, &report, "a");
    }

    #[test]
    fn a_rename_and_a_delete_the_filesystem_refuses_leave_the_entry_removing() {
        let world = lone_piece("a");
        let root = world.temp.root().to_path_buf();
        let report = run(
            &world,
            &Scripted::before(Step::MoveToTrash, move || mode_of(&root, 0o555)),
        );
        mode_of(world.temp.root(), 0o755);
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert!(
            piece_path(&world, "a").is_dir(),
            "the delete got as far as emptying the folder"
        );
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert!(!piece_path(&world, "a").exists());
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
    }

    #[test]
    fn a_failing_delete_in_place_leaves_the_entry_for_the_repair() {
        let world = lone_piece("a");
        let seams = Scripted::with_effects(
            |step, _| {
                let _ = step;
            },
            world.temp.root(),
            Failing {
                remove_tree: true,
                ..Failing::default()
            },
        )
        .failing_at(Step::MoveToTrash, libc::EXDEV);
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
    }

    #[test]
    fn a_failing_delete_in_the_trash_leaves_the_entry_for_the_repair() {
        let world = lone_piece("a");
        let seams = Scripted::with_effects(
            |_, _| {},
            world.temp.root(),
            Failing {
                remove_tree_at: true,
                ..Failing::default()
            },
        );
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert_eq!(trash_names(&world).len(), 1);
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert_eq!(trash_names(&world), Vec::<String>::new());
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
    }

    #[test]
    fn a_flush_failure_after_the_rename_does_not_stop_the_archive() {
        let world = lone_piece("a");
        let seams = Scripted::with_effects(
            |_, _| {},
            world.temp.root(),
            Failing {
                flush_from: Some(0),
                ..Failing::default()
            },
        );
        let report = run(&world, &seams);
        let [done] = archived(&report) else {
            panic!("the piece is archived: {report:?}");
        };
        for expected in [
            "is in the trash but the move cannot be flushed",
            "is deleted but the trash cannot be flushed",
        ] {
            assert!(
                report.warnings.iter().any(|w| w.message.contains(expected)),
                "{expected}: {report:?}"
            );
        }
        assert_eq!(
            entry(&world, done.id).expect("an entry").state,
            EntryState::Gone
        );
    }

    // ---- A process that dies in the removal ----

    /// Dies before `step`, after the folder was changed so that it differs
    /// from its archive when `differs`, and after a stranger took its path
    /// when `taken`.
    fn crash_in_removal(step: Step, differs: bool, taken: bool) -> World {
        let world = piece_with_subdirectory();
        let root = world.temp.root().to_path_buf();
        let seams = Scripted::on(move |at, _| {
            if at == Step::MoveToTrash && differs {
                fs::write(root.join("a/top.txt"), "changed content").expect("written");
            }
            if at == Step::GiveBackMove && taken {
                fs::create_dir(root.join("a")).expect("a stranger's folder");
            }
            if at == step {
                panic!("the process dies before {at:?}");
            }
            Ok(())
        });
        let result = catch_unwind(AssertUnwindSafe(|| run(&world, &seams)));
        assert!(result.is_err(), "the run was cut short before {step:?}");
        world
    }

    fn only_object(world: &World) -> ObjectId {
        let [id] = objects(world)[..] else {
            panic!("one object");
        };
        id
    }

    #[test]
    fn a_crash_before_the_move_to_the_trash_leaves_the_folder_for_the_repair_to_delete() {
        let world = crash_in_removal(Step::MoveToTrash, false, false);
        let id = only_object(&world);
        assert!(piece_path(&world, "a/top.txt").exists());
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert!(!piece_path(&world, "a").exists());
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
    }

    macro_rules! crash_with_the_tree_in_the_trash {
        ($name:ident, $step:expr) => {
            #[test]
            fn $name() {
                let world = crash_in_removal($step, false, false);
                let id = only_object(&world);
                assert!(!piece_path(&world, "a").exists());
                assert_eq!(trash_names(&world).len(), 1);
                assert_eq!(
                    entry(&world, id).expect("an entry").state,
                    EntryState::Removing
                );
                repaired(&world);
                assert_eq!(trash_names(&world), Vec::<String>::new());
                assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
            }
        };
    }

    crash_with_the_tree_in_the_trash!(
        a_crash_before_the_rewalk_leaves_the_trash_to_the_repair,
        Step::Rewalk
    );
    crash_with_the_tree_in_the_trash!(
        a_crash_before_the_delete_leaves_the_trash_to_the_repair,
        Step::DeleteInTrash
    );

    #[test]
    fn a_crash_before_the_gone_entry_leaves_the_entry_for_the_repair() {
        let world = crash_in_removal(Step::MarkGone, false, false);
        let id = only_object(&world);
        assert_eq!(trash_names(&world), Vec::<String>::new());
        assert!(!piece_path(&world, "a").exists());
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
    }

    #[test]
    fn a_crash_before_the_move_back_leaves_the_different_tree_in_the_trash() {
        let world = crash_in_removal(Step::GiveBackMove, true, false);
        let id = only_object(&world);
        assert_eq!(trash_names(&world).len(), 1);
        repaired(&world);
        assert_eq!(trash_names(&world), Vec::<String>::new());
        assert_eq!(
            fs::read_to_string(piece_path(&world, "a/top.txt")).expect("back"),
            "changed content"
        );
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Live);
    }

    #[test]
    fn a_crash_before_the_unearthed_entry_leaves_the_tree_at_its_path() {
        let world = crash_in_removal(Step::GiveBackEntry, true, false);
        let id = only_object(&world);
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        let after = entry(&world, id).expect("an entry");
        assert_eq!(after.state, EntryState::Live);
        assert_eq!(after.unearthed_payload_version, Some(1));
        assert_eq!(
            fs::read_to_string(piece_path(&world, "a/top.txt")).expect("kept"),
            "changed content"
        );
    }

    #[test]
    fn a_crash_before_the_move_to_recovered_leaves_the_tree_in_the_trash() {
        let world = crash_in_removal(Step::MoveRecovered, true, true);
        let id = only_object(&world);
        assert_eq!(trash_names(&world).len(), 1);
        repaired(&world);
        assert_eq!(trash_names(&world), Vec::<String>::new());
        assert_eq!(recovered_names(&world).len(), 1);
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
    }

    #[test]
    fn a_crash_before_the_recovered_entry_leaves_a_stranger_alone() {
        let world = crash_in_removal(Step::RecoveredEntry, true, true);
        let id = only_object(&world);
        assert_eq!(recovered_names(&world).len(), 1);
        repaired(&world);
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
        assert!(piece_path(&world, "a").is_dir(), "the stranger's folder");
        assert_eq!(recovered_names(&world).len(), 1);
    }

    // ---- Deep trees ----

    #[test]
    fn a_piece_with_a_path_past_1024_bytes_archives() {
        use crate::testing::deep_path;
        let world = world();
        let collection = TreeRoot::open(world.temp.root()).expect("opens");
        let top = RelPath::parse("a").expect("valid");
        collection
            .create_dir(&top, 0o755)
            .expect("the piece is made");
        let piece = collection.sub(&top).expect("opens");
        let deep = deep_path(6);
        let old = i128::from(days_ago(30)) * 1_000_000_000;
        let mut at = RelPath::root();
        let mut directories = Vec::new();
        for component in deep.components() {
            at = at.join(&crate::relpath::Name::from_bytes(component.to_vec()).expect("a name"));
            piece.create_dir(&at, 0o755).expect("the directory is made");
            directories.push(at.clone());
        }
        let file = deep.concat(&RelPath::parse("data.txt").expect("valid"));
        piece
            .create_file(&file, 0o644)
            .expect("the file is made")
            .write_all(b"deep")
            .expect("written");
        piece.set_times(&file, old).expect("the time is set");
        for directory in directories.iter().rev() {
            piece.set_times(directory, old).expect("the time is set");
        }
        let report = run(&world, &Scripted::real());
        assert_eq!(archived(&report).len(), 1, "{report:?}");
        assert!(!piece_path(&world, "a").exists());
        assert_eq!(trash_names(&world), Vec::<String>::new());
    }

    // ---- Coverage of the refusals in the removal ----

    #[test]
    fn an_excluded_entry_that_appeared_before_an_in_place_delete_keeps_the_folder() {
        let mut world = world();
        world.configure("exclude = [\"a/skip\"]");
        world.file("a/note.txt", 30);
        let root = world.temp.root().to_path_buf();
        let report = run(
            &world,
            &Scripted::on(move |step, _| {
                if step == Step::MoveToTrash {
                    fs::create_dir(root.join("a/skip")).expect("a directory");
                    return Err(io::Error::from_raw_os_error(libc::EXDEV));
                }
                Ok(())
            }),
        );
        assert_given_back(&world, &report, "a");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("exclude list")),
            "{report:?}"
        );
    }

    #[test]
    fn a_walk_that_fails_after_the_move_counts_as_a_difference() {
        let world = lone_piece("a");
        let mut seams = Scripted::real();
        let out_of_descriptors = Arc::clone(&seams.mount.1);
        seams.on = Box::new(move |step, _| {
            if step == Step::Rewalk {
                out_of_descriptors.store(true, Ordering::SeqCst);
            }
            Ok(())
        });
        let report = run(&world, &seams);
        assert_given_back(&world, &report, "a");
    }

    #[test]
    fn a_failing_seam_before_the_move_back_sends_the_tree_to_recovered() {
        let world = piece_with_subdirectory();
        let root = world.temp.root().to_path_buf();
        let report = run(
            &world,
            &Scripted::on(move |step, _| {
                match step {
                    Step::MoveToTrash => {
                        fs::write(root.join("a/top.txt"), "changed content").expect("written");
                    }
                    Step::GiveBackMove => return Err(io::Error::from_raw_os_error(libc::EIO)),
                    _ => {}
                }
                Ok(())
            }),
        );
        assert_recovered(&world, &report);
    }

    #[test]
    fn two_failing_seams_keep_the_tree_in_the_trash() {
        let world = piece_with_subdirectory();
        let root = world.temp.root().to_path_buf();
        let report = run(
            &world,
            &Scripted::on(move |step, _| {
                match step {
                    Step::MoveToTrash => {
                        fs::write(root.join("a/top.txt"), "changed content").expect("written");
                    }
                    Step::GiveBackMove | Step::MoveRecovered => {
                        return Err(io::Error::from_raw_os_error(libc::EIO));
                    }
                    _ => {}
                }
                Ok(())
            }),
        );
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert_eq!(trash_names(&world).len(), 1);
    }

    #[test]
    fn a_flush_failure_after_the_move_to_recovered_leaves_the_entry_for_the_repair() {
        let world = piece_with_subdirectory();
        let seams = Scripted::with_effects(
            |step, root| match step {
                Step::MoveToTrash => {
                    fs::write(root.join("a/top.txt"), "changed content").expect("written");
                }
                Step::GiveBackMove => fs::create_dir(root.join("a")).expect("a stranger's folder"),
                _ => {}
            },
            world.temp.root(),
            Failing {
                // Two flushes for the rename to the trash; the move back is
                // refused before it flushes anything.
                flush_from: Some(2),
                ..Failing::default()
            },
        );
        let report = run(&world, &seams);
        let [row] = skipped(&report) else {
            panic!("the piece is skipped: {report:?}");
        };
        assert_eq!(row.reason, SkipReason::Error);
        assert_eq!(recovered_names(&world).len(), 1);
        let id = objects(&world)[0];
        assert_eq!(
            entry(&world, id).expect("an entry").state,
            EntryState::Removing
        );
        repaired(&world);
        assert_eq!(entry(&world, id).expect("an entry").state, EntryState::Gone);
    }
}
