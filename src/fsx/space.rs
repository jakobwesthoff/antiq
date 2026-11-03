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
    Ok(bytes_available(stat.f_bavail, stat.f_frsize))
}

/// Free blocks an unprivileged process may use, counted in the
/// fundamental block size.
fn bytes_available(blocks_available: u64, fragment_size: u64) -> u64 {
    blocks_available.saturating_mul(fragment_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::Tree;

    #[test]
    fn the_free_space_of_the_temp_volume_is_positive() {
        let tree = Tree::new();
        let free = available_space(tree.root()).expect("answers");
        assert!(free > 0, "the test needs a temp volume with free space");
    }

    #[test]
    fn free_blocks_are_counted_in_the_fundamental_block_size() {
        assert_eq!(bytes_available(10, 4096), 40_960);
        assert_eq!(bytes_available(u64::MAX, 2), u64::MAX);
    }

    #[test]
    fn the_free_space_of_a_missing_path_is_an_error() {
        let tree = Tree::new();
        let error = available_space(&tree.path("nope")).expect_err("fails");
        assert!(error.to_string().contains("nope"), "{error}");
    }
}
