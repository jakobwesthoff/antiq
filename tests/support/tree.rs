// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Directory trees for tests, in a temp directory that removes itself.
//!
//! This file is shared: the binary tests include it through
//! `support::tree`, the library tests through `testing::tree`. It uses
//! only the standard library and the dev dependencies.

use filetime::FileTime;
use std::fs::{self, File, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

/// A tree of files in a temp directory. Paths are relative to its root.
pub struct Tree {
    root: TempDir,
    unreadable: Vec<PathBuf>,
}

impl Tree {
    pub fn new() -> Tree {
        Tree {
            root: tempfile::tempdir().expect("the temp dir is writable"),
            unreadable: Vec::new(),
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    /// A directory, with the directories above it.
    pub fn dir(&mut self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).expect("the directory can be created");
        path
    }

    /// A file with `content`, with the directories above it.
    pub fn file(&mut self, relative: &str, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("a file has a parent"))
            .expect("the parent can be created");
        fs::write(&path, content).expect("the file can be written");
        path
    }

    /// A file with the given permission bits and modification time in
    /// whole seconds since the epoch.
    pub fn file_with(
        &mut self,
        relative: &str,
        content: impl AsRef<[u8]>,
        mode: u32,
        mtime_seconds: i64,
    ) -> PathBuf {
        let path = self.file(relative, content);
        fs::set_permissions(&path, Permissions::from_mode(mode)).expect("the mode can be set");
        filetime::set_file_mtime(&path, FileTime::from_unix_time(mtime_seconds, 0))
            .expect("the time can be set");
        path
    }

    /// A symbolic link holding `target` as given: absolute, relative,
    /// dangling or pointing outside the tree.
    pub fn symlink(&mut self, relative: &str, target: impl AsRef<Path>) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("a link has a parent"))
            .expect("the parent can be created");
        symlink(target, &path).expect("the link can be created");
        path
    }

    /// A second name for the file at `existing`.
    pub fn hard_link(&mut self, existing: &str, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("a link has a parent"))
            .expect("the parent can be created");
        fs::hard_link(self.path(existing), &path).expect("the link can be created");
        path
    }

    pub fn fifo(&mut self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("a fifo has a parent"))
            .expect("the parent can be created");
        let status = Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("the mkfifo executable exists");
        assert!(status.success(), "mkfifo failed for {}", path.display());
        path
    }

    /// A file of `size` bytes that is one hole.
    pub fn sparse(&mut self, relative: &str, size: u64) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("a file has a parent"))
            .expect("the parent can be created");
        File::create(&path)
            .and_then(|file| file.set_len(size))
            .expect("the file can be made");
        path
    }

    /// A directory with mode 000. Its permissions are restored when the
    /// tree is dropped, so the temp directory can be removed.
    pub fn unreadable_dir(&mut self, relative: &str) -> PathBuf {
        let path = self.dir(relative);
        fs::set_permissions(&path, Permissions::from_mode(0o000)).expect("the mode can be set");
        self.unreadable.push(path.clone());
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        for path in &self.unreadable {
            // Best effort: the directory may be gone already.
            let _ = fs::set_permissions(path, Permissions::from_mode(0o755));
        }
    }
}

/// The ULID the minimal collection carries.
pub const COLLECTION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// A directory marked as a collection by the smallest `.antiq/` the
/// library can find: the config with the collection's ID and the empty
/// directories of the local layout.
pub struct TempCollection {
    tree: Tree,
}

impl TempCollection {
    pub fn minimal() -> TempCollection {
        let mut tree = Tree::new();
        for directory in ["tmp", "trash", "doomed", "recovered", "pieces"] {
            tree.dir(&format!(".antiq/{directory}"));
        }
        tree.file(
            ".antiq/config.toml",
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n"),
        );
        TempCollection { tree }
    }

    pub fn root(&self) -> &Path {
        self.tree.root()
    }

    pub fn tree(&mut self) -> &mut Tree {
        &mut self.tree
    }
}

/// Seconds since the epoch, `days` before now.
pub fn days_ago(days: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64;
    now - days * 86_400
}
