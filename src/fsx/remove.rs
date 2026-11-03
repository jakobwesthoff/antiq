// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Removing trees.
//!
//! Callers hold the run lock; see the module docs of [`crate::fsx`].

// TODO: the start-of-run repairs (I17) and `archive` (I19) are the
// callers; until they land the library has none.
#![allow(dead_code)]

use super::{FsxError, failure, io_error, shown};
use crate::fsx::tree::{DirEntry, TreeRoot};
use crate::relpath::{Name, RelPath};
use crate::tree::{EntryKind, TreeEntry};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Removes the entry at `path` below `root`, with everything inside it.
///
/// A directory that denies access is given owner access once and tried
/// again, the top included; the directory holding the top is never
/// changed. Nothing is followed across a device boundary: a top or a
/// directory on another device than `root` fails with
/// [`FsxError::ForeignDevice`], leaving what it holds untouched. An entry
/// that is gone already counts as removed.
pub(crate) fn remove_tree(root: &TreeRoot, path: &RelPath) -> Result<(), FsxError> {
    remove_tree_with(root, path, &|entry| entry.device, &|dir| {
        dir.read_dir(&RelPath::root())
    })
}

/// Removes the entry at `path`, for a path short enough to open its
/// parent. Entries below a piece in place go through [`remove_tree`] on
/// the collection's root instead.
pub(crate) fn remove_tree_at(path: &Path) -> Result<(), FsxError> {
    let Some((parent, name)) = split_path(path) else {
        let source = io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path has no parent directory and file name",
        );
        return Err(io_error("remove", path)(source));
    };
    let opened = skip_if_missing(TreeRoot::open(parent)).map_err(io_error("open", parent))?;
    let Some(root) = opened else { return Ok(()) };
    let name = Name::from_bytes(name.as_bytes().to_vec())
        .expect("a file name has no slash or NUL and is neither . nor ..");
    remove_tree(&root, &RelPath::root().join(&name))
}

/// The directory holding the last component of `path`, and that
/// component. A bare name lives in the current directory.
fn split_path(path: &Path) -> Option<(&Path, &OsStr)> {
    let name = path.file_name()?;
    let parent = path.parent()?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    Some((parent, name))
}

type DeviceOf<'a> = &'a dyn Fn(&TreeEntry) -> u64;
type List<'a> = &'a dyn Fn(&TreeRoot) -> io::Result<Vec<DirEntry>>;

/// `device_of` and `list` are seams: tests report other devices than the
/// filesystem has, and entries the directory does not describe truthfully.
fn remove_tree_with(
    root: &TreeRoot,
    path: &RelPath,
    device_of: DeviceOf<'_>,
    list: List<'_>,
) -> Result<(), FsxError> {
    // The device the removal has to stay on is the one of the directory
    // holding the top, so a top that is itself a mount point is refused.
    let holder = root
        .lstat(&RelPath::root())
        .map_err(failure("stat", root, &RelPath::root()))?;
    let reference = device_of(&holder);

    let stat = skip_if_missing(root.lstat(path)).map_err(failure("stat", root, path))?;
    let Some(top) = stat else { return Ok(()) };
    if top.kind != EntryKind::Directory {
        return skip_if_missing(root.remove_file(path))
            .map(|_| ())
            .map_err(failure("remove", root, path));
    }
    if device_of(&top) != reference {
        return Err(foreign_device(root, path));
    }

    let opened = retry_once_with_access(&mut || grant_owner(root, path, top.mode), &mut || {
        root.sub(path)
    });
    let opened = skip_if_missing(opened).map_err(failure("open", root, path))?;
    // A directory that vanished in between has nothing left to remove.
    opened
        .map(|directory| {
            let mut grant = || grant_owner(root, path, top.mode);
            remove_contents(&directory, &mut grant, reference, device_of, list)
        })
        .transpose()?;
    skip_if_missing(root.remove_dir(path))
        .map(|_| ())
        .map_err(failure("remove", root, path))
}

