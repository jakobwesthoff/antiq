// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The store under `.antiq/pieces/` (spec "Store layout" and "Damage"):
//! reading what it holds, finding a folder by the volume's own name
//! equality, and the operations that put a version in, take it back and
//! delete it.

use super::VersionsError;
use super::names::{VersionFile, VersionFileNames, parse_file_name};
use super::stamp::{MAX_COUNTER, Stamp};
use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::{Durable, RealDurable, create_dir_chain, flush_dir_of, named};
use crate::filesystem::tree::{DirEntry, TreeRoot};
use crate::relpath::{Name, RelPath};
use crate::tree::EntryKind;
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// =========================================================
// Reading the store
// =========================================================

/// One version in a store folder, with the files present for it. A slot
/// holds several names when files that differ in `<name>` only carry the
/// same stamp and suffix; antiq cannot tell which of them is the version,
/// so that is damage of the version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreVersion {
    pub stamp: Stamp,
    /// The `.tar.zst` files, by name.
    pub archives: Vec<OsString>,
    /// The `.sha256.zst` files, by name.
    pub listings: Vec<OsString>,
    /// The `.tar.zst.sha256` files, by name.
    pub checksums: Vec<OsString>,
}

impl StoreVersion {
    fn new(stamp: Stamp) -> StoreVersion {
        StoreVersion {
            stamp,
            archives: Vec::new(),
            listings: Vec::new(),
            checksums: Vec::new(),
        }
    }

    /// The names found for `kind`.
    pub fn files(&self, kind: VersionFile) -> &[OsString] {
        match kind {
            VersionFile::Archive => &self.archives,
            VersionFile::Listing => &self.listings,
            VersionFile::Checksum => &self.checksums,
        }
    }

    fn slot(&mut self, kind: VersionFile) -> &mut Vec<OsString> {
        match kind {
            VersionFile::Archive => &mut self.archives,
            VersionFile::Listing => &mut self.listings,
            VersionFile::Checksum => &mut self.checksums,
        }
    }

    /// A version exists if and only if its `.tar.zst` does; without it the
    /// sidecars that are left make an incomplete version.
    pub fn is_incomplete(&self) -> bool {
        self.archives.is_empty()
    }
}

/// A folder under `pieces/` that holds files of versions, named by its
/// path in the stored spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePiece {
    pub path: RelPath,
    /// By stamp.
    pub versions: Vec<StoreVersion>,
}

impl StorePiece {
    /// A folder is a piece of the store when it holds at least one
    /// `.tar.zst`.
    pub fn is_store_piece(&self) -> bool {
        self.versions.iter().any(|version| !version.is_incomplete())
    }
}

/// What a scan of `pieces/` found. `pieces`, `sidecar_only` and `strays`
/// are sorted by path; `empty_folders` has its own order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreScan {
    /// Folders with at least one `.tar.zst`.
    pub pieces: Vec<StorePiece>,
    /// Folders with files of versions and no `.tar.zst`: not pieces of the
    /// store, but their incomplete versions are reported.
    pub sidecar_only: Vec<StorePiece>,
    /// Files whose name is not one of a version's files, with the path
    /// below `pieces/`. They keep their folder from counting as empty.
    pub strays: Vec<RelPath>,
    /// Folders with no entry at all, the deepest first. A folder that
    /// holds only empty folders is not among them; removing the chain of a
    /// leaf takes it with it.
    pub empty_folders: Vec<RelPath>,
}

/// Reads `pieces_dir` into the pieces of the store and what else it holds.
///
/// Every directory below it is a candidate store folder, so a piece and
/// the pieces below its path can both be there. Everything that is not a
/// directory, a symlink included, is classified by its name alone and
/// never followed. A missing `pieces_dir` is an empty store.
pub fn scan(pieces_dir: &Path) -> Result<StoreScan, VersionsError> {
    let root = match TreeRoot::open(pieces_dir) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StoreScan::default());
        }
        Err(source) => return Err(io_error("open", pieces_dir.display().to_string(), source)),
    };
    scan_root(&root, &list_folder)
}

/// The entries of `folder`, or `None` when the folder is not there any
/// more. `status` and `find` scan without the lock while a run may be
/// removing folders, so a folder that was listed as a directory and is gone
/// by the time it is read is not an error.
fn list_folder(root: &TreeRoot, folder: &RelPath) -> Result<Option<Vec<DirEntry>>, VersionsError> {
    match root.read_dir(folder) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if !folder.is_root() && is_absent(&error) => Ok(None),
        Err(source) => Err(io_error("list", shown(folder), source)),
    }
}

type List<'a> = &'a dyn Fn(&TreeRoot, &RelPath) -> Result<Option<Vec<DirEntry>>, VersionsError>;

fn scan_root(root: &TreeRoot, list: List<'_>) -> Result<StoreScan, VersionsError> {
    let mut scan = StoreScan::default();
    let mut folders = vec![RelPath::root()];
    while let Some(folder) = folders.pop() {
        let Some(mut entries) = list(root, &folder)? else {
            continue;
        };
        entries.sort_by(|left, right| left.name.cmp(&right.name));

        let mut versions: BTreeMap<Stamp, StoreVersion> = BTreeMap::new();
        let mut vanished = 0;
        for entry in &entries {
            match is_directory(root, entry)? {
                Some(true) => {
                    folders.push(entry.path.clone());
                    continue;
                }
                Some(false) => {}
                None => {
                    vanished += 1;
                    continue;
                }
            }
            let name = OsStr::from_bytes(entry.name.as_bytes());
            match parse_file_name(name) {
                // A file at the top of `pieces/` belongs to no piece.
                Some((stamp, kind)) if !folder.is_root() => {
                    let version = versions
                        .entry(stamp)
                        .or_insert_with(|| StoreVersion::new(stamp));
                    version.slot(kind).push(name.to_os_string());
                }
                _ => scan.strays.push(entry.path.clone()),
            }
        }

        if entries.len() == vanished && !folder.is_root() {
            scan.empty_folders.push(folder);
        } else if !versions.is_empty() {
            let piece = StorePiece {
                path: folder,
                versions: versions.into_values().collect(),
            };
            if piece.is_store_piece() {
                scan.pieces.push(piece);
            } else {
                scan.sidecar_only.push(piece);
            }
        }
    }

    scan.pieces
        .sort_by(|left, right| left.path.cmp(&right.path));
    scan.sidecar_only
        .sort_by(|left, right| left.path.cmp(&right.path));
    scan.strays.sort();
    scan.empty_folders
        .sort_by(|left, right| depth(right).cmp(&depth(left)).then_with(|| left.cmp(right)));
    Ok(scan)
}

fn depth(path: &RelPath) -> usize {
    path.components().count()
}

/// Whether the entry is a directory itself, not a link to one, or `None`
/// when it is gone. A filesystem that reports no kind in the listing is
/// asked.
fn is_directory(root: &TreeRoot, entry: &DirEntry) -> Result<Option<bool>, VersionsError> {
    let kind = match entry.kind {
        EntryKind::Unknown => match root.lstat(&entry.path) {
            Ok(found) => found.kind,
            Err(error) if is_absent(&error) => return Ok(None),
            Err(source) => return Err(io_error("look at", shown(&entry.path), source)),
        },
        known => known,
    };
    Ok(Some(kind == EntryKind::Directory))
}

fn io_error(action: &'static str, path: String, source: io::Error) -> VersionsError {
    VersionsError::Io {
        action,
        path,
        source,
    }
}

fn shown(path: &RelPath) -> String {
    if path.is_root() {
        ".".to_string()
    } else {
        path.to_string()
    }
}

// =========================================================
// Finding a folder by the volume's own equality
// =========================================================

/// The directory `path` below `root` in the spelling it is stored under,
/// or `None` when there is none.
///
/// The volume decides which names are equal: a volume that folds case or
/// Unicode normalisation finds `projects` when asked for `Projects`. The
/// path is looked at with `lstat`, so the answer is the volume's, and the
/// stored spelling is then read from the listing of each parent. Nothing is
/// written, so dry runs can ask. A symlink, a file, a name too long for
/// the volume and a missing component all give `None`.
///
/// Meant for directories. Two names of one file in one folder cannot be
/// told apart by their inode; the first by name is taken then.
pub fn lookup_exact(root: &TreeRoot, path: &RelPath) -> Result<Option<RelPath>, VersionsError> {
    let mut stored = RelPath::root();
    for component in path.components() {
        let name = Name::from_bytes(component.to_vec()).expect("a component of a path is a name");
        let given = stored.join(&name);
        let entry = match root.lstat(&given) {
            Ok(entry) => entry,
            Err(error) if is_absent(&error) => return Ok(None),
            Err(source) => return Err(io_error("look at", shown(&given), source)),
        };
        if entry.kind != EntryKind::Directory {
            return Ok(None);
        }
        let listing = root
            .read_dir(&stored)
            .map_err(|source| io_error("list", shown(&stored), source))?;
        let found = stored_name(&listing, &name, |candidate| {
            let other = root.lstat(&stored.join(candidate))?;
            Ok(other.device == entry.device && other.inode == entry.inode)
        })
        .map_err(|source| io_error("look at", shown(&given), source))?;
        match found {
            Some(found) => stored = stored.join(&found),
            // The entry vanished between the two calls.
            None => return Ok(None),
        }
    }
    Ok(Some(stored))
}

/// Whether the error says there is no such thing, as opposed to a failure
/// to look. A symlink in the way gives `ELOOP` on macOS and `ENOTDIR` on
/// Linux.
fn is_absent(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || matches!(
            error.raw_os_error(),
            Some(libc::ENOTDIR | libc::ENAMETOOLONG | libc::ELOOP)
        )
}

/// The name in `listing` that stands for `given`: `given` itself when it is
/// listed byte for byte, else the first name by byte order for which
/// `same_entry` says it is the entry that was found under `given`.
fn stored_name(
    listing: &[DirEntry],
    given: &Name,
    mut same_entry: impl FnMut(&Name) -> io::Result<bool>,
) -> io::Result<Option<Name>> {
    if listing.iter().any(|entry| &entry.name == given) {
        return Ok(Some(given.clone()));
    }
    let mut names: Vec<&Name> = listing.iter().map(|entry| &entry.name).collect();
    names.sort();
    for name in names {
        if same_entry(name)? {
            return Ok(Some(name.clone()));
        }
    }
    Ok(None)
}

// =========================================================
// Choosing a stamp
// =========================================================

