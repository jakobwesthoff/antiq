// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Moving directories.
//!
//! Callers hold the run lock; see the module docs of [`crate::fsx`].

// TODO: `archive` (I19), `unearth` (I20) and the start-of-run repairs
// (I17) are the callers; until they land the library has none.
#![allow(dead_code)]

use super::tree::TreeRoot;
use super::{FsxError, failure, shown};
use crate::relpath::RelPath;
use crate::tree::EntryKind;
use std::io;

/// The outcome of a move that happened.
#[derive(Debug)]
#[must_use = "a failed restore of the mode is reported here"]
pub(crate) struct Moved {
    /// Set when the directory arrived but its permissions could not be put
    /// back as they were. The move itself stands.
    pub mode_restore_error: Option<FsxError>,
}

type Rename<'a> = &'a dyn Fn(&TreeRoot, &RelPath, &TreeRoot, &RelPath) -> io::Result<()>;
type Restore<'a> = &'a dyn Fn(&TreeRoot, &RelPath, u32) -> io::Result<()>;

/// Moves the directory `from` of `from_root` to `to` in `to_root`, each
/// relative to its own root. Never replaces an existing target.
///
/// Renaming a directory to another parent needs write permission on the
/// directory itself, so owner write is added first and the recorded mode
/// restored right after, whether the rename worked or not. Only the two
/// handles are used, so a directory deeper than the path limit moves like
/// any other. The root path of either handle is refused as an
/// `InvalidInput` failure before any mode changes.
pub(crate) fn move_dir(
    from_root: &TreeRoot,
    from: &RelPath,
    to_root: &TreeRoot,
    to: &RelPath,
) -> Result<Moved, FsxError> {
    move_dir_with(
        from_root,
        from,
        to_root,
        to,
        &|from_root, from, to_root, to| from_root.rename(from, to_root, to),
        &|root, path, mode| root.set_mode(path, mode),
    )
}

