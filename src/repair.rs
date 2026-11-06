// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repairs that open every run, after the lock is taken.

use crate::collection::Collection;
use crate::filesystem::atomic::{Durable, RealDurable, create_dir_chain, flush_dir_of};
use crate::filesystem::moves::move_dir;
use crate::filesystem::remove::{remove_tree, remove_tree_at};
use crate::filesystem::tree::TreeRoot;
use crate::lock::RunLock;
use crate::relpath::{Name, RelPath};
use crate::report::Warning;
use crate::tree::EntryKind;
use crate::versions::VersionsError;
use crate::versions::names::{VersionFile, parse_file_name};
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StoreScan, StoreVersion};
use crate::walk::MAX_DEPTH;
use serde::{Serialize, Serializer};
use std::collections::BTreeSet;
use std::error::Error as StdError;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Where a tree taken out of `trash/` went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// Back to its live path.
    Live,
    /// To `recovered/`, where `status` names it.
    Recovered,
}

/// The run directory a leftover chain of empty folders lies in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    Trash,
    Doomed,
}

impl fmt::Display for Area {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Area::Trash => "trash",
            Area::Doomed => "doomed",
        })
    }
}

fn stamp_text<S: Serializer>(stamp: &Stamp, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(stamp)
}

/// One repair of the start of a run (spec "Repairs at the start of a
/// run"). In JSON it is `{"repair": "<snake_case variant>", ...fields}`.
/// A `piece` or `folder` is relative to `pieces/` (to the collection root
/// for repair 5), a `tree` or `path` to `<area>/<stamp>/` (to `<area>/`
/// where `stamp` is absent); all are escaped like a path of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "repair", rename_all = "snake_case")]
pub enum Repair {
    /// 1: the sidecars of version `version` of the store folder `piece`,
    /// whose `.tar.zst` is still staged.
    DeleteSidecars {
        piece: RelPath,
        #[serde(serialize_with = "stamp_text")]
        version: Stamp,
    },
    /// 2: the entry `entry` of `tmp/`.
    DeleteTemporary { entry: Name },
    /// 3: the tree `tree` below `doomed/<stamp>/`. `stamp` is absent for an
    /// entry that lies directly in `doomed/`.
    DeleteDoomed {
        #[serde(skip_serializing_if = "Option::is_none")]
        stamp: Option<Name>,
        tree: RelPath,
    },
    /// 4: the tree `tree` below `trash/<stamp>/`, to `destination`. `stamp`
    /// is absent for an entry that lies directly in `trash/`.
    GiveBack {
        #[serde(skip_serializing_if = "Option::is_none")]
        stamp: Option<Name>,
        tree: RelPath,
        destination: Destination,
    },
    /// 5: version `version` of the live piece `piece` that a died unearth
    /// left. With `delete` false the stamp names no version and is only
    /// dropped from the marker.
    DeleteRestoredVersion {
        piece: RelPath,
        #[serde(serialize_with = "stamp_text")]
        version: Stamp,
        delete: bool,
    },
    /// 6: the empty folder `folder` below `pieces/`.
    RemoveEmptyFolder { folder: RelPath },
    /// 3 and 4: the folder `path` below `<area>/<stamp>/` that holds no
    /// non-directory entry. `path` is the root for the stamp folder itself.
    RemoveEmptyChain {
        area: Area,
        stamp: Name,
        path: RelPath,
    },
}

impl Repair {
    /// What the repair does, in the imperative and with paths relative to
    /// the collection root, as a dry run lists it.
    pub fn describe(&self) -> String {
        let stamped = |area: &str, stamp: &Option<Name>, tree: &RelPath| match stamp {
            Some(stamp) => format!(".antiq/{area}/{stamp}/{tree}"),
            None => format!(".antiq/{area}/{tree}"),
        };
        match self {
            Repair::DeleteSidecars { piece, version } => format!(
                "delete the sidecars of {version} in .antiq/pieces/{piece} (the archive is not there)"
            ),
            Repair::DeleteTemporary { entry } => format!("delete .antiq/tmp/{entry}"),
            Repair::DeleteDoomed { stamp, tree } => {
                format!("delete {}", stamped("doomed", stamp, tree))
            }
            Repair::GiveBack {
                stamp,
                tree,
                destination,
            } => {
                let from = stamped("trash", stamp, tree);
                match destination {
                    Destination::Live => format!("move {from} back to {tree}"),
                    Destination::Recovered => {
                        format!("move {from} to {}", stamped("recovered", stamp, tree))
                    }
                }
            }
            Repair::DeleteRestoredVersion {
                piece,
                version,
                delete: true,
            } => format!("delete version {version} of {piece} that an unearth left"),
            Repair::DeleteRestoredVersion {
                piece,
                version,
                delete: false,
            } => format!(
                "drop the stamp {version} from the marker of {piece}, which names no version"
            ),
            Repair::RemoveEmptyFolder { folder } => {
                format!("remove the empty folder .antiq/pieces/{folder}")
            }
            Repair::RemoveEmptyChain { area, stamp, path } => {
                let folder = stamped(&area.to_string(), &Some(stamp.clone()), path);
                format!("remove the empty folder {}", folder.trim_end_matches('/'))
            }
        }
    }
}

/// Why one repair failed: what it was doing, where, and the cause. A
/// failing repair is a warning and never fails the run.
#[derive(Debug, thiserror::Error)]
#[error("cannot {step} {place}: {source}")]
pub struct RepairError {
    pub step: &'static str,
    pub place: String,
    #[source]
    pub source: Box<dyn StdError + Send + Sync>,
}

/// What the repairs of a run did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The repairs that were done, as they were done.
    pub done: Vec<Repair>,
    /// The repairs that failed, and anything that could not be read.
    pub warnings: Vec<Warning>,
}

/// What a dry run lists: the repairs a run would attempt now, in order, and
/// what could not be read to find out.
#[derive(Debug, Default)]
pub struct Planned {
    pub repairs: Vec<Repair>,
    pub warnings: Vec<Warning>,
}

// =========================================================
// Naming and failing
// =========================================================

fn failure(
    step: &'static str,
    place: String,
    source: impl StdError + Send + Sync + 'static,
) -> RepairError {
    RepairError {
        step,
        place,
        source: Box::new(source),
    }
}

/// How an error names `rel`, a path below `.antiq/<area>`: root-qualified,
/// so two failures with equal relative paths in different areas differ.
fn place(area: &str, rel: &RelPath) -> String {
    if rel.is_root() {
        format!(".antiq/{area}")
    } else {
        format!(".antiq/{area}/{rel}")
    }
}

fn name_of(bytes: &[u8]) -> Name {
    Name::from_bytes(bytes.to_vec()).expect("a component of a path is a name")
}

fn parent_of(path: &RelPath) -> RelPath {
    let bytes = path.as_bytes();
    match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(split) => RelPath::from_bytes(bytes[..split].to_vec())
            .expect("the front of a valid path is valid"),
        None => RelPath::root(),
    }
}

fn child_of(dir: &RelPath, name: &Name) -> RelPath {
    dir.join(name)
}

