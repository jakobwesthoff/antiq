// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Free space.

use super::{FilesystemError, io_error};
use std::error::Error;
use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

/// The bytes an unprivileged process can still write on the filesystem
/// holding `path`.
pub(crate) fn available_space(path: &Path) -> Result<u64, FilesystemError> {
    let stat = rustix::fs::statvfs(path).map_err(|errno| io_error("stat", path)(errno.into()))?;
    Ok(bytes_available(stat.f_bavail, stat.f_frsize))
}

/// Why bytes could not be reserved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReserveError {
    #[error("{needed} bytes are needed and {available} are free")]
    Short { needed: u64, available: u64 },
    #[error(transparent)]
    Free(#[from] FilesystemError),
}

/// A claim on free space that a piece holds until its output is gone.
#[derive(Debug)]
pub(crate) struct Reservation<'a> {
    ledger: &'a SpaceLedger,
    bytes: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut reserved = self
            .ledger
            .reserved
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *reserved -= self.bytes;
    }
}

/// The space that pieces running side by side have claimed and not yet
/// written. The free space of the volume does not show a package that is
/// still to be written, so each piece checks against what is free minus
/// what the others reserved.
#[derive(Debug, Default)]
pub(crate) struct SpaceLedger {
    reserved: Mutex<u64>,
}

impl SpaceLedger {
    pub(crate) fn new() -> SpaceLedger {
        SpaceLedger::default()
    }

    /// Reserves `bytes` if `free` reports room for them on top of what is
    /// reserved already.
    pub(crate) fn try_reserve(
        &self,
        bytes: u64,
        free: impl FnOnce() -> Result<u64, FilesystemError>,
    ) -> Result<Reservation<'_>, ReserveError> {
        // The lock is held across the reading: a piece that reads while
        // another still holds its claim must not compare with a reading
        // taken before the other wrote and released.
        let mut reserved = self.reserved.lock().unwrap_or_else(PoisonError::into_inner);
        let free = free()?;
        let available = free.saturating_sub(*reserved);
        if bytes > available {
            return Err(ReserveError::Short {
                needed: bytes,
                available,
            });
        }
        *reserved += bytes;
        Ok(Reservation {
            ledger: self,
            bytes,
        })
    }
}

/// Whether the error or one of its causes says the volume is full: the
/// raw `ENOSPC` of the operating system, or the `StorageFull` kind that a
/// failure built without an errno carries.
///
/// The answer says nothing about what to remove: a
/// `versions::VersionsError::PartlyMoved` has the cause in its chain, and
/// its staging is kept even when the cause is a full disk.
pub(crate) fn is_out_of_space(error: &(dyn Error + 'static)) -> bool {
    let mut current: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(error) = current {
        if error.downcast_ref::<io::Error>().is_some_and(|error| {
            error.raw_os_error() == Some(libc::ENOSPC) || error.kind() == io::ErrorKind::StorageFull
        }) {
            return true;
        }
        current = error.source();
    }
    false
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

    /// An error that keeps its cause as the source, like a non-transparent
    /// wrapper does.
    #[derive(Debug, thiserror::Error)]
    #[error("wrapped")]
    struct Wrapper(#[source] Box<dyn Error + Send + Sync>);

    /// An error that forwards to its cause, like `#[error(transparent)]`.
    #[derive(Debug, thiserror::Error)]
    #[error(transparent)]
    struct Transparent(Box<dyn Error + Send + Sync>);

    fn enospc() -> io::Error {
        io::Error::from_raw_os_error(libc::ENOSPC)
    }

    #[test]
    fn a_raw_enospc_is_out_of_space() {
        assert!(is_out_of_space(&enospc()));
    }

    #[test]
    fn the_storage_full_kind_is_out_of_space() {
        assert!(is_out_of_space(&io::Error::from(
            io::ErrorKind::StorageFull
        )));
    }

    #[test]
    fn enospc_below_a_wrapper_is_found() {
        let error = Wrapper(Box::new(FilesystemError::Io {
            action: "write",
            path: "x".to_string(),
            source: enospc(),
        }));
        assert!(is_out_of_space(&error));
    }

    #[test]
    fn enospc_below_a_transparent_wrapper_is_found() {
        let error = Transparent(Box::new(Wrapper(Box::new(enospc()))));
        assert!(is_out_of_space(&error));
    }

    #[test]
    fn other_errors_are_not_out_of_space() {
        assert!(!is_out_of_space(&io::Error::from_raw_os_error(libc::EIO)));
        assert!(!is_out_of_space(&io::Error::other("plain")));
        assert!(!is_out_of_space(&Wrapper(Box::new(io::Error::other("x")))));
    }

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
    fn a_second_reservation_counts_the_first() {
        let ledger = SpaceLedger::new();
        let _first = ledger.try_reserve(60, || Ok(100)).expect("fits");
        let error = ledger
            .try_reserve(60, || Ok(100))
            .expect_err("does not fit");
        assert!(
            matches!(
                error,
                ReserveError::Short {
                    needed: 60,
                    available: 40
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_dropped_reservation_gives_its_bytes_back() {
        let ledger = SpaceLedger::new();
        drop(ledger.try_reserve(60, || Ok(100)).expect("fits"));
        ledger.try_reserve(60, || Ok(100)).expect("fits again");
    }

    // A reads the free space and waits to be overtaken. If B could reserve
    // and hand its claim back in the meantime (its package written, the
    // volume holding 60 bytes fewer than A's reading), A would claim on a
    // stale reading.
    #[test]
    fn the_free_space_is_read_and_claimed_in_one_step() {
        use std::sync::mpsc;
        use std::time::Duration;

        let ledger = SpaceLedger::new();
        let (a_reading, a_read) = mpsc::channel();
        let (b_done, b_finished) = mpsc::channel();
        let (first, second) = std::thread::scope(|scope| {
            let ledger = &ledger;
            let a = scope.spawn(move || {
                ledger
                    .try_reserve(60, || {
                        a_reading.send(()).expect("the test listens");
                        let _ = b_finished.recv_timeout(Duration::from_millis(300));
                        Ok(100)
                    })
                    .map(std::mem::forget)
                    .is_ok()
            });
            a_read.recv().expect("A starts reading");
            let b = ledger.try_reserve(60, || Ok(100)).is_ok();
            // A has stopped listening if B had to wait for A's lock.
            let _ = b_done.send(());
            (a.join().expect("A finishes"), b)
        });
        assert!(first, "A holds its claim");
        assert!(!second, "B finds A's claim and is short");
    }

    #[test]
    fn a_failing_reading_is_returned_and_reserves_nothing() {
        let tree = Tree::new();
        let ledger = SpaceLedger::new();
        let error = ledger
            .try_reserve(60, || available_space(&tree.path("nope")))
            .expect_err("the reading fails");
        assert!(matches!(error, ReserveError::Free(_)), "{error:?}");
        ledger
            .try_reserve(100, || Ok(100))
            .expect("nothing is held");
    }

    #[test]
    fn the_free_space_of_a_missing_path_is_an_error() {
        let tree = Tree::new();
        let error = available_space(&tree.path("nope")).expect_err("fails");
        assert!(error.to_string().contains("nope"), "{error}");
    }
}
