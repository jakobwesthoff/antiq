// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Removing trees.
//!
//! Callers hold the run lock; see the module docs of [`crate::filesystem`].

use super::{FilesystemError, failure, io_error, shown};
use crate::filesystem::tree::{DirEntry, TreeRoot};
use crate::relpath::{Name, RelPath};
use crate::tree::{EntryKind, TreeEntry};
use crate::walk::MAX_DEPTH;
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
/// [`FilesystemError::ForeignDevice`], leaving what it holds untouched. So does the
/// root of another mount on the same device, such as a bind mount, and so
/// does a directory above the top that is a mount root. An entry that is
/// gone already counts as removed. A directory more than
/// [`MAX_DEPTH`] levels below the top is not entered: the removal fails
/// with [`FilesystemError::TooDeep`] naming it, each level holding one directory
/// open. The root path is refused as an
/// `InvalidInput` failure, before anything is looked at: the tree cannot
/// remove itself.
pub(crate) fn remove_tree(root: &TreeRoot, path: &RelPath) -> Result<(), FilesystemError> {
    remove_tree_with(
        root,
        path,
        &|entry| entry.device,
        &|entry| entry.mount_root,
        &|dir| dir.read_dir(&RelPath::root()),
        MAX_DEPTH,
    )
}

/// Removes the entry at `path`, for a path short enough to open its
/// parent. Entries below a piece in place go through [`remove_tree`] on
/// the collection's root instead.
pub(crate) fn remove_tree_at(path: &Path) -> Result<(), FilesystemError> {
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
type MountRootOf<'a> = &'a dyn Fn(&TreeEntry) -> bool;
type OwnerOf<'a> = &'a dyn Fn(&TreeEntry) -> u32;
type List<'a> = &'a dyn Fn(&TreeRoot) -> io::Result<Vec<DirEntry>>;

/// `device_of`, `mount_root_of` and `list` are seams: tests report other
/// devices and mount roots than the filesystem has, and entries the
/// directory does not describe truthfully.
/// `max_depth` is the deepest level of directories below the top that is
/// entered.
fn remove_tree_with(
    root: &TreeRoot,
    path: &RelPath,
    device_of: DeviceOf<'_>,
    mount_root_of: MountRootOf<'_>,
    list: List<'_>,
    max_depth: usize,
) -> Result<(), FilesystemError> {
    remove_tree_owned(
        root,
        path,
        device_of,
        mount_root_of,
        list,
        max_depth,
        &|entry| entry.uid,
    )
}

/// [`remove_tree_with`] with a seam for the owner of an entry, which a
/// test cannot change.
fn remove_tree_owned(
    root: &TreeRoot,
    path: &RelPath,
    device_of: DeviceOf<'_>,
    mount_root_of: MountRootOf<'_>,
    list: List<'_>,
    max_depth: usize,
    owner_of: OwnerOf<'_>,
) -> Result<(), FilesystemError> {
    // The root path names the tree itself, which has no directory above it
    // to take the last `remove_dir` from. Refusing here keeps the removal
    // from emptying the tree and failing only afterwards.
    if path.is_root() {
        let source = io::Error::new(
            io::ErrorKind::InvalidInput,
            "it is the root of the tree itself",
        );
        return Err(failure("remove", root, path)(source));
    }
    // The removal has to stay on the device of `root`, so a top on another
    // device is refused, and so is the root of a mount on that same device
    // or a mount root above the top.
    let holder = root
        .lstat(&RelPath::root())
        .map_err(failure("stat", root, &RelPath::root()))?;
    let reference = device_of(&holder);

    // A mount root above the top shares the device of everything below it,
    // so only the directories above show it.
    let mut above = RelPath::root();
    let mut names: Vec<&[u8]> = path.components().collect();
    names.pop();
    for component in names {
        let name = Name::from_bytes(component.to_vec()).expect("a component of a path is a name");
        above = above.join(&name);
        let found = skip_if_missing(root.lstat(&above)).map_err(failure("stat", root, &above))?;
        if found.is_some_and(|entry| mount_root_of(&entry)) {
            return Err(foreign_device(root, &above));
        }
    }

    let stat = skip_if_missing(root.lstat(path)).map_err(failure("stat", root, path))?;
    let Some(top) = stat else { return Ok(()) };
    // Nothing may be removed from a tree that cannot be removed whole, so
    // every refusal comes before the first removal.
    refuse_locked_holder(root, path)?;
    if top.flag_locked {
        return Err(locked(root, path));
    }
    if top.kind != EntryKind::Directory {
        return skip_if_missing(root.remove_file(path))
            .map(|_| ())
            .map_err(failure("remove", root, path));
    }
    if device_of(&top) != reference || mount_root_of(&top) {
        return Err(foreign_device(root, path));
    }
    let scan = Scan {
        reference,
        device_of,
        mount_root_of,
        list,
        owner_of,
        max_depth,
    };
    scan.refuse_locked(root, path, &top)?;

    // One open directory per level is kept in a list of its own and not on
    // the call stack, so a deep tree costs no deep recursion. `max_depth`
    // keeps the open directories few.
    let mut frames: Vec<Frame> = Vec::new();
    frames.extend(open_frame(root, path, top.mode, mount_root_of, list)?);
    while let Some(innermost) = frames.len().checked_sub(1) {
        let Some(entry) = frames[innermost].entries.next() else {
            let finished = frames.pop().expect("a frame was just looked at");
            // The top itself is removed below, from the directory that
            // holds it, which is never changed.
            if let Some(parent) = innermost.checked_sub(1) {
                let holder = &frames[parent].directory;
                let removal =
                    retry_once_with_access(&mut || grant_frame(root, &frames, parent), &mut || {
                        holder.remove_dir(&finished.name)
                    });
                skip_if_missing(removal).map_err(failure("remove", holder, &finished.name))?;
            }
            continue;
        };

        let directory = &frames[innermost].directory;
        let name = RelPath::root().join(&entry.name);
        let stat = skip_if_missing(directory.lstat(&name));
        let stat = stat.map_err(failure("stat", directory, &name))?;
        let Some(stat) = stat else { continue };
        if stat.kind != EntryKind::Directory {
            let removal =
                retry_once_with_access(&mut || grant_frame(root, &frames, innermost), &mut || {
                    directory.remove_file(&name)
                });
            skip_if_missing(removal).map_err(failure("remove", directory, &name))?;
            continue;
        }
        if device_of(&stat) != reference || mount_root_of(&stat) {
            return Err(foreign_device(directory, &name));
        }
        if frames.len() > max_depth {
            return Err(FilesystemError::TooDeep {
                path: shown(directory, &name),
                limit: max_depth,
            });
        }
        if let Some(child) = open_frame(directory, &name, stat.mode, mount_root_of, list)? {
            frames.push(child);
        }
    }
    skip_if_missing(root.remove_dir(path))
        .map(|_| ())
        .map_err(failure("remove", root, path))
}

/// The refusal for the entry at `path`; the root of the original tree
/// is named ".".
fn locked(root: &TreeRoot, path: &RelPath) -> FilesystemError {
    let shown = shown(root, path);
    FilesystemError::Locked {
        path: if shown.is_empty() { ".".into() } else { shown },
    }
}

/// Refuses the removal when the directory that holds `path` carries a
/// flag or cannot be written by the user, whoever owns it: the entry is
/// removed from it, and antiq changes nothing outside the tree.
fn refuse_locked_holder(root: &TreeRoot, path: &RelPath) -> Result<(), FilesystemError> {
    let mut holder = RelPath::root();
    let mut names: Vec<&[u8]> = path.components().collect();
    names.pop();
    for component in names {
        let name = Name::from_bytes(component.to_vec()).expect("a component of a path is a name");
        holder = holder.join(&name);
    }
    let entry = root
        .lstat(&holder)
        .map_err(failure("stat", root, &holder))?;
    let writable =
        root.user_can_write(&holder)
            .map_err(failure("check the access to", root, &holder))?;
    if entry.flag_locked || !writable {
        return Err(locked(root, &holder));
    }
    Ok(())
}

/// The rules a tree is read by before anything in it is removed.
struct Scan<'a> {
    reference: u64,
    device_of: DeviceOf<'a>,
    mount_root_of: MountRootOf<'a>,
    list: List<'a>,
    owner_of: OwnerOf<'a>,
    max_depth: usize,
}