/// The directory as a root, or `None` when it is not there.
fn open_dir(path: &Path) -> io::Result<Option<TreeRoot>> {
    match TreeRoot::open(path) {
        Ok(root) => Ok(Some(root)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

// =========================================================
// Planning: what a run would do, found by reading only
// =========================================================

/// Finds the repairs a run would attempt now, in the order it would do
/// them, without writing anything. A repair that cannot be planned because
/// something cannot be read is a warning, and the others are still planned.
pub fn plan(collection: &Collection) -> Planned {
    let mut planner = Planner {
        collection,
        scan: None,
        repairs: Vec::new(),
        warnings: Vec::new(),
        gone: BTreeSet::new(),
    };
    planner.scan_store();
    planner.interrupted_archives();
    planner.temporary_files();
    planner.doomed();
    planner.trash();
    Planned {
        repairs: planner.repairs,
        warnings: planner.warnings,
    }
}

struct Planner<'a> {
    collection: &'a Collection,
    scan: Option<StoreScan>,
    repairs: Vec<Repair>,
    warnings: Vec<Warning>,
    /// Paths below `pieces/` that the repairs planned so far remove.
    gone: BTreeSet<RelPath>,
}

/// A tree or leftover entry below `<area>/<stamp>/`, found by [`Planner::trees_below`].
struct Found {
    stamp: Option<Name>,
    tree: RelPath,
    /// Only a directory can go back to its live path.
    is_dir: bool,
}

impl Planner<'_> {
    fn warn(&mut self, error: RepairError) {
        self.warnings.push(Warning::new(error.to_string()));
    }

    fn cannot_read(
        &mut self,
        area: &str,
        rel: &RelPath,
        source: impl StdError + Send + Sync + 'static,
    ) {
        self.warn(failure("read", place(area, rel), source));
    }

    fn scan_store(&mut self) {
        match store::scan(&self.collection.pieces_dir()) {
            Ok(scan) => self.scan = Some(scan),
            Err(error) => self.cannot_read("pieces", &RelPath::root(), error),
        }
    }

    // ---- 1: interrupted archive step 4 ----

    fn interrupted_archives(&mut self) {
        let tmp_pack = self.collection.tmp_dir().join("pack");
        let staging = match open_dir(&tmp_pack) {
            Ok(Some(root)) => root,
            Ok(None) => return,
            Err(error) => return self.cannot_read("tmp/pack", &RelPath::root(), error),
        };
        let Some(pieces) = self.pieces_root() else {
            return;
        };
        let stamps = match staging.read_dir(&RelPath::root()) {
            Ok(entries) => entries,
            Err(error) => return self.cannot_read("tmp/pack", &RelPath::root(), error),
        };
        let mut stamps: Vec<_> = stamps
            .into_iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .filter_map(|entry| {
                let stamp = Stamp::parse(&String::from_utf8_lossy(entry.name.as_bytes())).ok()?;
                Some((stamp, entry))
            })
            .collect();
        stamps.sort_by_key(|(stamp, _)| *stamp);
        for (stamp, entry) in stamps {
            let mut folders = Vec::new();
            self.staged_folders(&staging, &entry.path, &RelPath::root(), 0, &mut folders);
            folders.sort_by_key(|folder| (folder.components().count(), folder.clone()));
            for staged in folders {
                self.plan_sidecars(&pieces, &tmp_pack, &staged, &stamp);
            }
        }
    }

    fn pieces_root(&mut self) -> Option<TreeRoot> {
        match open_dir(&self.collection.pieces_dir()) {
            Ok(root) => root,
            Err(error) => {
                self.cannot_read("pieces", &RelPath::root(), error);
                None
            }
        }
    }

    /// Every directory below the stamp folder `stamp_dir` of the staging,
    /// as a path below the stamp folder.
    fn staged_folders(
        &mut self,
        staging: &TreeRoot,
        stamp_dir: &RelPath,
        below: &RelPath,
        depth: usize,
        found: &mut Vec<RelPath>,
    ) {
        if depth > MAX_DEPTH {
            let at = stamp_dir.concat(below);
            let error = io::Error::other(format!("is nested more than {MAX_DEPTH} levels"));
            return self.cannot_read("tmp/pack", &at, error);
        }
        let here = stamp_dir.concat(below);
        let entries = match staging.read_dir(&here) {
            Ok(entries) => entries,
            Err(error) => return self.cannot_read("tmp/pack", &here, error),
        };
        for entry in entries {
            if entry.kind != EntryKind::Directory {
                continue;
            }
            let inner = child_of(below, &entry.name);
            found.push(inner.clone());
            self.staged_folders(staging, stamp_dir, &inner, depth + 1, found);
        }
    }

    fn plan_sidecars(
        &mut self,
        pieces: &TreeRoot,
        tmp_pack: &Path,
        staged: &RelPath,
        stamp: &Stamp,
    ) {
        let stored = match store::lookup_exact(pieces, staged) {
            Ok(Some(stored)) => stored,
            Ok(None) => return,
            Err(error) => return self.cannot_read("pieces", staged, error),
        };
        // The folder may hold other whole versions, so it can be a store
        // piece as well as a folder of sidecars only.
        let Some(version) = self
            .scan
            .as_ref()
            .into_iter()
            .flat_map(|scan| scan.pieces.iter().chain(&scan.sidecar_only))
            .filter(|piece| piece.path == stored)
            .flat_map(|piece| &piece.versions)
            .find(|version| version.stamp == *stamp && version.is_incomplete())
            .cloned()
        else {
            return;
        };
        match store::is_being_stored(tmp_pack, staged, stamp) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                let at = RelPath::parse(&format!("{stamp}"))
                    .expect("a stamp is a path")
                    .concat(staged);
                return self.cannot_read("tmp/pack", &at, error);
            }
        }
        for kind in VersionFile::ALL {
            for file in version.files(kind) {
                self.gone
                    .insert(child_of(&stored, &name_of(file.as_bytes())));
            }
        }
        self.simulate_chain(pieces, &stored);
        self.repairs.push(Repair::DeleteSidecars {
            piece: stored,
            version: *stamp,
        });
    }

    /// Marks `folder` and the parents above it as gone when nothing is
    /// left in them, as [`store::remove_empty_chain`] removes them.
    fn simulate_chain(&mut self, pieces: &TreeRoot, folder: &RelPath) {
        let mut at = folder.clone();
        while !at.is_root() {
            let entries = match pieces.read_dir(&at) {
                Ok(entries) => entries,
                Err(error) => return self.cannot_read("pieces", &at, error),
            };
            if entries.iter().any(|entry| !self.gone.contains(&entry.path)) {
                return;
            }
            self.gone.insert(at.clone());
            at = parent_of(&at);
        }
    }

    // ---- 2: temporary files ----

    fn temporary_files(&mut self) {
        let root = match open_dir(&self.collection.tmp_dir()) {
            Ok(Some(root)) => root,
            Ok(None) => return,
            Err(error) => return self.cannot_read("tmp", &RelPath::root(), error),
        };
        match root.read_dir(&RelPath::root()) {
            Ok(mut entries) => {
                entries.sort_by(|left, right| left.name.cmp(&right.name));
                for entry in entries {
                    self.repairs
                        .push(Repair::DeleteTemporary { entry: entry.name });
                }
            }
            Err(error) => self.cannot_read("tmp", &RelPath::root(), error),
        }
    }

    // ---- 3: doomed/ ----

    fn doomed(&mut self) {
        let found = self.trees_below(Area::Doomed);
        for item in found {
            match item {
                Item::Tree(Found { stamp, tree, .. }) => {
                    self.repairs.push(Repair::DeleteDoomed { stamp, tree });
                }
                Item::Chain { stamp, path } => self.repairs.push(Repair::RemoveEmptyChain {
                    area: Area::Doomed,
                    stamp,
                    path,
                }),
            }
        }
    }

    // ---- 4: trash/ ----

    fn trash(&mut self) {
        // Without the store the trees cannot be told from leftovers, and
        // giving back the first component of every one would misplace them.
        if self.scan.is_none() {
            return;
        }
        let collection = match TreeRoot::open(self.collection.root()) {
            Ok(root) => root,
            Err(error) => return self.cannot_read("", &RelPath::root(), error),
        };
        let mut claimed: BTreeSet<RelPath> = BTreeSet::new();
        for item in self.trees_below(Area::Trash) {
            match item {
                Item::Tree(Found {
                    stamp,
                    tree,
                    is_dir,
                }) => {
                    let destination = if is_dir && self.live_is_free(&collection, &claimed, &tree) {
                        claimed.insert(tree.clone());
                        Destination::Live
                    } else {
                        Destination::Recovered
                    };
                    self.repairs.push(Repair::GiveBack {
                        stamp,
                        tree,
                        destination,
                    });
                }
                Item::Chain { stamp, path } => self.repairs.push(Repair::RemoveEmptyChain {
                    area: Area::Trash,
                    stamp,
                    path,
                }),
            }
        }
    }

    /// Whether a tree could go to `live` now: its parent is a directory,
    /// or will be one because an earlier tree goes there, and nothing is at
    /// `live`. Only a prediction; the move decides.
    fn live_is_free(
        &self,
        collection: &TreeRoot,
        claimed: &BTreeSet<RelPath>,
        live: &RelPath,
    ) -> bool {
        let parent = parent_of(live);
        let parent_claimed = claimed
            .iter()
            .any(|path| *path == parent || is_below(path, &parent));
        let parent_ok = parent.is_root()
            || parent_claimed
            || collection
                .lstat(&parent)
                .is_ok_and(|entry| entry.kind == EntryKind::Directory);
        let taken = claimed
            .iter()
            .any(|path| path == live || is_below(path, live))
            || collection.lstat(live).is_ok();
        parent_ok && !taken
    }

    /// The trees and leftovers below `trash/` or `doomed/`, then the
    /// chains of folders that hold nothing, in the order of the spec.
    ///
    /// Per stamp folder, in stamp order (names that are no stamp after
    /// them, by bytes): the trees, which are the shortest paths below the
    /// folder for which a version with that stamp exists, by the volume's
    /// name equality; then every other child that holds a non-directory
    /// entry, whole; then the empty chains, deepest first, the stamp
    /// folder last. Entries that are no directory and lie directly in the
    /// area come at the end, each as a tree of its own.
    fn trees_below(&mut self, area: Area) -> Vec<Item> {
        let dir = match area {
            Area::Trash => self.collection.trash_dir(),
            Area::Doomed => self.collection.doomed_dir(),
        };
        let area_name = area.to_string();
        let root = match open_dir(&dir) {
            Ok(Some(root)) => root,
            Ok(None) => return Vec::new(),
            Err(error) => {
                self.cannot_read(&area_name, &RelPath::root(), error);
                return Vec::new();
            }
        };
        let mut entries = match root.read_dir(&RelPath::root()) {
            Ok(entries) => entries,
            Err(error) => {
                self.cannot_read(&area_name, &RelPath::root(), error);
                return Vec::new();
            }
        };
        entries.sort_by_key(|entry| {
            let stamp = Stamp::parse(&String::from_utf8_lossy(entry.name.as_bytes())).ok();
            (stamp.is_none(), stamp, entry.name.clone())
        });
        let mut items = Vec::new();
        let mut loose = Vec::new();
        for entry in entries {
            if entry.kind != EntryKind::Directory {
                loose.push(entry.name);
                continue;
            }
            self.stamp_folder(area, &dir, &entry.name, &mut items);
        }
        for name in loose {
            items.push(Item::Tree(Found {
                stamp: None,
                tree: RelPath::root().join(&name),
                is_dir: false,
            }));
        }
        items
    }

    fn stamp_folder(&mut self, area: Area, dir: &Path, stamp: &Name, items: &mut Vec<Item>) {
        let area_name = area.to_string();
        let here = RelPath::root().join(stamp);
        let root = match TreeRoot::open(&dir.join(OsStr::from_bytes(stamp.as_bytes()))) {
            Ok(root) => root,
            Err(error) => return self.cannot_read(&area_name, &here, error),
        };
        let named = Stamp::parse(&String::from_utf8_lossy(stamp.as_bytes())).ok();
        let found = match (&named, &self.scan) {
            (Some(named), Some(scan)) => trees_of(&root, &RelPath::root(), scan, named),
            _ => Ok(Vec::new()),
        };
        let mut taken = match found {
            Ok(taken) => taken,
            Err(error) => return self.cannot_read(&area_name, &here, error),
        };
        let children = match root.read_dir(&RelPath::root()) {
            Ok(children) => children,
            Err(error) => return self.cannot_read(&area_name, &here, error),
        };
        let mut removed: BTreeSet<RelPath> = taken.iter().cloned().collect();
        let mut leftovers: Vec<(RelPath, bool)> = Vec::new();
        for child in &children {
            if removed.contains(&child.path) {
                continue;
            }
            let holds_tree = taken.iter().any(|tree| is_below(tree, &child.path));
            if holds_tree {
                continue;
            }
            let leftover = if child.kind == EntryKind::Directory {
                match has_non_directory(&root, &child.path, 0) {
                    Ok(holds) => holds,
                    Err(error) => {
                        return self.cannot_read(&area_name, &here.concat(&child.path), error);
                    }
                }
            } else {
                true
            };
            if leftover {
                removed.insert(child.path.clone());
                leftovers.push((child.path.clone(), child.kind == EntryKind::Directory));
            }
        }
        taken.sort_by_key(|tree| (tree.components().count(), tree.clone()));
        let trees = taken.into_iter().map(|tree| (tree, true));
        for (tree, is_dir) in trees.chain(leftovers) {
            items.push(Item::Tree(Found {
                stamp: Some(stamp.clone()),
                tree,
                is_dir,
            }));
        }
        let mut empties = Vec::new();
        if let Err(error) = empty_after(&root, &RelPath::root(), &removed, &mut empties) {
            return self.cannot_read(&area_name, &here, error);
        }
        for path in empties {
            items.push(Item::Chain {
                stamp: stamp.clone(),
                path,
            });
        }
    }
}

