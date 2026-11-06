// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filesystem primitives.
//!
//! The functions here sit below the locking rule: whoever writes into
//! `.antiq/` or deletes a folder holds the run lock (`lock::RunLock`) and
//! passes the proof on to the stores and operations that call these
//! primitives. They are crate-private so that nothing outside the library
//! can write or delete past that layer. `init` writes `config.toml` before
//! the collection, and with it the lock file, exists.

pub mod atomic;
pub mod device;
pub mod moves;
pub mod remove;
pub mod space;
pub mod tree;

use crate::relpath::RelPath;
use std::io;
use std::path::Path;
use tree::TreeRoot;

/// Errors of the filesystem primitives.
#[derive(Debug, thiserror::Error)]
pub enum FsxError {
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    /// A removal met a directory on another device than the tree it was
    /// asked to remove, or the root of another mount.
    #[error(
        "{path} is on another device than the tree being removed, or is the root of another mount"
    )]
    ForeignDevice { path: String },
    /// A removal met a directory nested deeper than it descends.
    #[error("{path} is nested more than {limit} levels below the tree being removed")]
    TooDeep { path: String, limit: usize },
    #[error("cannot move {from} to {to}: they are on different devices")]
    CrossesDevices { from: String, to: String },
}

impl crate::error::HasKind for FsxError {}

/// Wraps an I/O failure of `action` on `path`.
pub(crate) fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> FsxError {
    let path = path.display().to_string();
    move |source| FsxError::Io {
        action,
        path,
        source,
    }
}

/// An I/O failure of `action` on `path`, named relative to the original
/// root the `root` handle descends from.
pub(crate) fn failure(
    action: &'static str,
    root: &TreeRoot,
    path: &RelPath,
) -> impl FnOnce(io::Error) -> FsxError {
    let path = shown(root, path);
    move |source| FsxError::Io {
        action,
        path,
        source,
    }
}

/// How an error names `path`, below `root`.
pub(crate) fn shown(root: &TreeRoot, path: &RelPath) -> String {
    root.prefix().concat(path).to_string()
}

/// Gives directories their owner permissions back when a test ends, so
/// the temp directory holding them can be removed.
#[cfg(test)]
pub(crate) struct RestoreModes(pub Vec<std::path::PathBuf>);

#[cfg(test)]
impl Drop for RestoreModes {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        for path in &self.0 {
            // Best effort: the directory may be gone already.
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ErrorKind, HasKind};

    #[test]
    fn errors_name_what_failed_and_map_to_the_generic_kind() {
        let io = io_error("write", Path::new("/a/b"))(io::Error::from_raw_os_error(5));
        assert!(io.to_string().starts_with("cannot write /a/b: "), "{io}");
        let foreign = FsxError::ForeignDevice { path: "mnt".into() };
        assert_eq!(
            foreign.to_string(),
            "mnt is on another device than the tree being removed, or is the root of another mount"
        );
        let crossing = FsxError::CrossesDevices {
            from: "a".into(),
            to: "b".into(),
        };
        assert_eq!(
            crossing.to_string(),
            "cannot move a to b: they are on different devices"
        );
        let deep = FsxError::TooDeep {
            path: "t/d".into(),
            limit: 512,
        };
        assert_eq!(
            deep.to_string(),
            "t/d is nested more than 512 levels below the tree being removed"
        );
        for error in [io, foreign, crossing, deep] {
            assert_eq!(error.kind(), ErrorKind::Other);
        }
    }
}