fn move_dir_with(
    from_root: &TreeRoot,
    from: &RelPath,
    to_root: &TreeRoot,
    to: &RelPath,
    rename: Rename<'_>,
    restore: Restore<'_>,
) -> Result<Moved, FsxError> {
    // A root path has no directory above it to be renamed in or out of;
    // refusing before the mode is touched leaves nothing to restore.
    for (root, path) in [(from_root, from), (to_root, to)] {
        if path.is_root() {
            let source = io::Error::new(
                io::ErrorKind::InvalidInput,
                "it is the root of the tree itself",
            );
            return Err(failure("move", root, path)(source));
        }
    }
    let source = from_root
        .lstat(from)
        .map_err(failure("stat", from_root, from))?;
    if source.kind != EntryKind::Directory {
        let source = io::Error::new(io::ErrorKind::InvalidInput, "it is not a directory");
        return Err(failure("move", from_root, from)(source));
    }
    let recorded = source.mode & 0o7777;
    let writable = recorded | 0o200;
    let changed = writable != recorded;
    if changed {
        from_root
            .set_mode(from, writable)
            .map_err(failure("make writable", from_root, from))?;
    }

    if let Err(error) = rename(from_root, from, to_root, to) {
        if changed {
            // The rename's failure is the one worth reporting; the
            // directory is still where it was and nothing else can be
            // done for its mode.
            let _ = restore(from_root, from, recorded);
        }
        return Err(if error.kind() == io::ErrorKind::CrossesDevices {
            FsxError::CrossesDevices {
                from: shown(from_root, from),
                to: shown(to_root, to),
            }
        } else {
            failure("move", from_root, from)(error)
        });
    }

    let mode_restore_error = if changed {
        restore(to_root, to, recorded)
            .err()
            .map(failure("restore the mode of", to_root, to))
    } else {
        None
    };
    Ok(Moved { mode_restore_error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::RestoreModes;
    use crate::relpath::Name;
    use crate::testing::deep_path;
    use crate::testing::tree::Tree;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn root_of(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("the root opens")
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, Permissions::from_mode(mode)).expect("chmod");
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path)
            .expect("exists")
            .permissions()
            .mode()
            & 0o777
    }

    /// The precondition of the tests below: a directory without owner
    /// write cannot be renamed to another parent, which is why `move_dir`
    /// adds it.
    fn assert_plain_rename_is_denied(tree: &mut Tree) {
        tree.dir("probe/from/dir");
        tree.dir("probe/to");
        let dir = tree.path("probe/from/dir");
        let _guard = RestoreModes(vec![dir.clone()]);
        chmod(&dir, 0o555);
        let error = fs::rename(&dir, tree.path("probe/to/dir")).expect_err(
            "the test needs a non-root user on a filesystem that checks write on a moved directory",
        );
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_read_only_directory_moves_with_its_content_and_keeps_its_mode() {
        let mut tree = Tree::new();
        assert_plain_rename_is_denied(&mut tree);
        tree.file("a/ro/inner/f", "x");
        tree.dir("b");
        let _guard = RestoreModes(vec![tree.path("a/ro"), tree.path("b/ro")]);
        chmod(&tree.path("a/ro"), 0o555);
        let root = root_of(&tree);
        let moved = move_dir(&root, &path("a/ro"), &root, &path("b/ro")).expect("moves");
        assert!(moved.mode_restore_error.is_none());
        assert!(!tree.path("a/ro").exists());
        assert!(tree.path("b/ro/inner/f").exists());
        assert_eq!(mode_of(&tree.path("b/ro")), 0o555);
    }

    #[test]
    fn a_directory_without_any_permission_moves_and_gets_its_mode_back() {
        let mut tree = Tree::new();
        tree.file("a/locked/f", "x");
        tree.dir("b");
        let _guard = RestoreModes(vec![tree.path("a/locked"), tree.path("b/locked")]);
        chmod(&tree.path("a/locked"), 0o000);
        let root = root_of(&tree);
        let moved = move_dir(&root, &path("a/locked"), &root, &path("b/locked")).expect("moves");
        assert!(moved.mode_restore_error.is_none());
        assert!(!tree.path("a/locked").exists());
        assert_eq!(mode_of(&tree.path("b/locked")), 0o000);
    }

    #[test]
    fn a_directory_that_the_owner_can_write_keeps_its_mode() {
        let mut tree = Tree::new();
        tree.file("a/d/f", "x");
        tree.dir("b");
        chmod(&tree.path("a/d"), 0o750);
        let root = root_of(&tree);
        let moved = move_dir(&root, &path("a/d"), &root, &path("b/d")).expect("moves");
        assert!(moved.mode_restore_error.is_none());
        assert_eq!(mode_of(&tree.path("b/d")), 0o750);
    }

    #[test]
    fn a_move_into_a_missing_directory_fails_and_the_source_keeps_its_mode() {
        let mut tree = Tree::new();
        tree.file("a/ro/f", "x");
        let _guard = RestoreModes(vec![tree.path("a/ro")]);
        chmod(&tree.path("a/ro"), 0o555);
        let root = root_of(&tree);
        let error = move_dir(&root, &path("a/ro"), &root, &path("nowhere/ro")).expect_err("fails");
        assert!(
            matches!(error, FsxError::Io { action: "move", .. }),
            "{error}"
        );
        assert!(error.to_string().contains("a/ro"), "{error}");
        assert_eq!(mode_of(&tree.path("a/ro")), 0o555);
        assert!(tree.path("a/ro/f").exists());
    }

    #[test]
    fn an_existing_target_is_refused_empty_or_not_and_left_as_it_was() {
        for content in [None, Some("y")] {
            let mut tree = Tree::new();
            tree.file("a/ro/f", "x");
            tree.dir("b/taken");
            if let Some(content) = content {
                tree.file("b/taken/g", content);
            }
            let _guard = RestoreModes(vec![tree.path("a/ro"), tree.path("b/taken")]);
            chmod(&tree.path("a/ro"), 0o555);
            chmod(&tree.path("b/taken"), 0o711);
            let root = root_of(&tree);
            let error =
                move_dir(&root, &path("a/ro"), &root, &path("b/taken")).expect_err("refused");
            assert!(
                matches!(error, FsxError::Io { action: "move", .. }),
                "{error}"
            );
            assert_eq!(mode_of(&tree.path("a/ro")), 0o555);
            assert_eq!(mode_of(&tree.path("b/taken")), 0o711);
            assert!(tree.path("a/ro/f").exists());
        }
    }

    #[test]
    fn a_rename_across_devices_is_reported_as_such_and_the_mode_is_restored() {
        let mut tree = Tree::new();
        tree.file("a/ro/f", "x");
        tree.dir("b");
        let _guard = RestoreModes(vec![tree.path("a/ro")]);
        chmod(&tree.path("a/ro"), 0o555);
        let root = root_of(&tree);
        let across = |_: &TreeRoot, _: &RelPath, _: &TreeRoot, _: &RelPath| {
            Err(io::Error::from(io::ErrorKind::CrossesDevices))
        };
        let real = |root: &TreeRoot, path: &RelPath, mode: u32| root.set_mode(path, mode);
        let error = move_dir_with(&root, &path("a/ro"), &root, &path("b/ro"), &across, &real)
            .expect_err("fails");
        assert!(
            matches!(&error, FsxError::CrossesDevices { from, to } if from == "a/ro" && to == "b/ro"),
            "{error}"
        );
        assert_eq!(mode_of(&tree.path("a/ro")), 0o555);
    }

    #[test]
    fn a_mode_that_cannot_be_restored_after_the_move_is_reported_but_the_move_stands() {
        let mut tree = Tree::new();
        tree.file("a/ro/f", "x");
        tree.dir("b");
        let _guard = RestoreModes(vec![tree.path("b/ro")]);
        chmod(&tree.path("a/ro"), 0o555);
        let root = root_of(&tree);
        let real = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            from_root.rename(from, to_root, to)
        };
        let failing =
            |_: &TreeRoot, _: &RelPath, _: u32| Err(io::Error::from(io::ErrorKind::Other));
        let moved = move_dir_with(&root, &path("a/ro"), &root, &path("b/ro"), &real, &failing)
            .expect("the move happened");
        let warning = moved.mode_restore_error.expect("the failure is reported");
        assert!(warning.to_string().contains("b/ro"), "{warning}");
        assert!(tree.path("b/ro/f").exists());
    }

    #[test]
    fn a_symlink_a_file_and_a_missing_entry_are_not_moved() {
        let mut tree = Tree::new();
        tree.file("outside/keep", "x");
        tree.symlink("l", tree.path("outside"));
        tree.file("f", "x");
        tree.dir("b");
        let root = root_of(&tree);
        for source in ["l", "f", "missing"] {
            let error = move_dir(&root, &path(source), &root, &path("b/x")).expect_err("refused");
            assert!(matches!(error, FsxError::Io { .. }), "{source}: {error}");
        }
        assert!(tree.path("outside/keep").exists());
        assert!(!tree.path("b/x").exists());
    }

    /// Runs `move_dir_with` with the real rename and a restore that counts
    /// its calls: the restore runs exactly when the move had changed the
    /// mode of the source, so a refusal before any change leaves it at 0.
    fn restores_during(
        from_root: &TreeRoot,
        from: &RelPath,
        to_root: &TreeRoot,
        to: &RelPath,
    ) -> (Result<Moved, FsxError>, usize) {
        let restores = std::cell::Cell::new(0);
        let real_rename =
            |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
                from_root.rename(from, to_root, to)
            };
        let counting = |root: &TreeRoot, path: &RelPath, mode: u32| {
            restores.set(restores.get() + 1);
            root.set_mode(path, mode)
        };
        let result = move_dir_with(from_root, from, to_root, to, &real_rename, &counting);
        (result, restores.get())
    }

    #[test]
    fn the_root_of_the_tree_is_not_moved_and_its_mode_is_not_touched() {
        let mut tree = Tree::new();
        tree.dir("r");
        tree.dir("b");
        let _guard = RestoreModes(vec![tree.path("r")]);
        chmod(&tree.path("r"), 0o555);
        let from_root = TreeRoot::open(&tree.path("r")).expect("opens");
        let to_root = TreeRoot::open(&tree.path("b")).expect("opens");
        let (result, restores) =
            restores_during(&from_root, &RelPath::root(), &to_root, &path("moved"));
        let error = result.expect_err("refused");
        assert!(
            matches!(error, FsxError::Io { action: "move", .. }),
            "{error}"
        );
        assert_eq!(restores, 0, "the mode was changed before the refusal");
        assert_eq!(mode_of(&tree.path("r")), 0o555);
    }

    #[test]
    fn the_root_of_the_target_tree_is_not_a_target_and_the_source_mode_is_not_touched() {
        let mut tree = Tree::new();
        tree.file("a/ro/f", "x");
        let _guard = RestoreModes(vec![tree.path("a/ro")]);
        chmod(&tree.path("a/ro"), 0o555);
        let root = root_of(&tree);
        let (result, restores) = restores_during(&root, &path("a/ro"), &root, &RelPath::root());
        let error = result.expect_err("refused");
        assert!(
            matches!(error, FsxError::Io { action: "move", .. }),
            "{error}"
        );
        assert_eq!(restores, 0, "the mode was changed before the refusal");
        assert_eq!(mode_of(&tree.path("a/ro")), 0o555);
    }

    #[test]
    fn a_directory_deeper_than_the_path_limit_moves_into_a_short_path() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let mut so_far = RelPath::root();
        for component in deep_path(25).components() {
            let name = Name::from_bytes(component.to_vec()).expect("name");
            so_far = so_far.join(&name);
            root.create_dir(&so_far, 0o755).expect("created");
        }
        let piece = so_far.join(&Name::from_bytes(b"piece".to_vec()).expect("name"));
        root.create_dir(&piece, 0o755).expect("created");
        root.create_dir(&path("trash"), 0o755).expect("created");

        let moved = move_dir(&root, &piece, &root, &path("trash/piece")).expect("moves");
        assert!(moved.mode_restore_error.is_none());
        assert!(root.lstat(&path("trash/piece")).is_ok());
        assert!(root.lstat(&piece).is_err());
    }
}