/// Removes everything inside `directory`. The kind and device of every
/// entry come from its own `lstat`, as a directory listing may leave the
/// kind open.
///
/// `grant_itself` gives `directory` owner access through the directory
/// that holds it: asking the directory itself fails once it has lost its
/// search permission.
fn remove_contents(
    directory: &TreeRoot,
    grant_itself: &mut dyn FnMut() -> io::Result<()>,
    reference: u64,
    device_of: DeviceOf<'_>,
    list: List<'_>,
) -> Result<(), FsxError> {
    let itself = RelPath::root();
    let entries = retry_once_with_access(&mut *grant_itself, &mut || list(directory))
        .map_err(failure("list", directory, &itself))?;

    for entry in entries {
        let name = RelPath::root().join(&entry.name);
        let stat = skip_if_missing(directory.lstat(&name));
        let stat = stat.map_err(failure("stat", directory, &name))?;
        let Some(stat) = stat else { continue };
        let removal = if stat.kind == EntryKind::Directory {
            if device_of(&stat) != reference {
                return Err(foreign_device(directory, &name));
            }
            let opened = retry_once_with_access(
                &mut || grant_owner(directory, &name, stat.mode),
                &mut || directory.sub(&name),
            );
            let opened = skip_if_missing(opened).map_err(failure("open", directory, &name))?;
            let Some(child) = opened else { continue };
            let mut grant_child = || grant_owner(directory, &name, stat.mode);
            remove_contents(&child, &mut grant_child, reference, device_of, list)?;
            retry_once_with_access(&mut *grant_itself, &mut || directory.remove_dir(&name))
        } else {
            retry_once_with_access(&mut *grant_itself, &mut || directory.remove_file(&name))
        };
        skip_if_missing(removal).map_err(failure("remove", directory, &name))?;
    }
    Ok(())
}

/// Adds owner read, write and search to the directory at `path`.
fn grant_owner(root: &TreeRoot, path: &RelPath, mode: u32) -> io::Result<()> {
    root.set_mode(path, (mode & 0o7777) | 0o700)
}

/// Runs `operation`; when it is denied, runs `grant` once and then
/// `operation` once more. Any other failure, and a failing `grant`, leave
/// the denial as the result.
fn retry_once_with_access<T>(
    grant: &mut dyn FnMut() -> io::Result<()>,
    operation: &mut dyn FnMut() -> io::Result<T>,
) -> io::Result<T> {
    match operation() {
        Err(denied) if denied.kind() == io::ErrorKind::PermissionDenied => match grant() {
            Ok(()) => operation(),
            Err(_) => Err(denied),
        },
        other => other,
    }
}