impl Scan<'_> {
    /// Reads the tree below the directory `path` of `root` without
    /// changing it, and refuses it with [`FilesystemError::Locked`] when an entry
    /// carries a flag, or is a directory that another user owns and this
    /// one cannot write, with [`FilesystemError::ForeignDevice`] and
    /// [`FilesystemError::TooDeep`] as the removal would. A directory that denies
    /// access is given owner access to be read; its recorded mode comes
    /// back when the scan leaves it, also on a refusal.
    fn refuse_locked(
        &self,
        root: &TreeRoot,
        path: &RelPath,
        top: &TreeEntry,
    ) -> Result<(), FilesystemError> {
        let mut frames: Vec<Frame> = Vec::new();
        let result = self.descend(root, path, top, &mut frames);
        // Only a refusal leaves frames open, and its error is the one worth
        // reporting, so the modes come back on a best effort.
        while let Some(frame) = frames.pop() {
            let holder = frames.last().map_or(root, |above| &above.directory);
            let _ = restore_mode(holder, &frame);
        }
        result
    }

    fn descend(
        &self,
        root: &TreeRoot,
        path: &RelPath,
        top: &TreeEntry,
        frames: &mut Vec<Frame>,
    ) -> Result<(), FilesystemError> {
        self.check_directory(root, path, top)?;
        frames.extend(open_frame(
            root,
            path,
            top.mode,
            self.mount_root_of,
            self.list,
        )?);
        while let Some(innermost) = frames.len().checked_sub(1) {
            let Some(entry) = frames[innermost].entries.next() else {
                let finished = frames.pop().expect("a frame was just looked at");
                let holder = frames.last().map_or(root, |above| &above.directory);
                restore_mode(holder, &finished)?;
                continue;
            };
            let directory = &frames[innermost].directory;
            let name = RelPath::root().join(&entry.name);
            let stat = skip_if_missing(directory.lstat(&name));
            let stat = stat.map_err(failure("stat", directory, &name))?;
            let Some(stat) = stat else { continue };
            if stat.flag_locked {
                return Err(locked(directory, &name));
            }
            if stat.kind != EntryKind::Directory {
                continue;
            }
            self.check_directory(directory, &name, &stat)?;
            if frames.len() > self.max_depth {
                return Err(FilesystemError::TooDeep {
                    path: shown(directory, &name),
                    limit: self.max_depth,
                });
            }
            if let Some(child) = self.open(directory, &name, stat.mode)? {
                frames.push(child);
            }
        }
        Ok(())
    }

    /// [`open_frame`], taking the mode back when the directory was given
    /// owner access and then failed to open or list.
    fn open(
        &self,
        holder: &TreeRoot,
        name: &RelPath,
        mode: u32,
    ) -> Result<Option<Frame>, FilesystemError> {
        let opened = open_frame(holder, name, mode, self.mount_root_of, self.list);
        if opened.is_err() {
            let recorded = mode & 0o7777;
            if let Ok(current) = holder.lstat(name)
                && current.mode & 0o7777 != recorded
            {
                // Best effort: the failure of the open is the one to report.
                let _ = holder.set_mode(name, recorded);
            }
        }
        opened
    }

    /// Refuses the directory `stat` at `name` of `holder` when it is on
    /// another device, is a mount root, or is locked by its owner.
    fn check_directory(
        &self,
        holder: &TreeRoot,
        name: &RelPath,
        stat: &TreeEntry,
    ) -> Result<(), FilesystemError> {
        if (self.device_of)(stat) != self.reference || (self.mount_root_of)(stat) {
            return Err(foreign_device(holder, name));
        }
        let foreign = (self.owner_of)(stat) != rustix::process::geteuid().as_raw();
        if foreign {
            let writable = holder.user_can_write(name).map_err(failure(
                "check the access to",
                holder,
                name,
            ))?;
            if !writable {
                return Err(locked(holder, name));
            }
        }
        Ok(())
    }
}

