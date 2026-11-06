// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `reconcile --prune`: which pieces this machine drops, and the drop.
//!
//! A piece is dropped when nothing on this machine could still be the
//! only copy of its work: no archive here, no change waiting to be
//! committed, no open decision and no folder in place. What remains is a
//! gone piece, or a piece whose archive the remote lost.

use super::PieceRef;
use super::remote_part::Part;
use crate::collection::Collection;
use crate::error::Error;
use crate::fsx::atomic::flush_dir;
use crate::fsx::remove::remove_tree_at;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::meta::{self, PayloadKind};
use crate::reference::Candidate;
use crate::report::Warning;
use crate::store::{Source, Store, StoreError};
use crate::tombstone::Tombstones;
use crate::tracking::{self, EntryState, PieceState, TrackingEntry};
use crate::tree::EntryKind;
use std::collections::BTreeSet;
use std::fs;
use std::io;

/// The pieces to drop and the set the brake measures them against.
pub(super) struct Dropping {
    pub pieces: Vec<PieceRef>,
    /// Every entry this machine tracks and every object in the store.
    pub set_size: usize,
    /// Pieces kept because their folder could not be looked at.
    pub notes: Vec<Warning>,
}

impl Dropping {
    pub fn candidates(&self) -> Vec<Candidate> {
        self.pieces
            .iter()
            .map(|piece| Candidate {
                id: piece.id.to_string(),
                path: piece.path.clone(),
            })
            .collect()
    }
}

/// Which pieces `part` leaves for a drop. A mark counts when this run set
/// it, and only a remote that was reached backs a mark: otherwise the
/// archive may still be there.
pub(super) fn plan(collection: &Collection, part: &Part) -> Result<Dropping, Error> {
    let store = Store::new(collection);
    let tracked = tracking::read_all(collection)?;
    let root =
        TreeRoot::open(collection.root()).map_err(|source| open_error(collection, source))?;
    // The objects a dry run would pull count as stored, as they do after a
    // real pull.
    let mut set: BTreeSet<ObjectId> = store.objects()?.into_iter().collect();
    set.extend(part.would_pull.keys().copied());
    let mut pieces = Vec::new();
    let mut notes = Vec::new();
    for entry in &tracked.entries {
        set.insert(entry.id);
        if droppable(collection, &root, &store, entry, part, &mut notes)? {
            pieces.push(PieceRef {
                id: entry.id,
                path: entry.path.to_string(),
            });
        }
    }
    Ok(Dropping {
        pieces,
        set_size: set.len(),
        notes,
    })
}

fn marked(entry: &TrackingEntry, part: &Part) -> bool {
    let set_now = part.marked.iter().any(|piece| piece.id == entry.id);
    let cleared = part.unmarked.iter().any(|piece| piece.id == entry.id);
    (entry.missing_on_remote || set_now) && !cleared
}

fn droppable(
    collection: &Collection,
    root: &TreeRoot,
    store: &Store,
    entry: &TrackingEntry,
    part: &Part,
    notes: &mut Vec<Warning>,
) -> Result<bool, Error> {
    // The start-of-run repairs finish a removal or an unearthing from the
    // entry, so a piece in either state stays.
    if matches!(entry.state, EntryState::Removing | EntryState::Unearthing) {
        return Ok(false);
    }
    let pulled = part.would_pull.get(&entry.id).cloned().unwrap_or_default();
    let Ok(Some(current)) = store.current_with(entry.id, &pulled) else {
        return Ok(false);
    };
    if current.source == Source::Pending
        || !entry.pending_decisions.is_empty()
        || entry
            .local_payloads
            .iter()
            .any(|held| held.kind == PayloadKind::Archive)
    {
        return Ok(false);
    }
    // Only a missing path means the folder is gone. Any other failure, such
    // as a parent nobody may enter, leaves it unknown whether the folder
    // holds the work, so the piece stays.
    let folder_present = match root.lstat(&entry.path) {
        Ok(found) => found.kind == EntryKind::Directory,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            false
        }
        Err(error) => {
            notes.push(Warning::new(format!(
                "{} was kept: its folder could not be looked at ({error})",
                entry.path
            )));
            return Ok(false);
        }
    };
    let state = tracking::derive_state(
        Some(entry),
        Some(&current.metadata),
        folder_present,
        collection.id(),
    );
    match state {
        Ok(PieceState::Gone) => {}
        Ok(PieceState::RemoteOnly) if part.reached && marked(entry, part) => {}
        _ => return Ok(false),
    }
    // An archive of an older version that is still in the directory is
    // a copy of the work.
    let directory = collection.object_dir(entry.id);
    let mut versions: Vec<Option<Vec<u8>>> = Vec::new();
    for number in store.committed_versions(entry.id)? {
        versions.push(fs::read(store.meta_path(entry.id, number)).ok());
    }
    versions.extend(pulled.values().cloned().map(Some));
    for bytes in versions {
        // A version that cannot be read may name an archive: keep the piece.
        let Some(metadata) = bytes.and_then(|bytes| meta::parse(&bytes).ok()) else {
            return Ok(false);
        };
        let kept = metadata
            .payloads
            .iter()
            .filter(|payload| payload.kind == PayloadKind::Archive)
            .any(|payload| directory.join(payload.file.as_str()).exists());
        if kept {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Drops `planned`, checking each piece again first. The record of the
/// drop is written before anything is removed, so that a run that stops
/// halfway is finished by the start-of-run repairs.
pub(super) fn execute(
    collection: &Collection,
    lock: &RunLock,
    part: &Part,
    planned: &[PieceRef],
) -> Result<Vec<PieceRef>, Error> {
    let store = Store::new(collection);
    let root =
        TreeRoot::open(collection.root()).map_err(|source| open_error(collection, source))?;
    let mut dropped = Vec::new();
    for piece in planned {
        let Some(entry) = tracking::read(collection, piece.id)? else {
            continue;
        };
        if !droppable(collection, &root, &store, &entry, part, &mut Vec::new())? {
            continue;
        }
        let highest = store
            .committed_versions(piece.id)?
            .last()
            .copied()
            .unwrap_or(0);
        let mut tombstones = Tombstones::read(collection).map_err(StoreError::from)?;
        tombstones.record(piece.id, highest);
        tombstones
            .write(collection, lock)
            .map_err(StoreError::from)?;

        tracking::remove(collection, lock, piece.id)?;
        remove_file(&collection.manifest_path(piece.id))?;
        remove_tree_at(&collection.object_dir(piece.id))?;
        flush_dir(&collection.objects_dir())?;
        remove_file(&collection.index_dir().join(format!("{}.json", piece.id)))?;
        dropped.push(piece.clone());
    }
    Ok(dropped)
}

fn remove_file(path: &std::path::Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(Error::Fsx(crate::fsx::FsxError::Io {
                action: "remove",
                path: path.display().to_string(),
                source: error,
            }))
        }
        _ => Ok(()),
    }
}