/// The store folders that hold a version with `stamp`, the shortest path
/// first. A version without its `.tar.zst` is no version.
fn pieces_with(scan: &StoreScan, stamp: &Stamp) -> Vec<RelPath> {
    let mut paths: Vec<RelPath> = scan
        .pieces
        .iter()
        .filter(|piece| {
            piece
                .versions
                .iter()
                .any(|version| version.stamp == *stamp && !version.is_incomplete())
        })
        .map(|piece| piece.path.clone())
        .collect();
    paths.sort_by_key(|path| (path.components().count(), path.clone()));
    paths
}

/// The trees of the stamp folder `base` of `root`, found the way repair 4
/// and `status` name them: the shortest paths below `base` for which a
/// store folder with a version of `stamp` exists, by the volume's name
/// equality. No tree lies below another. The paths are relative to `base`
/// and spelled as they are on the volume, shortest first.
pub(crate) fn trees_of(
    root: &TreeRoot,
    base: &RelPath,
    scan: &StoreScan,
    stamp: &Stamp,
) -> Result<Vec<RelPath>, VersionsError> {
    let mut trees: Vec<RelPath> = Vec::new();
    for candidate in pieces_with(scan, stamp) {
        if trees.iter().any(|tree| is_below(&candidate, tree)) {
            continue;
        }
        if let Some(found) = store::lookup_exact(root, &base.concat(&candidate))? {
            trees.push(inside(&found, base));
        }
    }
    Ok(trees)
}

/// `path` without the leading `base`, which `path` starts with.
fn inside(path: &RelPath, base: &RelPath) -> RelPath {
    let rest = &path.as_bytes()[base.as_bytes().len()..];
    RelPath::from_bytes(rest.strip_prefix(b"/").unwrap_or(rest).to_vec())
        .expect("what follows a path in a longer path is a path")
}

enum Item {
    Tree(Found),
    Chain { stamp: Name, path: RelPath },
}

/// Whether `path` lies below `ancestor`, which is not `path` itself.
fn is_below(path: &RelPath, ancestor: &RelPath) -> bool {
    let (path, ancestor) = (path.as_bytes(), ancestor.as_bytes());
    path.len() > ancestor.len() && path.starts_with(ancestor) && path[ancestor.len()] == b'/'
}