/// Puts the mode `frame` recorded back, if the scan changed it.
fn restore_mode(holder: &TreeRoot, frame: &Frame) -> Result<(), FilesystemError> {
    let recorded = frame.mode & 0o7777;
    let current = holder
        .lstat(&frame.name)
        .map_err(failure("stat", holder, &frame.name))?;
    if current.mode & 0o7777 != recorded {
        holder.set_mode(&frame.name, recorded).map_err(failure(
            "restore the mode of",
            holder,
            &frame.name,
        ))?;
    }
    Ok(())
}

/// A directory whose entries are being removed.
struct Frame {
    directory: TreeRoot,
    /// Where the directory lies in the handle of the frame above it, or in
    /// the tree's root for the first frame.
    name: RelPath,
    /// The mode the directory had, which a grant builds on.
    mode: u32,
    entries: std::vec::IntoIter<DirEntry>,
}

/// Opens the directory `name` of `parent` and lists it, giving it owner
/// access once where either is denied. `None` when it has vanished. The
/// kind and device of every entry are read later from its own `lstat`, as
/// a directory listing may leave the kind open.
///
/// A directory that could not be opened before it was given owner access
/// may have been a mount root whose flag the `lstat` could not read, as on
/// macOS for a directory that cannot be searched. The flag is read again
/// once the directory is open; a mount root gets its mode back and fails as
/// [`FilesystemError::ForeignDevice`], with nothing inside it listed.
fn open_frame(
    parent: &TreeRoot,
    name: &RelPath,
    mode: u32,
    mount_root_of: MountRootOf<'_>,
    list: List<'_>,
) -> Result<Option<Frame>, FilesystemError> {
    let granted_to_open = std::cell::Cell::new(false);
    let grant = &mut || grant_owner(parent, name, mode);
    let grant_to_open = &mut || {
        granted_to_open.set(true);
        grant_owner(parent, name, mode)
    };
    let opened = retry_once_with_access(grant_to_open, &mut || parent.sub(name));
    let opened = skip_if_missing(opened).map_err(failure("open", parent, name))?;
    let Some(directory) = opened else {
        return Ok(None);
    };
    if granted_to_open.get() {
        let itself = RelPath::root();
        let entry = directory
            .lstat(&itself)
            .map_err(failure("stat", &directory, &itself))?;
        if mount_root_of(&entry) {
            // Best effort: the refusal is the result worth reporting.
            let _ = parent.set_mode(name, mode & 0o7777);
            return Err(foreign_device(parent, name));
        }
    }
    // The directory is given access through the one that holds it: asking
    // the directory itself fails once it has lost its search permission.
    let entries = retry_once_with_access(grant, &mut || list(&directory)).map_err(failure(
        "list",
        &directory,
        &RelPath::root(),
    ))?;
    Ok(Some(Frame {
        directory,
        name: name.clone(),
        mode,
        entries: entries.into_iter(),
    }))
}

