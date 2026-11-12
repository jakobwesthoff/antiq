// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixtures shared by library tests.

// The integration tests include the same file; each test crate uses a part of it.
#[allow(
    dead_code,
    reason = "shared with the integration tests, which each use a part"
)]
#[path = "../../tests/support/tree.rs"]
pub mod tree;

// The integration tests include the same file; each test crate uses a part of it.
#[allow(
    dead_code,
    reason = "shared with the integration tests, which each use a part"
)]
#[path = "../../tests/support/versions.rs"]
pub mod versions;

// The integration tests include the same file; each test crate uses a part of it.
#[allow(
    dead_code,
    reason = "shared with the integration tests, which each use a part"
)]
#[path = "../../tests/support/fake_rsync.rs"]
pub mod fake_rsync;

pub mod package;
mod pool;
pub mod recording;
pub use pool::shared_pool;

use crate::relpath::RelPath;

/// Holds the run lock of the collection at `root` in this process, so a
/// dry run in the same process finds it held and names the holder.
pub fn hold_lock(root: &std::path::Path) -> crate::lock::RunLock {
    let clock = crate::clock::FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
    crate::lock::acquire(&root.join(".antiq/lock"), "imperius", &clock).expect("the lock is free")
}

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
        for directory in ["tmp", "trash", "doomed", "recovered", "pieces"] {
            assert!(antiq.join(directory).is_dir(), "{directory} is missing");
        }
        for gone in ["objects", "tracking", "index", "staging"] {
            assert!(
                !antiq.join(gone).exists(),
                "{gone} is not part of the layout"
            );
        }
        let mut collection = collection;
        let extra = collection.tree().file("projects/a/readme", "x");
        assert!(extra.is_file());
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::tree::{Tree, snapshot};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn a_snapshot_sees_a_mode_a_replaced_symlink_and_a_changed_file() {
        let mut tree = Tree::new();
        tree.dir("d");
        tree.symlink("link", "one");
        tree.file("d/f", "x");
        // The directory starts with the mode the test changes and restores,
        // whatever mask the process has.
        fs::set_permissions(tree.path("d"), fs::Permissions::from_mode(0o755)).expect("chmod");
        let before = snapshot(tree.root());

        fs::set_permissions(tree.path("d"), fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_ne!(snapshot(tree.root()), before, "a mode change goes unseen");
        fs::set_permissions(tree.path("d"), fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(snapshot(tree.root()), before);

        fs::remove_file(tree.path("link")).expect("removes");
        symlink("two", tree.path("link")).expect("links");
        assert_ne!(
            snapshot(tree.root()),
            before,
            "a new link target goes unseen"
        );
        fs::remove_file(tree.path("link")).expect("removes");
        symlink("one", tree.path("link")).expect("links");

        fs::write(tree.path("d/f"), "y").expect("writes");
        assert_ne!(snapshot(tree.root()), before, "new content goes unseen");
    }

    #[test]
    fn a_snapshot_does_not_follow_a_symlink_to_a_directory() {
        let mut tree = Tree::new();
        let mut outside = Tree::new();
        outside.file("secret", "x");
        tree.symlink("link", outside.root());
        let seen = snapshot(tree.root());
        assert_eq!(seen.keys().collect::<Vec<_>>(), ["link"]);
        assert_eq!(seen["link"].kind, "symlink");
    }

    #[test]
    fn an_unreadable_directory_is_recorded_by_its_mode_alone() {
        let mut tree = Tree::new();
        tree.file("d/f", "x");
        let blocked = tree.unreadable_dir("locked");
        assert!(
            fs::read_dir(&blocked).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let seen = snapshot(tree.root());
        assert_eq!(seen["locked"].mode, 0);
        assert_eq!(seen.len(), 3, "{seen:?}");
    }
}

#[cfg(test)]
mod version_fixture_tests {
    use super::tree::Tree;
    use super::versions::Store;
    use crate::listing;
    use crate::versions::names::VersionFile;
    use std::fs::File;

    #[test]
    fn the_fixture_compiles_in_the_library_and_writes_a_readable_version() {
        let mut source = Tree::new();
        source.file("notes/a.txt", "alpha");
        let mut store = Store::new();
        let version = store.add_version(&source, "notes", "2026-11-04T090211");

        let scan = store.scan();
        assert_eq!(scan.pieces.len(), 1);
        assert_eq!(scan.pieces[0].versions[0].stamp, version.stamp);
        let file = File::open(version.path(VersionFile::Listing)).expect("the listing exists");
        let read = listing::read_full(file).expect("the listing reads in full");
        assert_eq!(read.header.archive, version.names.archive.as_str());
        assert_eq!(read.files, 1);
    }
}
