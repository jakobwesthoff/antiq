// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The guard around a state-changing run.
//!
//! A command that changes state calls [`start`] once. A real run gets a
//! [`RunGuard`] that holds the run lock; a dry run gets only a look at who
//! holds the lock.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::lock::{self, Holder, LockError, RunLock};
use crate::machine::{self, MachineError};
use crate::repair;
use crate::report::Warning;

/// Errors of the run guard.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Machine(#[from] MachineError),
}

impl HasKind for RunError {
    fn kind(&self) -> ErrorKind {
        match self {
            RunError::Lock(error) => error.kind(),
            RunError::Machine(error) => error.kind(),
        }
    }
}

/// The proof that this process is the one run of a collection that
/// changes state. It holds the run lock until it is dropped.
#[derive(Debug)]
#[must_use = "the lock is released when this is dropped"]
pub struct RunGuard {
    lock: RunLock,
    warnings: Vec<Warning>,
}

impl RunGuard {
    /// Takes the lock and repairs what an earlier run left behind.
    pub(crate) fn begin(collection: &Collection, clock: &dyn Clock) -> Result<RunGuard, RunError> {
        RunGuard::begin_as(collection, clock, machine::detect())
    }

    /// [`RunGuard::begin`] with the outcome of the machine detection given,
    /// so that a test can make it fail.
    fn begin_as(
        collection: &Collection,
        clock: &dyn Clock,
        machine: Result<String, MachineError>,
    ) -> Result<RunGuard, RunError> {
        let lock = lock::acquire(&collection.lock_path(), &machine?, clock)?;
        let outcome = repair::run(collection, &lock, clock);
        Ok(RunGuard {
            lock,
            warnings: outcome.warnings,
        })
    }

    /// The lock, which code that changes the collection takes as its proof
    /// that the run lock is held.
    pub fn lock(&self) -> &RunLock {
        &self.lock
    }

    /// What the repairs at the start of the run have to report.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// How a command is to run.
#[derive(Debug)]
pub enum Started {
    Guarded(RunGuard),
    /// A dry run, which holds no lock and changes nothing. `holder` is the
    /// run that holds the lock now, if there is one.
    DryRun {
        holder: Option<Holder>,
    },
}

/// Starts a state-changing command. With `dry_run` nothing is locked,
/// repaired or written, so a `DryRun` has no [`RunLock`] to hand to anything
/// that writes.
pub fn start(
    collection: &Collection,
    clock: &dyn Clock,
    dry_run: bool,
) -> Result<Started, RunError> {
    if dry_run {
        // Reading the lock file takes no lock and creates nothing.
        let holder = lock::holder(&collection.lock_path())?;
        return Ok(Started::DryRun { holder });
    }
    Ok(Started::Guarded(RunGuard::begin(collection, clock)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::config::Config;
    use crate::error::Error;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use std::fs;

    fn clock() -> FixedClock {
        FixedClock::new("2026-11-03T10:00:00Z".parse().expect("a timestamp"))
    }

    fn collection(temp: &TempCollection) -> Collection {
        let config = Config::new(COLLECTION_ID.parse().expect("an ID"));
        Collection::new(temp.root().to_path_buf(), config)
    }

    fn begin(collection: &Collection) -> Result<RunGuard, RunError> {
        RunGuard::begin(collection, &clock())
    }

    #[test]
    fn a_second_run_is_refused_with_the_holder_in_the_error() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let _first = begin(&collection).expect("the lock is free");
        let error = begin(&collection).expect_err("the lock is held");
        let error = Error::from(error);
        assert_eq!(error.kind(), ErrorKind::LockHeld);
        let details = error.details();
        assert_eq!(details["pid"], std::process::id());
        assert_eq!(details["started"], "2026-11-03T10:00:00Z");
    }

    #[test]
    fn the_lock_file_names_this_run_while_the_guard_lives_and_is_empty_after() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let guard = begin(&collection).expect("the lock is free");
        let holder = lock::holder(&collection.lock_path())
            .expect("reads")
            .expect("a holder");
        assert_eq!(holder.pid, std::process::id());
        assert_eq!(holder.machine, machine::detect().expect("a name"));
        drop(guard);
        assert_eq!(fs::read(collection.lock_path()).expect("exists"), b"");
    }

    #[test]
    fn a_machine_without_a_name_cannot_run() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let unavailable = MachineError::Unavailable {
            reason: "refused by the test".to_string(),
        };
        let error = RunGuard::begin_as(&collection, &clock(), Err(unavailable))
            .expect_err("no machine name");
        assert!(matches!(error, RunError::Machine(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(!collection.lock_path().exists(), "no lock was taken");
    }

    #[test]
    fn a_dry_run_names_the_run_that_holds_the_lock() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let _running = begin(&collection).expect("the lock is free");
        let started = start(&collection, &clock(), true).expect("a dry run never fails on a lock");
        assert!(
            matches!(
                &started,
                Started::DryRun { holder: Some(holder) } if holder.pid == std::process::id()
            ),
            "{started:?}"
        );
    }

    #[test]
    fn a_dry_run_without_a_holder_says_so() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let started = start(&collection, &clock(), true).expect("starts");
        assert!(
            matches!(started, Started::DryRun { holder: None }),
            "{started:?}"
        );
    }

    #[test]
    fn a_dry_run_creates_no_lock_file() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let _started = start(&collection, &clock(), true).expect("starts");
        assert!(!collection.lock_path().exists());
    }

