// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Repairs at the start of a run.

use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::fsx::moves::Moved;
use crate::fsx::tree::TreeRoot;
use crate::fsx::{FsxError, atomic, moves, remove};
use crate::id::ObjectId;
use crate::listing::{self, ListingEntry, ListingType};
use crate::lock::RunLock;
use crate::machine::MachineError;
use crate::meta::{HistoryAction, HistoryEntry, Lifecycle, Metadata, Payload, PayloadKind};
use crate::pack::WalkRules;
use crate::pack::verify::{Compared, Difference, Field, compare_with_listing};
use crate::relpath::{Name, RelPath};
use crate::report::Warning;
use crate::store::{Source, Store, StoreError};
use crate::tracking::{
    self, EntryState, LocalPayload, TrackingEntry, TrackingError, VersionTakenOut,
};
use crate::tree::EntryKind;
use jiff::Timestamp;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Errors of the start-of-run repairs.
#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    #[error(transparent)]
    Tracking(#[from] TrackingError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
}

impl crate::error::HasKind for RepairError {
    fn kind(&self) -> crate::error::ErrorKind {
        match self {
            RepairError::Tracking(error) => error.kind(),
            RepairError::Store(error) => error.kind(),
            RepairError::Machine(error) => error.kind(),
            RepairError::Io { .. } => crate::error::ErrorKind::Other,
        }
    }
}

/// Everything the repairs change goes through these calls, so that tests
/// can make one of them fail or watch the order they come in.
pub(crate) trait Effects {
    fn remove_tree_at(&self, path: &Path) -> Result<(), FsxError>;
    fn remove_tree(&self, root: &TreeRoot, path: &RelPath) -> Result<(), FsxError>;
    fn move_dir(
        &self,
        from_root: &TreeRoot,
        from: &RelPath,
        to_root: &TreeRoot,
        to: &RelPath,
    ) -> Result<Moved, FsxError>;
    fn flush_dir(&self, dir: &Path) -> Result<(), FsxError>;
    /// Removes a file; a file that is not there counts as removed.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn write_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        clock: &dyn Clock,
        entry: &mut TrackingEntry,
    ) -> Result<(), TrackingError>;
    fn remove_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        id: ObjectId,
    ) -> Result<(), TrackingError>;
    /// Records `metadata` as the object's next version.
    fn record_metadata(
        &self,
        collection: &Collection,
        lock: &RunLock,
        metadata: &Metadata,
    ) -> Result<(), StoreError>;
}

/// The effects on the real file system.
pub(crate) struct RealEffects;

impl Effects for RealEffects {
    fn remove_tree_at(&self, path: &Path) -> Result<(), FsxError> {
        remove::remove_tree_at(path)
    }

    fn remove_tree(&self, root: &TreeRoot, path: &RelPath) -> Result<(), FsxError> {
        remove::remove_tree(root, path)
    }

    fn move_dir(
        &self,
        from_root: &TreeRoot,
        from: &RelPath,
        to_root: &TreeRoot,
        to: &RelPath,
    ) -> Result<Moved, FsxError> {
        moves::move_dir(from_root, from, to_root, to)
    }

    fn flush_dir(&self, dir: &Path) -> Result<(), FsxError> {
        atomic::flush_dir(dir)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn write_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        clock: &dyn Clock,
        entry: &mut TrackingEntry,
    ) -> Result<(), TrackingError> {
        tracking::write(collection, lock, clock, entry)
    }

    fn remove_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        id: ObjectId,
    ) -> Result<(), TrackingError> {
        tracking::remove(collection, lock, id)
    }

    fn record_metadata(
        &self,
        collection: &Collection,
        lock: &RunLock,
        metadata: &Metadata,
    ) -> Result<(), StoreError> {
        Store::new(collection).write_pending(lock, metadata)
    }
}

/// The metadata change of an unearth (spec "Metadata changes"): the object
/// is live, owned by `own`, at `path`, and its history says who unearthed
/// it and when. `unearth` and the repair of an interrupted `unearth` make
/// the same change, so the repair can finish what `unearth` started.
pub(crate) fn record_unearth(
    metadata: &Metadata,
    own: ObjectId,
    machine: &str,
    time: Timestamp,
    path: &RelPath,
) -> Metadata {
    let mut changed = metadata.clone();
    changed.lifecycle = Lifecycle::Live;
    changed.collection = own;
    changed.path = path.clone();
    changed.name = path
        .components()
        .last()
        .and_then(|name| Name::from_bytes(name.to_vec()).ok())
        .expect("the path of a piece ends in a valid name");
    changed
        .history
        .push(HistoryEntry::new(time, machine, HistoryAction::Unearth));
    changed
}

/// Whether the history of `metadata` already holds the `unearth` entry
/// that `machine` wrote at `time`.
pub(crate) fn records_unearth(metadata: &Metadata, machine: &str, time: Timestamp) -> bool {
    let time = whole_seconds(time);
    metadata.history.iter().any(|entry| {
        entry.action == HistoryAction::Unearth.as_str()
            && entry.machine == machine
            && entry.time == time
    })
}