/// Whether `dir` holds an entry that is no directory, at any depth.
fn has_non_directory(root: &TreeRoot, dir: &RelPath, depth: usize) -> io::Result<bool> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other(format!(
            "is nested more than {MAX_DEPTH} levels"
        )));
    }
    for entry in root.read_dir(dir)? {
        if entry.kind != EntryKind::Directory || has_non_directory(root, &entry.path, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Collects the directories below `dir` (and `dir` itself) that hold
/// nothing but directories of this kind once `removed` is gone, the
/// deepest first. Returns whether `dir` is one.
fn empty_after(
    root: &TreeRoot,
    dir: &RelPath,
    removed: &BTreeSet<RelPath>,
    empties: &mut Vec<RelPath>,
) -> io::Result<bool> {
    let mut empty = true;
    let mut entries = root.read_dir(dir)?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in entries {
        if removed.contains(&entry.path) {
            continue;
        }
        if entry.kind == EntryKind::Directory {
            empty &= empty_after(root, &entry.path, removed, empties)?;
        } else {
            empty = false;
        }
    }
    if empty {
        empties.push(dir.clone());
    }
    Ok(empty)
}

// =========================================================
// Running: doing the repairs in order
// =========================================================

/// Repairs what an earlier run left behind. A repair that fails is a
/// warning and the others still run. `lock` is the proof that this process
/// is the one run that may change the collection.
pub fn run(collection: &Collection, lock: &RunLock) -> Outcome {
    let planned = plan(collection);
    let mut outcome = Outcome {
        done: Vec::new(),
        warnings: planned.warnings,
    };
    let mut state = State::default();
    for repair in planned.repairs {
        match apply(collection, lock, &repair, &mut state) {
            Ok(Some(done)) => outcome.done.push(done),
            Ok(None) => {}
            Err(error) => {
                if let Repair::DeleteSidecars { version, .. } = &repair {
                    state.kept.insert(name_of(version.to_string().as_bytes()));
                }
                outcome.warnings.push(Warning::new(error.to_string()));
            }
        }
        if matches!(&repair, Repair::DeleteTemporary { entry } if entry.as_bytes() == b"pack") {
            for stamp in &state.kept {
                outcome.warnings.push(Warning::new(format!(
                    ".antiq/tmp/pack/{stamp} stays: the sidecars it belongs to are not deleted yet"
                )));
            }
        }
    }
    outcome
}

/// The result of [`give_back`].
#[derive(Debug)]
pub(crate) struct GivenBack {
    pub destination: Destination,
    /// A failure after the tree arrived: the destination stands.
    pub note: Option<RepairError>,
}

/// Gives the tree `tree` of `trash/<stamp>/` back to `live` below the
/// collection, or, when that is not possible for any reason, moves it to
/// the same place below `recovered/` of `antiq`, creating and flushing the
/// chain there, `recovered/` itself included. `stamp` is absent for an
/// entry that lies directly in `trash/`.
/// Only a directory is tried at `live`: it never replaces anything, so a
/// path that is taken or has no directory as a parent fails the move. A
/// failure after the rename, a flush or the mode of the tree, does not
/// undo it and comes back as the note.
///
/// The empty folders left in `trash/` are not removed here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn give_back(
    trash: &TreeRoot,
    collection: &TreeRoot,
    antiq: &TreeRoot,
    stamp: Option<&Name>,
    tree: &RelPath,
    live: &RelPath,
    durable: &dyn Durable,
) -> Result<GivenBack, RepairError> {
    let step = "give a tree in trash/ back";
    let from = match stamp {
        Some(stamp) => RelPath::root().join(stamp).concat(tree),
        None => tree.clone(),
    };
    let here = place("trash", &from);
    let is_dir = trash
        .lstat(&from)
        .map_err(|error| failure(step, here.clone(), error))?
        .kind
        == EntryKind::Directory;

    let mut live_failure = None;
    if is_dir {
        match move_dir(trash, &from, collection, live) {
            Ok(moved) => {
                let late = moved
                    .mode_restore_error
                    .map(|error| failure(step, here.clone(), error));
                let note = late.or_else(|| {
                    after_move(durable, collection, live, trash, &from)
                        .err()
                        .map(|error| failure(step, here.clone(), error))
                });
                return Ok(GivenBack {
                    destination: Destination::Live,
                    note,
                });
            }
            Err(error) => live_failure = Some(error.to_string()),
        }
    }

    let reached = place("recovered", &from);
    let in_recovered = RelPath::root().join(&name_of(b"recovered")).concat(&from);
    let both = |error: String| match &live_failure {
        Some(first) => failure(
            step,
            here.clone(),
            io::Error::other(format!(
                "cannot go back to {live} ({first}) and cannot go to {reached} ({error})"
            )),
        ),
        None => failure(step, here.clone(), io::Error::other(error)),
    };
    create_dir_chain(durable, antiq, &parent_of(&in_recovered))
        .map_err(|error| both(error.to_string()))?;
    let late = if is_dir {
        move_dir(trash, &from, antiq, &in_recovered)
            .map_err(|error| both(error.to_string()))?
            .mode_restore_error
            .map(|error| failure(step, here.clone(), error))
    } else {
        trash
            .rename(&from, antiq, &in_recovered)
            .map_err(|error| both(error.to_string()))?;
        None
    };
    let note = late.or_else(|| {
        after_move(durable, antiq, &in_recovered, trash, &from)
            .err()
            .map(|error| failure(step, here.clone(), error))
    });
    Ok(GivenBack {
        destination: Destination::Recovered,
        note,
    })
}

/// Makes a rename durable: the parent that gained the tree, then the one
/// that lost it.
fn after_move(
    durable: &dyn Durable,
    to_root: &TreeRoot,
    to: &RelPath,
    from_root: &TreeRoot,
    from: &RelPath,
) -> Result<(), crate::filesystem::FilesystemError> {
    flush_dir_of(durable, to_root, &parent_of(to))?;
    flush_dir_of(durable, from_root, &parent_of(from))
}

/// What the repairs of a run pass on to each other and to the outcome.
#[derive(Default)]
struct State {
    /// What repair 2 must leave alone: the staging folders, by stamp, of
    /// repair 1 failures of this run.
    kept: BTreeSet<Name>,
    /// Failures after which a repair stands.
    notes: Vec<Warning>,
}

/// Does one repair and returns what it did, or `None` when there was
/// nothing left to do.
fn apply(
    collection: &Collection,
    _lock: &RunLock,
    repair: &Repair,
    state: &mut State,
) -> Result<Option<Repair>, RepairError> {
    match repair {
        Repair::DeleteSidecars { piece, version } => {
            delete_sidecars(collection, piece, version).map(|done| done.then(|| repair.clone()))
        }
        Repair::DeleteTemporary { entry } => {
            delete_temporary(collection, entry, &state.kept).map(|()| Some(repair.clone()))
        }
        Repair::DeleteDoomed { stamp, tree } => {
            delete_doomed(collection, stamp.as_ref(), tree).map(|()| Some(repair.clone()))
        }
        Repair::RemoveEmptyChain { area, stamp, path } => {
            remove_chain(collection, *area, stamp, path).map(|done| done.then(|| repair.clone()))
        }
        Repair::GiveBack { stamp, tree, .. } => {
            let given = give_back_in(collection, stamp.as_ref(), tree)?;
            state
                .notes
                .extend(given.note.map(|note| Warning::new(note.to_string())));
            Ok(Some(Repair::GiveBack {
                stamp: stamp.clone(),
                tree: tree.clone(),
                destination: given.destination,
            }))
        }
        _ => Ok(None),
    }
}

fn give_back_in(
    collection: &Collection,
    stamp: Option<&Name>,
    tree: &RelPath,
) -> Result<GivenBack, RepairError> {
    let step = "give a tree in trash/ back";
    let open = |dir: PathBuf, area: &str| {
        TreeRoot::open(&dir).map_err(|error| failure(step, place(area, &RelPath::root()), error))
    };
    let trash = open(collection.trash_dir(), "trash")?;
    let live = open(collection.root().to_path_buf(), "")?;
    let antiq = open(collection.antiq_dir(), "")?;
    give_back(&trash, &live, &antiq, stamp, tree, tree, &RealDurable)
}

fn delete_sidecars(
    collection: &Collection,
    piece: &RelPath,
    version: &Stamp,
) -> Result<bool, RepairError> {
    let step = "delete the sidecars of an interrupted archive";
    let here = place("pieces", piece);
    let pieces = TreeRoot::open(&collection.pieces_dir())
        .map_err(|error| failure(step, here.clone(), error))?;
    let mut stored = StoreVersion {
        stamp: *version,
        archives: Vec::new(),
        listings: Vec::new(),
        checksums: Vec::new(),
    };
    let entries = pieces
        .read_dir(piece)
        .map_err(|error| failure(step, here.clone(), error))?;
    for entry in entries {
        let file = OsStr::from_bytes(entry.name.as_bytes());
        if let Some((found, kind)) = parse_file_name(file)
            && found == *version
        {
            let list = match kind {
                VersionFile::Archive => &mut stored.archives,
                VersionFile::Listing => &mut stored.listings,
                VersionFile::Checksum => &mut stored.checksums,
            };
            list.push(file.to_os_string());
        }
    }
    // A `.tar.zst` that appeared since the plan was made makes it a version.
    if !stored.archives.is_empty() {
        return Ok(false);
    }
    store::delete_version(&pieces, piece, &stored).map_err(|error| failure(step, here, error))?;
    Ok(true)
}

fn delete_temporary(
    collection: &Collection,
    entry: &Name,
    kept: &BTreeSet<Name>,
) -> Result<(), RepairError> {
    let step = "delete a temporary file";
    let here = place("tmp", &RelPath::root().join(entry));
    let root = TreeRoot::open(&collection.tmp_dir())
        .map_err(|error| failure(step, here.clone(), error))?;
    if entry.as_bytes() == b"pack" && !kept.is_empty() {
        let pack = RelPath::root().join(entry);
        let children = root
            .read_dir(&pack)
            .map_err(|error| failure(step, here.clone(), error))?;
        for child in children.iter().filter(|child| !kept.contains(&child.name)) {
            remove_tree(&root, &child.path)
                .map_err(|error| failure(step, place("tmp", &child.path), error))?;
        }
        return flush_dir_of(&RealDurable, &root, &pack)
            .map_err(|error| failure(step, here, error));
    }
    remove_tree_at(
        &collection
            .tmp_dir()
            .join(OsStr::from_bytes(entry.as_bytes())),
    )
    .map_err(|error| failure(step, here.clone(), error))?;
    flush_dir_of(&RealDurable, &root, &RelPath::root()).map_err(|error| failure(step, here, error))
}

fn delete_doomed(
    collection: &Collection,
    stamp: Option<&Name>,
    tree: &RelPath,
) -> Result<(), RepairError> {
    let step = "delete a tree in doomed/";
    let below = match stamp {
        Some(stamp) => RelPath::root().join(stamp).concat(tree),
        None => tree.clone(),
    };
    let here = place("doomed", &below);
    let root = TreeRoot::open(&collection.doomed_dir())
        .map_err(|error| failure(step, here.clone(), error))?;
    remove_tree(&root, &below).map_err(|error| failure(step, here.clone(), error))?;
    flush_dir_of(&RealDurable, &root, &parent_of(&below))
        .map_err(|error| failure(step, here, error))
}

/// Removes the empty folder `path` below `<area>/<stamp>/` with `rmdir`
/// alone, so nothing is deleted that is not empty. Returns whether there
/// was a folder.
fn remove_chain(
    collection: &Collection,
    area: Area,
    stamp: &Name,
    path: &RelPath,
) -> Result<bool, RepairError> {
    let step = "remove an empty folder";
    let below = RelPath::root().join(stamp).concat(path);
    let here = place(&area.to_string(), &below);
    let dir = match area {
        Area::Trash => collection.trash_dir(),
        Area::Doomed => collection.doomed_dir(),
    };
    let root = TreeRoot::open(&dir).map_err(|error| failure(step, here.clone(), error))?;
    match root.remove_dir(&below) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(failure(step, here, error)),
    }
    flush_dir_of(&RealDurable, &root, &parent_of(&below))
        .map_err(|error| failure(step, here, error))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::config::Config;
    use crate::filesystem::RestoreModes;
    use crate::lock;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use crate::versions::names::file_name;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    fn name(text: &str) -> Name {
        Name::from_bytes(text.as_bytes().to_vec()).expect("a name")
    }

    fn stamp(text: &str) -> Stamp {
        text.parse().expect("a stamp")
    }

    fn every_repair() -> Vec<Repair> {
        vec![
            Repair::DeleteSidecars {
                piece: path("a/b"),
                version: stamp("2026-11-04T090211"),
            },
            Repair::DeleteTemporary {
                entry: name("upload.part"),
            },
            Repair::DeleteDoomed {
                stamp: Some(name("2026-11-04T090211")),
                tree: path("a/b"),
            },
            Repair::DeleteDoomed {
                stamp: None,
                tree: path("stray"),
            },
            Repair::GiveBack {
                stamp: Some(name("2026-11-04T090211")),
                tree: path("a/b"),
                destination: Destination::Live,
            },
            Repair::GiveBack {
                stamp: None,
                tree: path("stray"),
                destination: Destination::Recovered,
            },
            Repair::GiveBack {
                stamp: Some(name("2026-11-04T090211")),
                tree: path("a/b"),
                destination: Destination::Recovered,
            },
            Repair::DeleteRestoredVersion {
                piece: path("a/b"),
                version: stamp("2026-11-04T090211-2"),
                delete: true,
            },
            Repair::DeleteRestoredVersion {
                piece: path("a/b"),
                version: stamp("2026-11-04T090211"),
                delete: false,
            },
            Repair::RemoveEmptyFolder {
                folder: path("a/b"),
            },
            Repair::RemoveEmptyChain {
                area: Area::Trash,
                stamp: name("2026-11-04T090211"),
                path: path("x/y"),
            },
            Repair::RemoveEmptyChain {
                area: Area::Doomed,
                stamp: name("2026-11-04T090211"),
                path: RelPath::root(),
            },
        ]
    }

    #[test]
    fn each_repair_serialises_with_its_name_first_and_its_fields() {
        let json: Vec<String> = every_repair()
            .iter()
            .map(|repair| serde_json::to_string(repair).expect("serialises"))
            .collect();
        assert_eq!(
            json,
            [
                r#"{"repair":"delete_sidecars","piece":"a/b","version":"2026-11-04T090211"}"#,
                r#"{"repair":"delete_temporary","entry":"upload.part"}"#,
                r#"{"repair":"delete_doomed","stamp":"2026-11-04T090211","tree":"a/b"}"#,
                r#"{"repair":"delete_doomed","tree":"stray"}"#,
                r#"{"repair":"give_back","stamp":"2026-11-04T090211","tree":"a/b","destination":"live"}"#,
                r#"{"repair":"give_back","tree":"stray","destination":"recovered"}"#,
                r#"{"repair":"give_back","stamp":"2026-11-04T090211","tree":"a/b","destination":"recovered"}"#,
                r#"{"repair":"delete_restored_version","piece":"a/b","version":"2026-11-04T090211-2","delete":true}"#,
                r#"{"repair":"delete_restored_version","piece":"a/b","version":"2026-11-04T090211","delete":false}"#,
                r#"{"repair":"remove_empty_folder","folder":"a/b"}"#,
                r#"{"repair":"remove_empty_chain","area":"trash","stamp":"2026-11-04T090211","path":"x/y"}"#,
                r#"{"repair":"remove_empty_chain","area":"doomed","stamp":"2026-11-04T090211","path":""}"#,
            ]
        );
    }

    #[test]
    fn a_percent_sign_and_a_byte_that_is_no_text_are_escaped_in_json() {
        let repair = Repair::DeleteTemporary {
            entry: Name::from_bytes(b"a%b\xff".to_vec()).expect("a name"),
        };
        let value = serde_json::to_value(&repair).expect("serialises");
        assert_eq!(
            value,
            json!({"repair": "delete_temporary", "entry": "a%25b%FF"})
        );
    }

    #[test]
    fn each_repair_describes_what_it_does_with_paths_from_the_collection_root() {
        let lines: Vec<String> = every_repair().iter().map(Repair::describe).collect();
        assert_eq!(
            lines,
            [
                "delete the sidecars of 2026-11-04T090211 in .antiq/pieces/a/b (the archive is not there)",
                "delete .antiq/tmp/upload.part",
                "delete .antiq/doomed/2026-11-04T090211/a/b",
                "delete .antiq/doomed/stray",
                "move .antiq/trash/2026-11-04T090211/a/b back to a/b",
                "move .antiq/trash/stray to .antiq/recovered/stray",
                "move .antiq/trash/2026-11-04T090211/a/b to .antiq/recovered/2026-11-04T090211/a/b",
                "delete version 2026-11-04T090211-2 of a/b that an unearth left",
                "drop the stamp 2026-11-04T090211 from the marker of a/b, which names no version",
                "remove the empty folder .antiq/pieces/a/b",
                "remove the empty folder .antiq/trash/2026-11-04T090211/x/y",
                "remove the empty folder .antiq/doomed/2026-11-04T090211",
            ]
        );
    }

    #[test]
    fn a_failed_repair_reads_with_its_step_and_place() {
        let error = RepairError {
            step: "delete a temporary file",
            place: ".antiq/tmp/x".to_string(),
            source: Box::new(std::io::Error::from_raw_os_error(13)),
        };
        assert!(
            error
                .to_string()
                .starts_with("cannot delete a temporary file .antiq/tmp/x: "),
            "{error}"
        );
    }

    // =====================================================
    // The state of a crashed run, laid by hand
    // =====================================================

    const S: &str = "2026-11-04T090211";

    fn collection(temp: &TempCollection) -> Collection {
        let config = Config::new(COLLECTION_ID.parse().expect("an ID"));
        Collection::new(temp.root().to_path_buf(), config)
    }

    fn lock_of(collection: &Collection) -> RunLock {
        let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
        lock::acquire(&collection.lock_path(), "test", &clock).expect("the lock is free")
    }

    /// Repairs the collection the way a run does.
    fn repaired(temp: &TempCollection) -> Outcome {
        let collection = collection(temp);
        let lock = lock_of(&collection);
        run(&collection, &lock)
    }

    /// Writes the files `kinds` of the version `stamp` of `piece` below
    /// `base` (`.antiq/pieces` for the store, `.antiq/tmp/pack/<stamp>` for
    /// the staging).
    fn lay(temp: &mut TempCollection, base: &str, piece: &str, stamp: &str, kinds: &[VersionFile]) {
        for kind in kinds {
            let name = file_name(&path(piece), &stamp.parse().expect("a stamp"), *kind);
            temp.tree().file(&format!("{base}/{piece}/{name}"), "x");
        }
    }

    const SIDECARS: [VersionFile; 2] = [VersionFile::Listing, VersionFile::Checksum];
    const WHOLE: [VersionFile; 3] = [
        VersionFile::Archive,
        VersionFile::Listing,
        VersionFile::Checksum,
    ];

    fn exists(temp: &TempCollection, relative: &str) -> bool {
        fs::symlink_metadata(temp.root().join(relative)).is_ok()
    }

    fn done_lines(outcome: &Outcome) -> Vec<String> {
        outcome.done.iter().map(Repair::describe).collect()
    }

    fn stage(temp: &mut TempCollection, piece: &str, stamp: &str) {
        lay(
            temp,
            &format!(".antiq/tmp/pack/{stamp}"),
            piece,
            stamp,
            &[VersionFile::Archive],
        );
    }

    fn assert_not_root() {
        let probe = tempfile::tempdir().expect("a temp dir");
        fs::set_permissions(probe.path(), fs::Permissions::from_mode(0o555)).expect("chmod");
        let denied = fs::File::create(probe.path().join("f")).is_err();
        fs::set_permissions(probe.path(), fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(denied, "the test needs a user that mode 0555 locks out");
    }

    // ---- repair 1 ----

    #[test]
    fn the_sidecars_of_an_interrupted_archive_go_with_their_empty_chain() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &SIDECARS);
        stage(&mut temp, "a/b", S);

        let outcome = repaired(&temp);

        assert!(
            !exists(&temp, ".antiq/pieces/a"),
            "the chain of empty folders is removed"
        );
        assert!(exists(&temp, ".antiq/pieces"));
        assert!(
            !exists(&temp, ".antiq/tmp/pack"),
            "repair 2 clears the staging after"
        );
        assert_eq!(
            done_lines(&outcome),
            [
                format!(
                    "delete the sidecars of {S} in .antiq/pieces/a/b (the archive is not there)"
                ),
                "delete .antiq/tmp/pack".to_string(),
            ]
        );
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    #[test]
    fn sidecars_without_a_staged_archive_are_an_incomplete_version_and_stay() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &SIDECARS);

        let outcome = repaired(&temp);

        assert!(exists(&temp, ".antiq/pieces/a/b"));
        assert!(outcome.done.is_empty(), "{:?}", outcome.done);
    }

    #[test]
    fn a_version_the_store_holds_whole_loses_nothing_to_its_staging() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &WHOLE);
        stage(&mut temp, "a/b", S);

        repaired(&temp);

        let kept = fs::read_dir(temp.root().join(".antiq/pieces/a/b"))
            .expect("lists")
            .count();
        assert_eq!(kept, 3);
    }

    #[test]
    fn interrupted_sidecars_go_from_a_folder_that_holds_another_whole_version() {
        let earlier = "2026-11-04T090210";
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", earlier, &WHOLE);
        lay(&mut temp, ".antiq/pieces", "a/b", S, &SIDECARS);
        stage(&mut temp, "a/b", S);

        let outcome = repaired(&temp);

        let kept = fs::read_dir(temp.root().join(".antiq/pieces/a/b"))
            .expect("lists")
            .count();
        assert_eq!(kept, 3, "only the whole version stays");
        assert_eq!(
            done_lines(&outcome)[0],
            format!("delete the sidecars of {S} in .antiq/pieces/a/b (the archive is not there)")
        );
    }

    #[test]
    fn the_sidecars_of_another_stamp_stay() {
        let mut temp = TempCollection::minimal();
        lay(
            &mut temp,
            ".antiq/pieces",
            "a/b",
            "2026-11-04T090212",
            &SIDECARS,
        );
        stage(&mut temp, "a/b", S);

        repaired(&temp);

        let kept = fs::read_dir(temp.root().join(".antiq/pieces/a/b"))
            .expect("lists")
            .count();
        assert_eq!(kept, 2);
    }

    #[test]
    fn pieces_that_nest_under_one_stamp_are_both_repaired() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &SIDECARS);
        lay(&mut temp, ".antiq/pieces", "a/b", S, &SIDECARS);
        stage(&mut temp, "a", S);
        stage(&mut temp, "a/b", S);

        let outcome = repaired(&temp);

        assert!(!exists(&temp, ".antiq/pieces/a"));
        assert_eq!(
            done_lines(&outcome)[..2],
            [
                format!("delete the sidecars of {S} in .antiq/pieces/a (the archive is not there)"),
                format!(
                    "delete the sidecars of {S} in .antiq/pieces/a/b (the archive is not there)"
                ),
            ]
        );
    }

    #[test]
    fn a_stamp_with_a_counter_is_repaired_like_any_other() {
        let counted = "2026-11-04T090211-2";
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", counted, &SIDECARS);
        stage(&mut temp, "a", counted);

        repaired(&temp);

        assert!(!exists(&temp, ".antiq/pieces/a"));
    }

    #[test]
    fn a_staging_folder_that_is_no_stamp_is_left_to_repair_2() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &SIDECARS);
        temp.tree()
            .file(".antiq/tmp/pack/not-a-stamp/a/x--junk.tar.zst", "x");

        let outcome = repaired(&temp);

        assert!(
            exists(&temp, ".antiq/pieces/a"),
            "no staged archive of that stamp"
        );
        assert!(!exists(&temp, ".antiq/tmp/pack"));
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn a_staged_archive_without_a_store_folder_has_nothing_to_repair() {
        let mut temp = TempCollection::minimal();
        stage(&mut temp, "a/b", S);

        let outcome = repaired(&temp);

        assert_eq!(done_lines(&outcome), ["delete .antiq/tmp/pack"]);
    }

    #[test]
    fn a_failing_deletion_of_sidecars_is_a_warning_that_keeps_the_staging() {
        assert_not_root();
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &SIDECARS);
        stage(&mut temp, "a", S);
        temp.tree().file(".antiq/tmp/other", "x");
        let folder = temp.root().join(".antiq/pieces/a");
        let _restore = RestoreModes(vec![folder.clone()]);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).expect("chmod");

        let outcome = repaired(&temp);

        let messages: Vec<&str> = outcome
            .warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect();
        assert!(
            messages[0].starts_with(
                "cannot delete the sidecars of an interrupted archive .antiq/pieces/a: "
            ),
            "{messages:?}"
        );
        assert_eq!(
            messages[1],
            format!(".antiq/tmp/pack/{S} stays: the sidecars it belongs to are not deleted yet")
        );
        assert!(
            exists(&temp, &format!(".antiq/tmp/pack/{S}")),
            "the staging survives"
        );
        assert!(!exists(&temp, ".antiq/tmp/other"), "the rest of tmp/ goes");

        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).expect("chmod");
        let second = repaired(&temp);
        assert!(
            !exists(&temp, ".antiq/pieces/a"),
            "the next run finishes the repair"
        );
        assert!(!exists(&temp, ".antiq/tmp/pack"));
        assert!(second.warnings.is_empty(), "{:?}", second.warnings);
    }

    // ---- repair 2 ----

    #[test]
    fn everything_in_tmp_goes_and_tmp_itself_stays() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/tmp/upload.part", "x");
        temp.tree().file(".antiq/tmp/deep/er/file", "x");
        temp.tree().dir(".antiq/tmp/empty");

        let outcome = repaired(&temp);

        assert_eq!(
            fs::read_dir(temp.root().join(".antiq/tmp"))
                .expect("lists")
                .count(),
            0
        );
        assert_eq!(
            done_lines(&outcome),
            [
                "delete .antiq/tmp/deep",
                "delete .antiq/tmp/empty",
                "delete .antiq/tmp/upload.part"
            ]
        );
    }

    #[test]
    fn a_temporary_file_that_cannot_go_is_a_warning_and_the_next_repair_runs() {
        assert_not_root();
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/tmp/x", "x");
        temp.tree().file(".antiq/doomed/t/f", "x");
        let tmp = temp.root().join(".antiq/tmp");
        let _restore = RestoreModes(vec![tmp.clone()]);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).expect("chmod");

        let outcome = repaired(&temp);

        assert!(
            outcome.warnings[0]
                .message
                .starts_with("cannot delete a temporary file .antiq/tmp/x: "),
            "{:?}",
            outcome.warnings
        );
        assert!(!exists(&temp, ".antiq/doomed/t"), "repair 3 still ran");
    }

    // ---- repair 3 ----

    #[test]
    fn a_doomed_tree_is_deleted_without_a_comparison_and_its_stamp_folder_with_it() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &WHOLE);
        temp.tree().file(&format!(".antiq/doomed/{S}/a/b/f"), "x");
        temp.tree()
            .file(&format!(".antiq/doomed/{S}/a/b/deeper/g"), "y");

        let outcome = repaired(&temp);

        assert!(!exists(&temp, ".antiq/doomed/2026-11-04T090211"));
        assert!(exists(&temp, ".antiq/doomed"));
        assert_eq!(
            done_lines(&outcome),
            [
                format!("delete .antiq/doomed/{S}/a/b"),
                format!("remove the empty folder .antiq/doomed/{S}/a"),
                format!("remove the empty folder .antiq/doomed/{S}"),
            ]
        );
    }

    #[test]
    fn empty_chains_in_doomed_go_beside_the_tree() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        temp.tree().file(&format!(".antiq/doomed/{S}/a/f"), "x");
        temp.tree().dir(&format!(".antiq/doomed/{S}/x/y"));

        repaired(&temp);

        assert!(!exists(&temp, &format!(".antiq/doomed/{S}")));
    }

    #[test]
    fn two_pieces_under_one_stamp_are_two_trees() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/x", S, &WHOLE);
        lay(&mut temp, ".antiq/pieces", "c", S, &WHOLE);
        temp.tree().file(&format!(".antiq/doomed/{S}/a/x/f"), "x");
        temp.tree().file(&format!(".antiq/doomed/{S}/c/f"), "x");

        let outcome = repaired(&temp);

        assert_eq!(
            done_lines(&outcome)[..2],
            [
                format!("delete .antiq/doomed/{S}/c"),
                format!("delete .antiq/doomed/{S}/a/x")
            ]
        );
        assert!(!exists(&temp, &format!(".antiq/doomed/{S}")));
    }

    #[test]
    fn an_entry_in_doomed_that_is_no_directory_is_deleted_too() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/doomed/stray", "x");
        temp.tree().symlink(".antiq/doomed/link", "/nonexistent");

        let outcome = repaired(&temp);

        assert!(!exists(&temp, ".antiq/doomed/stray"));
        assert!(!exists(&temp, ".antiq/doomed/link"));
        assert_eq!(
            done_lines(&outcome),
            ["delete .antiq/doomed/link", "delete .antiq/doomed/stray"]
        );
    }

    #[test]
    fn a_tree_without_a_version_is_deleted_by_its_first_component() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/doomed/whatever/a/b/f", "x");

        let outcome = repaired(&temp);

        assert_eq!(
            done_lines(&outcome),
            [
                "delete .antiq/doomed/whatever/a".to_string(),
                "remove the empty folder .antiq/doomed/whatever".to_string(),
            ]
        );
    }

    #[test]
    fn a_tree_with_a_locked_directory_is_deleted_all_the_same() {
        assert_not_root();
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        temp.tree()
            .file(&format!(".antiq/doomed/{S}/a/inner/f"), "x");
        let inner = temp.root().join(format!(".antiq/doomed/{S}/a/inner"));
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o000)).expect("chmod");

        let outcome = repaired(&temp);

        assert!(
            !exists(&temp, &format!(".antiq/doomed/{S}")),
            "{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn a_doomed_tree_that_cannot_be_deleted_is_a_warning_and_repair_4_still_runs() {
        assert_not_root();
        let mut temp = TempCollection::minimal();
        temp.tree().file(&format!(".antiq/doomed/{S}/a/f"), "x");
        temp.tree().dir(".antiq/trash/empty-stamp");
        let stamp_dir = temp.root().join(format!(".antiq/doomed/{S}"));
        let _restore = RestoreModes(vec![stamp_dir.clone()]);
        fs::set_permissions(&stamp_dir, fs::Permissions::from_mode(0o555)).expect("chmod");

        let outcome = repaired(&temp);

        let first = &outcome.warnings[0].message;
        assert!(
            first.starts_with(&format!(
                "cannot delete a tree in doomed/ .antiq/doomed/{S}/a: "
            )),
            "{:?}",
            outcome.warnings
        );
        assert!(exists(&temp, &format!(".antiq/doomed/{S}/a/f")));
    }

    #[test]
    fn a_doomed_stamp_folder_nothing_matches_and_nothing_holds_is_removed() {
        let mut temp = TempCollection::minimal();
        temp.tree().dir(&format!(".antiq/doomed/{S}"));

        let outcome = repaired(&temp);

        assert_eq!(
            done_lines(&outcome),
            [format!("remove the empty folder .antiq/doomed/{S}")]
        );
    }

    // ---- repair 4 ----

    fn trash_tree(temp: &mut TempCollection, stamp: &str, tree: &str) {
        temp.tree()
            .file(&format!(".antiq/trash/{stamp}/{tree}/f"), "x");
    }

    fn live_content(temp: &TempCollection, relative: &str) -> Option<String> {
        fs::read_to_string(temp.root().join(relative)).ok()
    }

    #[test]
    fn a_trashed_tree_goes_back_to_its_live_path_and_its_empty_chain_goes() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &WHOLE);
        temp.tree().file("a/keep", "k");
        trash_tree(&mut temp, S, "a/b");
        temp.tree().dir(&format!(".antiq/trash/{S}/x/y"));

        let outcome = repaired(&temp);

        assert_eq!(live_content(&temp, "a/b/f").as_deref(), Some("x"));
        assert!(!exists(&temp, &format!(".antiq/trash/{S}")));
        assert_eq!(
            done_lines(&outcome),
            [
                format!("move .antiq/trash/{S}/a/b back to a/b"),
                format!("remove the empty folder .antiq/trash/{S}/a"),
                format!("remove the empty folder .antiq/trash/{S}/x/y"),
                format!("remove the empty folder .antiq/trash/{S}/x"),
                format!("remove the empty folder .antiq/trash/{S}"),
            ]
        );
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    #[test]
    fn a_taken_live_path_sends_the_tree_to_recovered_with_its_chain() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &WHOLE);
        temp.tree().file("a/b/mine", "m");
        trash_tree(&mut temp, S, "a/b");

        let outcome = repaired(&temp);

        assert_eq!(
            live_content(&temp, &format!(".antiq/recovered/{S}/a/b/f")).as_deref(),
            Some("x")
        );
        assert_eq!(live_content(&temp, "a/b/mine").as_deref(), Some("m"));
        assert!(
            done_lines(&outcome)[0].ends_with(&format!("to .antiq/recovered/{S}/a/b")),
            "{:?}",
            done_lines(&outcome)
        );
    }

    #[test]
    fn a_missing_recovered_folder_blocks_no_give_back_and_is_created_when_needed() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        lay(&mut temp, ".antiq/pieces", "b", S, &WHOLE);
        temp.tree().file("b/mine", "m");
        trash_tree(&mut temp, S, "a");
        trash_tree(&mut temp, S, "b");
        fs::remove_dir(temp.root().join(".antiq/recovered")).expect("recovered is empty");

        let outcome = repaired(&temp);

        assert_eq!(live_content(&temp, "a/f").as_deref(), Some("x"));
        assert_eq!(
            live_content(&temp, &format!(".antiq/recovered/{S}/b/f")).as_deref(),
            Some("x")
        );
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    #[test]
    fn a_missing_recovered_folder_does_not_stop_a_give_back_to_a_free_live_path() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        trash_tree(&mut temp, S, "a");
        fs::remove_dir(temp.root().join(".antiq/recovered")).expect("recovered is empty");

        let outcome = repaired(&temp);

        assert_eq!(live_content(&temp, "a/f").as_deref(), Some("x"));
        assert!(!exists(&temp, ".antiq/recovered"), "nothing needed it");
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    #[test]
    fn a_missing_parent_sends_the_tree_to_recovered() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &WHOLE);
        trash_tree(&mut temp, S, "a/b");

        repaired(&temp);

        assert!(exists(&temp, &format!(".antiq/recovered/{S}/a/b/f")));
        assert!(!exists(&temp, "a"));
    }

    #[test]
    fn a_parent_that_is_a_file_sends_the_tree_to_recovered() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a/b", S, &WHOLE);
        temp.tree().file("a", "in the way");
        trash_tree(&mut temp, S, "a/b");

        repaired(&temp);

        assert!(exists(&temp, &format!(".antiq/recovered/{S}/a/b/f")));
        assert_eq!(live_content(&temp, "a").as_deref(), Some("in the way"));
    }

    #[test]
    fn the_shortest_path_with_a_version_of_that_stamp_is_the_tree() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        lay(&mut temp, ".antiq/pieces", "a/b", S, &WHOLE);
        trash_tree(&mut temp, S, "a/b");

        let outcome = repaired(&temp);

        assert_eq!(live_content(&temp, "a/b/f").as_deref(), Some("x"));
        assert_eq!(
            done_lines(&outcome)[0],
            format!("move .antiq/trash/{S}/a back to a")
        );
    }

    #[test]
    fn without_a_version_of_that_stamp_the_first_component_goes_back() {
        let mut temp = TempCollection::minimal();
        trash_tree(&mut temp, S, "x/y");

        let outcome = repaired(&temp);

        assert_eq!(live_content(&temp, "x/y/f").as_deref(), Some("x"));
        assert_eq!(
            done_lines(&outcome)[0],
            format!("move .antiq/trash/{S}/x back to x")
        );
    }

    #[test]
    fn a_stamp_folder_that_is_no_stamp_gives_back_its_children() {
        let mut temp = TempCollection::minimal();
        trash_tree(&mut temp, "junk", "x");

        repaired(&temp);

        assert_eq!(live_content(&temp, "x/f").as_deref(), Some("x"));
        assert!(!exists(&temp, ".antiq/trash/junk"));
    }

    #[test]
    fn entries_that_are_no_directory_lie_directly_in_trash_go_to_recovered_unfollowed() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/trash/stray", "s");
        temp.tree().symlink(".antiq/trash/link", "/nonexistent");
        temp.tree().file(&format!(".antiq/trash/{S}/loose"), "l");

        let outcome = repaired(&temp);

        assert_eq!(
            live_content(&temp, ".antiq/recovered/stray").as_deref(),
            Some("s")
        );
        assert_eq!(
            live_content(&temp, &format!(".antiq/recovered/{S}/loose")).as_deref(),
            Some("l")
        );
        let link = temp.root().join(".antiq/recovered/link");
        assert_eq!(
            fs::read_link(link).expect("a symlink"),
            std::path::PathBuf::from("/nonexistent")
        );
        assert!(!exists(&temp, ".antiq/trash/stray"));
        assert!(!exists(&temp, "stray"), "a loose file never goes live");
        assert!(
            done_lines(&outcome)
                .iter()
                .all(|line| !line.contains("back to"))
        );
    }

    #[test]
    fn a_second_tree_for_one_path_goes_to_recovered() {
        let mut temp = TempCollection::minimal();
        let later = "2026-11-04T090212";
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        lay(&mut temp, ".antiq/pieces", "a", later, &WHOLE);
        trash_tree(&mut temp, S, "a");
        temp.tree().file(&format!(".antiq/trash/{later}/a/g"), "g");

        let outcome = repaired(&temp);

        assert_eq!(live_content(&temp, "a/f").as_deref(), Some("x"));
        assert_eq!(
            live_content(&temp, &format!(".antiq/recovered/{later}/a/g")).as_deref(),
            Some("g")
        );
        assert_eq!(
            done_lines(&outcome)
                .into_iter()
                .filter(|line| line.starts_with("move"))
                .collect::<Vec<_>>(),
            [
                format!("move .antiq/trash/{S}/a back to a"),
                format!("move .antiq/trash/{later}/a to .antiq/recovered/{later}/a"),
            ]
        );
    }

    #[test]
    fn a_tree_that_goes_nowhere_stays_in_trash_with_both_reasons_in_the_warning() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        temp.tree().file("a/mine", "m");
        temp.tree()
            .file(&format!(".antiq/recovered/{S}/a/theirs"), "t");
        trash_tree(&mut temp, S, "a");

        let outcome = repaired(&temp);

        assert_eq!(
            live_content(&temp, &format!(".antiq/trash/{S}/a/f")).as_deref(),
            Some("x")
        );
        let message = &outcome.warnings[0].message;
        assert!(
            message.starts_with(&format!(
                "cannot give a tree in trash/ back .antiq/trash/{S}/a: "
            )),
            "{message}"
        );
        assert!(
            message.contains("back to a") && message.contains(&format!("recovered/{S}/a")),
            "{message}"
        );
        assert!(
            outcome
                .done
                .iter()
                .all(|repair| !matches!(repair, Repair::GiveBack { .. }))
        );
    }

    #[test]
    fn a_leftover_child_goes_beside_a_matched_tree() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        trash_tree(&mut temp, S, "a");
        temp.tree().file(&format!(".antiq/trash/{S}/junk/g"), "g");

        repaired(&temp);

        assert_eq!(live_content(&temp, "a/f").as_deref(), Some("x"));
        assert_eq!(
            live_content(&temp, "junk/g").as_deref(),
            Some("g"),
            "no version: its first component goes back as it is"
        );
        assert!(!exists(&temp, &format!(".antiq/trash/{S}")));
    }

    #[test]
    fn repairs_2_3_and_6_never_delete_a_tree_in_trash() {
        let mut temp = TempCollection::minimal();
        lay(&mut temp, ".antiq/pieces", "a", S, &WHOLE);
        temp.tree().file("a/mine", "m");
        temp.tree()
            .file(&format!(".antiq/recovered/{S}/a/theirs"), "t");
        trash_tree(&mut temp, S, "a");
        temp.tree().file(".antiq/tmp/x", "x");
        temp.tree().file(&format!(".antiq/doomed/{S}/b/f"), "x");
        temp.tree().dir(".antiq/pieces/empty/er");

        repaired(&temp);

        assert!(exists(&temp, &format!(".antiq/trash/{S}/a/f")));
        assert!(!exists(&temp, ".antiq/tmp/x"));
        assert!(!exists(&temp, &format!(".antiq/doomed/{S}")));
    }

    mod give_back_unit {
        use super::*;
        use crate::filesystem::atomic::recording::{Flush, Recording};
        use std::path::PathBuf;

        fn roots(temp: &TempCollection) -> (TreeRoot, TreeRoot, TreeRoot) {
            let open = |relative: &str| TreeRoot::open(&temp.root().join(relative)).expect("opens");
            (open(".antiq/trash"), open("."), open(".antiq"))
        }

        fn dirs(recording: &Recording) -> Vec<PathBuf> {
            recording
                .flushes()
                .into_iter()
                .filter_map(|flush| match flush {
                    Flush::Dir(path) => Some(path),
                    Flush::File(_) => None,
                })
                .collect()
        }

        #[test]
        fn a_tree_in_recovered_has_its_chain_and_both_parents_flushed() {
            let mut temp = TempCollection::minimal();
            temp.tree().file("a/b/mine", "m");
            trash_tree(&mut temp, S, "a/b");
            let (trash, collection, antiq) = roots(&temp);
            let recording = Recording::new();

            let given = give_back(
                &trash,
                &collection,
                &antiq,
                Some(&name(S)),
                &path("a/b"),
                &path("a/b"),
                &recording,
            )
            .expect("the tree moves");

            assert_eq!(given.destination, Destination::Recovered);
            let expected: Vec<PathBuf> = [
                "recovered",
                ".",
                &format!("recovered/{S}"),
                "recovered",
                &format!("recovered/{S}/a"),
                &format!("recovered/{S}"),
                &format!("recovered/{S}/a"),
                &format!("{S}/a"),
            ]
            .iter()
            .map(PathBuf::from)
            .collect();
            assert_eq!(dirs(&recording), expected);
        }

        #[test]
        fn a_flush_that_fails_after_the_rename_leaves_the_tree_live_with_a_note() {
            let mut temp = TempCollection::minimal();
            temp.tree().file("a/keep", "k");
            trash_tree(&mut temp, S, "a/b");
            let (trash, collection, antiq) = roots(&temp);
            let recording = Recording::new();
            recording.fail_dirs_from(0);

            let given = give_back(
                &trash,
                &collection,
                &antiq,
                Some(&name(S)),
                &path("a/b"),
                &path("a/b"),
                &recording,
            )
            .expect("the tree moved");

            assert_eq!(given.destination, Destination::Live);
            let note = given.note.expect("the flush failure is told");
            assert!(
                note.to_string()
                    .starts_with("cannot give a tree in trash/ back "),
                "{note}"
            );
            assert!(exists(&temp, "a/b/f"));
        }
    }
}