/// The stamp for a new version of `piece` made at `time`: the second
/// without a counter when nothing uses it, otherwise the same second with
/// counter 2, then 3, and so on.
///
/// A stamp is used by any file with it in the store folder
/// `pieces_dir/<piece>`, whatever its `<name>`, and by a directory
/// `tmp_pack_dir/<stamp>/<piece>`, a packing in progress. Both are looked
/// at the way the volume compares names. A counter above [`MAX_COUNTER`]
/// is refused: the cut of `<name>` leaves room for no more digits.
pub fn fresh_stamp(
    pieces_dir: &Path,
    tmp_pack_dir: &Path,
    piece: &RelPath,
    time: Timestamp,
) -> Result<Stamp, VersionsError> {
    fresh_stamp_up_to(MAX_COUNTER, pieces_dir, tmp_pack_dir, piece, time)
}

fn fresh_stamp_up_to(
    limit: u32,
    pieces_dir: &Path,
    tmp_pack_dir: &Path,
    piece: &RelPath,
    time: Timestamp,
) -> Result<Stamp, VersionsError> {
    let mut used: BTreeSet<Stamp> = BTreeSet::new();
    if let Some(pieces) = open_if_present(pieces_dir)? {
        match pieces.read_dir(piece) {
            Ok(entries) => {
                for entry in entries {
                    let name = OsStr::from_bytes(entry.name.as_bytes());
                    if let Some((stamp, _)) = parse_file_name(name) {
                        used.insert(stamp);
                    }
                }
            }
            Err(error) if is_absent(&error) => {}
            Err(source) => return Err(io_error("list", shown(piece), source)),
        }
    }
    let staging = open_if_present(tmp_pack_dir)?;

    let first = Stamp::at(time)?;
    for counter in 1..=limit {
        let candidate = first.with_counter(counter);
        if used.contains(&candidate) {
            continue;
        }
        if let Some(staging) = &staging {
            let name = Name::from_bytes(candidate.to_string().into_bytes())
                .expect("a stamp is a valid name");
            let staged = RelPath::root().join(&name).concat(piece);
            match staging.lstat(&staged) {
                Ok(_) => continue,
                Err(error) if is_absent(&error) => {}
                Err(source) => return Err(io_error("look at", shown(&staged), source)),
            }
        }
        return Ok(candidate);
    }
    Err(VersionsError::CounterTooHigh {
        piece: piece.to_string(),
        limit,
    })
}

/// Whether the archive file of the version `stamp` of `piece` is staged in
/// `tmp_pack_dir/<stamp>/<piece>`, whatever its `<name>`: then the version
/// is being stored, and sidecars in the store folder without a `.tar.zst`
/// are the move of a running `archive` or of repair 1, not an incomplete
/// version. `status` and `find` run without the lock and meet this state
/// while an `archive` runs.
pub fn is_being_stored(
    tmp_pack_dir: &Path,
    piece: &RelPath,
    stamp: &Stamp,
) -> Result<bool, VersionsError> {
    let Some(staging) = open_if_present(tmp_pack_dir)? else {
        return Ok(false);
    };
    let name = Name::from_bytes(stamp.to_string().into_bytes()).expect("a stamp is a valid name");
    let folder = RelPath::root().join(&name).concat(piece);
    let entries = match staging.read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) if is_absent(&error) => return Ok(false),
        Err(source) => return Err(io_error("list", shown(&folder), source)),
    };
    Ok(entries.iter().any(|entry| {
        parse_file_name(OsStr::from_bytes(entry.name.as_bytes()))
            .is_some_and(|(found, kind)| found == *stamp && kind == VersionFile::Archive)
    }))
}

/// The directory as a root, or `None` when it does not exist.
fn open_if_present(path: &Path) -> Result<Option<TreeRoot>, VersionsError> {
    match TreeRoot::open(path) {
        Ok(root) => Ok(Some(root)),
        Err(error) if is_absent(&error) => Ok(None),
        Err(source) => Err(io_error("open", path.display().to_string(), source)),
    }
}

// =========================================================
// Moving a version in and out, deleting it
// =========================================================
//
// The `.tar.zst` is moved in last and deleted first: a version exists if
// and only if it is there, so a crash at any other point leaves sidecars
// that the repairs recognise, never a version that is not whole.

/// Moves the listing, then the checksum file, then the `.tar.zst` of a
/// version from `staging` into the store folder `folder` of `pieces`,
/// creating the folder when it is missing.
///
/// Each file is flushed where it lies, renamed, and the folder flushed,
/// before the next one moves. Nothing is replaced: a destination that
/// exists already is [`VersionsError::Taken`], and all three are looked at
/// before anything moves.
///
/// A failure before the `.tar.zst` has moved puts back what moved and
/// returns the failure; when that fails too, the result is
/// [`VersionsError::PartlyMoved`] and the staging must be kept for the
/// repairs, and `PartlyMoved` keeps it whatever the cause was. A failure to
/// flush the folder after the `.tar.zst` has moved is
/// [`VersionsError::StoredNotDurable`]: the version is in the store, and the
/// piece stops there with its live folder untouched.
pub(crate) fn move_in_with(
    durable: &dyn Durable,
    staging: &TreeRoot,
    pieces: &TreeRoot,
    folder: &RelPath,
    piece: &RelPath,
    stamp: &Stamp,
    names: &VersionFileNames,
) -> Result<(), VersionsError> {
    create_dir_chain(durable, pieces, folder)?;
    for kind in VersionFile::ALL {
        let target = file_in(folder, names.of(kind))?;
        match pieces.lstat(&target) {
            Ok(_) => {
                return Err(VersionsError::Taken {
                    path: shown(&target),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("look at", shown(&target), source)),
        }
    }

    let from = Place {
        root: staging,
        dir: &RelPath::root(),
    };
    let to = Place {
        root: pieces,
        dir: folder,
    };
    let mut moved = Vec::new();
    for kind in [
        VersionFile::Listing,
        VersionFile::Checksum,
        VersionFile::Archive,
    ] {
        match transfer(durable, &from, &to, names.of(kind), false) {
            Ok(()) => moved.push(kind),
            Err(Failure::After(source)) if kind == VersionFile::Archive => {
                return Err(VersionsError::StoredNotDurable {
                    piece: piece.to_string(),
                    stamp: stamp.to_string(),
                    source,
                });
            }
            Err(failure) => {
                let cause = match failure {
                    Failure::Before(error) => error,
                    // The file arrived, so it is among those to put back.
                    Failure::After(source) => {
                        moved.push(kind);
                        source.into()
                    }
                };
                return Err(undo(
                    durable, &to, &from, names, &moved, piece, stamp, cause,
                ));
            }
        }
    }
    Ok(())
}

/// Moves the sidecars named in `files`, in that order, from `staging` into
/// the existing store folder `folder` of `pieces`.
///
/// For a version that has its `.tar.zst` and lacks a sidecar, which is what
/// `rebuild` repairs. Every name is looked at before the first file moves,
/// and nothing is replaced: a destination that exists is
/// [`VersionsError::Taken`]. Each file is flushed where it lies, renamed,
/// and the folder flushed, before the next one moves. A failure after the
/// first file has moved leaves that file in the store: a sidecar of the
/// version is valid on its own.
pub fn move_sidecars_in(
    staging: &TreeRoot,
    pieces: &TreeRoot,
    folder: &RelPath,
    files: &[&str],
) -> Result<(), VersionsError> {
    move_sidecars_in_with(&RealDurable, staging, pieces, folder, files)
}

pub(crate) fn move_sidecars_in_with(
    durable: &dyn Durable,
    staging: &TreeRoot,
    pieces: &TreeRoot,
    folder: &RelPath,
    files: &[&str],
) -> Result<(), VersionsError> {
    for name in files {
        let target = file_in(folder, name)?;
        match pieces.lstat(&target) {
            Ok(_) => {
                return Err(VersionsError::Taken {
                    path: shown(&target),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("look at", shown(&target), source)),
        }
    }

    let from = Place {
        root: staging,
        dir: &RelPath::root(),
    };
    let to = Place {
        root: pieces,
        dir: folder,
    };
    for name in files {
        transfer(durable, &from, &to, name, false).map_err(|failure| match failure {
            Failure::Before(error) => error,
            Failure::After(source) => source.into(),
        })?;
    }
    Ok(())
}

/// Moves the files of a version back from the store folder `folder` of
/// `pieces` into `staging`, the `.tar.zst` first, then the checksum file,
/// then the listing. A file that is not in the store is skipped, so the
/// rollback of a [`move_in_with`] that stopped halfway moves what is there. The
/// store folder stays, empty when nothing else is in it.
///
/// A failure after some file moved is [`VersionsError::PartlyMoved`].
pub(crate) fn move_back_with(
    durable: &dyn Durable,
    pieces: &TreeRoot,
    staging: &TreeRoot,
    folder: &RelPath,
    piece: &RelPath,
    stamp: &Stamp,
    names: &VersionFileNames,
) -> Result<(), VersionsError> {
    let from = Place {
        root: pieces,
        dir: folder,
    };
    let to = Place {
        root: staging,
        dir: &RelPath::root(),
    };
    let mut any_moved = false;
    for kind in [
        VersionFile::Archive,
        VersionFile::Checksum,
        VersionFile::Listing,
    ] {
        let name = names.of(kind);
        let source = file_in(folder, name)?;
        match pieces.lstat(&source) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error("look at", shown(&source), error)),
        }
        match transfer(durable, &from, &to, name, true) {
            Ok(()) => any_moved = true,
            Err(failure) => {
                let (arrived, error) = match failure {
                    Failure::Before(error) => (any_moved, error),
                    Failure::After(source) => (true, source.into()),
                };
                return Err(if arrived {
                    VersionsError::PartlyMoved {
                        piece: piece.to_string(),
                        stamp: stamp.to_string(),
                        source: Box::new(error),
                    }
                } else {
                    error
                });
            }
        }
    }
    Ok(())
}

/// Puts back the files in `moved` after a failure, in the reverse order of
/// moving them in. Returns the failure that started it, or
/// [`VersionsError::PartlyMoved`] when putting back fails too.
#[allow(clippy::too_many_arguments)]
fn undo(
    durable: &dyn Durable,
    store: &Place<'_>,
    staging: &Place<'_>,
    names: &VersionFileNames,
    moved: &[VersionFile],
    piece: &RelPath,
    stamp: &Stamp,
    cause: VersionsError,
) -> VersionsError {
    for kind in moved.iter().rev() {
        if transfer(durable, store, staging, names.of(*kind), true).is_err() {
            return VersionsError::PartlyMoved {
                piece: piece.to_string(),
                stamp: stamp.to_string(),
                source: Box::new(cause),
            };
        }
    }
    cause
}

/// A directory of a root.
struct Place<'a> {
    root: &'a TreeRoot,
    dir: &'a RelPath,
}

/// How a [`transfer`] failed.
enum Failure {
    /// Before the file was renamed: it is where it was.
    Before(VersionsError),
    /// After the rename: the file arrived, and making it durable failed.
    After(FilesystemError),
}

/// Moves one file: flush it where it lies, rename it without replacing,
/// flush the directory it arrived in and, when `flush_source` says so, the
/// directory it left (the removal of the `.tar.zst` from the store folder
/// must be durable before a sidecar follows).
fn transfer(
    durable: &dyn Durable,
    from: &Place<'_>,
    to: &Place<'_>,
    name: &str,
    flush_source: bool,
) -> Result<(), Failure> {
    let before = Failure::Before;
    let source = file_in(from.dir, name).map_err(before)?;
    let target = file_in(to.dir, name).map_err(before)?;

    let file = from
        .root
        .open_read(&source)
        .map_err(|error| before(io_error("open", shown(&source), error)))?;
    durable
        .file(&file, &named(from.root, &source))
        .map_err(|error| before(error.into()))?;
    from.root
        .rename(&source, to.root, &target)
        .map_err(|error| {
            before(if error.kind() == io::ErrorKind::AlreadyExists {
                VersionsError::Taken {
                    path: shown(&target),
                }
            } else {
                io_error("move", shown(&source), error)
            })
        })?;

    flush_dir_of(durable, to.root, to.dir).map_err(Failure::After)?;
    if flush_source {
        flush_dir_of(durable, from.root, from.dir).map_err(Failure::After)?;
    }
    Ok(())
}

/// The path of the file `name` in `dir`, byte for byte: a file name from
/// outside the store need not be text.
fn file_in(dir: &RelPath, name: impl AsRef<OsStr>) -> Result<RelPath, VersionsError> {
    let name = name.as_ref();
    let name = Name::from_bytes(name.as_bytes().to_vec()).map_err(|error| {
        io_error(
            "name",
            name.to_string_lossy().into_owned(),
            io::Error::new(io::ErrorKind::InvalidInput, error),
        )
    })?;
    Ok(dir.join(&name))
}

/// Deletes the files of `version` from the store folder `folder` of
/// `pieces`: every `.tar.zst` first, the folder flushed, then the
/// listings, the folder flushed, then the checksum files, the folder
/// flushed. A folder left empty goes with the parents that become empty
/// ([`remove_empty_chain_with`]). The names to delete come from the scan, so a
/// version stored under another `<name>` is deleted like any other; a file
/// that has gone since is an error.
pub fn delete_version(
    pieces: &TreeRoot,
    folder: &RelPath,
    version: &StoreVersion,
) -> Result<(), VersionsError> {
    delete_version_with(&RealDurable, pieces, folder, version)
}

pub(crate) fn delete_version_with(
    durable: &dyn Durable,
    pieces: &TreeRoot,
    folder: &RelPath,
    version: &StoreVersion,
) -> Result<(), VersionsError> {
    for kind in VersionFile::ALL {
        let files = version.files(kind);
        for file in files {
            let path = file_in(folder, file)?;
            pieces
                .remove_file(&path)
                .map_err(|error| io_error("delete", shown(&path), error))?;
        }
        if !files.is_empty() {
            flush_dir_of(durable, pieces, folder)?;
        }
    }
    remove_empty_chain_with(durable, pieces, folder).map(drop)
}

/// Removes the store folder `folder` and every parent above it that is
/// empty, up to but not including `pieces` itself, flushing the parent of
/// each removed folder. A folder that holds anything, a file whose name
/// does not parse included, stops it. A folder that is gone already does
/// not. Returns the folders it removed, the deepest first.
pub(crate) fn remove_empty_chain_with(
    durable: &dyn Durable,
    pieces: &TreeRoot,
    folder: &RelPath,
) -> Result<Vec<RelPath>, VersionsError> {
    let mut chain = Vec::new();
    let mut prefix = RelPath::root();
    for component in folder.components() {
        let name = Name::from_bytes(component.to_vec()).expect("a component of a path is a name");
        prefix = prefix.join(&name);
        chain.push(prefix.clone());
    }

    let mut removed = Vec::new();
    for path in chain.into_iter().rev() {
        match pieces.remove_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) if is_not_empty(&error) => break,
            Err(source) => return Err(io_error("remove", shown(&path), source)),
        }
        let parent = parent_of(&path);
        flush_dir_of(durable, pieces, &parent)?;
        removed.push(path);
    }
    Ok(removed)
}

