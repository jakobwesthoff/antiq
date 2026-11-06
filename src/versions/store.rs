// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The store under `.antiq/pieces/` (spec "Store layout" and "Damage"):
//! reading what it holds, finding a folder by the volume's own name
//! equality, and the operations that put a version in, take it back and
//! delete it.

use super::VersionsError;
use super::names::{VersionFile, parse_file_name};
use super::stamp::Stamp;
use crate::fsx::tree::{DirEntry, TreeRoot};
use crate::relpath::{Name, RelPath};
use crate::tree::EntryKind;
use std::collections::BTreeMap;
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
/// so that is damage of the version ([`StoreVersion::is_doubled`]).
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

    /// Whether two files claim one slot.
    pub fn is_doubled(&self) -> bool {
        VersionFile::ALL
            .into_iter()
            .any(|kind| self.files(kind).len() > 1)
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
/// more. `status` and `list` scan without the lock while a run may be
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::RestoreModes;
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
        assert!(!lacking.is_doubled());
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
        assert!(version.is_doubled());
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
        assert!(version.is_doubled());
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
}