/// An entry that is gone already is no failure of a removal: `None`.
fn skip_if_missing<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn foreign_device(root: &TreeRoot, path: &RelPath) -> FsxError {
    FsxError::ForeignDevice {
        path: shown(root, path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::RestoreModes;
    use crate::fsx::tree::TreeRoot;
    use crate::relpath::Name;
    use crate::testing::deep_path;
    use crate::testing::tree::Tree;
    use rustix::io::Errno;
    use std::fs::{self, File, Permissions};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn root_of(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("the root opens")
    }

    fn real_list(dir: &TreeRoot) -> io::Result<Vec<DirEntry>> {
        dir.read_dir(&RelPath::root())
    }

    fn last_name(entry: &TreeEntry) -> &[u8] {
        entry.path.components().last().unwrap_or(b"")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("exists").mode_bits()
    }

    trait ModeBits {
        fn mode_bits(&self) -> u32;
    }

    impl ModeBits for fs::Metadata {
        fn mode_bits(&self) -> u32 {
            self.permissions().mode() & 0o777
        }
    }

    fn locked_out(dir: &Path) {
        let probe = File::create(dir.join("probe")).expect_err("the test needs a non-root user");
        assert_eq!(
            probe.kind(),
            io::ErrorKind::PermissionDenied,
            "a read-only directory has to refuse new files"
        );
    }

    #[test]
    fn a_tree_of_every_kind_is_removed_and_nothing_outside_it() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        tree.file("t/d/b", "y");
        tree.file("outside/keep", "z");
        tree.symlink("t/link", tree.path("outside"));
        tree.symlink("t/dangling", "nowhere");
        tree.fifo("t/q");
        remove_tree(&root_of(&tree), &path("t")).expect("removes");
        assert!(!tree.path("t").exists());
        assert!(tree.path("outside/keep").exists());
    }

    #[test]
    fn read_only_and_locked_directories_do_not_stop_the_removal() {
        let mut tree = Tree::new();
        tree.file("t/ro/inner/f", "x");
        tree.file("t/ro/g", "x");
        tree.file("t/locked/h", "x");
        let (ro, locked) = (tree.path("t/ro"), tree.path("t/locked"));
        let _guard = RestoreModes(vec![ro.clone(), locked.clone()]);
        fs::set_permissions(&ro, Permissions::from_mode(0o555)).expect("chmod");
        fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("chmod");
        locked_out(&ro);
        remove_tree(&root_of(&tree), &path("t")).expect("removes");
        assert!(!tree.path("t").exists());
    }

    #[test]
    fn a_read_only_top_is_removed_too() {
        let mut tree = Tree::new();
        tree.file("t/f", "x");
        let top = tree.path("t");
        let _guard = RestoreModes(vec![top.clone()]);
        fs::set_permissions(&top, Permissions::from_mode(0o555)).expect("chmod");
        locked_out(&top);
        remove_tree(&root_of(&tree), &path("t")).expect("removes");
        assert!(!top.exists());
    }

    #[test]
    fn a_tree_deeper_than_the_path_limit_is_removed() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let deep = deep_path(25);
        let mut so_far = RelPath::root();
        for component in deep.components() {
            let name = Name::from_bytes(component.to_vec()).expect("name");
            so_far = so_far.join(&name);
            root.create_dir(&so_far, 0o755).expect("created");
        }
        root.create_file(
            &deep.join(&Name::from_bytes(b"f".to_vec()).expect("name")),
            0o644,
        )
        .expect("created");
        let whole = tree
            .root()
            .join(std::ffi::OsStr::from_bytes(deep.as_bytes()));
        let refused = fs::symlink_metadata(&whole).expect_err("too long for a path call");
        assert_eq!(
            refused.raw_os_error(),
            Some(Errno::NAMETOOLONG.raw_os_error())
        );

        let first =
            Name::from_bytes(deep.components().next().expect("one").to_vec()).expect("name");
        remove_tree(&root, &RelPath::root().join(&first)).expect("removes");
        assert!(root.read_dir(&RelPath::root()).expect("listed").is_empty());
    }

    #[test]
    fn a_directory_on_another_device_stops_the_removal() {
        let mut tree = Tree::new();
        tree.file("t/mnt/file", "x");
        tree.file("t/other", "y");
        let device = |entry: &TreeEntry| if last_name(entry) == b"mnt" { 9 } else { 8 };
        let error =
            remove_tree_with(&root_of(&tree), &path("t"), &device, &real_list).expect_err("stops");
        assert!(
            matches!(&error, FsxError::ForeignDevice { path } if path == "t/mnt"),
            "{error}"
        );
        assert!(tree.path("t/mnt/file").exists());
    }

    #[test]
    fn a_top_on_another_device_than_its_directory_is_not_entered() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        let device = |entry: &TreeEntry| if last_name(entry) == b"t" { 9 } else { 8 };
        let error = remove_tree_with(&root_of(&tree), &path("t"), &device, &real_list)
            .expect_err("refused");
        assert!(
            matches!(&error, FsxError::ForeignDevice { path } if path == "t"),
            "{error}"
        );
        assert!(tree.path("t/a").exists());
    }

    #[test]
    fn a_directory_listed_with_an_unknown_kind_is_still_removed_as_one() {
        use crate::tree::EntryKind;
        let mut tree = Tree::new();
        tree.file("t/dir/f", "x");
        let unknown = |dir: &TreeRoot| {
            let mut entries = real_list(dir)?;
            for entry in &mut entries {
                entry.kind = EntryKind::Unknown;
            }
            Ok(entries)
        };
        let device = |entry: &TreeEntry| entry.device;
        remove_tree_with(&root_of(&tree), &path("t"), &device, &unknown).expect("removes");
        assert!(!tree.path("t").exists());
    }

    #[test]
    fn an_entry_that_vanished_since_the_listing_is_not_an_error() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        let ghost = |dir: &TreeRoot| {
            let mut entries = real_list(dir)?;
            let name = Name::from_bytes(b"ghost".to_vec()).expect("name");
            entries.push(DirEntry {
                path: RelPath::root().join(&name),
                name,
                kind: crate::tree::EntryKind::Directory,
            });
            Ok(entries)
        };
        let device = |entry: &TreeEntry| entry.device;
        remove_tree_with(&root_of(&tree), &path("t"), &device, &ghost).expect("removes");
        assert!(!tree.path("t").exists());
    }

    #[test]
    fn a_missing_top_is_already_removed() {
        let tree = Tree::new();
        remove_tree(&root_of(&tree), &path("nothing")).expect("nothing to do");
    }

    #[test]
    fn a_path_through_a_file_is_an_error_naming_it() {
        let mut tree = Tree::new();
        tree.file("f", "x");
        let error = remove_tree(&root_of(&tree), &path("f/x")).expect_err("fails");
        assert!(error.to_string().contains("f/x"), "{error}");
    }

    #[test]
    fn a_file_and_a_symlink_are_removed_alone() {
        let mut tree = Tree::new();
        tree.file("outside/keep", "z");
        tree.file("f", "x");
        tree.symlink("l", tree.path("outside"));
        let root = root_of(&tree);
        remove_tree(&root, &path("f")).expect("removes");
        remove_tree(&root, &path("l")).expect("removes");
        assert!(!tree.path("f").exists());
        assert!(fs::symlink_metadata(tree.path("l")).is_err());
        assert!(tree.path("outside/keep").exists());
    }

    #[test]
    fn a_top_in_a_directory_that_refuses_the_removal_fails_and_changes_nothing_above_it() {
        let mut tree = Tree::new();
        tree.file("p/t/f", "x");
        let parent = tree.path("p");
        let root = TreeRoot::open(&parent).expect("opens");
        let _guard = RestoreModes(vec![parent.clone()]);
        fs::set_permissions(&parent, Permissions::from_mode(0o555)).expect("chmod");
        locked_out(&parent);
        let error = remove_tree(&root, &path("t")).expect_err("fails");
        assert!(error.to_string().contains("remove t"), "{error}");
        assert!(parent.join("t").exists());
        assert_eq!(mode_of(&parent), 0o555);
    }

    /// Runs the retry with an operation that fails with `failure` on every
    /// attempt (or works when it is `None`), and a grant that fails when
    /// `grant_fails`. Returns the result, the attempts and the grants.
    fn retried(failure: Option<io::ErrorKind>, grant_fails: bool) -> (io::Result<u32>, u32, u32) {
        let (attempts, grants) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
        let result = retry_once_with_access(
            &mut || {
                grants.set(grants.get() + 1);
                if grant_fails {
                    Err(io::Error::from(io::ErrorKind::Other))
                } else {
                    Ok(())
                }
            },
            &mut || {
                attempts.set(attempts.get() + 1);
                failure.map_or(Ok(7), |kind| Err(io::Error::from(kind)))
            },
        );
        (result, attempts.get(), grants.get())
    }

    #[test]
    fn a_denied_operation_is_granted_access_and_retried_once() {
        let (result, attempts, grants) = retried(Some(io::ErrorKind::PermissionDenied), false);
        assert!(result.is_err());
        assert_eq!((attempts, grants), (2, 1));
    }

    #[test]
    fn an_operation_that_works_is_not_granted_anything() {
        let (result, attempts, grants) = retried(None, false);
        assert_eq!(result.expect("works"), 7);
        assert_eq!((attempts, grants), (1, 0));
    }

    #[test]
    fn another_failure_is_not_retried() {
        let (result, attempts, grants) = retried(Some(io::ErrorKind::NotFound), false);
        assert_eq!(result.expect_err("fails").kind(), io::ErrorKind::NotFound);
        assert_eq!((attempts, grants), (1, 0));
    }

    #[test]
    fn a_failing_grant_gives_up_with_the_original_denial() {
        let (result, attempts, grants) = retried(Some(io::ErrorKind::PermissionDenied), true);
        let kind = result.expect_err("fails").kind();
        assert_eq!(kind, io::ErrorKind::PermissionDenied);
        assert_eq!((attempts, grants), (1, 1));
    }

    #[test]
    fn a_missing_entry_is_none_and_other_failures_stay() {
        assert_eq!(skip_if_missing(Ok(3)).expect("works"), Some(3));
        let gone = skip_if_missing::<()>(Err(io::Error::from(io::ErrorKind::NotFound)));
        assert_eq!(gone.expect("not an error"), None);
        let denied = skip_if_missing::<()>(Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(denied.is_err());
    }

    #[test]
    fn a_path_splits_into_its_directory_and_name() {
        let split = |text| split_path(Path::new(text));
        assert_eq!(split("a/b"), Some((Path::new("a"), OsStr::new("b"))));
        assert_eq!(split("b"), Some((Path::new("."), OsStr::new("b"))));
        assert_eq!(split("/b"), Some((Path::new("/"), OsStr::new("b"))));
        assert_eq!(split("/"), None);
        assert_eq!(split("a/.."), None);
    }

    #[test]
    fn remove_tree_at_fails_when_the_parent_is_not_a_directory() {
        let mut tree = Tree::new();
        let file = tree.file("f", "x");
        let error = remove_tree_at(&file.join("child")).expect_err("fails");
        assert!(error.to_string().contains("cannot open"), "{error}");
    }

    #[test]
    fn remove_tree_at_removes_a_directory_by_its_path() {
        let mut tree = Tree::new();
        tree.file("t/a/b", "x");
        remove_tree_at(&tree.path("t")).expect("removes");
        assert!(!tree.path("t").exists());
    }

    #[test]
    fn remove_tree_at_treats_a_missing_parent_as_removed() {
        let tree = Tree::new();
        remove_tree_at(&tree.path("no/such/t")).expect("nothing to do");
    }

    #[test]
    fn a_path_without_a_file_name_cannot_be_removed() {
        let error = remove_tree_at(Path::new("/")).expect_err("refused");
        assert!(error.to_string().contains("remove /"), "{error}");
    }
}
