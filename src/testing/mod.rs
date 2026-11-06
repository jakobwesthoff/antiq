// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixtures shared by library tests.

// The binary tests use the same file; each test crate uses a part of it.
#[allow(dead_code)]
#[path = "../../tests/support/tree.rs"]
pub mod tree;

// The binary tests use the same file; each test crate uses a part of it.
#[allow(dead_code)]
#[path = "../../tests/support/versions.rs"]
pub mod versions;

pub mod package;
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

/// An error that can say the run lock was found held with no holder
/// recorded.
pub trait LockHeldWithoutHolder {
    fn is_lock_held_without_holder(&self) -> bool;
}

impl LockHeldWithoutHolder for crate::run::RunError {
    fn is_lock_held_without_holder(&self) -> bool {
        matches!(
            self,
            crate::run::RunError::Lock(crate::lock::LockError::HeldUnknown)
        )
    }
}

impl LockHeldWithoutHolder for crate::error::Error {
    fn is_lock_held_without_holder(&self) -> bool {
        match self {
            crate::error::Error::Run(error) => error.is_lock_held_without_holder(),
            crate::error::Error::Lock(error) => {
                matches!(error, crate::lock::LockError::HeldUnknown)
            }
            _ => false,
        }
    }
}

/// How often [`rerun_while_lock_unattributed`] runs an operation again.
const LOCK_RETRIES: usize = 100_000;

/// Runs `operation` again while it fails with `LockError::HeldUnknown`,
/// at most [`LOCK_RETRIES`] times.
///
/// Test threads fork processes while other threads hold a lock. The child
/// keeps the descriptor of the lock file until it has executed, so a lock
/// that was released a moment ago is still held, and its file is empty.
/// An operation takes the lock before it does anything, so running it
/// again is safe. Any other error, including a lock held by a recorded
/// holder, is returned at once.
pub fn rerun_while_lock_unattributed<T, E: LockHeldWithoutHolder>(
    mut operation: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut reruns = 0;
    loop {
        match operation() {
            Err(error) if error.is_lock_held_without_holder() && reruns < LOCK_RETRIES => {
                reruns += 1;
                std::thread::yield_now();
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LOCK_RETRIES, rerun_while_lock_unattributed};
    use crate::lock::LockError;
    use crate::run::RunError;

    fn unattributed() -> RunError {
        RunError::Lock(LockError::HeldUnknown)
    }

    #[test]
    fn an_operation_that_finds_the_lock_held_without_a_holder_runs_again() {
        let mut calls = 0;
        let result = rerun_while_lock_unattributed(|| {
            calls += 1;
            if calls < 3 {
                Err(unattributed())
            } else {
                Ok(calls)
            }
        });
        assert_eq!(result.expect("the third run succeeds"), 3);
    }

    #[test]
    fn the_errors_that_wrap_a_run_error_pass_the_lock_state_on() {
        use super::LockHeldWithoutHolder;
        use crate::error::Error;

        assert!(Error::Run(unattributed()).is_lock_held_without_holder());
        assert!(Error::Lock(LockError::HeldUnknown).is_lock_held_without_holder());
        let holder = LockError::Held {
            pid: 7,
            started: "2026-11-03T10:00:00Z".parse().expect("a time"),
        };
        assert!(!Error::Lock(holder).is_lock_held_without_holder());
        assert!(!Error::Usage("x".into()).is_lock_held_without_holder());
    }

    #[test]
    fn a_lock_held_by_a_recorded_holder_is_returned_at_once() {
        let mut calls = 0;
        let result: Result<(), RunError> = rerun_while_lock_unattributed(|| {
            calls += 1;
            Err(RunError::Lock(LockError::Held {
                pid: 7,
                started: "2026-11-03T10:00:00Z".parse().expect("a time"),
            }))
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn an_operation_stops_after_the_retry_cap_with_its_error() {
        let mut calls = 0;
        let result: Result<(), RunError> = rerun_while_lock_unattributed(|| {
            calls += 1;
            Err(unattributed())
        });
        assert!(matches!(
            result,
            Err(RunError::Lock(LockError::HeldUnknown))
        ));
        assert_eq!(calls, LOCK_RETRIES + 1);
    }

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