/// Runs the repairs in the order of spec "Operations". The lock proves
/// that no other run changes the collection meanwhile.
///
/// Problems with one tree or one entry become warnings and the run goes on.
/// Only what stops every later step ends it: the collection root or the
/// machine name that cannot be found, the tracking or objects directory
/// that cannot be listed, an entry that cannot be written or removed.
pub(crate) fn run(
    collection: &Collection,
    lock: &RunLock,
    clock: &dyn Clock,
    effects: &dyn Effects,
) -> Result<Vec<Warning>, RepairError> {
    let root = TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;
    let machine = collection.machine_name()?;
    let tracked = tracking::read_all(collection)?;
    let mut repairs = Repairs {
        collection,
        lock,
        clock,
        effects,
        root,
        machine,
        entries: tracked.entries,
        protected: HashSet::new(),
        unreadable: tracked.damaged.iter().map(|damaged| damaged.id).collect(),
        warnings: Vec::new(),
    };

    // A tree in the trash whose entry cannot be read may be the only copy
    // of what the entry was about to remove, and no comparison is possible
    // without the entry.
    for damaged in &tracked.damaged {
        repairs.protected.insert(damaged.id);
        repairs.warn(format!(
            "the tracking entry of {} cannot be used ({}); its tree in the trash, if there is one, stays",
            damaged.id, damaged.error
        ));
    }

    // The order matters: a tree that differs from its archive has to leave
    // the trash before the trash is emptied.
    repairs.finish_removals()?;
    repairs.empty_run_directories();
    repairs.remove_write_leftovers();
    repairs.settle_unearthing()?;
    repairs.catch_up_with_archives()?;
    repairs.restore_missing_entries()?;
    repairs.remove_orphan_manifests();
    Ok(repairs.warnings)
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> RepairError {
    let path = path.display().to_string();
    move |source| RepairError::Io {
        action,
        path,
        source,
    }
}

/// The file name of a time in the name of a recovered tree. Fixed width
/// and UTC, so names sort by time.
fn stamp(time: Timestamp) -> String {
    time.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// The name of the tree of `id` in `recovered/`.
pub(crate) fn recovered_name(id: ObjectId, time: Timestamp) -> String {
    format!("{id}-{}", stamp(time))
}

/// How moving a trashed tree went.
pub(crate) enum Outcome {
    Done,
    /// The tree moved but the move could not be made durable. It is
    /// reported and the entry stays as it is.
    Stuck,
    /// The tree stayed in the trash.
    Refused(String),
}

/// Moves the trashed tree of `id` to `to` below `to_dir` and makes the
/// move durable before the caller writes the entry that depends on it:
/// if the entry reached the disk and the move did not, the next run
/// would find a tree in the trash with no `removing` entry.
///
/// `flush` is the directory that gains the tree. The warnings are for the
/// caller to report; a `Stuck` tree is for the caller to protect.
pub(crate) fn move_trashed(
    effects: &dyn Effects,
    collection: &Collection,
    id: ObjectId,
    to_dir: &Path,
    to: &RelPath,
    flush: &Path,
) -> (Outcome, Vec<Warning>) {
    let trash_dir = collection.trash_dir();
    let name = RelPath::parse(&id.to_string()).expect("an ID is a plain name");
    let roots = TreeRoot::open(&trash_dir).and_then(|from| Ok((from, TreeRoot::open(to_dir)?)));
    let (from_root, to_root) = match roots {
        Ok(roots) => roots,
        Err(error) => return (Outcome::Refused(error.to_string()), Vec::new()),
    };
    let moved = match effects.move_dir(&from_root, &name, &to_root, to) {
        Ok(moved) => moved,
        Err(error) => return (Outcome::Refused(error.to_string()), Vec::new()),
    };
    for dir in [flush, trash_dir.as_path()] {
        if let Err(error) = effects.flush_dir(dir) {
            let warning = Warning::new(format!(
                "the tree of {id} was moved but cannot be flushed: {error}"
            ));
            return (Outcome::Stuck, vec![warning]);
        }
    }
    let mut warnings = Vec::new();
    if let Some(error) = moved.mode_restore_error {
        warnings.push(Warning::new(format!(
            "cannot restore the mode of a moved tree: {error}"
        )));
    }
    (Outcome::Done, warnings)
}

/// Where the tree of a `removing` entry is.
enum Located {
    /// Archive step 3's rename happened.
    Trashed,
    /// The rename failed, or the delete has been going on in place.
    Folder,
    /// An earlier repair moved the tree to `recovered/` and was cut short
    /// before it could write the entry.
    Recovered,
    /// Whether the trash holds the tree could not be found out. The
    /// entry stays as it is.
    Unknown(String),
    Nothing,
}

/// Whether a tree may be deleted.
enum Verdict {
    /// It holds nothing the archive lacks.
    Nothing,
    /// It holds something the archive lacks, or it cannot be compared; the
    /// reason is for the user.
    Differs(String),
}

struct Repairs<'a> {
    collection: &'a Collection,
    lock: &'a RunLock,
    clock: &'a dyn Clock,
    effects: &'a dyn Effects,
    root: TreeRoot,
    /// The name of this machine, as payloads and history name their writer.
    machine: String,
    /// The entries as they stand now: updated with every write.
    entries: Vec<TrackingEntry>,
    /// Trees in the trash that the emptying must leave alone.
    protected: HashSet<ObjectId>,
    /// IDs whose tracking file exists and cannot be used.
    unreadable: HashSet<ObjectId>,
    warnings: Vec<Warning>,
}

impl Repairs<'_> {
    fn warn(&mut self, message: String) {
        self.warnings.push(Warning::new(message));
    }

    fn now(&self) -> Timestamp {
        whole_seconds(self.clock.now())
    }

    fn entry(&self, id: ObjectId) -> TrackingEntry {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .expect("only IDs of read entries are asked for")
            .clone()
    }

    /// Stores `entry` and makes it the one the later steps see.
    fn write(&mut self, mut entry: TrackingEntry) -> Result<(), RepairError> {
        self.effects
            .write_entry(self.collection, self.lock, self.clock, &mut entry)?;
        match self.entries.iter_mut().find(|known| known.id == entry.id) {
            Some(known) => *known = entry,
            None => self.entries.push(entry),
        }
        Ok(())
    }

    fn set_gone(&mut self, mut entry: TrackingEntry) -> Result<(), RepairError> {
        entry.state = EntryState::Gone;
        entry.removing_payload = None;
        self.write(entry)
    }

    /// The folder is in place, whole or not: the archive is durable and
    /// the folder is what was unearthed from it.
    fn set_unearthed(
        &mut self,
        mut entry: TrackingEntry,
        version: Option<u32>,
    ) -> Result<(), RepairError> {
        entry.state = EntryState::Live;
        entry.removing_payload = None;
        entry.unearthed_payload_version = version;
        entry.unearthed = Some(self.now());
        self.write(entry)
    }

    /// Whether another entry that has a folder claims `path`.
    fn held_by_other(&self, path: &RelPath, id: ObjectId) -> bool {
        self.entries
            .iter()
            .any(|other| other.id != id && other.path == *path && other.state != EntryState::Gone)
    }

    // =========================================================
    // Finishing `removing` entries
    // =========================================================

    fn finish_removals(&mut self) -> Result<(), RepairError> {
        let removing: Vec<ObjectId> = self
            .entries
            .iter()
            .filter(|entry| entry.state == EntryState::Removing)
            .map(|entry| entry.id)
            .collect();
        for id in removing {
            self.finish_removal(id)?;
        }
        Ok(())
    }

    fn finish_removal(&mut self, id: ObjectId) -> Result<(), RepairError> {
        let entry = self.entry(id);
        let shown = entry.path.to_string();
        match self.locate(&entry) {
            Located::Nothing => self.set_gone(entry),
            Located::Unknown(error) => {
                self.protected.insert(id);
                self.warn(format!(
                    "cannot tell whether the trash holds the tree of `{shown}` ({id}): {error}"
                ));
                Ok(())
            }
            Located::Recovered => {
                if self.folder_in_place(&entry) {
                    self.warn(format!(
                        "`{shown}` is not the tree of {id}; it is a new piece"
                    ));
                }
                self.set_gone(entry)
            }
            Located::Folder => {
                if self.held_by_other(&entry.path, id) {
                    self.warn(format!(
                        "`{shown}` belongs to another tracking entry; the tree of {id} is not claimed"
                    ));
                    return self.set_gone(entry);
                }
                let verdict = match self.root.sub(&entry.path) {
                    Ok(tree) => self.judge(&entry, &tree),
                    Err(error) => Verdict::Differs(error.to_string()),
                };
                match verdict {
                    Verdict::Nothing => match self.effects.remove_tree(&self.root, &entry.path) {
                        Ok(()) => self.set_gone(entry),
                        Err(error) => {
                            self.warn(format!("cannot remove `{shown}` ({id}): {error}"));
                            Ok(())
                        }
                    },
                    Verdict::Differs(reason) => {
                        self.warn(format!(
                            "`{shown}` differs from its archive ({reason}); it is kept as unearthed"
                        ));
                        let version = self.archive_version(&entry);
                        self.set_unearthed(entry, version)
                    }
                }
            }
            Located::Trashed => self.finish_trashed(entry),
        }
    }

    fn finish_trashed(&mut self, entry: TrackingEntry) -> Result<(), RepairError> {
        let id = entry.id;
        let shown = entry.path.to_string();
        let trash = self.collection.trash_path(id);
        let tree = match TreeRoot::open(&trash) {
            Ok(tree) => tree,
            Err(error) => {
                self.protected.insert(id);
                self.warn(format!(
                    "cannot open the trashed tree of `{shown}` ({id}): {error}"
                ));
                return Ok(());
            }
        };
        let reason = match self.judge(&entry, &tree) {
            Verdict::Nothing => match self.effects.remove_tree_at(&trash) {
                Ok(()) => return self.set_gone(entry),
                Err(error) => {
                    self.protected.insert(id);
                    self.warn(format!(
                        "cannot remove the trashed tree of `{shown}` ({id}): {error}"
                    ));
                    return Ok(());
                }
            },
            Verdict::Differs(reason) => reason,
        };

        // The tree holds something the archive lacks. It goes back to its
        // path, or to recovered/ when the path is not free.
        let version = self.archive_version(&entry);
        let mut why = "another entry holds its path".to_string();
        if !self.held_by_other(&entry.path, id) {
            let target = self
                .collection
                .root()
                .join(OsStr::from_bytes(entry.path.as_bytes()));
            let parent = target.parent().unwrap_or(self.collection.root());
            match self.move_durably(id, self.collection.root(), &entry.path, parent) {
                Outcome::Done => {
                    self.warn(format!(
                        "`{shown}` differs from its archive ({reason}); it is kept as unearthed"
                    ));
                    return self.set_unearthed(entry, version);
                }
                Outcome::Stuck => return Ok(()),
                Outcome::Refused(refusal) => why = format!("it could not go back ({refusal})"),
            }
        }

        let name = recovered_name(id, self.now());
        let recovered = self.collection.recovered_dir();
        let to = RelPath::parse(&name).expect("an ID and a time make a plain name");
        match self.move_durably(id, &recovered, &to, &recovered) {
            Outcome::Done => {
                self.warn(format!(
                    "`{shown}` differs from its archive ({reason}) and {why}; it is kept in .antiq/recovered/{name}"
                ));
                self.set_gone(entry)
            }
            Outcome::Stuck => Ok(()),
            Outcome::Refused(refusal) => {
                self.protected.insert(id);
                self.warn(format!(
                    "the trashed tree of `{shown}` ({id}) differs from its archive and could not be moved ({refusal}); it stays in the trash"
                ));
                Ok(())
            }
        }
    }

    /// Moves the trashed tree of `id` with [`move_trashed`]. A move that
    /// could not be made durable protects the tree from the emptying.
    fn move_durably(&mut self, id: ObjectId, to_dir: &Path, to: &RelPath, flush: &Path) -> Outcome {
        let (outcome, warnings) =
            move_trashed(self.effects, self.collection, id, to_dir, to, flush);
        if matches!(outcome, Outcome::Stuck) {
            self.protected.insert(id);
        }
        self.warnings.extend(warnings);
        outcome
    }

    fn locate(&self, entry: &TrackingEntry) -> Located {
        match fs::symlink_metadata(self.collection.trash_path(entry.id)) {
            Ok(_) => return Located::Trashed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Located::Unknown(error.to_string()),
        }
        if self.moved_to_recovered(entry) {
            Located::Recovered
        } else if self.folder_in_place(entry) {
            Located::Folder
        } else {
            Located::Nothing
        }
    }

    fn folder_in_place(&self, entry: &TrackingEntry) -> bool {
        self.root
            .lstat(&entry.path)
            .is_ok_and(|found| found.kind == EntryKind::Directory)
    }

    /// Whether `recovered/` holds a tree of this entry's ID that is newer
    /// than the entry: only a repair of this removal can have put it there.
    fn moved_to_recovered(&self, entry: &TrackingEntry) -> bool {
        let Ok(items) = fs::read_dir(self.collection.recovered_dir()) else {
            return false;
        };
        let prefix = format!("{}-", entry.id);
        let since = stamp(entry.written);
        items.flatten().any(|item| {
            item.file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .is_some_and(|moved_at| moved_at >= since.as_str())
        })
    }

    // =========================================================
    // Comparing a tree with its archive
    // =========================================================

    /// Whether `tree` holds nothing that the archive of `entry` lacks. A
    /// tree that only misses paths is what an interrupted delete leaves.
    /// Anything that cannot be compared counts as holding something.
    fn judge(&self, entry: &TrackingEntry, tree: &TreeRoot) -> Verdict {
        let payload = match self.archive_payload(entry) {
            Ok(payload) => payload,
            Err(reason) => return Verdict::Differs(reason),
        };
        let (listing, compared) = match self.compare(entry.id, &payload, tree) {
            Ok(both) => both,
            Err(reason) => return Verdict::Differs(reason),
        };
        match compared
            .differences()
            .iter()
            .find(|difference| !only_missing(difference, &listing, tree))
        {
            None => Verdict::Nothing,
            Some(difference) => Verdict::Differs(difference.to_string()),
        }
    }

    /// The listing of `payload` and what differs between it and `tree`.
    fn compare(
        &self,
        id: ObjectId,
        payload: &Payload,
        tree: &TreeRoot,
    ) -> Result<(Vec<ListingEntry>, Compared), String> {
        let listing = self.read_listing(id, payload)?;
        let rules = WalkRules {
            junk: self.junk_names(&listing),
            staged: false,
        };
        let compared =
            compare_with_listing(&listing, tree, &rules).map_err(|error| error.to_string())?;
        Ok((listing, compared))
    }

    /// The archive payload that a `removing` entry names.
    fn archive_payload(&self, entry: &TrackingEntry) -> Result<Payload, String> {
        let file = entry
            .removing_payload
            .as_ref()
            .ok_or("the entry names no archive payload")?;
        let current = Store::new(self.collection)
            .current(entry.id)
            .map_err(|error| error.to_string())?
            .ok_or("the object has no metadata")?;
        current
            .metadata
            .payloads
            .into_iter()
            .find(|payload| payload.kind == PayloadKind::Archive && payload.file == *file)
            .ok_or_else(|| format!("the metadata has no payload {}", file.as_str()))
    }

    fn archive_version(&self, entry: &TrackingEntry) -> Option<u32> {
        self.archive_payload(entry)
            .ok()
            .map(|payload| payload.version)
    }

    fn read_listing(&self, id: ObjectId, payload: &Payload) -> Result<Vec<ListingEntry>, String> {
        let name = payload
            .listing
            .as_ref()
            .ok_or("the payload has no listing")?;
        let path = Store::new(self.collection).file_path(id, name);
        let file = File::open(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        listing::read_all(file).map_err(|error| error.to_string())
    }

    /// The configured junk names, less the ones that the archive holds as
    /// files: a name that became junk after packing must not hide an edit
    /// of a file the archive has.
    fn junk_names(&self, listing: &[ListingEntry]) -> Vec<Name> {
        let archived: HashSet<&[u8]> = listing
            .iter()
            .filter(|entry| entry.kind != ListingType::Dir)
            .filter_map(|entry| entry.path.components().last())
            .collect();
        self.collection
            .config()
            .collection
            .junk
            .iter()
            .filter(|junk| !archived.contains(junk.as_bytes()))
            .filter_map(|junk| Name::from_bytes(junk.as_bytes().to_vec()).ok())
            .collect()
    }

    // =========================================================
    // `unearthing` entries
    // =========================================================

    /// An `unearthing` entry is `unearth` between the entry and its `live`
    /// write: the folder arrives, then the metadata change is recorded.
    ///
    /// When the metadata already records the unearth, the folder is the
    /// unearthed piece whatever it holds: a difference from the archive is
    /// the user's edit, and dropping the entry would lose the payload
    /// version that reclaiming needs. Otherwise a folder that equals the
    /// archive it names gets the metadata change recorded first. Any
    /// other entry goes back to what it was before the unearth, and a
    /// folder at its path is a new piece.
    fn settle_unearthing(&mut self) -> Result<(), RepairError> {
        let unearthing: Vec<ObjectId> = self
            .entries
            .iter()
            .filter(|entry| entry.state == EntryState::Unearthing)
            .map(|entry| entry.id)
            .collect();
        for id in unearthing {
            let mut entry = self.entry(id);
            let present = self.folder_in_place(&entry);
            let usable = present && !self.held_by_other(&entry.path, id);
            // Metadata that cannot be read says nothing about the unearth; the
            // entry waits for a run that can read it.
            let current = match Store::new(self.collection).current(id) {
                Ok(current) => current,
                Err(error) => {
                    self.warn(format!(
                        "cannot read the metadata of {id}: {error}; its unearthing entry stays for the next run"
                    ));
                    continue;
                }
            };
            let recorded = current.as_ref().is_some_and(|current| {
                records_unearth(&current.metadata, &self.machine, entry.written)
            });

            if usable && recorded {
                let source = current.map_or(Source::Pending, |current| current.source);
                self.set_unearth_live(entry, source)?;
                continue;
            }
            if usable
                && self.is_unearthed_tree(&entry)
                && let Some(current) = &current
            {
                let changed = record_unearth(
                    &current.metadata,
                    self.collection.id(),
                    &self.machine,
                    entry.written,
                    &entry.path,
                );
                match self
                    .effects
                    .record_metadata(self.collection, self.lock, &changed)
                {
                    Ok(()) => self.set_unearth_live(entry, Source::Pending)?,
                    Err(error) => self.warn(format!(
                        "cannot record the unearth of {id}: {error}; its entry stays for the next run"
                    )),
                }
                continue;
            }
            // A piece that this collection owns and that is live stays at
            // its path whatever stands there, so nothing there is new.
            if present && !self.owns_live(id) {
                self.warn(format!(
                    "`{}` is not the unearthed tree of {id}; it is a new piece",
                    entry.path
                ));
            }
            if let Some(current) = &current {
                if recorded {
                    entry.version_taken_out = taken_out(current.source);
                } else {
                    // An unearth to another path wrote the target into the
                    // entry; the object is still where its metadata says.
                    entry.path = current.metadata.path.clone();
                }
            }
            self.drop_unearthing(entry)?;
        }
        Ok(())
    }

    /// The `unearthing` entry becomes `live`, unearthed when it was written.
    fn set_unearth_live(
        &mut self,
        mut entry: TrackingEntry,
        source: Source,
    ) -> Result<(), RepairError> {
        entry.state = EntryState::Live;
        entry.unearthed = Some(entry.written);
        entry.version_taken_out = taken_out(source);
        self.write(entry)
    }

    /// Whether the metadata of `id` says live and owned by this collection.
    fn owns_live(&self, id: ObjectId) -> bool {
        matches!(
            Store::new(self.collection).current(id),
            Ok(Some(current))
                if current.metadata.collection == self.collection.id()
                    && current.metadata.lifecycle == Lifecycle::Live
        )
    }

    /// Whether the folder at the entry's path equals the listing of the
    /// archive payload version that the entry was unearthed from.
    fn is_unearthed_tree(&self, entry: &TrackingEntry) -> bool {
        let Some(version) = entry.unearthed_payload_version else {
            return false;
        };
        let Ok(Some(current)) = Store::new(self.collection).current(entry.id) else {
            return false;
        };
        // Two machines can both make version N. The ones this machine
        // holds are the candidates, and the folder may be any of them;
        // without a local one every payload of the version is.
        let of_version: Vec<&Payload> = current
            .metadata
            .payloads
            .iter()
            .filter(|payload| payload.kind == PayloadKind::Archive && payload.version == version)
            .collect();
        let held: Vec<&Payload> = of_version
            .iter()
            .copied()
            .filter(|payload| {
                entry
                    .local_payloads
                    .iter()
                    .any(|local| local.file == payload.file)
            })
            .collect();
        let candidates = if held.is_empty() { of_version } else { held };
        let Ok(tree) = self.root.sub(&entry.path) else {
            return false;
        };
        candidates.into_iter().any(|payload| {
            self.compare(entry.id, payload, &tree)
                .is_ok_and(|(_, compared)| compared.differences().is_empty())
        })
    }

    /// Takes the entry back to `gone` when it holds what an archived piece
    /// keeps, and removes it otherwise.
    fn drop_unearthing(&mut self, mut entry: TrackingEntry) -> Result<(), RepairError> {
        if entry.local_payloads.is_empty() && entry.committed_versions.is_empty() {
            self.effects
                .remove_entry(self.collection, self.lock, entry.id)?;
            self.entries.retain(|known| known.id != entry.id);
            return Ok(());
        }
        entry.unearthed = None;
        entry.unearthed_payload_version = None;
        self.set_gone(entry)
    }

    // =========================================================
    // Entries that lag behind the metadata
    // =========================================================

    /// A `live` entry with a folder whose metadata is archived and owned
    /// here, and whose newest archive payload from this machine is newer
    /// than the entry's unearthing, is an archive that died after its
    /// metadata and before its `removing` entry: the folder is what was
    /// archived, so it is unearthed from that payload, now.
    fn catch_up_with_archives(&mut self) -> Result<(), RepairError> {
        let live: Vec<ObjectId> = self
            .entries
            .iter()
            .filter(|entry| entry.state == EntryState::Live)
            .map(|entry| entry.id)
            .collect();
        let store = Store::new(self.collection);
        for id in live {
            let mut entry = self.entry(id);
            if !self.folder_in_place(&entry) {
                continue;
            }
            let Ok(Some(current)) = store.current(id) else {
                continue;
            };
            let metadata = &current.metadata;
            if metadata.lifecycle != Lifecycle::Archived
                || metadata.collection != self.collection.id()
            {
                continue;
            }
            let newest = metadata
                .payloads
                .iter()
                .filter(|payload| {
                    payload.kind == PayloadKind::Archive
                        && !payload.superseded
                        && payload.machine == self.machine
                })
                .max_by_key(|payload| (payload.created, payload.version));
            let Some(payload) = newest.filter(|payload| {
                entry
                    .unearthed
                    .is_none_or(|unearthed| payload.created > unearthed)
                    && Some(payload.version) != entry.unearthed_payload_version
            }) else {
                continue;
            };

            entry.unearthed_payload_version = Some(payload.version);
            entry.unearthed = Some(self.now());
            entry.version_taken_out = taken_out(current.source);
            let held = entry
                .local_payloads
                .iter()
                .any(|local| local.file == payload.file);
            if !held && store.file_path(id, &payload.file).exists() {
                entry.local_payloads.push(LocalPayload {
                    kind: PayloadKind::Archive,
                    version: payload.version,
                    file: payload.file.clone(),
                });
            }
            self.write(entry)?;
        }
        Ok(())
    }

    // =========================================================
    // Metadata without an entry
    // =========================================================

    /// Writes the entry of an object that this collection owns, whose
    /// metadata says live and which has none: the run died between the
    /// metadata of a new ID and its entry. The folder is in place, or the
    /// piece is gone.
    fn restore_missing_entries(&mut self) -> Result<(), RepairError> {
        let store = Store::new(self.collection);
        for id in store.objects()? {
            if self.entries.iter().any(|entry| entry.id == id) || self.unreadable.contains(&id) {
                continue;
            }
            let current = match store.current(id) {
                Ok(Some(current)) => current,
                Ok(None) => continue,
                Err(error) => {
                    self.warn(format!("cannot read the metadata of {id}: {error}"));
                    continue;
                }
            };
            let metadata = &current.metadata;
            if metadata.collection != self.collection.id() || metadata.lifecycle != Lifecycle::Live
            {
                continue;
            }
            let present = self
                .root
                .lstat(&metadata.path)
                .is_ok_and(|found| found.kind == EntryKind::Directory);
            if present && self.held_by_other(&metadata.path, id) {
                self.warn(format!(
                    "`{}` belongs to another tracking entry; {id} gets none",
                    metadata.path
                ));
                continue;
            }
            let state = if present {
                EntryState::Live
            } else {
                EntryState::Gone
            };
            let entry = TrackingEntry::new(
                id,
                metadata.path.clone(),
                state,
                taken_out(current.source),
                self.now(),
            );
            self.write(entry)?;
        }
        Ok(())
    }

    // =========================================================
    // Emptying the run directories
    // =========================================================

    fn empty_run_directories(&mut self) {
        let trash = self.collection.trash_dir();
        let directories = [
            trash.clone(),
            self.collection.tmp_dir(),
            self.collection.staging_dir(),
        ];
        for directory in directories {
            let items = match fs::read_dir(&directory) {
                Ok(items) => items,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    self.warn(format!("cannot list {}: {error}", directory.display()));
                    continue;
                }
            };
            for item in items.flatten() {
                if directory == trash && self.is_protected(&item.file_name()) {
                    continue;
                }
                if let Err(error) = self.effects.remove_tree_at(&item.path()) {
                    self.warn(format!("cannot empty {}: {error}", item.path().display()));
                }
            }
        }
    }

    fn is_protected(&self, name: &OsStr) -> bool {
        name.to_str()
            .and_then(|name| name.parse::<ObjectId>().ok())
            .is_some_and(|id| self.protected.contains(&id))
    }

    // =========================================================
    // Leftovers of atomic writes
    // =========================================================

    /// Removes the temp files that a write cut short left in `.antiq/`.
    /// The run lock is ours, so no write of this collection is under way.
    /// The run directories are emptied already and `recovered/` holds
    /// what users may still want, so they are not entered.
    fn remove_write_leftovers(&mut self) {
        let skipped = [
            self.collection.recovered_dir(),
            self.collection.trash_dir(),
            self.collection.tmp_dir(),
            self.collection.staging_dir(),
        ];
        let mut pending = vec![self.collection.antiq_dir()];
        while let Some(directory) = pending.pop() {
            let items = match fs::read_dir(&directory) {
                Ok(items) => items,
                Err(error) => {
                    self.warn(format!("cannot list {}: {error}", directory.display()));
                    continue;
                }
            };
            for item in items.flatten() {
                let path = item.path();
                let Ok(kind) = item.file_type() else { continue };
                if kind.is_dir() {
                    if !skipped.contains(&path) {
                        pending.push(path);
                    }
                } else if kind.is_file() && is_write_leftover(&item.file_name()) {
                    self.remove_file(&path);
                }
            }
        }
    }

    // =========================================================
    // Manifests
    // =========================================================

    /// Removes the live manifests of IDs that have no tracking entry: a
    /// manifest belongs to an entry and is written after it. The entry of
    /// an ID whose file cannot be read may still exist, so its manifest
    /// stays.
    fn remove_orphan_manifests(&mut self) {
        let directory = self.collection.tracking_dir();
        let Ok(items) = fs::read_dir(&directory) else {
            return;
        };
        for item in items.flatten() {
            let Some(id) = manifest_id(&item.file_name()) else {
                continue;
            };
            let known =
                self.entries.iter().any(|entry| entry.id == id) || self.unreadable.contains(&id);
            if !known {
                self.remove_file(&item.path());
            }
        }
    }

    fn remove_file(&mut self, path: &Path) {
        if let Err(error) = self.effects.remove_file(path) {
            self.warn(format!("cannot remove {}: {error}", path.display()));
        }
    }
}

