// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Free space.

// TODO: `archive` (I19) and `unearth` (I20) check free space before they
// pack or fetch; until they land the library has no caller.
#![allow(dead_code)]

use super::{FsxError, io_error};
use std::path::Path;

/// The bytes an unprivileged process can still write on the filesystem
/// holding `path`.
pub(crate) fn available_space(path: &Path) -> Result<u64, FsxError> {
    let stat = rustix::fs::statvfs(path).map_err(|errno| io_error("stat", path)(errno.into()))?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::Tree;

    #[test]
    fn the_free_space_is_positive_and_below_the_size_of_the_filesystem() {
        let tree = Tree::new();
        let free = available_space(tree.root()).expect("answers");
        assert!(free > 0, "the test needs a temp volume with free space");
        let stat = rustix::fs::statvfs(tree.root()).expect("statvfs");
        assert!(free <= stat.f_blocks * stat.f_frsize, "{free}");
        // Other processes write while the test runs, so the two readings
        // of the live volume differ a little.
        let reading = stat.f_bavail * stat.f_frsize;
        assert!(free.abs_diff(reading) < 1 << 30, "{free} against {reading}");
    }

    #[test]
    fn the_free_space_of_a_missing_path_is_an_error() {
        let tree = Tree::new();
        let error = available_space(&tree.path("nope")).expect_err("fails");
        assert!(error.to_string().contains("nope"), "{error}");
    }
}