fn open_error(collection: &Collection, source: io::Error) -> Error {
    Error::Fsx(crate::fsx::FsxError::Io {
        action: "open",
        path: collection.root().display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::relpath::RelPath;
    use crate::remote::testing::{metadata_bytes, object};
    use crate::testing::tree::COLLECTION_ID;
    use crate::testing::tree::TempCollection;
    use crate::tracking::VersionTakenOut;

    struct World {
        _temp: TempCollection,
        collection: Collection,
        lock: RunLock,
    }

    /// A collection with the gone piece `alpha`.
    fn world() -> World {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        let clock = FixedClock::new("2026-11-04T12:00:00Z".parse().expect("a time"));
        let lock = crate::lock::acquire(&collection.lock_path(), "laptop", &clock).expect("free");
        let id = object(1);
        let directory = collection.object_dir(id);
        fs::create_dir_all(&directory).expect("dirs");
        let mut document: serde_json::Value =
            serde_json::from_slice(&metadata_bytes(id, COLLECTION_ID, "alpha", &[])).expect("json");
        document["lifecycle"] = "live".into();
        fs::write(directory.join("meta.1"), document.to_string()).expect("meta");
        let mut entry = TrackingEntry::new(
            id,
            RelPath::parse("alpha").expect("a path"),
            EntryState::Gone,
            VersionTakenOut::Committed(1),
            "2026-11-04T12:00:00Z".parse().expect("a time"),
        );
        tracking::write(&collection, &lock, &clock, &mut entry).expect("written");
        World {
            _temp: temp,
            collection,
            lock,
        }
    }

    fn planned(world: &World) -> Vec<PieceRef> {
        plan(&world.collection, &Part::default())
            .expect("plans")
            .pieces
    }

    #[test]
    fn a_gone_piece_is_planned() {
        let world = world();
        assert_eq!(planned(&world).len(), 1);
    }

    fn with_state(world: &World, state: EntryState) {
        let mut entry = tracking::read(&world.collection, object(1))
            .expect("reads")
            .expect("an entry");
        entry.state = state;
        let clock = FixedClock::new("2026-11-04T12:00:00Z".parse().expect("a time"));
        tracking::write(&world.collection, &world.lock, &clock, &mut entry).expect("written");
    }

    #[test]
    fn a_piece_that_is_being_removed_is_not_planned() {
        let world = world();
        with_state(&world, EntryState::Removing);
        assert!(planned(&world).is_empty());
    }

    #[test]
    fn a_piece_that_is_being_unearthed_is_not_planned() {
        let world = world();
        with_state(&world, EntryState::Unearthing);
        assert!(planned(&world).is_empty());
    }

    #[test]
    fn a_version_that_cannot_be_read_keeps_the_piece() {
        let world = world();
        fs::write(
            world.collection.object_dir(object(1)).join("meta.2"),
            b"{ no",
        )
        .expect("w");
        assert!(planned(&world).is_empty());
    }

    #[test]
    fn a_piece_that_is_no_longer_droppable_at_the_drop_is_kept() {
        let world = world();
        let pieces = planned(&world);
        fs::write(
            world.collection.meta_pending_path(object(1)),
            fs::read(world.collection.object_dir(object(1)).join("meta.1")).expect("meta"),
        )
        .expect("a recorded change");

        let dropped =
            execute(&world.collection, &world.lock, &Part::default(), &pieces).expect("executes");

        assert!(dropped.is_empty());
        assert!(world.collection.object_dir(object(1)).exists());
    }

    #[test]
    fn a_piece_whose_entry_is_gone_at_the_drop_is_skipped() {
        let world = world();
        let pieces = planned(&world);
        tracking::remove(&world.collection, &world.lock, object(1)).expect("removed");

        let dropped =
            execute(&world.collection, &world.lock, &Part::default(), &pieces).expect("executes");

        assert!(dropped.is_empty());
    }

    #[test]
    fn a_manifest_that_cannot_be_removed_stops_the_drop() {
        let world = world();
        let pieces = planned(&world);
        fs::create_dir(world.collection.manifest_path(object(1))).expect("a directory");

        let error = execute(&world.collection, &world.lock, &Part::default(), &pieces)
            .expect_err("a directory is no manifest");

        assert!(matches!(error, Error::Fsx(_)), "{error}");
    }
}
