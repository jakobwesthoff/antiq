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
use crate::meta::{Payload, PayloadKind};
use crate::pack::WalkRules;
use crate::pack::verify::{Difference, Field, compare_with_listing};
use crate::relpath::{Name, RelPath};
use crate::report::Warning;
use crate::store::Store;
use crate::tracking::{self, EntryState, TrackingEntry, TrackingError};
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
    fn write_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        clock: &dyn Clock,
        entry: &mut TrackingEntry,
    ) -> Result<(), TrackingError>;
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

    fn write_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        clock: &dyn Clock,
        entry: &mut TrackingEntry,
    ) -> Result<(), TrackingError> {
        tracking::write(collection, lock, clock, entry)
    }
}

/// Runs the repairs in the order of spec "Operations". The lock proves
/// that no other run changes the collection meanwhile.
///
/// Problems with one tree or one entry become warnings and the run goes on.
/// Only what stops every later step ends it: a collection that cannot be
/// opened, the tracking directory, an entry that cannot be written.
pub(crate) fn run(
    collection: &Collection,
    lock: &RunLock,
    clock: &dyn Clock,
    effects: &dyn Effects,
) -> Result<Vec<Warning>, RepairError> {
    let root = TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;
    let tracked = tracking::read_all(collection)?;
    let mut repairs = Repairs {
        collection,
        lock,
        clock,
        effects,
        root,
        entries: tracked.entries,
        protected: HashSet::new(),
        warnings: Vec::new(),
    };

    // A tree in the trash whose entry cannot be read may be the only copy
    // of what the entry was about to remove, and no comparison is possible
    // without the entry.
    for damaged in &tracked.damaged {
        repairs.protected.insert(damaged.id);
        repairs.warn(format!(
            "the tracking entry of {} cannot be used ({}); a tree of it in the trash is kept",
            damaged.id, damaged.error
        ));
    }

    // The order matters: a tree that differs from its archive has to leave
    // the trash before the trash is emptied.
    repairs.finish_removals()?;
    repairs.empty_run_directories();
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

/// Where the tree of a `removing` entry is.
enum Located {
    /// Archive step 3's rename happened.
    Trashed,
    /// The rename failed, or the delete has been going on in place.
    Folder,
    /// An earlier repair moved the tree to `recovered/` and was cut short
    /// before it could write the entry.
    Recovered,
    Nothing,
}

/// How moving a trashed tree went.
enum Outcome {
    Done,
    /// The tree moved but the move could not be made durable. It is
    /// reported and the entry stays as it is.
    Stuck,
    /// The tree stayed in the trash.
    Refused(String),
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
    /// The entries as they stand now: updated with every write.
    entries: Vec<TrackingEntry>,
    /// Trees in the trash that the emptying must leave alone.
    protected: HashSet<ObjectId>,
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
        if let Some(known) = self.entries.iter_mut().find(|known| known.id == entry.id) {
            *known = entry;
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
                Outcome::Refused(_) => {}
            }
        }

        let name = format!("{id}-{}", stamp(self.now()));
        let recovered = self.collection.recovered_dir();
        let to = RelPath::parse(&name).expect("an ID and a time make a plain name");
        match self.move_durably(id, &recovered, &to, &recovered) {
            Outcome::Done => {
                self.warn(format!(
                    "`{shown}` differs from its archive ({reason}) and its path is taken; it is kept in .antiq/recovered/{name}"
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

    /// Moves the trashed tree of `id` to `to` below `to_dir` and makes the
    /// move durable before the caller writes the entry that depends on it:
    /// if the entry reached the disk and the move did not, the next run
    /// would find a tree in the trash with no `removing` entry.
    fn move_durably(&mut self, id: ObjectId, to_dir: &Path, to: &RelPath, flush: &Path) -> Outcome {
        let trash_dir = self.collection.trash_dir();
        let name = RelPath::parse(&id.to_string()).expect("an ID is a plain name");
        let roots = TreeRoot::open(&trash_dir).and_then(|from| Ok((from, TreeRoot::open(to_dir)?)));
        let (from_root, to_root) = match roots {
            Ok(roots) => roots,
            Err(error) => return Outcome::Refused(error.to_string()),
        };
        let moved = match self.effects.move_dir(&from_root, &name, &to_root, to) {
            Ok(moved) => moved,
            Err(error) => return Outcome::Refused(error.to_string()),
        };
        for dir in [flush, trash_dir.as_path()] {
            if let Err(error) = self.effects.flush_dir(dir) {
                self.protected.insert(id);
                self.warn(format!(
                    "the tree of {id} was moved but cannot be flushed: {error}"
                ));
                return Outcome::Stuck;
            }
        }
        if let Some(error) = moved.mode_restore_error {
            self.warn(format!("cannot restore the mode of a moved tree: {error}"));
        }
        Outcome::Done
    }

    fn locate(&self, entry: &TrackingEntry) -> Located {
        if fs::symlink_metadata(self.collection.trash_path(entry.id)).is_ok() {
            Located::Trashed
        } else if self.moved_to_recovered(entry) {
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
        let listing = match self.read_listing(entry.id, &payload) {
            Ok(listing) => listing,
            Err(reason) => return Verdict::Differs(reason),
        };
        let rules = WalkRules {
            junk: self.junk_names(&listing),
            staged: false,
        };
        let compared = match compare_with_listing(&listing, tree, &rules) {
            Ok(compared) => compared,
            Err(error) => return Verdict::Differs(error.to_string()),
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

    fn unusable_listing(break_it: impl FnOnce(&World, &Archived, &mut TrackingEntry)) {
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
        assert!(mentions(&warnings, "projects/a"), "{warnings:?}");
    }

    #[test]
    fn an_entry_that_names_no_payload_keeps_the_tree() {
        unusable_listing(|_, _, entry| entry.removing_payload = None);
    }

    #[test]
    fn an_entry_that_names_an_unknown_payload_keeps_the_tree() {
        unusable_listing(|_, _, entry| {
            entry.removing_payload =
                Some(FileName::new("0000000000000000-x.tar.zst").expect("a name"));
        });
    }

    #[test]
    fn an_object_without_metadata_keeps_the_tree() {
        unusable_listing(|world, _, _| {
            let dir = world.collection.object_dir(id(ID));
            fs::remove_file(dir.join("meta.1")).expect("no metadata");
        });
    }

    #[test]
    fn a_missing_listing_file_keeps_the_tree() {
        unusable_listing(|world, _, _| {
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
        unusable_listing(|world, _, _| {
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
    fn a_failed_move_back_goes_to_recovered_and_both_failing_protects_the_tree() {
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
        unusable_listing(|world, _, _| {
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
        unusable_listing(|world, _, _| {
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
}