fn is_not_empty(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST))
}

/// The path without its last component; the root for a single one.
fn parent_of(path: &RelPath) -> RelPath {
    let bytes = path.as_bytes();
    match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(split) => RelPath::from_bytes(bytes[..split].to_vec())
            .expect("the front of a valid path is valid"),
        None => RelPath::root(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::RestoreModes;
    use crate::testing::tree::Tree;
    use crate::versions::names::file_name;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    fn stamp(text: &str) -> Stamp {
        Stamp::parse(text).expect("a valid stamp")
    }

    /// Writes the given files of the version `stamp` of `piece` into the
    /// store folder `folder` of the tree.
    fn put(tree: &mut Tree, folder: &str, piece: &str, stamp_text: &str, kinds: &[VersionFile]) {
        for kind in kinds {
            let name = file_name(&path(piece), &stamp(stamp_text), *kind);
            tree.file(&format!("{folder}/{name}"), b"x");
        }
    }

    const ALL: [VersionFile; 3] = VersionFile::ALL;

    fn scan_of(tree: &Tree) -> StoreScan {
        scan(tree.root()).expect("the store is read")
    }

    fn paths(pieces: &[StorePiece]) -> Vec<String> {
        pieces.iter().map(|piece| piece.path.to_string()).collect()
    }

    fn stamps(piece: &StorePiece) -> Vec<String> {
        piece
            .versions
            .iter()
            .map(|version| version.stamp.to_string())
            .collect()
    }

    #[test]
    fn the_scan_finds_versions_ordered_by_stamp_with_the_files_present() {
        let mut tree = Tree::new();
        for text in [
            "2026-11-04T090211-10",
            "2026-11-04T090211",
            "2026-11-04T090211-2",
        ] {
            put(
                &mut tree,
                "projects/scanner",
                "projects/scanner",
                text,
                &ALL,
            );
        }
        put(
            &mut tree,
            "old/client-site",
            "old/client-site",
            "2026-09-03T101500",
            &[VersionFile::Archive, VersionFile::Listing],
        );
        let found = scan_of(&tree);
        assert_eq!(
            paths(&found.pieces),
            ["old/client-site", "projects/scanner"]
        );
        assert_eq!(
            stamps(&found.pieces[1]),
            [
                "2026-11-04T090211",
                "2026-11-04T090211-2",
                "2026-11-04T090211-10"
            ]
        );
        let lacking = &found.pieces[0].versions[0];
        assert_eq!(lacking.archives.len(), 1);
        assert_eq!(lacking.listings.len(), 1);
        assert!(lacking.checksums.is_empty());
        assert!(!lacking.is_incomplete());
        assert!(found.sidecar_only.is_empty());
        assert!(found.strays.is_empty());
        assert!(found.empty_folders.is_empty());
    }

    #[test]
    fn a_folder_with_sidecars_only_is_no_store_piece_but_reports_its_incomplete_versions() {
        let mut tree = Tree::new();
        put(
            &mut tree,
            "gone",
            "gone",
            "2026-09-03T101500",
            &[VersionFile::Listing, VersionFile::Checksum],
        );
        let found = scan_of(&tree);
        assert!(found.pieces.is_empty());
        assert_eq!(paths(&found.sidecar_only), ["gone"]);
        assert!(found.sidecar_only[0].versions[0].is_incomplete());
        assert!(!found.sidecar_only[0].is_store_piece());
    }

    #[test]
    fn a_version_without_an_archive_next_to_a_complete_one_is_incomplete() {
        let mut tree = Tree::new();
        put(&mut tree, "a", "a", "2026-09-03T101500", &ALL);
        put(
            &mut tree,
            "a",
            "a",
            "2026-09-04T101500",
            &[VersionFile::Listing],
        );
        let found = scan_of(&tree);
        assert!(found.pieces[0].is_store_piece());
        assert!(!found.pieces[0].versions[0].is_incomplete());
        assert!(found.pieces[0].versions[1].is_incomplete());
    }

    #[test]
    fn names_that_do_not_parse_are_strays_and_files_at_the_top_belong_to_no_piece() {
        let mut tree = Tree::new();
        put(&mut tree, "a", "a", "2026-09-03T101500", &ALL);
        tree.file("a/notes.txt", b"x");
        tree.file("a/x--bad.tar.zst", b"x");
        tree.file("loose--2026-09-03T101500.tar.zst", b"x");
        let found = scan_of(&tree);
        let strays: Vec<String> = found.strays.iter().map(RelPath::to_string).collect();
        assert_eq!(
            strays,
            [
                "a/notes.txt",
                "a/x--bad.tar.zst",
                "loose--2026-09-03T101500.tar.zst"
            ]
        );
        assert_eq!(paths(&found.pieces), ["a"]);
    }

    #[test]
    fn a_folder_with_a_stray_only_is_neither_a_piece_nor_empty() {
        let mut tree = Tree::new();
        tree.file("a/notes.txt", b"x");
        let found = scan_of(&tree);
        assert!(found.pieces.is_empty());
        assert!(found.sidecar_only.is_empty());
        assert!(found.empty_folders.is_empty());
        assert_eq!(found.strays.len(), 1);
    }

    #[test]
    fn a_second_file_for_one_slot_makes_the_version_doubled_not_a_stray() {
        let mut tree = Tree::new();
        put(&mut tree, "a", "a", "2026-09-03T101500", &ALL);
        put(
            &mut tree,
            "a",
            "other-name",
            "2026-09-03T101500",
            &[VersionFile::Archive],
        );
        let found = scan_of(&tree);
        assert!(found.strays.is_empty());
        let version = &found.pieces[0].versions[0];
        assert_eq!(found.pieces[0].versions.len(), 1);
        assert_eq!(
            version.archives,
            [
                OsString::from("a--2026-09-03T101500.tar.zst"),
                OsString::from("other-name--2026-09-03T101500.tar.zst")
            ]
        );
        assert_eq!(version.listings.len(), 1);
    }

    #[test]
    fn a_doubled_sidecar_slot_is_found_too() {
        let mut tree = Tree::new();
        put(&mut tree, "a", "a", "2026-09-03T101500", &ALL);
        put(
            &mut tree,
            "a",
            "b",
            "2026-09-03T101500",
            &[VersionFile::Checksum],
        );
        let version = &scan_of(&tree).pieces[0].versions[0];
        assert_eq!(version.checksums.len(), 2);
    }

    #[test]
    fn the_scan_keeps_the_file_names_it_found() {
        let mut tree = Tree::new();
        put(
            &mut tree,
            "a/b",
            "unrelated/name",
            "2026-09-03T101500",
            &ALL,
        );
        let version = &scan_of(&tree).pieces[0].versions[0];
        assert_eq!(
            version.files(VersionFile::Listing),
            [OsString::from(
                "unrelated-name--2026-09-03T101500.sha256.zst"
            )]
        );
        assert_eq!(version.files(VersionFile::Archive).len(), 1);
        assert_eq!(version.files(VersionFile::Checksum).len(), 1);
    }

    #[test]
    fn a_piece_and_a_piece_below_its_path_are_both_found() {
        let mut tree = Tree::new();
        put(&mut tree, "old", "old", "2026-09-03T101500", &ALL);
        put(
            &mut tree,
            "old/client-site",
            "old/client-site",
            "2026-09-04T101500",
            &ALL,
        );
        assert_eq!(paths(&scan_of(&tree).pieces), ["old", "old/client-site"]);
    }

    #[test]
    fn a_missing_pieces_directory_is_an_empty_store() {
        let tree = Tree::new();
        let found = scan(&tree.path("absent")).expect("an empty store");
        assert_eq!(found, StoreScan::default());
    }

    #[test]
    fn empty_folders_are_listed_deepest_first_and_the_top_is_not() {
        let mut tree = Tree::new();
        tree.dir("a/b/c");
        tree.dir("z");
        let found = scan_of(&tree);
        let empty: Vec<String> = found.empty_folders.iter().map(RelPath::to_string).collect();
        assert_eq!(empty, ["a/b/c", "z"]);
    }

    #[test]
    fn an_empty_store_has_no_empty_folder() {
        let tree = Tree::new();
        assert_eq!(scan_of(&tree), StoreScan::default());
    }

    #[test]
    fn a_directory_named_like_an_archive_is_a_folder_not_a_file() {
        let mut tree = Tree::new();
        tree.dir("a--2026-09-03T101500.tar.zst");
        let found = scan_of(&tree);
        assert!(found.pieces.is_empty());
        assert_eq!(
            found.empty_folders[0].to_string(),
            "a--2026-09-03T101500.tar.zst"
        );
    }

    #[test]
    fn a_symlink_named_like_an_archive_counts_as_that_file_and_is_not_followed() {
        let mut tree = Tree::new();
        tree.dir("target-dir");
        tree.file("target-dir/inside--2026-01-01T000000.tar.zst", b"x");
        tree.symlink("a/a--2026-09-03T101500.tar.zst", "../target-dir");
        tree.symlink("a/dangling--2026-09-04T101500.tar.zst", "nowhere");
        let found = scan_of(&tree);
        let piece = found
            .pieces
            .iter()
            .find(|piece| piece.path == path("a"))
            .expect("a is a piece");
        assert_eq!(piece.versions.len(), 2);
        assert_eq!(piece.versions[0].archives.len(), 1);
    }

    #[test]
    fn a_symlink_to_a_directory_with_another_name_is_a_stray_and_not_entered() {
        let mut tree = Tree::new();
        tree.file("real/x--2026-01-01T000000.tar.zst", b"x");
        tree.symlink("link", "real");
        let found = scan_of(&tree);
        let strays: Vec<String> = found.strays.iter().map(RelPath::to_string).collect();
        assert_eq!(strays, ["link"]);
        assert_eq!(paths(&found.pieces), ["real"]);
    }

    #[test]
    fn an_unreadable_folder_is_an_error_naming_it() {
        let mut tree = Tree::new();
        tree.unreadable_dir("a");
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = scan(tree.root()).expect_err("unreadable");
        assert!(
            matches!(&error, VersionsError::Io { path, .. } if path == "a"),
            "{error:?}"
        );
    }

    #[test]
    fn an_unusable_pieces_directory_is_an_error() {
        let mut tree = Tree::new();
        let file = tree.file("not-a-dir", b"x");
        let error = scan(&file).expect_err("a file");
        assert!(
            matches!(error, VersionsError::Io { action: "open", .. }),
            "{error:?}"
        );
    }

    fn root_of(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("opens")
    }

    #[test]
    fn an_existing_folder_is_found_under_its_spelling() {
        let mut tree = Tree::new();
        tree.dir("old/client-site");
        assert_eq!(
            lookup_exact(&root_of(&tree), &path("old/client-site")).expect("answers"),
            Some(path("old/client-site"))
        );
    }

    #[test]
    fn the_empty_path_is_the_root() {
        let tree = Tree::new();
        assert_eq!(
            lookup_exact(&root_of(&tree), &RelPath::root()).expect("answers"),
            Some(RelPath::root())
        );
    }

    #[test]
    fn a_missing_folder_is_not_found() {
        let mut tree = Tree::new();
        tree.dir("old");
        let root = root_of(&tree);
        assert_eq!(
            lookup_exact(&root, &path("old/client-site")).expect("answers"),
            None
        );
        assert_eq!(
            lookup_exact(&root, &path("absent/x")).expect("answers"),
            None
        );
    }

    #[test]
    fn a_name_the_volume_cannot_hold_is_not_found() {
        let tree = Tree::new();
        let long = "n".repeat(300);
        assert_eq!(
            lookup_exact(&root_of(&tree), &path(&long)).expect("answers"),
            None
        );
    }

    #[test]
    fn a_file_or_a_symlink_is_not_a_folder() {
        let mut tree = Tree::new();
        tree.file("file", b"x");
        tree.dir("real/inner");
        tree.symlink("link", "real");
        let root = root_of(&tree);
        assert_eq!(lookup_exact(&root, &path("file")).expect("answers"), None);
        assert_eq!(lookup_exact(&root, &path("file/x")).expect("answers"), None);
        assert_eq!(lookup_exact(&root, &path("link")).expect("answers"), None);
        assert_eq!(
            lookup_exact(&root, &path("link/inner")).expect("answers"),
            None
        );
    }

    #[test]
    fn a_folder_that_cannot_be_looked_into_is_an_error() {
        let mut tree = Tree::new();
        tree.dir("locked/inner");
        let locked = tree.path("locked");
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .expect("mode");
        let _restore = RestoreModes(vec![locked]);
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = lookup_exact(&root_of(&tree), &path("locked/inner")).expect_err("denied");
        assert!(matches!(error, VersionsError::Io { .. }), "{error:?}");
    }

    /// Whether the volume of the temp directory finds `LOWER` for `lower`.
    fn folds_case(tree: &mut Tree) -> bool {
        tree.file("antiq-probe-case", b"");
        tree.path("ANTIQ-PROBE-CASE").exists()
    }

    #[test]
    fn the_stored_spelling_is_found_on_a_volume_that_folds_case_and_none_on_one_that_does_not() {
        let mut tree = Tree::new();
        tree.dir("old/client");
        let folds = folds_case(&mut tree);
        let found = lookup_exact(&root_of(&tree), &path("Old/Client")).expect("answers");
        if folds {
            assert_eq!(found, Some(path("old/client")));
        } else {
            assert_eq!(found, None);
        }
    }

    fn entry(name: &str) -> DirEntry {
        let name = Name::from_bytes(name.as_bytes().to_vec()).expect("a name");
        DirEntry {
            path: RelPath::root().join(&name),
            name,
            kind: EntryKind::Directory,
        }
    }

    fn name(text: &str) -> Name {
        Name::from_bytes(text.as_bytes().to_vec()).expect("a name")
    }

    #[test]
    fn a_name_listed_byte_for_byte_is_its_own_spelling_without_asking_the_volume() {
        let listing = [entry("b"), entry("a")];
        let found = stored_name(&listing, &name("a"), |_| panic!("not asked")).expect("answers");
        assert_eq!(found, Some(name("a")));
    }

    #[test]
    fn the_listed_name_the_volume_says_is_the_same_entry_is_the_spelling() {
        let listing = [entry("Other"), entry("projects")];
        let found = stored_name(&listing, &name("PROJECTS"), |candidate| {
            Ok(candidate == &name("projects"))
        })
        .expect("answers");
        assert_eq!(found, Some(name("projects")));
    }

    #[test]
    fn with_several_candidates_the_first_by_name_wins() {
        let listing = [entry("b"), entry("a")];
        let found = stored_name(&listing, &name("X"), |_| Ok(true)).expect("answers");
        assert_eq!(found, Some(name("a")));
    }

    #[test]
    fn no_candidate_means_the_entry_is_gone() {
        let listing = [entry("a")];
        let found = stored_name(&listing, &name("X"), |_| Ok(false)).expect("answers");
        assert_eq!(found, None);
    }

    #[test]
    fn a_failing_question_is_passed_on() {
        let listing = [entry("a")];
        let error =
            stored_name(&listing, &name("X"), |_| Err(io::Error::other("no"))).expect_err("fails");
        assert_eq!(error.to_string(), "no");
    }

    // ---------------------------------------------------------
    // Operations
    // ---------------------------------------------------------

    use crate::filesystem::atomic::recording::{Flush, Recording};
    use std::sync::{Arc, Mutex};

    const STAMP: &str = "2026-11-04T090211";
    const LISTING: &str = "p--2026-11-04T090211.sha256.zst";
    const CHECKSUM: &str = "p--2026-11-04T090211.tar.zst.sha256";
    const ARCHIVE: &str = "p--2026-11-04T090211.tar.zst";

    /// A temp tree with an empty `pieces/` and a `stage/` that holds the
    /// three files of the version `STAMP` of the piece `p`.
    struct Env {
        tree: Tree,
        names: VersionFileNames,
    }

    impl Env {
        fn new() -> Env {
            let mut tree = Tree::new();
            tree.dir("pieces");
            let names = VersionFileNames::new(&path("p"), &stamp(STAMP));
            for name in [&names.listing, &names.checksum, &names.archive] {
                tree.file(&format!("stage/{name}"), name.as_bytes());
            }
            Env { tree, names }
        }

        fn staging(&self) -> TreeRoot {
            TreeRoot::open(&self.tree.path("stage")).expect("opens")
        }

        fn pieces(&self) -> TreeRoot {
            TreeRoot::open(&self.tree.path("pieces")).expect("opens")
        }

        fn stored(&self, folder: &str) -> Vec<String> {
            names_in(&self.tree.path(&format!("pieces/{folder}")))
        }

        fn staged(&self) -> Vec<String> {
            names_in(&self.tree.path("stage"))
        }

        fn move_in(&self, durable: &dyn Durable, folder: &str) -> Result<(), VersionsError> {
            move_in_with(
                durable,
                &self.staging(),
                &self.pieces(),
                &path(folder),
                &path("p"),
                &stamp(STAMP),
                &self.names,
            )
        }

        fn move_back(&self, durable: &dyn Durable, folder: &str) -> Result<(), VersionsError> {
            move_back_with(
                durable,
                &self.pieces(),
                &self.staging(),
                &path(folder),
                &path("p"),
                &stamp(STAMP),
                &self.names,
            )
        }
    }

    fn names_in(dir: &std::path::Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut found: Vec<String> = entries
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        found.sort();
        found
    }

    fn describe(flush: &Flush) -> String {
        match flush {
            Flush::File(path) => format!("file {}", path.display()),
            Flush::Dir(path) => format!("dir {}", path.display()),
        }
    }

    fn flushed(recording: &Recording) -> Vec<String> {
        recording.flushes().iter().map(describe).collect()
    }

    type Seen = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    /// Records, with every flush, the names in `watched` at that moment.
    fn watch(recording: &Recording, watched: std::path::PathBuf) -> Seen {
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        recording.observe(move |flush, _| {
            sink.lock()
                .expect("not poisoned")
                .push((describe(flush), names_in(&watched)));
        });
        seen
    }

    fn all_three() -> Vec<String> {
        vec![
            LISTING.to_string(),
            ARCHIVE.to_string(),
            CHECKSUM.to_string(),
        ]
    }

    #[test]
    fn move_in_moves_listing_checksum_and_archive_in_that_order_flushing_each() {
        let env = Env::new();
        let recording = Recording::new();
        let seen = watch(&recording, env.tree.path("pieces/p"));
        env.move_in(&recording, "p").expect("moved");

        assert_eq!(
            flushed(&recording),
            [
                "dir p".to_string(),
                "dir .".to_string(),
                format!("file {LISTING}"),
                "dir p".to_string(),
                format!("file {CHECKSUM}"),
                "dir p".to_string(),
                format!("file {ARCHIVE}"),
                "dir p".to_string(),
            ]
        );
        assert_eq!(env.stored("p"), all_three());
        assert!(env.staged().is_empty());

        // At each flush of a file, the files after it are not in the store
        // yet; the archive is the last to arrive.
        let seen = seen.lock().expect("not poisoned");
        let at = |label: &str| -> Vec<String> {
            seen.iter()
                .find(|(flush, _)| flush == label)
                .expect("flushed")
                .1
                .clone()
        };
        assert!(at(&format!("file {LISTING}")).is_empty());
        assert_eq!(at(&format!("file {CHECKSUM}")), [LISTING]);
        assert_eq!(at(&format!("file {ARCHIVE}")), [LISTING, CHECKSUM]);
    }

    #[test]
    fn move_in_creates_the_store_folder_chain_flushed() {
        let env = Env::new();
        let recording = Recording::new();
        env.move_in(&recording, "a/b").expect("moved");
        assert_eq!(
            &flushed(&recording)[..4],
            ["dir a", "dir .", "dir a/b", "dir a"]
        );
        assert_eq!(env.stored("a/b"), all_three());
    }

    #[test]
    fn move_in_beside_another_version_keeps_it() {
        let mut env = Env::new();
        env.tree
            .file("pieces/p/p--2025-01-01T000000.tar.zst", b"old");
        env.move_in(&Recording::new(), "p").expect("moved");
        assert_eq!(env.stored("p").len(), 4);
    }

    impl Env {
        /// The store folder `p` holds the archive of the version, as it does
        /// when a sidecar is to be added.
        fn with_stored_archive(mut self) -> Env {
            self.tree.file(&format!("pieces/p/{ARCHIVE}"), b"archive");
            self
        }

        fn move_sidecars(
            &self,
            durable: &dyn Durable,
            files: &[&str],
        ) -> Result<(), VersionsError> {
            move_sidecars_in_with(durable, &self.staging(), &self.pieces(), &path("p"), files)
        }
    }

    #[test]
    fn move_sidecars_in_moves_the_named_files_in_order_flushing_each() {
        let env = Env::new().with_stored_archive();
        let recording = Recording::new();
        env.move_sidecars(&recording, &[LISTING, CHECKSUM])
            .expect("moved");

        assert_eq!(
            flushed(&recording),
            [
                format!("file {LISTING}"),
                "dir p".to_string(),
                format!("file {CHECKSUM}"),
                "dir p".to_string(),
            ]
        );
        assert_eq!(env.stored("p"), all_three());
        assert_eq!(env.staged(), [ARCHIVE]);
    }

    #[test]
    fn move_sidecars_in_moves_one_file_and_leaves_the_others_staged() {
        let env = Env::new().with_stored_archive();
        env.move_sidecars(&Recording::new(), &[CHECKSUM])
            .expect("moved");
        assert_eq!(env.stored("p"), [ARCHIVE, CHECKSUM]);
        assert_eq!(env.staged(), [LISTING, ARCHIVE]);
    }

    #[test]
    fn move_sidecars_in_looks_at_every_name_before_it_moves_the_first() {
        for taken in [LISTING, CHECKSUM] {
            let mut env = Env::new().with_stored_archive();
            env.tree.file(&format!("pieces/p/{taken}"), b"already");
            let recording = Recording::new();
            let error = env
                .move_sidecars(&recording, &[LISTING, CHECKSUM])
                .expect_err("taken");
            assert!(
                matches!(&error, VersionsError::Taken { path } if path == &format!("p/{taken}")),
                "{error:?}"
            );
            assert_eq!(
                env.stored("p"),
                [ARCHIVE.to_string(), taken.to_string()]
                    .map(String::from)
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            );
            assert_eq!(env.staged(), all_three());
            assert!(flushed(&recording).is_empty());
        }
    }

    #[test]
    fn move_sidecars_in_keeps_what_moved_when_a_later_file_fails_before_its_rename() {
        let env = Env::new().with_stored_archive();
        let error = env
            .move_sidecars(&Recording::new(), &[LISTING, "p--missing"])
            .expect_err("the second file is not staged");
        assert!(
            matches!(&error, VersionsError::Io { action: "open", .. }),
            "{error:?}"
        );
        assert_eq!(env.stored("p"), [LISTING, ARCHIVE]);
    }

    #[test]
    fn move_sidecars_in_reports_a_flush_that_fails_after_the_rename() {
        let env = Env::new().with_stored_archive();
        let recording = Recording::new();
        recording.fail_dirs_from(0);
        let error = env
            .move_sidecars(&recording, &[LISTING, CHECKSUM])
            .expect_err("the folder flush fails");
        assert!(matches!(error, VersionsError::Filesystem(_)), "{error:?}");
        assert_eq!(env.stored("p"), [LISTING, ARCHIVE]);
        assert_eq!(env.staged(), [ARCHIVE, CHECKSUM]);
    }

    #[test]
    fn move_sidecars_in_into_a_folder_that_is_a_file_fails_before_it_moves_anything() {
        let mut env = Env::new();
        env.tree.file("pieces/p", b"not a folder");
        let error = env
            .move_sidecars(&Recording::new(), &[LISTING])
            .expect_err("p is no folder");
        assert!(
            matches!(
                &error,
                VersionsError::Io {
                    action: "look at",
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(env.staged(), all_three());
    }

    fn taken(slot: &str) {
        let mut env = Env::new();
        env.tree.file(&format!("pieces/p/{slot}"), b"already");
        let error = env.move_in(&Recording::new(), "p").expect_err("taken");
        assert!(
            matches!(&error, VersionsError::Taken { path } if path == &format!("p/{slot}")),
            "{error:?}"
        );
        assert_eq!(env.stored("p"), [slot]);
        assert_eq!(env.staged(), all_three());
    }

    #[test]
    fn a_taken_listing_slot_moves_nothing() {
        taken(LISTING);
    }

    #[test]
    fn a_taken_checksum_slot_moves_nothing() {
        taken(CHECKSUM);
    }

    #[test]
    fn a_taken_archive_slot_moves_nothing() {
        taken(ARCHIVE);
    }

    #[test]
    fn a_dangling_symlink_in_a_slot_is_taken() {
        let mut env = Env::new();
        env.tree.symlink(&format!("pieces/p/{ARCHIVE}"), "nowhere");
        let error = env.move_in(&Recording::new(), "p").expect_err("taken");
        assert!(matches!(error, VersionsError::Taken { .. }), "{error:?}");
        assert_eq!(env.staged(), all_three());
    }

    /// Fails the `nth` file flush only, so the undo can run.
    fn fails_file_flush(nth: usize) -> (Env, VersionsError) {
        let env = Env::new();
        let recording = Recording::new();
        recording.fail_files_at(nth);
        let error = env.move_in(&recording, "p").expect_err("a flush fails");
        (env, error)
    }

    fn undone(env: &Env, error: &VersionsError) {
        assert!(matches!(error, VersionsError::Filesystem(_)), "{error:?}");
        assert!(env.stored("p").is_empty(), "{:?}", env.stored("p"));
        assert_eq!(env.staged(), all_three());
    }

    #[test]
    fn a_failing_listing_flush_leaves_everything_in_staging() {
        let (env, error) = fails_file_flush(0);
        undone(&env, &error);
    }

    #[test]
    fn a_failing_checksum_flush_puts_the_listing_back() {
        let (env, error) = fails_file_flush(1);
        undone(&env, &error);
    }

    #[test]
    fn a_failing_archive_flush_puts_the_sidecars_back() {
        let (env, error) = fails_file_flush(2);
        undone(&env, &error);
    }

    #[test]
    fn a_failing_folder_flush_after_the_listing_puts_it_back() {
        // Two flushes of the chain come first, then the listing's.
        let env = Env::new();
        let recording = Recording::new();
        recording.fail_dirs_at(2);
        let error = env.move_in(&recording, "p").expect_err("a flush fails");
        undone(&env, &error);
    }

    #[test]
    fn a_failing_undo_is_partly_moved_and_keeps_what_moved_where_it_is() {
        let env = Env::new();
        let recording = Recording::new();
        recording.fail_files_from(1);
        let error = env.move_in(&recording, "p").expect_err("fails");
        assert!(
            matches!(&error, VersionsError::PartlyMoved { piece, stamp, .. } if piece == "p" && stamp == STAMP),
            "{error:?}"
        );
        assert_eq!(env.stored("p"), [LISTING]);
        assert_eq!(env.staged(), [ARCHIVE, CHECKSUM]);
    }

    #[test]
    fn a_failing_folder_flush_after_the_archive_is_stored_not_durable() {
        // Chain 2, listing 1, checksum 1, then the archive's.
        let env = Env::new();
        let recording = Recording::new();
        recording.fail_dirs_at(4);
        let error = env.move_in(&recording, "p").expect_err("a flush fails");
        assert!(
            matches!(&error, VersionsError::StoredNotDurable { piece, stamp, .. } if piece == "p" && stamp == STAMP),
            "{error:?}"
        );
        assert_eq!(env.stored("p"), all_three());
        assert!(env.staged().is_empty());
    }

    #[test]
    fn a_missing_staged_checksum_is_named_and_the_listing_goes_back() {
        let env = Env::new();
        std::fs::remove_file(env.tree.path(&format!("stage/{CHECKSUM}"))).expect("removed");
        let error = env.move_in(&Recording::new(), "p").expect_err("missing");
        assert!(
            matches!(&error, VersionsError::Io { action: "open", path, .. } if path == CHECKSUM),
            "{error:?}"
        );
        assert!(env.stored("p").is_empty());
        assert_eq!(env.staged(), [LISTING, ARCHIVE]);
    }

    #[test]
    fn a_missing_staged_listing_moves_nothing() {
        let env = Env::new();
        std::fs::remove_file(env.tree.path(&format!("stage/{LISTING}"))).expect("removed");
        env.move_in(&Recording::new(), "p").expect_err("missing");
        assert!(env.stored("p").is_empty());
    }

    #[test]
    fn a_rename_the_folder_refuses_is_an_error_and_moves_nothing() {
        let mut env = Env::new();
        let folder = env.tree.dir("pieces/p");
        std::fs::set_permissions(&folder, std::os::unix::fs::PermissionsExt::from_mode(0o555))
            .expect("mode");
        let _restore = RestoreModes(vec![folder]);
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = env.move_in(&Recording::new(), "p").expect_err("denied");
        assert!(
            matches!(error, VersionsError::Io { action: "move", .. }),
            "{error:?}"
        );
        assert_eq!(env.staged(), all_three());
    }

    #[test]
    fn a_staged_name_that_is_no_file_name_is_refused() {
        let env = Env::new();
        let mut names = env.names.clone();
        names.archive = "a/b".to_string();
        let error = move_in_with(
            &Recording::new(),
            &env.staging(),
            &env.pieces(),
            &path("p"),
            &path("p"),
            &stamp(STAMP),
            &names,
        )
        .expect_err("bad name");
        assert!(
            matches!(error, VersionsError::Io { action: "name", .. }),
            "{error:?}"
        );
    }

    fn unknown(tree: &Tree, name: &str) -> (TreeRoot, DirEntry) {
        let mut entry = entry(name);
        entry.kind = EntryKind::Unknown;
        (root_of(tree), entry)
    }

    #[test]
    fn an_entry_of_unknown_kind_is_asked_and_a_directory_is_one() {
        let mut tree = Tree::new();
        tree.dir("d");
        let (root, entry) = unknown(&tree, "d");
        assert_eq!(is_directory(&root, &entry).expect("answers"), Some(true));
    }

    #[test]
    fn an_entry_of_unknown_kind_that_is_a_file_or_a_link_is_no_directory() {
        let mut tree = Tree::new();
        tree.file("f", b"x");
        tree.dir("real");
        tree.symlink("l", "real");
        for name in ["f", "l"] {
            let (root, entry) = unknown(&tree, name);
            assert_eq!(
                is_directory(&root, &entry).expect("answers"),
                Some(false),
                "{name}"
            );
        }
    }

    #[test]
    fn an_entry_of_unknown_kind_that_has_gone_is_none() {
        let tree = Tree::new();
        let (root, entry) = unknown(&tree, "gone");
        assert_eq!(is_directory(&root, &entry).expect("answers"), None);
    }

    #[test]
    fn an_entry_of_unknown_kind_that_cannot_be_looked_at_is_an_error() {
        let tree = Tree::new();
        let (root, entry) = unknown(&tree, &"n".repeat(300));
        let error = is_directory(&root, &entry);
        // A name too long for the volume is not an absent entry on every
        // platform; both answers are fine, a panic or a wrong kind is not.
        assert!(
            matches!(error, Ok(None) | Err(VersionsError::Io { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_folder_that_has_gone_is_not_an_error_but_the_root_listing_is() {
        let mut tree = Tree::new();
        tree.file("f", b"x");
        let root = root_of(&tree);
        assert!(
            list_folder(&root, &path("absent"))
                .expect("answers")
                .is_none()
        );
        assert!(list_folder(&root, &path("f")).expect("answers").is_none());
        assert!(
            list_folder(&root, &RelPath::root())
                .expect("answers")
                .is_some()
        );
    }

    #[test]
    fn a_folder_that_cannot_be_listed_is_an_error() {
        let mut tree = Tree::new();
        tree.unreadable_dir("locked");
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = list_folder(&root_of(&tree), &path("locked")).expect_err("denied");
        assert!(
            matches!(error, VersionsError::Io { action: "list", .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_folder_that_vanishes_during_the_scan_is_skipped() {
        let mut tree = Tree::new();
        put(&mut tree, "kept", "kept", "2026-09-03T101500", &ALL);
        tree.dir("vanishing");
        let list = |root: &TreeRoot, folder: &RelPath| {
            if folder == &path("vanishing") {
                return Ok(None);
            }
            list_folder(root, folder)
        };
        let found = scan_root(&root_of(&tree), &list).expect("scanned");
        assert_eq!(paths(&found.pieces), ["kept"]);
        assert!(found.empty_folders.is_empty());
    }

    #[test]
    fn an_entry_that_vanishes_during_the_scan_does_not_make_its_folder_a_piece_or_stray() {
        let mut tree = Tree::new();
        tree.dir("folder");
        let list = |_: &TreeRoot, folder: &RelPath| {
            if folder.is_root() {
                let mut gone = entry("folder");
                gone.kind = EntryKind::Directory;
                return Ok(Some(vec![gone]));
            }
            let mut ghost = entry("ghost--2026-09-03T101500.tar.zst");
            ghost.path = path("folder/ghost--2026-09-03T101500.tar.zst");
            ghost.kind = EntryKind::Unknown;
            Ok(Some(vec![ghost]))
        };
        let found = scan_root(&root_of(&tree), &list).expect("scanned");
        assert!(found.pieces.is_empty());
        assert!(found.strays.is_empty());
        assert_eq!(found.empty_folders, [path("folder")]);
    }

    #[test]
    fn a_file_name_that_is_no_text_keeps_its_bytes_in_the_path() {
        let name = OsStr::from_bytes(b"caf\xff--2026-11-04T090211.tar.zst");
        let found = file_in(&path("p"), name).expect("a name");
        assert_eq!(found.as_bytes(), b"p/caf\xff--2026-11-04T090211.tar.zst");
    }

    #[test]
    fn a_name_with_a_slash_is_refused() {
        let error = file_in(&path("p"), "a/b").expect_err("no name");
        assert!(
            matches!(error, VersionsError::Io { action: "name", .. }),
            "{error:?}"
        );
    }

    /// Linux lets a file name be any bytes; macOS refuses some.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_version_stored_under_a_name_that_is_no_text_is_scanned_and_deleted() {
        let mut env = Env::new();
        let folder = env.tree.dir("pieces/p");
        for suffix in [".tar.zst", ".sha256.zst", ".tar.zst.sha256"] {
            let mut name = b"caf\xff--2026-11-04T090211".to_vec();
            name.extend_from_slice(suffix.as_bytes());
            std::fs::write(folder.join(OsStr::from_bytes(&name)), b"x").expect("written");
        }
        let found = scan(&env.tree.path("pieces")).expect("scanned");
        assert!(found.strays.is_empty());
        let version = found.pieces[0].versions[0].clone();
        delete_version_with(&Recording::new(), &env.pieces(), &path("p"), &version)
            .expect("deleted");
        assert!(!env.tree.path("pieces/p").exists());
    }

    #[test]
    fn a_destination_that_cannot_be_looked_at_is_an_error_and_moves_nothing() {
        let env = Env::new();
        let mut names = env.names.clone();
        names.listing = "l".repeat(300);
        let error = move_in_with(
            &Recording::new(),
            &env.staging(),
            &env.pieces(),
            &path("p"),
            &path("p"),
            &stamp(STAMP),
            &names,
        )
        .expect_err("too long");
        assert!(
            matches!(
                error,
                VersionsError::Io {
                    action: "look at",
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(env.staged(), all_three());
    }

    #[test]
    fn move_back_of_a_name_the_volume_cannot_look_at_is_an_error() {
        let env = Env::new();
        let mut names = env.names.clone();
        names.archive = "a".repeat(300);
        std::fs::create_dir(env.tree.path("pieces/p")).expect("folder");
        let error = move_back_with(
            &Recording::new(),
            &env.pieces(),
            &env.staging(),
            &path("p"),
            &path("p"),
            &stamp(STAMP),
            &names,
        )
        .expect_err("too long");
        assert!(
            matches!(
                error,
                VersionsError::Io {
                    action: "look at",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_staging_path_that_cannot_be_opened_is_an_error() {
        let mut env = Env::new();
        env.tree.unreadable_dir("locked");
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = fresh_stamp(
            &env.tree.path("pieces"),
            &env.tree.path("locked/pack"),
            &path("p"),
            time(),
        )
        .expect_err("denied");
        assert!(
            matches!(error, VersionsError::Io { action: "open", .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_root_that_can_be_entered_but_not_listed_is_named_with_a_dot() {
        let mut tree = Tree::new();
        let pieces = tree.dir("pieces");
        std::fs::set_permissions(&pieces, std::os::unix::fs::PermissionsExt::from_mode(0o100))
            .expect("mode");
        let _restore = RestoreModes(vec![pieces.clone()]);
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = scan(&pieces).expect_err("denied");
        assert!(
            matches!(&error, VersionsError::Io { action: "list", path, .. } if path == "."),
            "{error:?}"
        );
    }

    #[test]
    fn a_root_that_cannot_be_entered_is_an_error() {
        let mut tree = Tree::new();
        let pieces = tree.unreadable_dir("pieces");
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = scan(&pieces).expect_err("denied");
        assert!(matches!(error, VersionsError::Io { .. }), "{error:?}");
    }

    #[test]
    fn the_plain_move_in_and_move_back_use_the_real_flush() {
        let env = Env::new();
        move_in_with(
            &RealDurable,
            &env.staging(),
            &env.pieces(),
            &path("p"),
            &path("p"),
            &stamp(STAMP),
            &env.names,
        )
        .expect("moved");
        assert_eq!(env.stored("p"), all_three());
        move_back_with(
            &RealDurable,
            &env.pieces(),
            &env.staging(),
            &path("p"),
            &path("p"),
            &stamp(STAMP),
            &env.names,
        )
        .expect("moved back");
        assert_eq!(env.staged(), all_three());
    }

    #[test]
    fn move_back_returns_archive_checksum_listing_flushing_the_folder_after_each() {
        let env = Env::new();
        env.move_in(&Recording::new(), "p").expect("moved");
        let recording = Recording::new();
        let seen = watch(&recording, env.tree.path("pieces/p"));
        env.move_back(&recording, "p").expect("moved back");

        assert_eq!(
            flushed(&recording),
            [
                format!("file p/{ARCHIVE}"),
                "dir .".to_string(),
                "dir p".to_string(),
                format!("file p/{CHECKSUM}"),
                "dir .".to_string(),
                "dir p".to_string(),
                format!("file p/{LISTING}"),
                "dir .".to_string(),
                "dir p".to_string(),
            ]
        );
        assert_eq!(env.staged(), all_three());
        assert_eq!(env.stored("p"), Vec::<String>::new());
        let seen = seen.lock().expect("not poisoned");
        // When the folder is flushed after the archive left, the sidecars
        // are still in it.
        assert_eq!(
            seen[2],
            (
                "dir p".to_string(),
                vec![LISTING.to_string(), CHECKSUM.to_string()]
            )
        );
    }

    #[test]
    fn move_back_leaves_the_empty_store_folder() {
        let env = Env::new();
        env.move_in(&Recording::new(), "a/b").expect("moved");
        env.move_back(&Recording::new(), "a/b").expect("moved back");
        assert!(env.tree.path("pieces/a/b").is_dir());
    }

    #[test]
    fn move_back_moves_what_a_halfway_move_in_left() {
        let env = Env::new();
        let recording = Recording::new();
        recording.fail_files_from(1);
        env.move_in(&recording, "p").expect_err("partly moved");
        env.move_back(&Recording::new(), "p").expect("rolled back");
        assert!(env.stored("p").is_empty());
        assert_eq!(env.staged(), all_three());
    }

    #[test]
    fn move_back_of_nothing_is_fine() {
        let env = Env::new();
        std::fs::create_dir(env.tree.path("pieces/p")).expect("folder");
        env.move_back(&Recording::new(), "p")
            .expect("nothing to do");
        assert_eq!(env.staged(), all_three());
    }

    #[test]
    fn move_back_refuses_a_taken_staging_name_and_moves_nothing() {
        let env = Env::new();
        env.move_in(&Recording::new(), "p").expect("moved");
        std::fs::write(env.tree.path(&format!("stage/{ARCHIVE}")), b"in the way").expect("file");
        let error = env.move_back(&Recording::new(), "p").expect_err("taken");
        assert!(matches!(error, VersionsError::Taken { .. }), "{error:?}");
        assert_eq!(env.stored("p"), all_three());
    }

    #[test]
    fn move_back_failing_after_the_archive_moved_is_partly_moved() {
        let env = Env::new();
        env.move_in(&Recording::new(), "p").expect("moved");
        std::fs::write(env.tree.path(&format!("stage/{CHECKSUM}")), b"in the way").expect("file");
        let error = env.move_back(&Recording::new(), "p").expect_err("taken");
        assert!(
            matches!(error, VersionsError::PartlyMoved { .. }),
            "{error:?}"
        );
        assert_eq!(env.staged(), [ARCHIVE, CHECKSUM]);
        assert_eq!(env.stored("p"), [LISTING, CHECKSUM]);
    }

    #[test]
    fn move_back_failing_to_flush_after_the_archive_arrived_is_partly_moved() {
        let env = Env::new();
        env.move_in(&Recording::new(), "p").expect("moved");
        let recording = Recording::new();
        recording.fail_dirs_at(0);
        let error = env.move_back(&recording, "p").expect_err("fails");
        assert!(
            matches!(error, VersionsError::PartlyMoved { .. }),
            "{error:?}"
        );
        assert_eq!(env.staged(), [ARCHIVE]);
    }

    #[test]
    fn move_back_failing_in_its_first_flush_is_not_partly_moved() {
        let env = Env::new();
        env.move_in(&Recording::new(), "p").expect("moved");
        let recording = Recording::new();
        recording.fail_files_at(0);
        let error = env.move_back(&recording, "p").expect_err("fails");
        assert!(matches!(error, VersionsError::Filesystem(_)), "{error:?}");
        assert_eq!(env.stored("p"), all_three());
    }

    // ---------------------------------------------------------
    // delete_version and remove_empty_chain
    // ---------------------------------------------------------

    fn stored_env(folder: &str) -> (Env, StoreVersion) {
        let env = Env::new();
        env.move_in(&Recording::new(), folder).expect("moved");
        let found = scan(&env.tree.path("pieces")).expect("scan");
        let version = found.pieces[0].versions[0].clone();
        (env, version)
    }

    #[test]
    fn delete_version_deletes_the_archive_first_flushing_between() {
        let (env, version) = stored_env("p");
        let recording = Recording::new();
        let seen = watch(&recording, env.tree.path("pieces/p"));
        delete_version_with(&recording, &env.pieces(), &path("p"), &version).expect("deleted");

        let seen = seen.lock().expect("not poisoned");
        assert_eq!(
            seen[0],
            (
                "dir p".to_string(),
                vec![LISTING.to_string(), CHECKSUM.to_string()]
            )
        );
        assert_eq!(seen[1], ("dir p".to_string(), vec![CHECKSUM.to_string()]));
        assert_eq!(seen[2], ("dir p".to_string(), Vec::<String>::new()));
        assert!(!env.tree.path("pieces/p").exists());
    }

    #[test]
    fn a_failing_flush_after_the_archive_leaves_the_sidecars() {
        let (env, version) = stored_env("p");
        let recording = Recording::new();
        recording.fail_dirs_at(0);
        delete_version_with(&recording, &env.pieces(), &path("p"), &version).expect_err("fails");
        assert_eq!(env.stored("p"), [LISTING, CHECKSUM]);
    }

    #[test]
    fn delete_version_of_an_incomplete_version_deletes_what_is_there() {
        let mut env = Env::new();
        env.tree.file(&format!("pieces/p/{LISTING}"), b"x");
        let found = scan(&env.tree.path("pieces")).expect("scan");
        let version = found.sidecar_only[0].versions[0].clone();
        delete_version_with(&Recording::new(), &env.pieces(), &path("p"), &version)
            .expect("deleted");
        assert!(!env.tree.path("pieces/p").exists());
    }

    #[test]
    fn delete_version_deletes_every_file_of_a_doubled_slot_archives_first() {
        let (mut env, _) = stored_env("p");
        env.tree.file("pieces/p/q--2026-11-04T090211.tar.zst", b"x");
        let found = scan(&env.tree.path("pieces")).expect("scan");
        let version = found.pieces[0].versions[0].clone();
        assert_eq!(version.archives.len(), 2);
        let recording = Recording::new();
        let seen = watch(&recording, env.tree.path("pieces/p"));
        delete_version_with(&recording, &env.pieces(), &path("p"), &version).expect("deleted");
        assert_eq!(seen.lock().expect("not poisoned")[0].1, [LISTING, CHECKSUM]);
        assert!(!env.tree.path("pieces/p").exists());
    }

    #[test]
    fn delete_version_of_a_file_that_has_gone_is_an_error_naming_it() {
        let (env, version) = stored_env("p");
        std::fs::remove_file(env.tree.path(&format!("pieces/p/{LISTING}"))).expect("removed");
        let error = delete_version_with(&Recording::new(), &env.pieces(), &path("p"), &version)
            .expect_err("gone");
        assert!(
            matches!(&error, VersionsError::Io { action: "delete", path, .. } if path == &format!("p/{LISTING}")),
            "{error:?}"
        );
        assert!(
            env.stored("p").contains(&CHECKSUM.to_string()),
            "the checksum is not touched"
        );
    }

    #[test]
    fn the_last_version_takes_its_folder_and_the_empty_parents_with_it() {
        let (env, version) = stored_env("a/b");
        let recording = Recording::new();
        delete_version_with(&recording, &env.pieces(), &path("a/b"), &version).expect("deleted");
        assert!(!env.tree.path("pieces/a").exists());
        assert!(env.tree.path("pieces").is_dir());
        let flushes = flushed(&recording);
        assert_eq!(&flushes[flushes.len() - 2..], ["dir a", "dir ."]);
    }

    #[test]
    fn another_version_keeps_the_folder() {
        let (mut env, version) = stored_env("a/b");
        env.tree
            .file("pieces/a/b/b--2025-01-01T000000.tar.zst", b"x");
        delete_version_with(&Recording::new(), &env.pieces(), &path("a/b"), &version)
            .expect("deleted");
        assert_eq!(env.stored("a/b").len(), 1);
    }

    #[test]
    fn a_stray_keeps_the_folder_and_its_parents() {
        let (mut env, version) = stored_env("a/b");
        env.tree.file("pieces/a/b/notes.txt", b"x");
        delete_version_with(&Recording::new(), &env.pieces(), &path("a/b"), &version)
            .expect("deleted");
        assert_eq!(env.stored("a/b"), ["notes.txt"]);
    }

    #[test]
    fn a_piece_below_keeps_the_parent() {
        let (mut env, version) = stored_env("a");
        env.tree
            .file("pieces/a/b/b--2025-01-01T000000.tar.zst", b"x");
        delete_version_with(&Recording::new(), &env.pieces(), &path("a"), &version)
            .expect("deleted");
        assert!(env.tree.path("pieces/a/b").is_dir());
    }

    #[test]
    fn the_plain_delete_and_chain_removal_use_the_real_flush() {
        let (env, version) = stored_env("a/b");
        delete_version(&env.pieces(), &path("a/b"), &version).expect("deleted");
        assert!(!env.tree.path("pieces/a").exists());
        std::fs::create_dir_all(env.tree.path("pieces/c/d")).expect("dirs");
        let removed =
            remove_empty_chain_with(&RealDurable, &env.pieces(), &path("c/d")).expect("removed");
        assert_eq!(removed, [path("c/d"), path("c")]);
    }

    #[test]
    fn a_missing_folder_does_not_stop_the_chain() {
        let mut env = Env::new();
        env.tree.dir("pieces/a/b");
        let removed = remove_empty_chain_with(&Recording::new(), &env.pieces(), &path("a/b/c"))
            .expect("removed");
        assert_eq!(removed, [path("a/b"), path("a")]);
        assert!(env.pieces().lstat(&path("a")).is_err());
    }

    #[test]
    fn the_chain_stops_at_the_first_folder_that_is_not_empty() {
        let mut env = Env::new();
        env.tree.dir("pieces/a/b/c");
        env.tree.file("pieces/a/keep", b"x");
        let removed = remove_empty_chain_with(&Recording::new(), &env.pieces(), &path("a/b/c"))
            .expect("removed");
        assert_eq!(removed, [path("a/b/c"), path("a/b")]);
        assert!(env.tree.path("pieces/a/keep").is_file());
    }

    #[test]
    fn a_folder_that_cannot_be_removed_is_an_error() {
        let mut env = Env::new();
        env.tree.dir("pieces/a/b");
        let parent = env.tree.path("pieces/a");
        std::fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o555))
            .expect("mode");
        let _restore = RestoreModes(vec![parent]);
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = remove_empty_chain_with(&Recording::new(), &env.pieces(), &path("a/b"))
            .expect_err("denied");
        assert!(
            matches!(
                error,
                VersionsError::Io {
                    action: "remove",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_failing_flush_of_the_parent_is_an_error() {
        let mut env = Env::new();
        env.tree.dir("pieces/a");
        let recording = Recording::new();
        recording.fail_dirs_at(0);
        let error =
            remove_empty_chain_with(&recording, &env.pieces(), &path("a")).expect_err("fails");
        assert!(matches!(error, VersionsError::Filesystem(_)), "{error:?}");
    }

    // ---------------------------------------------------------
    // fresh_stamp
    // ---------------------------------------------------------

    fn time() -> Timestamp {
        "2026-11-04T09:02:11.5Z".parse().expect("a time")
    }

    fn fresh(env: &Env) -> Result<Stamp, VersionsError> {
        fresh_stamp(
            &env.tree.path("pieces"),
            &env.tree.path("tmp/pack"),
            &path("p"),
            time(),
        )
    }

    #[test]
    fn a_time_outside_the_range_of_stamps_gets_none() {
        let env = Env::new();
        let error = fresh_stamp(
            &env.tree.path("pieces"),
            &env.tree.path("tmp/pack"),
            &path("p"),
            "9999-12-30T22:00:00Z".parse().expect("a time"),
        )
        .expect_err("past the last stamp");
        assert!(matches!(error, VersionsError::InvalidStamp(_)), "{error:?}");
    }

    fn stored_at(env: &Env, text: &str) -> Result<bool, VersionsError> {
        is_being_stored(&env.tree.path("tmp/pack"), &path("p"), &stamp(text))
    }

    #[test]
    fn a_staged_archive_means_the_version_is_being_stored() {
        let mut env = Env::new();
        env.tree
            .file(&format!("tmp/pack/{STAMP}/p/{ARCHIVE}"), b"x");
        assert!(stored_at(&env, STAMP).expect("answers"));
    }

    #[test]
    fn a_staged_archive_under_another_name_counts() {
        let mut env = Env::new();
        env.tree
            .file(&format!("tmp/pack/{STAMP}/p/other--{STAMP}.tar.zst"), b"x");
        assert!(stored_at(&env, STAMP).expect("answers"));
    }

    #[test]
    fn staged_sidecars_alone_do_not() {
        let mut env = Env::new();
        env.tree
            .file(&format!("tmp/pack/{STAMP}/p/{LISTING}"), b"x");
        env.tree
            .file(&format!("tmp/pack/{STAMP}/p/{CHECKSUM}"), b"x");
        assert!(!stored_at(&env, STAMP).expect("answers"));
    }

    #[test]
    fn a_staged_archive_of_another_stamp_or_piece_does_not() {
        let mut env = Env::new();
        env.tree
            .file(&format!("tmp/pack/{STAMP}/q/{ARCHIVE}"), b"x");
        env.tree.file(
            "tmp/pack/2025-01-01T000000/p/p--2025-01-01T000000.tar.zst",
            b"x",
        );
        assert!(!stored_at(&env, STAMP).expect("answers"));
    }

    #[test]
    fn without_a_staging_folder_nothing_is_being_stored() {
        let env = Env::new();
        assert!(!stored_at(&env, STAMP).expect("answers"));
        let mut env = Env::new();
        env.tree
            .file("tmp/pack", b"a file where the folder would be");
        assert!(!stored_at(&env, STAMP).expect("answers"));
    }

    #[test]
    fn a_staging_folder_that_cannot_be_listed_is_an_error() {
        let mut env = Env::new();
        env.tree.unreadable_dir(&format!("tmp/pack/{STAMP}/p"));
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = stored_at(&env, STAMP).expect_err("denied");
        assert!(
            matches!(error, VersionsError::Io { action: "list", .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_staging_path_that_cannot_be_opened_is_an_error_here_too() {
        let mut env = Env::new();
        env.tree.unreadable_dir("locked");
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = is_being_stored(&env.tree.path("locked/pack"), &path("p"), &stamp(STAMP))
            .expect_err("denied");
        assert!(
            matches!(error, VersionsError::Io { action: "open", .. }),
            "{error:?}"
        );
    }

    #[test]
    fn without_a_clash_the_second_has_no_counter() {
        let env = Env::new();
        assert_eq!(fresh(&env).expect("free"), stamp(STAMP));
    }

    #[test]
    fn a_file_with_the_stamp_in_the_store_folder_gives_counter_2() {
        let mut env = Env::new();
        env.tree.file(&format!("pieces/p/{LISTING}"), b"x");
        assert_eq!(fresh(&env).expect("free"), stamp("2026-11-04T090211-2"));
    }

    #[test]
    fn a_file_under_another_name_clashes_too() {
        let mut env = Env::new();
        env.tree
            .file("pieces/p/other--2026-11-04T090211.tar.zst", b"x");
        assert_eq!(fresh(&env).expect("free"), stamp("2026-11-04T090211-2"));
    }

    #[test]
    fn a_packing_in_progress_for_the_piece_gives_counter_2() {
        let mut env = Env::new();
        env.tree.dir(&format!("tmp/pack/{STAMP}/p"));
        assert_eq!(fresh(&env).expect("free"), stamp("2026-11-04T090211-2"));
    }

    #[test]
    fn a_packing_of_another_piece_does_not_clash() {
        let mut env = Env::new();
        env.tree.dir(&format!("tmp/pack/{STAMP}/q"));
        assert_eq!(fresh(&env).expect("free"), stamp(STAMP));
    }

    #[test]
    fn both_kinds_of_clash_are_skipped_in_turn() {
        let mut env = Env::new();
        env.tree.file(&format!("pieces/p/{LISTING}"), b"x");
        env.tree.dir(&format!("tmp/pack/{STAMP}-2/p"));
        assert_eq!(fresh(&env).expect("free"), stamp("2026-11-04T090211-3"));
    }

    #[test]
    fn a_name_that_does_not_parse_does_not_clash() {
        let mut env = Env::new();
        env.tree.file("pieces/p/notes--2026-11-04T090211.txt", b"x");
        assert_eq!(fresh(&env).expect("free"), stamp(STAMP));
    }

    #[test]
    fn a_clash_found_through_the_volumes_own_equality() {
        let mut env = Env::new();
        env.tree.dir(&format!("tmp/pack/{STAMP}/P"));
        let folds = folds_case(&mut env.tree);
        let expected = if folds { "2026-11-04T090211-2" } else { STAMP };
        assert_eq!(fresh(&env).expect("free"), stamp(expected));
    }

    #[test]
    fn the_counter_stops_at_the_limit_with_an_error_naming_the_piece() {
        let mut env = Env::new();
        env.tree.file(&format!("pieces/p/{LISTING}"), b"x");
        env.tree
            .file("pieces/p/p--2026-11-04T090211-2.tar.zst", b"x");
        let free = fresh_stamp_up_to(
            3,
            &env.tree.path("pieces"),
            &env.tree.path("tmp/pack"),
            &path("p"),
            time(),
        )
        .expect("3 is free");
        assert_eq!(free, stamp("2026-11-04T090211-3"));
        env.tree
            .file("pieces/p/p--2026-11-04T090211-3.tar.zst", b"x");
        let error = fresh_stamp_up_to(
            3,
            &env.tree.path("pieces"),
            &env.tree.path("tmp/pack"),
            &path("p"),
            time(),
        )
        .expect_err("no stamp left");
        assert!(
            matches!(&error, VersionsError::CounterTooHigh { piece, limit: 3 } if piece == "p"),
            "{error:?}"
        );
    }

    #[test]
    fn counter_9999_is_still_given_and_the_next_is_refused() {
        let mut env = Env::new();
        let first = stamp(STAMP);
        env.tree.file(&format!("pieces/p/{LISTING}"), b"x");
        for counter in 2..MAX_COUNTER {
            let name = format!("p--{}.tar.zst", first.with_counter(counter));
            env.tree.file(&format!("pieces/p/{name}"), b"");
        }
        assert_eq!(fresh(&env).expect("free"), first.with_counter(MAX_COUNTER));
        let name = format!("p--{}.tar.zst", first.with_counter(MAX_COUNTER));
        env.tree.file(&format!("pieces/p/{name}"), b"");
        let error = fresh(&env).expect_err("none left");
        assert!(
            matches!(error, VersionsError::CounterTooHigh { limit: 9999, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_staging_directory_that_cannot_be_looked_into_is_an_error() {
        let mut env = Env::new();
        env.tree.unreadable_dir(&format!("tmp/pack/{STAMP}"));
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = fresh(&env).expect_err("denied");
        assert!(
            matches!(
                error,
                VersionsError::Io {
                    action: "look at",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn an_unreadable_store_folder_is_an_error() {
        let mut env = Env::new();
        env.tree.unreadable_dir("pieces/p");
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error = fresh(&env).expect_err("denied");
        assert!(
            matches!(error, VersionsError::Io { action: "list", .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_missing_store_folder_and_staging_are_no_clash() {
        let env = Env::new();
        let stamp_found = fresh_stamp(
            &env.tree.path("absent"),
            &env.tree.path("absent-too"),
            &path("p"),
            time(),
        )
        .expect("free");
        assert_eq!(stamp_found, stamp(STAMP));
    }
}