/// Whether a name is `.<name>.<pid>.<counter>.tmp`, what `write_atomic`
/// names its temp file.
fn is_write_leftover(name: &OsStr) -> bool {
    let Some(stem) = name
        .to_str()
        .and_then(|name| name.strip_prefix('.'))
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = stem.rsplitn(3, '.');
    let counter = parts.next().unwrap_or_default();
    let pid = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    digits(counter) && digits(pid) && !target.is_empty()
}

/// The ID of a file named `<ULID>.manifest.json`, in the spelling the
/// collection writes.
fn manifest_id(name: &OsStr) -> Option<ObjectId> {
    let name = name.to_str()?;
    let id: ObjectId = name.strip_suffix(".manifest.json")?.parse().ok()?;
    (name == format!("{id}.manifest.json")).then_some(id)
}

fn taken_out(source: Source) -> VersionTakenOut {
    match source {
        Source::Pending => VersionTakenOut::Pending,
        Source::Committed(number) => VersionTakenOut::Committed(number),
    }
}

/// Whether a difference between a listing and a tree is a path that is
/// missing, or a directory whose mode is the listed one with owner access
/// added, which is what removing the contents of a locked directory leaves.
fn only_missing(difference: &Difference, listing: &[ListingEntry], tree: &TreeRoot) -> bool {
    match difference {
        Difference::Removed(_) => true,
        Difference::Changed { path, fields } if fields.as_slice() == [Field::Mode] => {
            const OWNER: u32 = 0o700;
            const BITS: u32 = 0o7777;
            let listed = listing.iter().find(|entry| entry.path == *path);
            let found = tree.lstat(path);
            match (listed, found) {
                (Some(listed), Ok(found)) => {
                    listed.kind == ListingType::Dir
                        && listed.mode & BITS & !OWNER == found.mode & BITS & !OWNER
                }
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::config::Config;
    use crate::error::{ErrorKind, HasKind};
    use crate::id::ObjectId;
    use crate::listing;
    use crate::lock;
    use crate::meta::{
        self, Checksum, CreatedSource, FileName, HistoryAction, HistoryEntry, Lifecycle, Metadata,
        Payload, PayloadKind,
    };
    use crate::relpath::Name;
    use crate::testing::package::pack_piece;
    use crate::testing::tree::{COLLECTION_ID, TempCollection, Tree};
    use crate::tracking::{EntryState, LocalPayload, VersionTakenOut};
    use serde_json::Map;
    use std::cell::{Cell, RefCell};
    use std::fs::{self, File, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
    const OTHER_ID: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    const ARCHIVED_AT: &str = "2026-11-03T10:00:00Z";
    const NOW: &str = "2026-11-04T12:00:00Z";
    /// When the entries of the scenarios were last written.
    const WRITTEN: &str = "2026-11-04T09:00:00Z";
    const NOW_STAMP: &str = "20261104T120000Z";
    const FILE_TIME: i64 = 1_000_000_000;

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a valid ID")
    }

    fn time(text: &str) -> jiff::Timestamp {
        text.parse().expect("a valid time")
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    /// An archive as the repairs find it: its payload and listing in the
    /// object's directory and the metadata that names them.
    struct Archived {
        payload: FileName,
        version: u32,
    }

    /// A collection with the run lock held, a clock standing at [`NOW`]
    /// and helpers to build the states a crashed run leaves behind.
    struct World {
        temp: TempCollection,
        collection: Collection,
        clock: FixedClock,
        lock: RunLock,
    }

    impl World {
        fn new() -> World {
            let temp = TempCollection::minimal();
            let mut config = Config::new(id(COLLECTION_ID));
            config.machine.name = Some("imperius".to_string());
            let collection = Collection::new(temp.root().to_path_buf(), config);
            let clock = FixedClock::new(time(NOW));
            let lock = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("a lock");
            World {
                temp,
                collection,
                clock,
                lock,
            }
        }

        fn tree(&mut self) -> &mut Tree {
            self.temp.tree()
        }

        fn at(&self, relative: &str) -> PathBuf {
            self.temp.root().join(relative)
        }

        fn trash(&self, id: &str) -> PathBuf {
            self.at(&format!(".antiq/trash/{id}"))
        }

        fn run(&self) -> Vec<Warning> {
            self.run_with(&RealEffects).expect("the repairs run")
        }

        fn run_with(&self, effects: &dyn Effects) -> Result<Vec<Warning>, RepairError> {
            run(&self.collection, &self.lock, &self.clock, effects)
        }

        fn entry(&self, id: &str) -> TrackingEntry {
            tracking::read(&self.collection, super::tests::id(id))
                .expect("the entry reads")
                .expect("the entry exists")
        }

        fn store_entry(&self, entry: &TrackingEntry) {
            let mut bytes = serde_json::to_vec(entry).expect("an entry serialises");
            bytes.push(b'\n');
            fs::write(self.collection.tracking_path(entry.id), bytes).expect("the entry is stored");
        }

        /// A folder `piece` holding `files`, its archive (payload version
        /// 1, listing and committed metadata) and nothing else: the state
        /// of archive step 2.
        fn archived(&mut self, object: &str, piece: &str, files: &[(&str, &str)]) -> Archived {
            for (name, content) in files {
                self.tree()
                    .file_with(&format!("{piece}/{name}"), content, 0o644, FILE_TIME);
            }
            // Directories carry the time they were made, which would make
            // every build of the same scenario pack to other bytes.
            let fixed = filetime::FileTime::from_unix_time(FILE_TIME, 0);
            let mut directories = vec![self.at(piece)];
            for (name, _) in files {
                let mut parent = self.at(&format!("{piece}/{name}"));
                while parent.pop() && parent != self.at(piece) {
                    directories.push(parent.clone());
                }
            }
            for directory in directories {
                filetime::set_file_mtime(directory, fixed).expect("the time can be set");
            }
            let packaged = pack_piece(self.tree(), piece);
            let sha = packaged.packed.sha256.clone();
            let payload = FileName::new(format!("{}-a.tar.zst", &sha[..16])).expect("a name");
            let listing_file = listing::file_name(&sha).expect("a listing name");

            let object_dir = self.collection.object_dir(super::tests::id(object));
            fs::create_dir_all(&object_dir).expect("the object directory");
            let out = File::create(object_dir.join(listing_file.as_str())).expect("a file");
            listing::write(packaged.packed.listing.iter(), out).expect("the listing is written");

            let top = path(piece);
            let name = top
                .components()
                .last()
                .map(|name| Name::from_bytes(name.to_vec()).expect("a name"))
                .expect("a piece has a name");
            let archived_at = time(ARCHIVED_AT);
            let metadata = Metadata {
                format: meta::FORMAT,
                id: super::tests::id(object),
                collection: id(COLLECTION_ID),
                path: top,
                name,
                created: archived_at,
                created_source: CreatedSource::Birth,
                lifecycle: Lifecycle::Archived,
                payloads: vec![Payload {
                    kind: PayloadKind::Archive,
                    version: 1,
                    file: payload.clone(),
                    package_format: "tar+zstd/1".to_string(),
                    size: packaged.packed.size,
                    checksum: Checksum::sha256(sha),
                    listing: Some(listing_file),
                    machine: "imperius".to_string(),
                    created: archived_at,
                    verified: None,
                    superseded: false,
                    encoder: None,
                    extra: Map::new(),
                }],
                history: vec![HistoryEntry::new(
                    archived_at,
                    "imperius",
                    HistoryAction::Archive,
                )],
                extra: Map::new(),
            };
            fs::write(
                object_dir.join("meta.1"),
                meta::to_bytes(&metadata).expect("the metadata serialises"),
            )
            .expect("the metadata is stored");
            Archived {
                payload,
                version: 1,
            }
        }

        /// The tracking entry archive step 3 leaves: `removing`, naming
        /// the payload the folder is compared with.
        fn removing(&self, object: &str, piece: &str, archive: &Archived) -> TrackingEntry {
            let mut entry = TrackingEntry::new(
                id(object),
                path(piece),
                EntryState::Removing,
                VersionTakenOut::Committed(1),
                time(ARCHIVED_AT),
            );
            entry.removing_payload = Some(archive.payload.clone());
            entry.local_payloads = vec![LocalPayload {
                kind: PayloadKind::Archive,
                version: archive.version,
                file: archive.payload.clone(),
            }];
            self.store_entry(&entry);
            entry
        }

        /// Changes the committed metadata document of `object`.
        fn edit_meta(&self, object: &str, edit: impl FnOnce(&mut serde_json::Value)) {
            let path = self.collection.object_dir(id(object)).join("meta.1");
            let mut document: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).expect("reads")).expect("json");
            edit(&mut document);
            fs::write(&path, serde_json::to_vec(&document).expect("json")).expect("written");
        }

        /// A further archive payload in the metadata, as a later archive
        /// leaves it. Its file exists in the object's directory.
        fn add_archive(&self, object: &str, version: u32, machine: &str, created: &str) -> String {
            let file = format!("{version:016x}-a.tar.zst");
            self.edit_meta(object, |document| {
                let mut payload = document["payloads"][0].clone();
                payload["version"] = serde_json::json!(version);
                payload["file"] = serde_json::json!(file);
                payload["machine"] = serde_json::json!(machine);
                payload["created"] = serde_json::json!(created);
                document["payloads"]
                    .as_array_mut()
                    .expect("payloads")
                    .push(payload);
            });
            fs::write(
                self.collection.object_dir(id(object)).join(&file),
                "payload",
            )
            .expect("the payload file");
            file
        }

        /// An entry in state `state` for `piece`, stored.
        fn entry_in(&self, object: &str, piece: &str, state: EntryState) -> TrackingEntry {
            let entry = TrackingEntry::new(
                id(object),
                path(piece),
                state,
                VersionTakenOut::Committed(1),
                time(WRITTEN),
            );
            self.store_entry(&entry);
            entry
        }

        /// Archive step 3's rename.
        fn to_trash(&self, object: &str, piece: &str) {
            fs::rename(self.at(piece), self.trash(object)).expect("the folder moves to the trash");
        }
    }

    const FILES: &[(&str, &str)] = &[("notes.txt", "hello"), ("src/main.rs", "fn main() {}")];

    /// Effects that log every call and fail the ones `fails` names. Called
    /// with the name of the effect and what it acts on.
    struct Scripted<'a> {
        log: RefCell<Vec<String>>,
        fails: &'a dyn Fn(&str, &str) -> bool,
        restore_error: Cell<bool>,
    }

    impl<'a> Scripted<'a> {
        fn new(fails: &'a dyn Fn(&str, &str) -> bool) -> Scripted<'a> {
            Scripted {
                log: RefCell::new(Vec::new()),
                fails,
                restore_error: Cell::new(false),
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn call(&self, what: &str, subject: &str) -> Result<(), FsxError> {
            self.log.borrow_mut().push(format!("{what} {subject}"));
            if (self.fails)(what, subject) {
                return Err(FsxError::Io {
                    action: "test",
                    path: subject.to_string(),
                    source: io::Error::other("refused by the test"),
                });
            }
            Ok(())
        }
    }

    impl Effects for Scripted<'_> {
        fn remove_tree_at(&self, path: &Path) -> Result<(), FsxError> {
            self.call("remove", &path.display().to_string())?;
            RealEffects.remove_tree_at(path)
        }

        fn remove_tree(&self, root: &TreeRoot, path: &RelPath) -> Result<(), FsxError> {
            self.call("remove", &root.prefix().concat(path).to_string())?;
            RealEffects.remove_tree(root, path)
        }

        fn move_dir(
            &self,
            from_root: &TreeRoot,
            from: &RelPath,
            to_root: &TreeRoot,
            to: &RelPath,
        ) -> Result<Moved, FsxError> {
            self.call("move", &to_root.prefix().concat(to).to_string())?;
            let mut moved = RealEffects.move_dir(from_root, from, to_root, to)?;
            if self.restore_error.get() {
                moved.mode_restore_error = Some(FsxError::Io {
                    action: "restore the mode of",
                    path: to.to_string(),
                    source: io::Error::other("refused by the test"),
                });
            }
            Ok(moved)
        }

        fn flush_dir(&self, dir: &Path) -> Result<(), FsxError> {
            self.call("flush", &dir.display().to_string())?;
            RealEffects.flush_dir(dir)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.call("remove_file", &path.display().to_string())
                .map_err(|_| io::Error::other("refused by the test"))?;
            RealEffects.remove_file(path)
        }

        fn write_entry(
            &self,
            collection: &Collection,
            lock: &RunLock,
            clock: &dyn Clock,
            entry: &mut TrackingEntry,
        ) -> Result<(), TrackingError> {
            let subject = entry.id.to_string();
            self.log.borrow_mut().push(format!("write {subject}"));
            if (self.fails)("write", &subject) {
                return Err(TrackingError::Io {
                    action: "write",
                    path: subject,
                    source: io::Error::other("refused by the test"),
                });
            }
            RealEffects.write_entry(collection, lock, clock, entry)
        }

        fn remove_entry(
            &self,
            collection: &Collection,
            lock: &RunLock,
            id: ObjectId,
        ) -> Result<(), TrackingError> {
            let subject = id.to_string();
            self.log
                .borrow_mut()
                .push(format!("remove_entry {subject}"));
            if (self.fails)("remove_entry", &subject) {
                return Err(TrackingError::Io {
                    action: "remove",
                    path: subject,
                    source: io::Error::other("refused by the test"),
                });
            }
            RealEffects.remove_entry(collection, lock, id)
        }

        fn record_metadata(
            &self,
            collection: &Collection,
            lock: &RunLock,
            metadata: &Metadata,
        ) -> Result<(), StoreError> {
            let subject = metadata.id.to_string();
            self.log
                .borrow_mut()
                .push(format!("record_metadata {subject}"));
            if (self.fails)("record_metadata", &subject) {
                return Err(StoreError::Io {
                    action: "write",
                    path: subject,
                    source: io::Error::other("refused by the test"),
                });
            }
            RealEffects.record_metadata(collection, lock, metadata)
        }
    }

    fn never(_: &str, _: &str) -> bool {
        false
    }

    fn mentions(warnings: &[Warning], text: &str) -> bool {
        warnings
            .iter()
            .any(|warning| warning.message.contains(text))
    }

    // =====================================================================
    // Finishing `removing` entries
    // =====================================================================

    #[test]
    fn nothing_to_repair_gives_no_warnings() {
        let world = World::new();
        assert!(world.run().is_empty());
    }

    #[test]
    fn a_trashed_tree_that_equals_its_archive_is_deleted_and_the_entry_is_gone() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!world.trash(ID).exists());
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.removing_payload, None);
        assert_eq!(entry.local_payloads.len(), 1, "the local payloads stay");
        assert_eq!(entry.written, time(NOW));
    }

    #[test]
    fn a_folder_in_place_that_equals_its_archive_is_deleted_there() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!world.at("projects/a").exists());
        assert!(world.at("projects").is_dir(), "only the piece goes");
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn an_entry_whose_tree_is_gone_is_set_gone() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        fs::remove_dir_all(world.at("projects/a")).expect("the delete had finished");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn the_trashed_tree_decides_over_a_folder_in_place() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        world
            .tree()
            .file("projects/a/other.txt", "made after the rename");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!world.trash(ID).exists());
        assert!(world.at("projects/a/other.txt").exists(), "left alone");
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    /// The state after `changes` were made to the trashed tree: the entry
    /// that the repair left, the warnings and the world.
    fn differing_trash(changes: impl FnOnce(&mut World)) -> (World, TrackingEntry, Vec<Warning>) {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        changes(&mut world);
        let warnings = world.run();
        let entry = world.entry(ID);
        (world, entry, warnings)
    }

    fn kept_as_unearthed(world: &World, entry: &TrackingEntry, warnings: &[Warning]) {
        assert!(!world.trash(ID).exists(), "the tree left the trash");
        assert!(world.at("projects/a").is_dir(), "the tree is back");
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.removing_payload, None);
        assert_eq!(entry.unearthed_payload_version, Some(1));
        assert_eq!(entry.unearthed, Some(time(NOW)));
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
        assert!(mentions(warnings, "projects/a"), "{warnings:?}");
    }

    #[test]
    fn a_trashed_tree_with_a_changed_file_returns_as_unearthed() {
        let (world, entry, warnings) = differing_trash(|world| {
            fs::write(world.trash(ID).join("notes.txt"), "HELLO").expect("same size, new content");
        });
        kept_as_unearthed(&world, &entry, &warnings);
        assert_eq!(
            fs::read_to_string(world.at("projects/a/notes.txt")).expect("reads"),
            "HELLO"
        );
    }

    #[test]
    fn a_trashed_tree_with_an_added_file_returns_as_unearthed() {
        let (world, entry, warnings) = differing_trash(|world| {
            fs::write(world.trash(ID).join("new.txt"), "x").expect("a new file");
        });
        kept_as_unearthed(&world, &entry, &warnings);
        assert!(world.at("projects/a/new.txt").exists());
    }

    #[test]
    fn a_remnant_with_files_missing_is_finished() {
        let (world, entry, warnings) = differing_trash(|world| {
            fs::remove_file(world.trash(ID).join("notes.txt")).expect("half a delete");
        });
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!world.trash(ID).exists());
        assert!(!world.at("projects/a").exists());
        assert_eq!(entry.state, EntryState::Gone);
    }

    #[test]
    fn a_remnant_with_a_file_missing_and_one_changed_is_kept() {
        let (world, entry, warnings) = differing_trash(|world| {
            fs::remove_file(world.trash(ID).join("notes.txt")).expect("half a delete");
            fs::write(world.trash(ID).join("src/main.rs"), "fn main() { }").expect("an edit");
        });
        kept_as_unearthed(&world, &entry, &warnings);
        assert!(!world.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn a_remnant_directory_that_was_given_owner_access_is_still_a_remnant() {
        let mut world = World::new();
        world.tree().dir("projects/a/locked");
        world
            .tree()
            .file_with("projects/a/locked/f", "x", 0o644, FILE_TIME);
        fs::set_permissions(world.at("projects/a/locked"), Permissions::from_mode(0o500))
            .expect("chmod");
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        // What an interrupted delete leaves: the directory has been given
        // owner access to be emptied.
        fs::set_permissions(
            world.trash(ID).join("locked"),
            Permissions::from_mode(0o700),
        )
        .expect("chmod");
        fs::remove_file(world.trash(ID).join("locked/f")).expect("emptied");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!world.trash(ID).exists());
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_directory_whose_group_mode_changed_is_not_a_remnant() {
        let (world, entry, warnings) = differing_trash(|world| {
            fs::set_permissions(world.trash(ID).join("src"), Permissions::from_mode(0o750))
                .expect("chmod");
        });
        kept_as_unearthed(&world, &entry, &warnings);
    }

    #[test]
    fn a_differing_tree_whose_path_is_taken_goes_to_recovered() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        world.tree().file("projects/a/stranger.txt", "not ours");

        let warnings = world.run();

        let recovered = world.at(&format!(".antiq/recovered/{ID}-{NOW_STAMP}"));
        assert!(recovered.join("new.txt").exists(), "the tree is kept");
        assert!(!world.trash(ID).exists());
        assert!(world.at("projects/a/stranger.txt").exists(), "untouched");
        assert_eq!(world.entry(ID).state, EntryState::Gone);
        assert!(mentions(&warnings, "recovered"), "{warnings:?}");
    }

    #[test]
    fn a_differing_folder_in_place_is_marked_unearthed() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        fs::write(world.at("projects/a/notes.txt"), "HELLO").expect("an edit");

        let warnings = world.run();

        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed_payload_version, Some(1));
        assert!(mentions(&warnings, "projects/a"), "{warnings:?}");
        assert_eq!(
            fs::read_to_string(world.at("projects/a/notes.txt")).expect("reads"),
            "HELLO"
        );
    }

    #[test]
    fn a_differing_tree_whose_path_belongs_to_another_entry_goes_to_recovered() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        let other = TrackingEntry::new(
            id(OTHER_ID),
            path("projects/a"),
            EntryState::Live,
            VersionTakenOut::Pending,
            time(ARCHIVED_AT),
        );
        world.store_entry(&other);

        world.run();

        assert!(
            world
                .at(&format!(".antiq/recovered/{ID}-{NOW_STAMP}/new.txt"))
                .exists()
        );
        assert_eq!(world.entry(ID).state, EntryState::Gone);
        assert_eq!(world.entry(OTHER_ID).state, EntryState::Live);
    }

    #[test]
    fn a_folder_in_place_that_belongs_to_another_entry_is_not_claimed() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        fs::write(world.at("projects/a/notes.txt"), "HELLO").expect("an edit");
        let other = TrackingEntry::new(
            id(OTHER_ID),
            path("projects/a"),
            EntryState::Live,
            VersionTakenOut::Pending,
            time(ARCHIVED_AT),
        );
        world.store_entry(&other);

        let warnings = world.run();

        assert_eq!(world.entry(ID).state, EntryState::Gone);
        assert!(mentions(&warnings, "another"), "{warnings:?}");
        assert!(world.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn a_recovered_tree_of_this_entry_means_the_move_happened() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        fs::write(world.at("projects/a/notes.txt"), "a stranger's edit").expect("a stranger");
        let recovered = world.at(&format!(".antiq/recovered/{ID}-{NOW_STAMP}"));
        fs::create_dir_all(&recovered).expect("the earlier move");

        let warnings = world.run();

        assert_eq!(world.entry(ID).state, EntryState::Gone);
        assert!(recovered.is_dir());
        assert_eq!(
            fs::read_to_string(world.at("projects/a/notes.txt")).expect("reads"),
            "a stranger's edit"
        );
        assert!(mentions(&warnings, "new piece"), "{warnings:?}");
    }

    #[test]
    fn a_recovered_tree_from_before_the_entry_was_written_is_not_this_ones() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        fs::create_dir_all(world.at(&format!(".antiq/recovered/{ID}-20200101T000000Z")))
            .expect("an old move");

        world.run();

        assert!(!world.at("projects/a").exists(), "the folder was deleted");
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    fn unusable_listing(
        version: Option<u32>,
        break_it: impl FnOnce(&World, &Archived, &mut TrackingEntry),
    ) {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        let mut entry = world.removing(ID, "projects/a", &archive);
        break_it(&world, &archive, &mut entry);
        world.store_entry(&entry);
        world.to_trash(ID, "projects/a");

        let warnings = world.run();

        assert!(
            world.at("projects/a/notes.txt").exists(),
            "the tree is kept"
        );
        assert!(!world.trash(ID).exists());
        assert_eq!(world.entry(ID).state, EntryState::Live);
        assert_eq!(world.entry(ID).unearthed_payload_version, version);
        assert!(mentions(&warnings, "projects/a"), "{warnings:?}");
    }

    #[test]
    fn an_entry_that_names_no_payload_keeps_the_tree() {
        unusable_listing(None, |_, _, entry| entry.removing_payload = None);
    }

    #[test]
    fn an_entry_that_names_an_unknown_payload_keeps_the_tree() {
        unusable_listing(None, |_, _, entry| {
            entry.removing_payload =
                Some(FileName::new("0000000000000000-x.tar.zst").expect("a name"));
        });
    }

    #[test]
    fn an_object_without_metadata_keeps_the_tree() {
        unusable_listing(None, |world, _, _| {
            let dir = world.collection.object_dir(id(ID));
            fs::remove_file(dir.join("meta.1")).expect("no metadata");
        });
    }

    #[test]
    fn a_missing_listing_file_keeps_the_tree() {
        unusable_listing(Some(1), |world, _, _| {
            let dir = world.collection.object_dir(id(ID));
            for item in fs::read_dir(dir).expect("lists") {
                let item = item.expect("an item").path();
                if item.to_string_lossy().ends_with(".listing.zst") {
                    fs::remove_file(item).expect("removed");
                }
            }
        });
    }

    #[test]
    fn a_damaged_listing_keeps_the_tree() {
        unusable_listing(Some(1), |world, _, _| {
            let dir = world.collection.object_dir(id(ID));
            for item in fs::read_dir(dir).expect("lists") {
                let item = item.expect("an item").path();
                if item.to_string_lossy().ends_with(".listing.zst") {
                    fs::write(item, b"not a listing").expect("damaged");
                }
            }
        });
    }

    #[test]
    fn a_name_added_to_the_junk_list_after_packing_does_not_hide_an_edit() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("notes.txt"), "HELLO").expect("an edit");
        let mut config = Config::new(id(COLLECTION_ID));
        config.machine.name = Some("imperius".to_string());
        config.collection.junk = vec!["notes.txt".to_string()];
        let collection = Collection::new(world.temp.root().to_path_buf(), config);

        run(&collection, &world.lock, &world.clock, &RealEffects).expect("runs");

        assert!(
            world.at("projects/a/notes.txt").exists(),
            "kept, not deleted"
        );
        assert_eq!(world.entry(ID).state, EntryState::Live);
    }

    #[test]
    fn a_junk_file_that_was_never_archived_does_not_count() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join(".DS_Store"), "finder").expect("junk");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!world.trash(ID).exists());
    }

    #[test]
    fn a_removal_that_fails_is_a_warning_and_the_tree_stays_out_of_the_emptying() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        let fails = |what: &str, subject: &str| what == "remove" && subject.ends_with(ID);
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(mentions(&warnings, ID), "{warnings:?}");
        assert!(
            world.trash(ID).join("notes.txt").exists(),
            "kept for the next run"
        );
        assert_eq!(world.entry(ID).state, EntryState::Removing);
        let removals: Vec<_> = effects
            .log()
            .into_iter()
            .filter(|call| call.starts_with("remove") && call.ends_with(ID))
            .collect();
        assert_eq!(removals.len(), 1, "step 2 does not try the protected tree");

        let again = world.run();
        assert!(again.is_empty(), "{again:?}");
        assert!(!world.trash(ID).exists());
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_move_back_that_is_refused_goes_to_recovered_and_the_warning_gives_the_reason() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        let fails = |what: &str, subject: &str| what == "move" && subject == "projects/a";
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(
            world
                .at(&format!(".antiq/recovered/{ID}-{NOW_STAMP}/new.txt"))
                .exists()
        );
        assert_eq!(world.entry(ID).state, EntryState::Gone);
        assert!(mentions(&warnings, "refused by the test"), "{warnings:?}");
        assert!(!mentions(&warnings, "path is taken"), "{warnings:?}");
    }

    #[test]
    fn a_taken_path_is_named_as_the_reason() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        world.entry_in(OTHER_ID, "projects/a", EntryState::Live);

        let warnings = world.run();

        assert!(
            mentions(&warnings, "another entry holds its path"),
            "{warnings:?}"
        );
    }

    #[test]
    fn two_refused_moves_keep_the_tree_and_a_later_run_does_not_delete_it() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        let fails = |what: &str, _: &str| what == "move";
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(mentions(&warnings, ID), "{warnings:?}");
        assert!(world.trash(ID).join("new.txt").exists(), "never emptied");
        assert_eq!(world.entry(ID).state, EntryState::Removing);

        world.run();

        assert!(world.at("projects/a/new.txt").exists(), "back at its path");
        assert_eq!(world.entry(ID).state, EntryState::Live);
    }

    #[test]
    fn a_trash_tree_that_cannot_be_looked_at_keeps_its_entry_and_its_tree() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        let trash = world.at(".antiq/trash");
        fs::set_permissions(&trash, Permissions::from_mode(0o600)).expect("chmod");
        assert!(
            fs::symlink_metadata(world.trash(ID)).is_err(),
            "this test needs a user that a directory without search permission locks out"
        );

        let warnings = world.run();
        fs::set_permissions(&trash, Permissions::from_mode(0o755)).expect("chmod back");

        assert!(mentions(&warnings, "cannot tell"), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Removing);
        assert!(world.trash(ID).join("new.txt").exists());

        world.run();

        assert!(world.at("projects/a/new.txt").exists(), "kept, not deleted");
        assert_eq!(world.entry(ID).state, EntryState::Live);
    }

    #[test]
    fn a_failed_flush_after_a_move_counts_as_a_failed_move() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        let fails = |what: &str, _: &str| what == "flush";
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(mentions(&warnings, "flush"), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Removing);
        assert!(
            world.at("projects/a/new.txt").exists() || world.trash(ID).exists(),
            "the tree is somewhere"
        );
    }

    #[test]
    fn the_move_and_both_flushes_come_before_the_entry_is_written() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        let effects = Scripted::new(&never);

        world.run_with(&effects).expect("runs");

        let log = effects.log();
        let write = log
            .iter()
            .position(|call| call.starts_with("write"))
            .expect("the entry is written");
        let moved = log
            .iter()
            .position(|call| call.starts_with("move"))
            .expect("the tree moves");
        let flushes: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, call)| call.starts_with("flush"))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(flushes.len(), 2, "{log:?}");
        assert!(moved < flushes[0] && flushes[1] < write, "{log:?}");
    }

    #[test]
    fn a_mode_that_could_not_be_restored_after_a_move_is_a_warning() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        let effects = Scripted::new(&never);
        effects.restore_error.set(true);

        let warnings = world.run_with(&effects).expect("runs");

        assert!(mentions(&warnings, "restore the mode"), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Live, "the move stood");
    }

    #[test]
    fn a_differing_tree_is_kept_before_the_trash_is_emptied() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        world.tree().file(".antiq/trash/leftover/f", "x");

        world.run();

        assert!(world.at("projects/a/new.txt").exists());
        assert!(!world.at(".antiq/trash/leftover").exists());
    }

    // =====================================================================
    // Damaged entries
    // =====================================================================

    #[test]
    fn the_trash_tree_of_a_damaged_entry_is_not_emptied() {
        let mut world = World::new();
        world
            .tree()
            .file(&format!(".antiq/trash/{ID}/precious"), "x");
        fs::write(world.collection.tracking_path(id(ID)), "not json").expect("a damaged entry");

        let warnings = world.run();

        assert!(world.trash(ID).join("precious").exists());
        assert!(mentions(&warnings, ID), "{warnings:?}");
        assert!(
            world.collection.tracking_path(id(ID)).exists(),
            "damaged entries are listed, not deleted"
        );
    }

    #[test]
    fn the_trash_tree_of_an_unreadable_entry_is_not_emptied() {
        let mut world = World::new();
        world
            .tree()
            .file(&format!(".antiq/trash/{ID}/precious"), "x");
        let entry = world.collection.tracking_path(id(ID));
        fs::write(&entry, "{}").expect("a file");
        fs::set_permissions(&entry, Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read(&entry).is_err(),
            "this test needs a user that mode 000 locks out"
        );

        let warnings = world.run();
        fs::set_permissions(&entry, Permissions::from_mode(0o644)).expect("chmod back");

        assert!(world.trash(ID).join("precious").exists());
        assert!(mentions(&warnings, ID), "{warnings:?}");
    }

    #[test]
    fn a_trash_tree_without_any_entry_is_emptied() {
        let mut world = World::new();
        world.tree().file(&format!(".antiq/trash/{ID}/x"), "x");

        world.run();

        assert!(!world.trash(ID).exists());
    }

    // =====================================================================
    // Emptying the run directories
    // =====================================================================

    #[test]
    fn leftovers_in_trash_tmp_and_staging_are_removed() {
        let mut world = World::new();
        world.tree().file(".antiq/trash/x/deep/er/file", "x");
        world.tree().file(".antiq/tmp/upload.part", "x");
        world.tree().file(".antiq/staging/tree/f", "x");
        world.tree().dir(".antiq/staging/locked");
        world.tree().file(".antiq/staging/locked/f", "x");
        fs::set_permissions(
            world.at(".antiq/staging/locked"),
            Permissions::from_mode(0o500),
        )
        .expect("chmod");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        for directory in ["trash", "tmp", "staging"] {
            let left: Vec<_> = fs::read_dir(world.at(&format!(".antiq/{directory}")))
                .expect("the directory stays")
                .collect();
            assert!(left.is_empty(), "{directory}: {left:?}");
        }
    }

    #[test]
    fn recovered_is_never_emptied() {
        let mut world = World::new();
        world.tree().file(".antiq/recovered/old-tree/f", "x");

        world.run();

        assert!(world.at(".antiq/recovered/old-tree/f").exists());
    }

    #[test]
    fn a_leftover_that_resists_is_a_warning_and_the_others_still_go() {
        let mut world = World::new();
        world.tree().file(".antiq/trash/stuck/f", "x");
        world.tree().file(".antiq/tmp/a", "x");
        world.tree().file(".antiq/staging/b", "x");
        let fails = |_: &str, subject: &str| subject.ends_with("stuck");
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(world.at(".antiq/trash/stuck/f").exists());
        assert!(!world.at(".antiq/tmp/a").exists());
        assert!(!world.at(".antiq/staging/b").exists());
        assert!(mentions(&warnings, "stuck"), "{warnings:?}");
    }

    #[test]
    fn a_run_directory_that_cannot_be_listed_is_a_warning() {
        let world = World::new();
        fs::remove_dir(world.at(".antiq/tmp")).expect("empty");
        fs::write(world.at(".antiq/tmp"), "a file, not a directory").expect("a file");

        let warnings = world.run();

        assert!(mentions(&warnings, "tmp"), "{warnings:?}");
        assert!(!mentions(&warnings, "staging"), "{warnings:?}");
    }

    // =====================================================================
    // `unearthing` entries
    // =====================================================================

    /// An `unearthing` entry for `piece` that names payload version 1.
    fn unearthing(world: &World, object: &str, piece: &str) -> TrackingEntry {
        let mut entry = world.entry_in(object, piece, EntryState::Unearthing);
        entry.unearthed_payload_version = Some(1);
        world.store_entry(&entry);
        entry
    }

    #[test]
    fn an_unearthing_entry_whose_folder_matches_the_listing_becomes_live_at_the_time_it_was_written()
     {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed, Some(time(WRITTEN)));
        assert_eq!(entry.unearthed_payload_version, Some(1));
    }

    #[test]
    fn an_unearthing_entry_whose_folder_differs_is_dropped_and_the_folder_is_a_new_piece() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        fs::write(world.at("projects/a/notes.txt"), "HELLO").expect("an edit");
        unearthing(&world, ID, "projects/a");

        let warnings = world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(mentions(&warnings, "new piece"), "{warnings:?}");
        assert!(world.at("projects/a/notes.txt").exists(), "untouched");
    }

    #[test]
    fn an_unearthing_entry_without_a_folder_is_dropped_quietly() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(world.at("projects/a")).expect("never renamed into place");
        unearthing(&world, ID, "projects/a");

        let warnings = world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn an_unearthing_entry_without_a_recorded_version_of_an_archived_piece_is_dropped() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        let mut entry = unearthing(&world, ID, "projects/a");
        entry.unearthed_payload_version = None;
        world.store_entry(&entry);

        let warnings = world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(mentions(&warnings, "new piece"), "{warnings:?}");
    }

    #[test]
    fn an_unearthing_from_a_live_package_leaves_the_folder_to_its_piece() {
        let mut world = World::new();
        own_live(&mut world, ID, "projects/a");
        let mut entry = unearthing(&world, ID, "projects/a");
        entry.unearthed_payload_version = None;
        world.store_entry(&entry);

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path("projects/a"));
    }

    #[test]
    fn an_unearthing_entry_whose_listing_is_missing_is_dropped() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");
        for item in fs::read_dir(world.collection.object_dir(id(ID))).expect("lists") {
            let item = item.expect("an item").path();
            if item.to_string_lossy().ends_with(".listing.zst") {
                fs::remove_file(item).expect("removed");
            }
        }

        let warnings = world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(mentions(&warnings, "new piece"), "{warnings:?}");
    }

    #[test]
    fn an_unearthing_entry_without_metadata_is_dropped() {
        let mut world = World::new();
        world.tree().file("projects/a/f", "x");
        unearthing(&world, ID, "projects/a");

        let warnings = world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(mentions(&warnings, "new piece"), "{warnings:?}");
    }

    #[test]
    fn an_unearthing_entry_naming_a_version_the_metadata_lacks_is_dropped() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        let mut entry = unearthing(&world, ID, "projects/a");
        entry.unearthed_payload_version = Some(7);
        world.store_entry(&entry);

        world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
    }

    #[test]
    fn an_unearthing_folder_that_cannot_be_entered_is_not_recognised() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");
        let folder = world.at("projects/a");
        fs::set_permissions(&folder, Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(&folder).is_err(),
            "this test needs a user that mode 000 locks out"
        );

        let warnings = world.run();
        fs::set_permissions(&folder, Permissions::from_mode(0o755)).expect("chmod back");

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(mentions(&warnings, "new piece"), "{warnings:?}");
    }

    #[test]
    fn an_entry_is_removed_through_the_effects() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(world.at("projects/a")).expect("never renamed into place");
        unearthing(&world, ID, "projects/a");
        let effects = Scripted::new(&never);

        world.run_with(&effects).expect("runs");

        assert!(effects.log().contains(&format!("remove_entry {ID}")));
        assert!(!world.collection.tracking_path(id(ID)).exists());
    }

    #[test]
    fn a_dropped_unearthing_goes_back_to_gone_when_the_entry_holds_a_local_archive() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(world.at("projects/a")).expect("never renamed into place");
        let mut entry = unearthing(&world, ID, "projects/a");
        entry.local_payloads = vec![LocalPayload {
            kind: PayloadKind::Archive,
            version: 1,
            file: archive.payload.clone(),
        }];
        entry.committed_versions = vec![1];
        entry.unearthed = Some(time(NOW));
        world.store_entry(&entry);

        world.run();

        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.unearthed, None);
        assert_eq!(entry.unearthed_payload_version, None);
        assert_eq!(entry.local_payloads.len(), 1);
        assert_eq!(entry.committed_versions, vec![1]);
        let metadata = Store::new(&world.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata")
            .metadata;
        assert_eq!(
            tracking::derive_state(Some(&entry), Some(&metadata), false, id(COLLECTION_ID))
                .expect("derives"),
            tracking::PieceState::Archived
        );
    }

    #[test]
    fn a_dropped_unearthing_goes_back_to_gone_when_it_only_has_committed_versions() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(world.at("projects/a")).expect("never renamed into place");
        let mut entry = unearthing(&world, ID, "projects/a");
        entry.committed_versions = vec![1];
        world.store_entry(&entry);

        world.run();

        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn an_unearthing_whose_path_belongs_to_another_entry_is_dropped() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");
        world.entry_in(OTHER_ID, "projects/a", EntryState::Live);

        let warnings = world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert_eq!(world.entry(OTHER_ID).state, EntryState::Live);
        assert!(mentions(&warnings, "new piece"), "{warnings:?}");
    }

    // ---------------------------------------------------------------------
    // The metadata change of an interrupted unearth
    // ---------------------------------------------------------------------

    fn metadata_of(world: &World, object: &str) -> crate::store::Current {
        Store::new(&world.collection)
            .current(id(object))
            .expect("the metadata reads")
            .expect("the object has metadata")
    }

    /// The `unearth` entries of the history, as `(machine, time)`.
    fn unearths(metadata: &Metadata) -> Vec<(String, jiff::Timestamp)> {
        metadata
            .history
            .iter()
            .filter(|entry| entry.action == "unearth")
            .map(|entry| (entry.machine.clone(), entry.time))
            .collect()
    }

    /// What the metadata says once the unearth of an entry written at
    /// [`WRITTEN`] is recorded.
    fn record_the_unearth(world: &World, object: &str) {
        world.edit_meta(object, |document| {
            document["lifecycle"] = serde_json::json!("live");
            document["history"]
                .as_array_mut()
                .expect("a history")
                .push(serde_json::json!({
                    "time": WRITTEN,
                    "machine": "imperius",
                    "action": "unearth",
                }));
        });
    }

    #[test]
    fn an_unearthing_whose_folder_matches_has_its_metadata_change_recorded() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        let current = metadata_of(&world, ID);
        assert_eq!(current.source, Source::Pending);
        assert_eq!(current.metadata.lifecycle, Lifecycle::Live);
        assert_eq!(current.metadata.collection, id(COLLECTION_ID));
        assert_eq!(current.metadata.path, path("projects/a"));
        assert_eq!(
            unearths(&current.metadata),
            [("imperius".to_string(), time(WRITTEN))]
        );
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed, Some(time(WRITTEN)));
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
    }

    #[test]
    fn the_metadata_change_takes_the_ownership_and_the_path_of_the_entry() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        world.edit_meta(ID, |document| {
            document["collection"] = serde_json::json!(OTHER_ID);
            document["path"] = serde_json::json!("elsewhere/b");
            document["name"] = serde_json::json!("b");
        });
        unearthing(&world, ID, "projects/a");

        world.run();

        let metadata = metadata_of(&world, ID).metadata;
        assert_eq!(metadata.collection, id(COLLECTION_ID));
        assert_eq!(metadata.path, path("projects/a"));
        assert_eq!(metadata.name.as_bytes(), b"a");
    }

    #[test]
    fn a_metadata_change_that_is_recorded_already_is_not_recorded_again() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        record_the_unearth(&world, ID);
        unearthing(&world, ID, "projects/a");

        world.run();

        let current = metadata_of(&world, ID);
        assert_eq!(current.source, Source::Committed(1), "nothing was written");
        assert_eq!(unearths(&current.metadata).len(), 1);
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
    }

    #[test]
    fn a_gone_piece_with_live_metadata_gets_its_unearth_recorded() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        world.edit_meta(ID, |document| {
            document["lifecycle"] = serde_json::json!("live");
        });
        unearthing(&world, ID, "projects/a");

        world.run();

        let metadata = metadata_of(&world, ID).metadata;
        assert_eq!(
            unearths(&metadata),
            [("imperius".to_string(), time(WRITTEN))]
        );
    }

    #[test]
    fn a_dropped_unearthing_leaves_the_metadata_and_the_version_taken_out_alone() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        fs::write(world.at("projects/a/notes.txt"), "HELLO").expect("an edit");
        let mut entry = unearthing(&world, ID, "projects/a");
        entry.committed_versions = vec![1];
        world.store_entry(&entry);

        world.run();

        let current = metadata_of(&world, ID);
        assert_eq!(current.source, Source::Committed(1));
        assert_eq!(current.metadata.lifecycle, Lifecycle::Archived);
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
    }

    #[test]
    fn metadata_that_cannot_be_read_keeps_the_unearthing_entry_for_the_next_run() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");
        // The object's directory cannot be listed, so its metadata cannot be
        // found.
        let object = world.collection.object_dir(id(ID));
        let _restore = crate::fsx::RestoreModes(vec![object.clone()]);
        fs::set_permissions(&object, Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(&object).is_err(),
            "this test needs a user that mode 000 locks out"
        );

        let warnings = world.run();

        assert!(mentions(&warnings, "metadata"), "{warnings:?}");
        assert!(!mentions(&warnings, "new piece"), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Unearthing);
        assert!(world.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn a_dropped_unearthing_to_another_path_goes_back_to_the_stored_path() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(world.at("projects/a")).expect("never renamed into place");
        let mut entry = world.entry_in(ID, "projects/b", EntryState::Unearthing);
        entry.unearthed_payload_version = Some(1);
        entry.committed_versions = vec![1];
        world.store_entry(&entry);

        world.run();

        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.path, path("projects/a"));
    }

    #[test]
    fn an_edited_folder_of_a_recorded_unearth_stays_the_unearthed_piece() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        record_the_unearth(&world, ID);
        fs::write(world.at("projects/a/notes.txt"), "HELLO").expect("an edit");
        unearthing(&world, ID, "projects/a");

        let warnings = world.run();

        assert!(!mentions(&warnings, "new piece"), "{warnings:?}");
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed_payload_version, Some(1));
        assert_eq!(entry.unearthed, Some(time(WRITTEN)));
    }

    #[test]
    fn a_recorded_unearth_whose_folder_is_gone_leaves_a_pending_gone_entry() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        record_the_unearth(&world, ID);
        let object = world.collection.object_dir(id(ID));
        fs::rename(object.join("meta.1"), object.join("meta.pending")).expect("pending");
        fs::remove_dir_all(world.at("projects/a")).expect("deleted after the crash");
        let mut entry = unearthing(&world, ID, "projects/a");
        entry.committed_versions = vec![1];
        world.store_entry(&entry);

        world.run();

        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
    }

    #[test]
    fn the_folder_may_equal_any_local_payload_of_the_recorded_version() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        // A second version-1 payload from another machine, listed after
        // the first and describing a different tree.
        let other_listing = listing::file_name(&"11".repeat(32)).expect("a listing name");
        let object = world.collection.object_dir(id(ID));
        listing::write(
            std::iter::empty(),
            File::create(object.join(other_listing.as_str())).expect("a file"),
        )
        .expect("an empty listing");
        let second = world.add_archive(ID, 1, "other", "2026-11-03T11:00:00Z");
        world.edit_meta(ID, |document| {
            document["payloads"][1]["listing"] = serde_json::json!(other_listing.as_str());
        });
        let mut entry = unearthing(&world, ID, "projects/a");
        let first = metadata_of(&world, ID).metadata.payloads[0].file.clone();
        entry.local_payloads = [first.as_str().to_string(), second]
            .into_iter()
            .map(|file| LocalPayload {
                kind: PayloadKind::Archive,
                version: 1,
                file: FileName::new(file).expect("a name"),
            })
            .collect();
        world.store_entry(&entry);

        world.run();

        assert_eq!(world.entry(ID).state, EntryState::Live);
    }

    #[test]
    fn a_metadata_change_that_cannot_be_written_keeps_the_entry_for_the_next_run() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");
        let refuse = |what: &str, _: &str| what == "record_metadata";
        let effects = Scripted::new(&refuse);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(mentions(&warnings, "unearth"), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Unearthing);
        assert!(world.at("projects/a/notes.txt").exists());

        world.run();

        assert_eq!(world.entry(ID).state, EntryState::Live);
        assert_eq!(
            unearths(&metadata_of(&world, ID).metadata),
            [("imperius".to_string(), time(WRITTEN))]
        );
    }

    #[test]
    fn a_live_entry_that_lags_behind_an_archive_keeps_its_archived_metadata() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        lagging(&world, ID, "projects/a");

        world.run();

        let current = metadata_of(&world, ID);
        assert_eq!(current.source, Source::Committed(1));
        assert_eq!(current.metadata.lifecycle, Lifecycle::Archived);
    }

    #[test]
    fn the_unearth_change_keeps_what_else_the_metadata_says() {
        let metadata = {
            let mut world = World::new();
            world.archived(ID, "projects/a", FILES);
            metadata_of(&world, ID).metadata
        };
        let target = RelPath::parse("elsewhere/b%FF").expect("a path");

        let changed = record_unearth(&metadata, id(OTHER_ID), "laptop", time(WRITTEN), &target);

        assert_eq!(changed.lifecycle, Lifecycle::Live);
        assert_eq!(changed.collection, id(OTHER_ID));
        assert_eq!(changed.path, target);
        assert_eq!(changed.name.as_bytes(), b"b\xff");
        assert_eq!(changed.payloads, metadata.payloads);
        assert_eq!(changed.created, metadata.created);
        assert_eq!(changed.history.len(), metadata.history.len() + 1);
        assert!(records_unearth(&changed, "laptop", time(WRITTEN)));
        assert!(!records_unearth(&changed, "imperius", time(WRITTEN)));
        assert!(!records_unearth(&changed, "laptop", time(NOW)));
        assert!(!records_unearth(&metadata, "laptop", time(WRITTEN)));
    }

    // =====================================================================
    // Entries that lag behind the metadata
    // =====================================================================

    /// A `live` entry that has not caught up with the archive.
    fn lagging(world: &World, object: &str, piece: &str) -> TrackingEntry {
        world.entry_in(object, piece, EntryState::Live)
    }

    #[test]
    fn a_live_entry_of_an_archived_piece_with_a_folder_is_unearthed_now() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        fs::write(
            world
                .collection
                .object_dir(id(ID))
                .join(archive.payload.as_str()),
            "payload",
        )
        .expect("the payload file");
        lagging(&world, ID, "projects/a");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed_payload_version, Some(1));
        assert_eq!(entry.unearthed, Some(time(NOW)));
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
        assert_eq!(
            entry.local_payloads,
            vec![LocalPayload {
                kind: PayloadKind::Archive,
                version: 1,
                file: archive.payload,
            }]
        );
    }

    #[test]
    fn a_second_run_changes_a_caught_up_entry_no_more() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        lagging(&world, ID, "projects/a");
        world.run();
        let first = fs::read(world.collection.tracking_path(id(ID))).expect("reads");

        world.run();

        assert_eq!(
            fs::read(world.collection.tracking_path(id(ID))).expect("reads"),
            first
        );
    }

    #[test]
    fn a_change_that_is_still_pending_is_taken_out_as_pending() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        let object = world.collection.object_dir(id(ID));
        fs::rename(object.join("meta.1"), object.join("meta.pending")).expect("pending");
        lagging(&world, ID, "projects/a");

        world.run();

        assert_eq!(world.entry(ID).version_taken_out, VersionTakenOut::Pending);
    }

    #[test]
    fn a_payload_whose_file_is_not_in_the_store_is_not_added_to_the_local_ones() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        // The payload file itself was never put in place by the fixture.
        assert!(
            !world
                .collection
                .object_dir(id(ID))
                .join(archive.payload.as_str())
                .exists()
        );
        lagging(&world, ID, "projects/a");

        world.run();

        assert!(world.entry(ID).local_payloads.is_empty());
        assert_eq!(world.entry(ID).unearthed_payload_version, Some(1));
    }

    #[test]
    fn an_unearthed_piece_archived_again_catches_up_with_the_new_payload() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        let second = world.add_archive(ID, 2, "imperius", "2026-11-03T11:00:00Z");
        let mut entry = lagging(&world, ID, "projects/a");
        entry.unearthed = Some(time("2026-11-02T00:00:00Z"));
        entry.unearthed_payload_version = Some(1);
        world.store_entry(&entry);

        world.run();

        let entry = world.entry(ID);
        assert_eq!(entry.unearthed_payload_version, Some(2));
        assert_eq!(entry.unearthed, Some(time(NOW)));
        assert!(
            entry
                .local_payloads
                .iter()
                .any(|payload| payload.file == second.as_str())
        );
    }

    #[test]
    fn an_unearth_of_an_older_version_that_died_before_its_metadata_is_left_alone() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        world.add_archive(ID, 2, "imperius", "2026-11-03T11:00:00Z");
        let mut entry = lagging(&world, ID, "projects/a");
        entry.unearthed = Some(time("2026-11-03T12:00:00Z"));
        entry.unearthed_payload_version = Some(1);
        world.store_entry(&entry);
        let before = fs::read(world.collection.tracking_path(id(ID))).expect("reads");

        world.run();

        assert_eq!(
            fs::read(world.collection.tracking_path(id(ID))).expect("reads"),
            before
        );
    }

    #[test]
    fn payloads_of_other_machines_and_superseded_ones_do_not_count() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        world.add_archive(ID, 2, "laptop", "2026-11-03T11:00:00Z");
        world.add_archive(ID, 3, "imperius", "2026-11-03T12:00:00Z");
        world.edit_meta(ID, |document| {
            document["payloads"][2]["superseded"] = serde_json::json!(true);
        });
        let mut entry = lagging(&world, ID, "projects/a");
        entry.unearthed = Some(time("2026-11-02T00:00:00Z"));
        entry.unearthed_payload_version = Some(1);
        world.store_entry(&entry);
        let before = fs::read(world.collection.tracking_path(id(ID))).expect("reads");

        world.run();

        assert_eq!(
            fs::read(world.collection.tracking_path(id(ID))).expect("reads"),
            before
        );
    }

    #[test]
    fn the_newest_payload_of_this_machine_wins() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        world.add_archive(ID, 2, "imperius", "2026-11-03T11:00:00Z");
        world.add_archive(ID, 3, "imperius", "2026-11-03T12:00:00Z");
        lagging(&world, ID, "projects/a");

        world.run();

        assert_eq!(world.entry(ID).unearthed_payload_version, Some(3));
    }

    #[test]
    fn metadata_of_another_collection_or_a_live_piece_is_not_caught_up_with() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        world.edit_meta(ID, |document| {
            document["collection"] = serde_json::json!(OTHER_ID);
        });
        lagging(&world, ID, "projects/a");
        world.archived(OTHER_ID, "projects/b", &[("f", "x")]);
        world.edit_meta(OTHER_ID, |document| {
            document["lifecycle"] = serde_json::json!("live");
        });
        lagging(&world, OTHER_ID, "projects/b");

        world.run();

        assert_eq!(world.entry(ID).unearthed_payload_version, None);
        assert_eq!(world.entry(OTHER_ID).unearthed_payload_version, None);
    }

    #[test]
    fn a_live_entry_without_a_folder_or_without_metadata_is_left_alone() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(world.at("projects/a")).expect("no folder");
        lagging(&world, ID, "projects/a");
        lagging(&world, OTHER_ID, "projects/b");

        world.run();

        assert_eq!(world.entry(ID).unearthed, None);
        assert_eq!(world.entry(OTHER_ID).unearthed, None);
    }

    // =====================================================================
    // Own live metadata without an entry
    // =====================================================================

    fn own_live(world: &mut World, object: &str, piece: &str) {
        world.archived(object, piece, FILES);
        world.edit_meta(object, |document| {
            document["lifecycle"] = serde_json::json!("live");
        });
    }

    #[test]
    fn own_live_metadata_with_a_folder_gets_its_live_entry() {
        let mut world = World::new();
        own_live(&mut world, ID, "projects/a");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path("projects/a"));
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
        assert_eq!(entry.written, time(NOW));
    }

    #[test]
    fn a_change_not_committed_yet_gives_a_pending_entry() {
        let mut world = World::new();
        own_live(&mut world, ID, "projects/a");
        let object = world.collection.object_dir(id(ID));
        fs::rename(object.join("meta.1"), object.join("meta.pending")).expect("pending");

        world.run();

        assert_eq!(world.entry(ID).version_taken_out, VersionTakenOut::Pending);
    }

    #[test]
    fn own_live_metadata_without_a_folder_gets_a_gone_entry() {
        let mut world = World::new();
        own_live(&mut world, ID, "projects/a");
        fs::remove_dir_all(world.at("projects/a")).expect("no folder");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = world.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert!(entry.local_payloads.is_empty());
        let metadata = Store::new(&world.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata")
            .metadata;
        assert_eq!(
            tracking::derive_state(Some(&entry), Some(&metadata), false, id(COLLECTION_ID))
                .expect("derives"),
            tracking::PieceState::Gone
        );
    }

    #[test]
    fn own_live_metadata_whose_path_another_entry_holds_gets_no_entry() {
        let mut world = World::new();
        own_live(&mut world, ID, "projects/a");
        world.entry_in(OTHER_ID, "projects/a", EntryState::Live);

        let warnings = world.run();

        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(mentions(&warnings, "another"), "{warnings:?}");
    }

    #[test]
    fn a_path_given_to_another_id_by_this_run_is_not_given_twice() {
        let mut world = World::new();
        let archive = world.archived(OTHER_ID, "projects/a", FILES);
        world.removing(OTHER_ID, "projects/a", &archive);
        fs::write(world.at("projects/a/notes.txt"), "HELLO").expect("an edit");
        own_live(&mut world, ID, "projects/b");
        world.edit_meta(ID, |document| {
            document["path"] = serde_json::json!("projects/a");
            document["name"] = serde_json::json!("a");
        });

        world.run();

        assert_eq!(world.entry(OTHER_ID).state, EntryState::Live);
        assert!(!world.collection.tracking_path(id(ID)).exists());
    }

    #[test]
    fn metadata_that_needs_no_entry_gets_none() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        own_live(&mut world, OTHER_ID, "projects/b");
        world.edit_meta(OTHER_ID, |document| {
            document["collection"] = serde_json::json!("01ARZ3NDEKTSV4RRFFQ69G5FAW");
        });
        fs::write(world.collection.tracking_path(id(ID)), "not json").expect("damaged");
        let third = "01BX5ZZKBKACTAV9WEVGEMMVS0";
        world.archived(third, "projects/c", FILES);

        world.run();

        assert!(
            !world.collection.tracking_path(id(OTHER_ID)).exists(),
            "not ours"
        );
        assert!(
            !world.collection.tracking_path(id(third)).exists(),
            "archived"
        );
        assert_eq!(
            fs::read(world.collection.tracking_path(id(ID))).expect("reads"),
            b"not json"
        );
    }

    #[test]
    fn an_object_without_metadata_is_skipped_with_a_warning_when_it_cannot_be_read() {
        let mut world = World::new();
        own_live(&mut world, ID, "projects/a");
        fs::write(
            world.collection.object_dir(id(ID)).join("meta.1"),
            "not json",
        )
        .expect("damaged");
        fs::create_dir_all(world.collection.object_dir(id(OTHER_ID))).expect("an empty object");

        let warnings = world.run();

        assert!(mentions(&warnings, ID), "{warnings:?}");
        assert!(!world.collection.tracking_path(id(ID)).exists());
        assert!(!world.collection.tracking_path(id(OTHER_ID)).exists());
    }

    #[test]
    fn an_objects_directory_that_cannot_be_listed_stops_the_run() {
        let world = World::new();
        let objects = world.at(".antiq/objects");
        fs::set_permissions(&objects, Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(&objects).is_err(),
            "this test needs a user that mode 000 locks out"
        );

        let result = world.run_with(&RealEffects);
        fs::set_permissions(&objects, Permissions::from_mode(0o755)).expect("chmod back");

        let error = result.expect_err("the objects cannot be listed");
        assert!(matches!(error, RepairError::Store(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_machine_without_a_name_cannot_repair() {
        let world = World::new();
        let mut config = Config::new(id(COLLECTION_ID));
        config.machine.name = Some(String::new());
        let collection = Collection::new(world.temp.root().to_path_buf(), config);

        let error =
            run(&collection, &world.lock, &world.clock, &RealEffects).expect_err("no machine name");

        assert!(matches!(error, RepairError::Machine(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn an_entry_that_cannot_be_removed_stops_the_run() {
        let mut world = World::new();
        world.archived(ID, "projects/a", FILES);
        unearthing(&world, ID, "projects/a");
        fs::remove_dir_all(world.at("projects/a")).expect("never renamed into place");
        let fails = |what: &str, _: &str| what == "remove_entry";
        let effects = Scripted::new(&fails);

        let error = world.run_with(&effects).expect_err("the entry stays");

        assert!(matches!(error, RepairError::Tracking(_)), "{error}");
        assert_eq!(world.entry(ID).state, EntryState::Unearthing);
    }

    // =====================================================================
    // Leftovers of atomic writes and orphan manifests
    // =====================================================================

    const LEFTOVERS: &[&str] = &[
        ".antiq/.config.toml.4242.0.tmp",
        ".antiq/objects/01BX5ZZKBKACTAV9WEVGEMMVRZ/.meta.pending.4242.7.tmp",
        ".antiq/tracking/.01BX5ZZKBKACTAV9WEVGEMMVRZ.json.4242.3.tmp",
        ".antiq/index/.entries.1.2.tmp",
    ];

    #[test]
    fn the_temp_files_of_interrupted_atomic_writes_are_removed() {
        let mut world = World::new();
        for leftover in LEFTOVERS {
            world.tree().file(leftover, "half a write");
        }

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        for leftover in LEFTOVERS {
            assert!(!world.at(leftover).exists(), "{leftover}");
        }
    }

    #[test]
    fn names_that_only_look_like_temp_files_stay() {
        let mut world = World::new();
        let stay = [
            ".antiq/.keep.tmp",
            ".antiq/a.1.2.tmp",
            ".antiq/.a.1.2.tmpx",
            ".antiq/.a.x.2.tmp",
            ".antiq/..1.2.tmp",
            ".antiq/objects/.upload.1.2.part",
            ".antiq/recovered/old/.config.toml.4242.0.tmp",
        ];
        for name in stay {
            world.tree().file(name, "x");
        }
        world.tree().dir(".antiq/.a.1.2.tmp.d");
        world.tree().dir(".antiq/.dir.1.2.tmp");
        world.tree().symlink(".antiq/.link.1.2.tmp", "nowhere");

        world.run();

        for name in stay {
            assert!(world.at(name).exists(), "{name}");
        }
        assert!(world.at(".antiq/.dir.1.2.tmp").is_dir());
        assert!(
            fs::symlink_metadata(world.at(".antiq/.link.1.2.tmp")).is_ok(),
            "a link is not a file"
        );
    }

    #[test]
    fn a_temp_file_that_cannot_be_removed_is_a_warning() {
        let mut world = World::new();
        world.tree().file(".antiq/.config.toml.4242.0.tmp", "x");
        world.tree().file(".antiq/tracking/.x.1.2.tmp", "x");
        let fails = |what: &str, subject: &str| what == "remove_file" && subject.ends_with("0.tmp");
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(
            mentions(&warnings, ".config.toml.4242.0.tmp"),
            "{warnings:?}"
        );
        assert!(world.at(".antiq/.config.toml.4242.0.tmp").exists());
        assert!(!world.at(".antiq/tracking/.x.1.2.tmp").exists());
    }

    #[test]
    fn a_directory_that_cannot_be_listed_is_a_warning_and_the_walk_goes_on() {
        let mut world = World::new();
        world.tree().file(".antiq/index/.entries.1.2.tmp", "x");
        world.tree().file(".antiq/objects/.x.1.2.tmp", "x");
        let locked = world.tree().dir(".antiq/index");
        fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(&locked).is_err(),
            "this test needs a user that mode 000 locks out"
        );

        let warnings = world.run();
        fs::set_permissions(&locked, Permissions::from_mode(0o755)).expect("chmod back");

        assert!(mentions(&warnings, "index"), "{warnings:?}");
        assert!(!world.at(".antiq/objects/.x.1.2.tmp").exists());
    }

    #[test]
    fn a_file_that_is_not_there_counts_as_removed() {
        let world = World::new();
        RealEffects
            .remove_file(&world.at("missing"))
            .expect("nothing to remove");
    }

    fn manifest(world: &World, name: &str) -> PathBuf {
        world.at(&format!(".antiq/tracking/{name}.manifest.json"))
    }

    #[test]
    fn a_manifest_without_an_entry_is_removed() {
        let mut world = World::new();
        world
            .tree()
            .file(&format!(".antiq/tracking/{ID}.manifest.json"), "{}");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!manifest(&world, ID).exists());
    }

    #[test]
    fn a_manifest_with_an_entry_stays() {
        let mut world = World::new();
        world
            .tree()
            .file(&format!(".antiq/tracking/{ID}.manifest.json"), "{}");
        world.store_entry(&TrackingEntry::new(
            id(ID),
            path("projects/a"),
            EntryState::Gone,
            VersionTakenOut::Committed(1),
            time(ARCHIVED_AT),
        ));

        world.run();

        assert!(manifest(&world, ID).exists());
    }

    #[test]
    fn a_manifest_whose_entry_is_damaged_stays() {
        let mut world = World::new();
        world
            .tree()
            .file(&format!(".antiq/tracking/{ID}.manifest.json"), "{}");
        fs::write(world.collection.tracking_path(id(ID)), "not json").expect("damaged");

        world.run();

        assert!(manifest(&world, ID).exists());
    }

    #[test]
    fn files_that_are_no_manifest_of_an_id_stay() {
        let mut world = World::new();
        let lower = ID.to_lowercase();
        let stay = [
            format!(".antiq/tracking/{lower}.manifest.json"),
            ".antiq/tracking/notes.manifest.json".to_string(),
            format!(".antiq/tracking/{ID}.manifest.json.bak"),
        ];
        for name in &stay {
            world.tree().file(name, "{}");
        }

        world.run();

        for name in &stay {
            assert!(world.at(name).exists(), "{name}");
        }
    }

    #[test]
    fn a_manifest_that_cannot_be_removed_is_a_warning() {
        let mut world = World::new();
        world
            .tree()
            .file(&format!(".antiq/tracking/{ID}.manifest.json"), "{}");
        let fails = |what: &str, _: &str| what == "remove_file";
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(mentions(&warnings, ID), "{warnings:?}");
        assert!(manifest(&world, ID).exists());
    }

    #[test]
    fn a_manifest_of_an_entry_made_by_this_run_stays() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::remove_file(world.trash(ID).join("notes.txt")).expect("half a delete");
        world
            .tree()
            .file(&format!(".antiq/tracking/{ID}.manifest.json"), "{}");

        world.run();

        assert_eq!(world.entry(ID).state, EntryState::Gone);
        assert!(manifest(&world, ID).exists(), "the gone entry keeps it");
    }

    #[test]
    fn a_missing_tracking_directory_gives_no_warning() {
        let world = World::new();
        fs::remove_dir(world.at(".antiq/tracking")).expect("empty");
        assert!(world.run().is_empty());
    }

    // =====================================================================
    // Crash points
    // =====================================================================

    /// Every path below the collection with its content, directories as
    /// `None`, apart from the lock file.
    fn snapshot(world: &World) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
        fn walk(
            base: &Path,
            directory: &Path,
            seen: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
        ) {
            for item in fs::read_dir(directory).expect("lists") {
                let item = item.expect("an item");
                let path = item.path();
                let name = path
                    .strip_prefix(base)
                    .expect("below")
                    .display()
                    .to_string();
                if name == ".antiq/lock" {
                    continue;
                }
                let kind = item.file_type().expect("a type");
                if kind.is_dir() {
                    seen.insert(name, None);
                    walk(base, &path, seen);
                } else {
                    seen.insert(name, Some(fs::read(&path).unwrap_or_default()));
                }
            }
        }
        let mut seen = std::collections::BTreeMap::new();
        walk(world.temp.root(), world.temp.root(), &mut seen);
        seen
    }

    /// Whether some file below the collection has exactly this content.
    fn holds(world: &World, content: &[u8]) -> bool {
        snapshot(world)
            .values()
            .any(|found| found.as_deref() == Some(content))
    }

    /// Runs the repairs of the world `build` makes with a crash after `n`
    /// effects, for every `n` that crashes it, and then once more without.
    /// Each time the second run has to end in the state a run without a
    /// crash ends in, and `precious` must not have been lost on the way.
    fn survives_every_crash(build: impl Fn() -> World, precious: &[&[u8]]) {
        let expected = {
            let world = build();
            world.run();
            snapshot(&world)
        };
        for content in precious {
            // The scenario keeps what the user had, in one place or another.
            let world = build();
            assert!(holds(&world, content), "the scenario holds {content:?}");
        }

        let mut crash_points = 0;
        for limit in 0.. {
            let world = build();
            let calls = Cell::new(0usize);
            let fails = |_: &str, _: &str| {
                let call = calls.get();
                calls.set(call + 1);
                call >= limit
            };
            let effects = Scripted::new(&fails);
            let _ = world.run_with(&effects);
            if calls.get() <= limit {
                break;
            }
            crash_points += 1;
            for content in precious {
                assert!(
                    holds(&world, content),
                    "{content:?} lost at crash point {limit}"
                );
            }

            world.run();
            let after = snapshot(&world);
            let differing: Vec<&String> = expected
                .keys()
                .chain(after.keys())
                .filter(|name| expected.get(*name) != after.get(*name))
                .collect();
            assert!(
                differing.is_empty(),
                "after a crash at point {limit} these differ from a run without one: {differing:?}"
            );
            for content in precious {
                assert!(
                    holds(&world, content),
                    "{content:?} lost after point {limit}"
                );
            }
        }
        assert!(crash_points > 0, "the scenario changes something");
    }

    const UNSAVED: &[u8] = b"unsaved work";

    fn differing_trash_world(taken: bool) -> World {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("unsaved.txt"), UNSAVED).expect("an added file");
        if taken {
            world.tree().file("projects/a/stranger.txt", "not ours");
        }
        world
    }

    #[test]
    fn a_crash_anywhere_in_putting_a_differing_tree_back_loses_nothing() {
        survives_every_crash(|| differing_trash_world(false), &[UNSAVED]);
    }

    #[test]
    fn a_crash_anywhere_in_recovering_a_differing_tree_loses_nothing() {
        survives_every_crash(|| differing_trash_world(true), &[UNSAVED, b"not ours"]);
    }

    #[test]
    fn a_crash_anywhere_in_finishing_a_trashed_tree_ends_in_the_same_state() {
        survives_every_crash(
            || {
                let mut world = World::new();
                let archive = world.archived(ID, "projects/a", FILES);
                world.removing(ID, "projects/a", &archive);
                world.to_trash(ID, "projects/a");
                world
            },
            &[],
        );
    }

    #[test]
    fn a_crash_anywhere_in_finishing_a_folder_in_place_ends_in_the_same_state() {
        survives_every_crash(
            || {
                let mut world = World::new();
                let archive = world.archived(ID, "projects/a", FILES);
                world.removing(ID, "projects/a", &archive);
                world
            },
            &[],
        );
    }

    #[test]
    fn a_crash_anywhere_in_keeping_a_differing_folder_in_place_loses_nothing() {
        survives_every_crash(
            || {
                let mut world = World::new();
                let archive = world.archived(ID, "projects/a", FILES);
                world.removing(ID, "projects/a", &archive);
                fs::write(world.at("projects/a/unsaved.txt"), UNSAVED).expect("an added file");
                world
            },
            &[UNSAVED],
        );
    }

    #[test]
    fn a_crash_anywhere_in_settling_an_unearthing_ends_in_the_same_state() {
        survives_every_crash(
            || {
                let mut world = World::new();
                world.archived(ID, "projects/a", FILES);
                unearthing(&world, ID, "projects/a");
                world
            },
            &[],
        );
    }

    #[test]
    fn a_crash_anywhere_in_settling_a_recorded_unearth_ends_in_the_same_state() {
        survives_every_crash(
            || {
                let mut world = World::new();
                world.archived(ID, "projects/a", FILES);
                record_the_unearth(&world, ID);
                fs::write(world.at("projects/a/unsaved.txt"), UNSAVED).expect("an added file");
                unearthing(&world, ID, "projects/a");
                world
            },
            &[UNSAVED],
        );
    }

    #[test]
    fn a_crash_anywhere_in_dropping_an_unearthing_keeps_the_folder() {
        survives_every_crash(
            || {
                let mut world = World::new();
                world.archived(ID, "projects/a", FILES);
                fs::write(world.at("projects/a/unsaved.txt"), UNSAVED).expect("an added file");
                unearthing(&world, ID, "projects/a");
                world
            },
            &[UNSAVED],
        );
    }

    #[test]
    fn a_crash_anywhere_in_catching_up_with_an_archive_ends_in_the_same_state() {
        survives_every_crash(
            || {
                let mut world = World::new();
                world.archived(ID, "projects/a", FILES);
                lagging(&world, ID, "projects/a");
                world
            },
            &[],
        );
    }

    #[test]
    fn a_crash_anywhere_in_restoring_missing_entries_ends_in_the_same_state() {
        survives_every_crash(
            || {
                let mut world = World::new();
                own_live(&mut world, ID, "projects/a");
                world.tree().file(".antiq/trash/leftover/f", "x");
                world.tree().file(".antiq/.config.toml.4242.0.tmp", "x");
                world
                    .tree()
                    .file(&format!(".antiq/tracking/{OTHER_ID}.manifest.json"), "{}");
                world
            },
            &[],
        );
    }

    // =====================================================================
    // Failures
    // =====================================================================

    #[test]
    fn a_collection_root_that_cannot_be_opened_stops_the_run() {
        let world = World::new();
        let collection = Collection::new(world.at("missing"), Config::new(id(COLLECTION_ID)));
        let error = run(&collection, &world.lock, &world.clock, &RealEffects)
            .expect_err("there is no collection");
        assert!(
            matches!(error, RepairError::Io { action: "open", .. }),
            "{error}"
        );
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_tracking_directory_that_cannot_be_listed_stops_the_run() {
        let world = World::new();
        let directory = world.at(".antiq/tracking");
        fs::set_permissions(&directory, Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(&directory).is_err(),
            "this test needs a user that mode 000 locks out"
        );

        let result = world.run_with(&RealEffects);
        fs::set_permissions(&directory, Permissions::from_mode(0o755)).expect("chmod back");

        let error = result.expect_err("the entries cannot be read");
        assert!(matches!(error, RepairError::Tracking(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn an_entry_that_cannot_be_written_stops_the_run() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        let fails = |what: &str, _: &str| what == "write";
        let effects = Scripted::new(&fails);

        let error = world
            .run_with(&effects)
            .expect_err("the entry is not stored");

        assert!(matches!(error, RepairError::Tracking(_)), "{error}");
        assert_eq!(world.entry(ID).state, EntryState::Removing);
    }

    #[test]
    fn a_trash_entry_that_is_a_file_is_kept_and_reported() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world
            .tree()
            .file(&format!(".antiq/trash/{ID}"), "not a tree");

        let warnings = world.run();

        assert!(mentions(&warnings, ID), "{warnings:?}");
        assert!(world.trash(ID).exists());
        assert_eq!(world.entry(ID).state, EntryState::Removing);
    }

    #[test]
    fn a_folder_in_place_that_cannot_be_removed_is_a_warning() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        let fails = |what: &str, subject: &str| what == "remove" && subject.ends_with("projects/a");
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(
            mentions(&warnings, "cannot remove `projects/a`"),
            "{warnings:?}"
        );
        assert!(world.at("projects/a/notes.txt").exists());
        assert_eq!(world.entry(ID).state, EntryState::Removing);
    }

    #[test]
    fn a_folder_in_place_is_removed_through_the_effects() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        let effects = Scripted::new(&never);

        world.run_with(&effects).expect("runs");

        assert!(!world.at("projects/a").exists());
        let log = effects.log();
        assert!(
            log.iter().any(|call| call.ends_with("remove projects/a")),
            "{log:?}"
        );
    }

    #[test]
    fn a_recovered_tree_without_a_folder_in_place_is_nothing_to_report() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        fs::remove_dir_all(world.at("projects/a")).expect("no folder");
        fs::create_dir_all(world.at(&format!(".antiq/recovered/{ID}-{NOW_STAMP}")))
            .expect("the earlier move");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn without_a_recovered_directory_the_folder_in_place_is_the_tree() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        fs::remove_dir(world.at(".antiq/recovered")).expect("empty");

        world.run();

        assert!(!world.at("projects/a").exists());
        assert_eq!(world.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_payload_without_a_listing_keeps_the_tree() {
        unusable_listing(Some(1), |world, _, _| {
            let path = world.collection.object_dir(id(ID)).join("meta.1");
            let mut document: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).expect("reads")).expect("json");
            document["payloads"][0]
                .as_object_mut()
                .expect("a payload")
                .remove("listing");
            fs::write(&path, serde_json::to_vec(&document).expect("json")).expect("written");
        });
    }

    #[test]
    fn metadata_that_cannot_be_read_keeps_the_tree() {
        unusable_listing(None, |world, _, _| {
            let path = world.collection.object_dir(id(ID)).join("meta.1");
            fs::write(path, "not json").expect("damaged");
        });
    }

    #[test]
    fn a_folder_that_cannot_be_entered_is_kept() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        let folder = world.at("projects/a");
        fs::set_permissions(&folder, Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(&folder).is_err(),
            "this test needs a user that mode 000 locks out"
        );

        let warnings = world.run();
        fs::set_permissions(&folder, Permissions::from_mode(0o755)).expect("chmod back");

        assert!(world.at("projects/a/notes.txt").exists());
        assert_eq!(world.entry(ID).state, EntryState::Live);
        assert!(mentions(&warnings, "projects/a"), "{warnings:?}");
    }

    #[test]
    fn a_trashed_tree_with_an_unreadable_directory_is_kept() {
        let (world, entry, warnings) = differing_trash(|world| {
            fs::set_permissions(world.trash(ID).join("src"), Permissions::from_mode(0o000))
                .expect("chmod");
            assert!(
                fs::read_dir(world.trash(ID).join("src")).is_err(),
                "this test needs a user that mode 000 locks out"
            );
        });
        fs::set_permissions(world.at("projects/a/src"), Permissions::from_mode(0o755))
            .expect("chmod back");
        assert_eq!(entry.state, EntryState::Live);
        assert!(mentions(&warnings, "projects/a"), "{warnings:?}");
    }

    #[test]
    fn a_recovered_move_that_cannot_open_its_directory_keeps_the_tree_in_the_trash() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        world.tree().file("projects/a/stranger.txt", "not ours");
        fs::remove_dir(world.at(".antiq/recovered")).expect("empty");

        let warnings = world.run();

        assert!(world.trash(ID).join("new.txt").exists());
        assert!(mentions(&warnings, "stays in the trash"), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Removing);
    }

    #[test]
    fn a_failed_flush_after_a_move_to_recovered_keeps_the_entry() {
        let mut world = World::new();
        let archive = world.archived(ID, "projects/a", FILES);
        world.removing(ID, "projects/a", &archive);
        world.to_trash(ID, "projects/a");
        fs::write(world.trash(ID).join("new.txt"), "x").expect("an added file");
        world.tree().file("projects/a/stranger.txt", "not ours");
        let fails = |what: &str, _: &str| what == "flush";
        let effects = Scripted::new(&fails);

        let warnings = world.run_with(&effects).expect("the run goes on");

        assert!(mentions(&warnings, "flushed"), "{warnings:?}");
        assert_eq!(world.entry(ID).state, EntryState::Removing);
        assert!(
            world
                .at(&format!(".antiq/recovered/{ID}-{NOW_STAMP}/new.txt"))
                .exists()
        );
    }

    #[test]
    fn a_difference_that_is_not_a_missing_path_is_not_missing() {
        let world = World::new();
        let tree = TreeRoot::open(world.temp.root()).expect("opens");
        let unlisted = Difference::Changed {
            path: path("nowhere"),
            fields: vec![Field::Mode],
        };
        assert!(!only_missing(&unlisted, &[], &tree));
    }

    #[test]
    fn a_missing_run_directory_is_nothing_to_empty() {
        let world = World::new();
        fs::remove_dir(world.at(".antiq/staging")).expect("empty");

        let warnings = world.run();

        assert!(warnings.is_empty(), "{warnings:?}");
    }

    // =========================================================
    // The move other callers share
    // =========================================================

    #[test]
    fn a_recovered_tree_is_named_by_its_id_and_the_time() {
        let name = recovered_name(
            id("01HZZZZZZZZZZZZZZZZZZZZZZZ"),
            time("2026-11-01T12:00:00Z"),
        );
        assert_eq!(name, "01HZZZZZZZZZZZZZZZZZZZZZZZ-20261101T120000Z");
    }

    #[test]
    fn a_trashed_tree_moves_to_its_target_and_the_move_is_flushed() {
        let temp = crate::testing::tree::TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        let object = id("01HZZZZZZZZZZZZZZZZZZZZZZZ");
        fs::create_dir_all(collection.trash_path(object)).expect("a trashed tree");
        fs::write(collection.trash_path(object).join("f"), "x").expect("a file");

        let (outcome, warnings) = move_trashed(
            &RealEffects,
            &collection,
            object,
            collection.root(),
            &path("back"),
            collection.root(),
        );

        assert!(matches!(outcome, Outcome::Done));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(collection.root().join("back/f").exists());
        assert!(!collection.trash_path(object).exists());
    }

    #[test]
    fn a_taken_target_refuses_the_move_and_leaves_the_tree_in_the_trash() {
        let temp = crate::testing::tree::TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        let object = id("01HZZZZZZZZZZZZZZZZZZZZZZZ");
        fs::create_dir_all(collection.trash_path(object)).expect("a trashed tree");
        fs::create_dir_all(collection.root().join("back")).expect("a stranger's folder");

        let (outcome, _) = move_trashed(
            &RealEffects,
            &collection,
            object,
            collection.root(),
            &path("back"),
            collection.root(),
        );

        assert!(matches!(outcome, Outcome::Refused(_)));
        assert!(collection.trash_path(object).exists());
    }
}