    #[test]
    fn a_dry_run_holds_no_lock() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let _started = start(&collection, &clock(), true).expect("starts");
        let _guard = begin(&collection).expect("the dry run holds nothing");
    }

    #[test]
    fn a_dry_run_cannot_read_a_lock_file_that_is_a_directory() {
        let mut temp = TempCollection::minimal();
        temp.tree().dir(".antiq/lock");
        let collection = collection(&temp);
        let error = start(&collection, &clock(), true).expect_err("the lock file is unreadable");
        assert!(
            matches!(error, RunError::Lock(LockError::Io { .. })),
            "{error}"
        );
    }

    /// Every path below `dir` with its content, directories as `None`.
    fn snapshot(dir: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
        fn walk(
            base: &std::path::Path,
            dir: &std::path::Path,
            seen: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
        ) {
            for item in fs::read_dir(dir).expect("lists") {
                let path = item.expect("an item").path();
                let name = path
                    .strip_prefix(base)
                    .expect("below")
                    .display()
                    .to_string();
                if path.is_dir() {
                    seen.insert(name, None);
                    walk(base, &path, seen);
                } else {
                    seen.insert(name, Some(fs::read(&path).expect("reads")));
                }
            }
        }
        let mut seen = std::collections::BTreeMap::new();
        walk(dir, dir, &mut seen);
        seen
    }

    #[test]
    fn a_dry_run_leaves_the_run_directories_and_everything_else_alone() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/trash/leftover/f", "x");
        temp.tree().file(".antiq/tmp/upload.part", "x");
        temp.tree().file(".antiq/doomed/tree/f", "x");
        temp.tree().file(".antiq/recovered/old/f", "x");
        temp.tree().file(".antiq/tmp/leftover", "x");
        temp.tree().file("projects/a/notes.txt", "hello");
        let collection = collection(&temp);
        let before = snapshot(temp.root());

        let started = start(&collection, &clock(), true).expect("starts");

        assert_eq!(snapshot(temp.root()), before);
        assert!(matches!(started, Started::DryRun { holder: None }));
    }

    #[test]
    fn a_real_start_guards_the_run() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp);
        let started = start(&collection, &clock(), false).expect("starts");
        assert!(
            matches!(
                &started,
                Started::Guarded(guard)
                    if guard.warnings().is_empty() && std::ptr::eq(guard.lock(), &guard.lock)
            ),
            "{started:?}"
        );
        let error = begin(&collection).expect_err("the guard holds the lock");
        assert_eq!(error.kind(), ErrorKind::LockHeld);
    }
}
