// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Directory trees for tests, in a temp directory that removes itself.
//!
//! This file is shared: the binary tests include it through
//! `support::tree`, the library tests through `testing::tree`. It uses
//! only the standard library and the dev dependencies.

use filetime::FileTime;
use std::collections::BTreeMap;
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
    immutable: Vec<PathBuf>,
}

impl Tree {
    pub fn new() -> Tree {
        Tree {
            root: tempfile::tempdir().expect("the temp dir is writable"),
            unreadable: Vec::new(),
            immutable: Vec::new(),
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

    /// Sets the user immutable flag on the file or directory at
    /// `relative`, and checks that it took. The flag is cleared when the
    /// tree is dropped, also when a test panics, so the temp directory can
    /// be removed.
    #[cfg(target_os = "macos")]
    pub fn immutable(&mut self, relative: &str) -> PathBuf {
        use std::os::macos::fs::MetadataExt;
        const UF_IMMUTABLE: u32 = 0x2;
        let path = self.path(relative);
        let status = Command::new("chflags")
            .arg("uchg")
            .arg(&path)
            .status()
            .expect("the chflags executable exists");
        assert!(status.success(), "chflags failed for {}", path.display());
        self.immutable.push(path.clone());
        let flags = fs::symlink_metadata(&path)
            .expect("the entry can be read")
            .st_flags();
        assert!(flags & UF_IMMUTABLE != 0, "the flag did not take");
        path
    }

    /// Records that the immutable entry is about to be moved to `relative`,
    /// so the flag is cleared there when the tree is dropped, also after a
    /// panic.
    #[cfg(target_os = "macos")]
    pub fn immutable_moves_to(&mut self, relative: &str) {
        self.immutable.push(self.path(relative));
    }

    /// A unix socket bound at `relative`.
    pub fn socket(&mut self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().expect("a socket has a parent"))
            .expect("the parent can be created");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("the socket binds");
        // The socket file stays after the listener closes.
        drop(listener);
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        for path in &self.immutable {
            // Best effort: the entry may be gone already.
            let _ = Command::new("chflags").arg("nouchg").arg(path).status();
        }
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
        tree.file(".antiq/README.md", antiq::collection::README);
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

/// What a [`snapshot`] records of one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// `dir`, `file`, `symlink` or `other`.
    pub kind: &'static str,
    /// The permission bits.
    pub mode: u32,
    /// The content of a file, the target of a symlink.
    pub detail: Option<Vec<u8>>,
}

/// Every path below `root` with its kind, mode and content, taken with
/// `lstat` so that a symlink is recorded and never followed. A directory
/// that cannot be read is recorded by its mode alone.
pub fn snapshot(root: &Path) -> BTreeMap<String, Seen> {
    fn walk(base: &Path, dir: &Path, seen: &mut BTreeMap<String, Seen>) {
        let Ok(items) = fs::read_dir(dir) else { return };
        for item in items {
            let path = item.expect("an item").path();
            let name = path
                .strip_prefix(base)
                .expect("below")
                .display()
                .to_string();
            let meta = fs::symlink_metadata(&path).expect("the entry exists");
            let mode = meta.permissions().mode() & 0o7777;
            let kind = meta.file_type();
            if kind.is_dir() {
                seen.insert(
                    name,
                    Seen {
                        kind: "dir",
                        mode,
                        detail: None,
                    },
                );
                walk(base, &path, seen);
            } else if kind.is_symlink() {
                let target = fs::read_link(&path).expect("reads the link");
                seen.insert(
                    name,
                    Seen {
                        kind: "symlink",
                        mode,
                        detail: Some(target.as_os_str().as_encoded_bytes().to_vec()),
                    },
                );
            } else if kind.is_file() {
                seen.insert(
                    name,
                    Seen {
                        kind: "file",
                        mode,
                        detail: Some(fs::read(&path).expect("reads")),
                    },
                );
            } else {
                seen.insert(
                    name,
                    Seen {
                        kind: "other",
                        mode,
                        detail: None,
                    },
                );
            }
        }
    }
    let mut seen = BTreeMap::new();
    walk(root, root, &mut seen);
    seen
}