/// Gives the directory of `frames[index]` owner access through the
/// directory that holds it.
fn grant_frame(root: &TreeRoot, frames: &[Frame], index: usize) -> io::Result<()> {
    let holder = match index.checked_sub(1) {
        Some(above) => &frames[above].directory,
        None => root,
    };
    grant_owner(holder, &frames[index].name, frames[index].mode)
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

fn foreign_device(root: &TreeRoot, path: &RelPath) -> FilesystemError {
    FilesystemError::ForeignDevice {
        path: shown(root, path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::RestoreModes;
    use crate::filesystem::tree::TreeRoot;
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

    fn not_a_mount_root(_: &TreeEntry) -> bool {
        false
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
    fn a_directory_that_can_be_entered_but_not_listed_is_granted_read_and_listed_again() {
        let mut tree = Tree::new();
        tree.file("t/sub/f", "x");
        let sub = tree.path("t/sub");
        let _guard = RestoreModes(vec![sub.clone()]);
        fs::set_permissions(&sub, Permissions::from_mode(0o100)).expect("chmod");
        let listing = fs::read_dir(&sub).expect_err("the test needs a non-root user");
        assert_eq!(
            listing.kind(),
            io::ErrorKind::PermissionDenied,
            "a search-only directory has to refuse its listing"
        );
        remove_tree(&root_of(&tree), &path("t")).expect("removes");
        assert!(!tree.path("t").exists());
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
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &not_a_mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("stops");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "t/mnt"),
            "{error}"
        );
        assert!(tree.path("t/mnt/file").exists());
    }

    #[test]
    fn a_top_on_another_device_than_its_directory_is_not_entered() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        let device = |entry: &TreeEntry| if last_name(entry) == b"t" { 9 } else { 8 };
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &not_a_mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("refused");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "t"),
            "{error}"
        );
        assert!(tree.path("t/a").exists());
    }

    #[test]
    fn a_mount_root_in_the_tree_stops_the_removal_on_the_same_device() {
        let mut tree = Tree::new();
        tree.file("t/mnt/file", "x");
        tree.file("t/other", "y");
        let device = |entry: &TreeEntry| entry.device;
        let mount_root = |entry: &TreeEntry| last_name(entry) == b"mnt";
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("stops");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "t/mnt"),
            "{error}"
        );
        assert!(tree.path("t/mnt/file").exists());
    }

    #[test]
    fn a_mount_root_above_the_top_stops_the_removal_before_anything_is_removed() {
        let mut tree = Tree::new();
        tree.file("a/b/t/f", "x");
        let device = |entry: &TreeEntry| entry.device;
        let mount_root = |entry: &TreeEntry| entry.path == path("a");
        let error = remove_tree_with(
            &root_of(&tree),
            &path("a/b/t"),
            &device,
            &mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("refused");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "a"),
            "{error}"
        );
        assert!(tree.path("a/b/t/f").exists());
    }

    #[test]
    fn a_top_that_is_a_mount_root_is_not_entered() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        let device = |entry: &TreeEntry| entry.device;
        let mount_root = |entry: &TreeEntry| last_name(entry) == b"t";
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("refused");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "t"),
            "{error}"
        );
        assert!(tree.path("t/a").exists());
    }

    /// The directory could not be opened before it was given owner access,
    /// so its mount root flag could not be read. It is read again once it
    /// is open, and a mount root gets its mode back and is not entered.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_mount_root_found_after_the_grant_gets_its_mode_back_and_is_not_entered() {
        let mut tree = Tree::new();
        tree.file("t/locked/f", "x");
        let locked = tree.path("t/locked");
        let _guard = RestoreModes(vec![locked.clone()]);
        fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("chmod");
        fs::read_dir(&locked).expect_err("the test needs a non-root user");
        let asked = std::cell::Cell::new(0);
        let device = |entry: &TreeEntry| entry.device;
        let hidden_at_first = |entry: &TreeEntry| {
            if last_name(entry) != b"locked" {
                return false;
            }
            asked.set(asked.get() + 1);
            asked.get() > 1
        };
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &hidden_at_first,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("refused");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "t/locked"),
            "{error}"
        );
        assert_eq!(mode_of(&locked), 0o000);
        assert!(tree.path("t").exists());
        fs::set_permissions(&locked, Permissions::from_mode(0o755)).expect("chmod");
        assert!(locked.join("f").exists());
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
        remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &not_a_mount_root,
            &unknown,
            MAX_DEPTH,
        )
        .expect("removes");
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
        remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &not_a_mount_root,
            &ghost,
            MAX_DEPTH,
        )
        .expect("removes");
        assert!(!tree.path("t").exists());
    }

    /// Raises the soft limit of open files, which a tree this deep needs
    /// one of per level, and asserts that it is high enough. The default
    /// soft limit on macOS is 256.
    fn allow_many_open_files() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        const NEEDED: u64 = 2_048;
        let limit = getrlimit(Resource::Nofile);
        let wanted = limit.maximum.map_or(NEEDED, |maximum| maximum.min(NEEDED));
        if limit.current.is_none_or(|current| current < wanted) {
            let raised = Rlimit {
                current: Some(wanted),
                maximum: limit.maximum,
            };
            setrlimit(Resource::Nofile, raised).expect("the soft limit can be raised");
        }
        let now = getrlimit(Resource::Nofile).current;
        assert!(
            now.is_none_or(|now| now >= NEEDED),
            "the test needs {NEEDED} open files, the limit is {now:?}"
        );
    }

    /// `t/d/d/.../d/f` with `levels` directories named `d` below `t`.
    /// Built level by level through the directory just made, since the
    /// whole path is far past the path limit.
    fn chain(root: &TreeRoot, levels: usize) {
        let d = Name::from_bytes(b"d".to_vec()).expect("name");
        let d = RelPath::root().join(&d);
        root.create_dir(&path("t"), 0o755).expect("created");
        let mut deepest = root.sub(&path("t")).expect("opens");
        for _ in 0..levels {
            deepest.create_dir(&d, 0o755).expect("created");
            deepest = deepest.sub(&d).expect("opens");
        }
        deepest.create_file(&path("f"), 0o644).expect("created");
    }

    /// `t` followed by `levels` times `/d`.
    fn chain_path(levels: usize) -> RelPath {
        path(&format!("t{}", "/d".repeat(levels)))
    }

    #[test]
    fn a_tree_512_levels_deep_is_removed() {
        allow_many_open_files();
        let tree = Tree::new();
        let root = root_of(&tree);
        chain(&root, 512);
        remove_tree(&root, &path("t")).expect("removes");
        assert!(root.read_dir(&RelPath::root()).expect("listed").is_empty());
    }

    #[test]
    fn a_tree_600_levels_deep_is_refused_at_the_first_directory_past_the_cap() {
        allow_many_open_files();
        let tree = Tree::new();
        let root = root_of(&tree);
        chain(&root, 600);
        let error = remove_tree(&root, &path("t")).expect_err("too deep");
        let first_past = chain_path(513).to_string();
        assert!(
            matches!(&error, FilesystemError::TooDeep { path, limit } if *path == first_past && *limit == 512),
            "{error}"
        );
        assert!(
            root.lstat(&chain_path(600)).is_ok(),
            "the deepest directory is gone"
        );
    }

    #[test]
    fn a_cap_below_the_default_refuses_the_first_directory_past_it() {
        let tree = Tree::new();
        let root = root_of(&tree);
        chain(&root, 4);
        let device = |entry: &TreeEntry| entry.device;
        let error = remove_tree_with(&root, &path("t"), &device, &not_a_mount_root, &real_list, 2)
            .expect_err("deep");
        let first_past = chain_path(3).to_string();
        assert!(
            matches!(&error, FilesystemError::TooDeep { path, limit } if *path == first_past && *limit == 2),
            "{error}"
        );
        assert!(
            root.lstat(&chain_path(4)).is_ok(),
            "the deepest directory is gone"
        );
    }

    #[test]
    fn a_locked_directory_six_levels_down_is_granted_through_its_parent() {
        let mut tree = Tree::new();
        tree.file("t/a/b/c/d/e/locked/f", "x");
        let locked = tree.path("t/a/b/c/d/e/locked");
        let _guard = RestoreModes(vec![locked.clone()]);
        fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("chmod");
        locked_out(&locked);
        remove_tree(&root_of(&tree), &path("t")).expect("removes");
        assert!(!tree.path("t").exists());
    }

    #[test]
    fn the_root_of_the_tree_is_refused_and_everything_in_it_stays() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        tree.file("b", "y");
        let error = remove_tree(&root_of(&tree), &RelPath::root()).expect_err("refused");
        assert!(
            matches!(
                error,
                FilesystemError::Io {
                    action: "remove",
                    ..
                }
            ),
            "{error}"
        );
        assert!(tree.path("t/a").exists(), "the tree was emptied");
        assert!(tree.path("b").exists(), "the tree was emptied");
    }

    #[test]
    fn a_directory_that_cannot_be_listed_fails_the_removal_naming_it() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        let broken = |_: &TreeRoot| Err(io::Error::from(io::ErrorKind::Other));
        let device = |entry: &TreeEntry| entry.device;
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &not_a_mount_root,
            &broken,
            MAX_DEPTH,
        )
        .expect_err("fails");
        assert!(
            matches!(&error, FilesystemError::Io { action: "list", path, .. } if path == "t"),
            "{error}"
        );
        assert!(tree.path("t/a").exists());
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
    fn a_top_in_a_directory_the_user_cannot_write_is_refused_and_nothing_changes() {
        let mut tree = Tree::new();
        tree.file("p/t/f", "x");
        let parent = tree.path("p");
        let root = TreeRoot::open(&parent).expect("opens");
        let _guard = RestoreModes(vec![parent.clone()]);
        fs::set_permissions(&parent, Permissions::from_mode(0o555)).expect("chmod");
        locked_out(&parent);
        let error = remove_tree(&root, &path("t")).expect_err("fails");
        assert_locked(error, ".");
        assert!(parent.join("t/f").exists());
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

    /// Every path under `tree` with its mode, in a stable order.
    fn snapshot(tree: &Tree) -> Vec<(String, u32)> {
        fn visit(dir: &Path, prefix: &str, out: &mut Vec<(String, u32)>) {
            let mut names: Vec<_> = match fs::read_dir(dir) {
                Ok(listing) => listing
                    .map(|entry| entry.expect("entry").file_name())
                    .collect(),
                Err(_) => return,
            };
            names.sort();
            for name in names {
                let full = dir.join(&name);
                let shown = format!("{prefix}/{}", name.to_string_lossy());
                out.push((shown.clone(), mode_of(&full)));
                if fs::symlink_metadata(&full).expect("exists").is_dir() {
                    visit(&full, &shown, out);
                }
            }
        }
        let mut out = Vec::new();
        visit(tree.root(), "", &mut out);
        out
    }

    fn assert_locked(error: FilesystemError, expected: &str) {
        assert!(
            matches!(&error, FilesystemError::Locked { path } if path == expected),
            "{error}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_locked_file_deep_in_the_tree_refuses_the_removal_before_anything_goes() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        tree.file("t/d/e/f", "x");
        tree.file("t/z", "x");
        tree.file("t/closed/g", "x");
        tree.immutable("t/d/e/f");
        let closed = tree.path("t/closed");
        let _guard = RestoreModes(vec![closed.clone()]);
        fs::set_permissions(&closed, Permissions::from_mode(0o000)).expect("chmod");
        let before = snapshot(&tree);

        let error = remove_tree(&root_of(&tree), &path("t")).expect_err("refused");

        assert_locked(error, "t/d/e/f");
        assert_eq!(snapshot(&tree), before);
        assert_eq!(mode_of(&closed), 0o000, "the granted access is taken back");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_locked_file_below_a_closed_directory_is_found_and_the_mode_comes_back() {
        let mut tree = Tree::new();
        tree.file("t/closed/g", "x");
        tree.immutable("t/closed/g");
        let closed = tree.path("t/closed");
        let _guard = RestoreModes(vec![closed.clone()]);
        fs::set_permissions(&closed, Permissions::from_mode(0o000)).expect("chmod");

        let error = remove_tree(&root_of(&tree), &path("t")).expect_err("refused");

        assert_locked(error, "t/closed/g");
        assert_eq!(mode_of(&closed), 0o000);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_locked_directory_the_top_and_a_lone_locked_file_are_refused() {
        let mut tree = Tree::new();
        tree.file("t/dir/f", "x");
        tree.file("top/f", "x");
        tree.file("lone", "x");
        tree.immutable("t/dir");
        tree.immutable("top");
        tree.immutable("lone");
        let root = root_of(&tree);
        assert_locked(
            remove_tree(&root, &path("t")).expect_err("refused"),
            "t/dir",
        );
        assert_locked(
            remove_tree(&root, &path("top")).expect_err("refused"),
            "top",
        );
        assert_locked(
            remove_tree(&root, &path("lone")).expect_err("refused"),
            "lone",
        );
        assert!(tree.path("t/dir/f").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_locked_holder_of_the_top_refuses_the_removal() {
        let mut tree = Tree::new();
        tree.file("area/t/f", "x");
        tree.immutable("area");
        let error = remove_tree(&root_of(&tree), &path("area/t")).expect_err("refused");
        assert_locked(error, "area");
        assert!(tree.path("area/t/f").exists());
    }

    #[test]
    fn a_holder_the_user_cannot_write_refuses_the_removal() {
        let mut tree = Tree::new();
        tree.file("area/t/f", "x");
        let area = tree.path("area");
        let _guard = RestoreModes(vec![area.clone()]);
        fs::set_permissions(&area, Permissions::from_mode(0o555)).expect("chmod");
        locked_out(&area);
        let error = remove_tree(&root_of(&tree), &path("area/t")).expect_err("refused");
        assert_locked(error, "area");
        assert!(tree.path("area/t/f").exists());
    }

    #[test]
    fn a_directory_someone_else_owns_and_the_user_cannot_write_refuses_the_removal() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        tree.file("t/theirs/f", "x");
        let theirs = tree.path("t/theirs");
        let _guard = RestoreModes(vec![theirs.clone()]);
        fs::set_permissions(&theirs, Permissions::from_mode(0o555)).expect("chmod");
        locked_out(&theirs);
        let before = snapshot(&tree);
        let foreign = |entry: &TreeEntry| {
            if last_name(entry) == b"theirs" {
                entry.uid + 1
            } else {
                entry.uid
            }
        };
        let error = remove_tree_owned(
            &root_of(&tree),
            &path("t"),
            &|entry| entry.device,
            &not_a_mount_root,
            &real_list,
            MAX_DEPTH,
            &foreign,
        )
        .expect_err("refused");
        assert_locked(error, "t/theirs");
        assert_eq!(snapshot(&tree), before);
    }

    #[test]
    fn a_foreign_device_below_earlier_siblings_leaves_them_in_place() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        tree.file("t/mnt/file", "x");
        let device = |entry: &TreeEntry| if last_name(entry) == b"mnt" { 9 } else { 8 };
        let before = snapshot(&tree);
        remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &not_a_mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("stops");
        assert_eq!(snapshot(&tree), before);
    }

    /// Answers `before` for the first `scan_calls` questions about the
    /// entry named `name` and `after` from then on: the scan asks first,
    /// the removal pass second.
    fn flips<T: Copy>(
        name: &'static [u8],
        before: T,
        after: T,
        otherwise: T,
    ) -> impl Fn(&TreeEntry) -> T {
        let asked = std::cell::Cell::new(0);
        move |entry: &TreeEntry| {
            if last_name(entry) != name {
                return otherwise;
            }
            asked.set(asked.get() + 1);
            if asked.get() == 1 { before } else { after }
        }
    }

    #[test]
    fn the_removal_pass_still_stops_at_a_directory_that_moved_to_another_device() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        tree.file("t/mnt/file", "x");
        let device = flips(b"mnt", 8u64, 9, 8);
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &device,
            &not_a_mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("stops");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "t/mnt"),
            "{error}"
        );
        assert!(!tree.path("t/a").exists(), "the earlier sibling went");
        assert!(tree.path("t/mnt/file").exists());
    }

    #[test]
    fn the_removal_pass_still_stops_at_a_directory_that_became_a_mount_root() {
        let mut tree = Tree::new();
        tree.file("t/a", "x");
        tree.file("t/mnt/file", "x");
        let mount_root = flips(b"mnt", false, true, false);
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &|entry| entry.device,
            &mount_root,
            &real_list,
            MAX_DEPTH,
        )
        .expect_err("stops");
        assert!(
            matches!(&error, FilesystemError::ForeignDevice { path } if path == "t/mnt"),
            "{error}"
        );
        assert!(tree.path("t/mnt/file").exists());
    }

    #[test]
    fn the_removal_pass_still_stops_at_a_directory_past_the_depth_cap() {
        let mut tree = Tree::new();
        tree.file("t/a/b/f", "x");
        // The scan finds `a` empty, so it sees nothing too deep. The
        // removal pass lists `a` again and meets `b`.
        let listed = std::cell::Cell::new(0);
        let list = |dir: &TreeRoot| {
            let mut entries = real_list(dir)?;
            if dir.prefix().components().last() == Some(&b"a"[..]) {
                listed.set(listed.get() + 1);
                if listed.get() == 1 {
                    entries.retain(|entry| entry.name.as_bytes() != b"b");
                }
            }
            Ok(entries)
        };
        let error = remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &|entry| entry.device,
            &not_a_mount_root,
            &list,
            1,
        )
        .expect_err("stops");
        assert!(
            matches!(&error, FilesystemError::TooDeep { path, limit: 1 } if path == "t/a/b"),
            "{error}"
        );
        assert!(tree.path("t/a/b/f").exists());
    }

    #[test]
    fn a_scan_that_fails_after_a_grant_puts_the_mode_back() {
        let mut tree = Tree::new();
        tree.file("t/closed/f", "x");
        let closed = tree.path("t/closed");
        let _guard = RestoreModes(vec![closed.clone()]);
        fs::set_permissions(&closed, Permissions::from_mode(0o000)).expect("chmod");
        let list = |dir: &TreeRoot| {
            if dir.prefix().components().last() == Some(&b"closed"[..]) {
                return Err(io::Error::from_raw_os_error(Errno::IO.raw_os_error()));
            }
            real_list(dir)
        };
        remove_tree_with(
            &root_of(&tree),
            &path("t"),
            &|entry| entry.device,
            &not_a_mount_root,
            &list,
            MAX_DEPTH,
        )
        .expect_err("the listing fails");
        assert_eq!(mode_of(&closed), 0o000);
    }
}
