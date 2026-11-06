// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The points where a test replaces what the real run does, so it can
//! fill a disk, change a folder between two steps or record the order of
//! the flushes.

use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::{Durable, RealDurable};
use crate::filesystem::moves::{Moved, move_dir};
use crate::filesystem::remove::remove_tree;
use crate::filesystem::space::available_space;
use crate::filesystem::tree::TreeRoot;
use crate::relpath::RelPath;
use crate::walk::{Hooks, RealFs};
use std::io::Write;
use std::path::Path;

pub(crate) trait Seams: Send + Sync {
    /// How files and directories are made durable.
    fn durable(&self) -> &dyn Durable {
        &RealDurable
    }

    /// The bytes free on the volume holding `dir`.
    fn free(&self, dir: &Path) -> Result<u64, FilesystemError> {
        available_space(dir)
    }

    /// What the archive file is written through.
    fn pack_sink<'a>(&self, sink: Box<dyn Write + 'a>) -> Box<dyn Write + 'a> {
        sink
    }

    /// Runs after the files of the version are written and flushed, before
    /// the archive is read back for its hash. `staging` is the directory that holds them.
    fn before_read_back(&self, _staging: &Path) {}

    /// Runs after the hash of the archive was read back, before the
    /// folder is compared with the archive.
    fn before_verify(&self, _staging: &Path) {}

    /// Runs after the folder was renamed into `trash/`, before it is
    /// walked again. `tree` is its path there.
    fn after_trash_rename(&self, _tree: &Path) {}

    /// What the walk of the trashed tree asks about the disk.
    fn walk_hooks(&self) -> &dyn Hooks {
        &RealFs
    }

    /// What the walk that looks for files shared between pieces asks about
    /// the disk. It is apart from `walk_hooks` so that a fault meant for the
    /// walks of the steps is not spent on it.
    fn link_hooks(&self) -> &dyn Hooks {
        &RealFs
    }

    /// Moves the folder `from` of `from_root` to `to` in `to_root`.
    fn move_dir(
        &self,
        from_root: &TreeRoot,
        from: &RelPath,
        to_root: &TreeRoot,
        to: &RelPath,
    ) -> Result<Moved, FilesystemError> {
        move_dir(from_root, from, to_root, to)
    }

    /// Deletes the tree at `path` below `root`.
    fn remove(&self, root: &TreeRoot, path: &RelPath) -> Result<(), FilesystemError> {
        remove_tree(root, path)
    }
}

/// The real run.
pub(crate) struct RealSeams;

impl Seams for RealSeams {}
