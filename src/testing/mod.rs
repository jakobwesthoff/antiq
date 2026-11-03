// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixtures shared by library tests.

// The binary tests use the same file; each test crate uses a part of it.
#[allow(dead_code)]
#[path = "../../tests/support/tree.rs"]
pub mod tree;

mod pool;
pub use pool::shared_pool;

use crate::relpath::RelPath;

/// A path of `levels` nested names of 200 bytes each. Past five levels it
/// is longer than the 1,024 bytes macOS accepts in a path, and past 20
/// longer than the 4,096 bytes of Linux.
pub fn deep_path(levels: usize) -> RelPath {
    let names: Vec<String> = (0..levels)
        .map(|level| format!("{level:02}{}", "x".repeat(198)))
        .collect();
    RelPath::parse(&names.join("/")).expect("the names are valid")
}

#[cfg(test)]
mod tests {
    use super::tree::{COLLECTION_ID, TempCollection, Tree};
    use std::fs;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    #[test]
    fn a_file_has_the_content_mode_and_time_it_was_given() {
        let mut tree = Tree::new();
        let path = tree.file_with("a/b/note.txt", "hello", 0o600, 1_000_000_000);
        let metadata = fs::metadata(path).expect("the file exists");
        assert_eq!(metadata.len(), 5);
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.mtime(), 1_000_000_000);
    }

    #[test]
    fn symlinks_keep_their_target_as_given() {
        let mut tree = Tree::new();
        let dangling = tree.symlink("dangling", "nowhere");
        let outside = tree.symlink("sub/outside", "/etc");
        assert_eq!(
            fs::read_link(&dangling).expect("a link"),
            std::path::Path::new("nowhere")
        );
        assert!(fs::metadata(&dangling).is_err(), "the target is missing");
        assert_eq!(
            fs::read_link(outside).expect("a link"),
            std::path::Path::new("/etc")
        );
    }

    #[test]
    fn a_hard_link_is_a_second_name_of_the_same_file() {
        let mut tree = Tree::new();
        let first = tree.file("one", "x");
        let second = tree.hard_link("one", "dir/two");
        let (first, second) = (
            fs::metadata(first).expect("exists"),
            fs::metadata(second).expect("exists"),
        );
        assert_eq!(first.ino(), second.ino());
        assert_eq!(first.nlink(), 2);
    }

    #[test]
    fn a_fifo_is_a_fifo() {
        let mut tree = Tree::new();
        let path = tree.fifo("queue");
        assert!(
            fs::symlink_metadata(path)
                .expect("exists")
                .file_type()
                .is_fifo()
        );
    }

    #[test]
    fn a_sparse_file_has_less_allocated_than_its_size() {
        let mut tree = Tree::new();
        let path = tree.sparse("hole", 1 << 20);
        let metadata = fs::metadata(path).expect("exists");
        assert_eq!(metadata.len(), 1 << 20);
        let allocated = metadata.blocks() * 512;
        assert!(allocated < metadata.len(), "{allocated} bytes allocated");
    }

    #[test]
    fn an_unreadable_directory_cannot_be_read_and_does_not_block_cleanup() {
        let mut tree = Tree::new();
        let path = tree.unreadable_dir("locked");
        assert!(
            fs::read_dir(&path).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        drop(tree);
        assert!(!path.exists(), "the temp directory was removed");
    }

    #[test]
    fn a_minimal_collection_has_its_id_and_the_local_layout() {
        let collection = TempCollection::minimal();
        let antiq = collection.root().join(".antiq");
        assert_eq!(
            fs::read_to_string(antiq.join("config.toml")).expect("the config exists"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n")
        );
        for directory in [
            "objects",
            "tracking",
            "index",
            "tmp",
            "staging",
            "trash",
            "recovered",
        ] {
            assert!(antiq.join(directory).is_dir(), "{directory} is missing");
        }
        let mut collection = collection;
        let extra = collection.tree().file("projects/a/readme", "x");
        assert!(extra.is_file());
    }
}
