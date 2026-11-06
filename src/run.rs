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
use crate::repair::{self, Repair};
use crate::report::Warning;

/// Errors of the run guard.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RunError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    #[diagnostic(transparent)]
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
    repaired: Vec<Repair>,
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
        let outcome = repair::run(collection, &lock);
        Ok(RunGuard {
            lock,
            repaired: outcome.done,
            warnings: outcome.warnings,
        })
    }

    /// The lock, which code that changes the collection takes as its proof
    /// that the run lock is held.
    pub fn lock(&self) -> &RunLock {
        &self.lock
    }

    /// The repairs at the start of the run that were done, as they were done.
    pub fn repaired(&self) -> &[Repair] {
        &self.repaired
    }

    /// The repairs at the start of the run that failed, and what could not
    /// be read to repair.
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
        /// The repairs a real run would start with, in order. Empty while
        /// another run holds the lock.
        repairs: Vec<Repair>,
        /// What could not be read to find them.
        warnings: Vec<Warning>,
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
        // The run that holds the lock is doing the repairs, and what it is
        // moving would be read half way.
        if holder.is_some() {
            return Ok(Started::DryRun {
                holder,
                repairs: Vec::new(),
                warnings: Vec::new(),
            });
        }
        let planned = repair::plan(collection);
        return Ok(Started::DryRun {
            holder,
            repairs: planned.repairs,
            warnings: planned.warnings,
        });
    }
    Ok(Started::Guarded(RunGuard::begin(collection, clock)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::config::Config;
    use crate::error::Error;
    use crate::testing::tree::{COLLECTION_ID, TempCollection, snapshot};
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
                Started::DryRun { holder: Some(holder), .. } if holder.pid == std::process::id()
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
            matches!(started, Started::DryRun { holder: None, .. }),
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
        assert!(matches!(started, Started::DryRun { holder: None, .. }));
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

    // ---- repairs at the start ----

    use crate::relpath::{Name, RelPath};
    use crate::repair::Repair;
    use crate::versions::names::{VersionFile, file_name};
    use std::os::unix::fs::PermissionsExt;

    fn delete_temporary(name: &str) -> Repair {
        Repair::DeleteTemporary {
            entry: Name::from_bytes(name.as_bytes().to_vec()).expect("a name"),
        }
    }

    #[test]
    fn a_real_start_repairs_and_reports_what_it_did() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/tmp/upload.part", "x");
        let collection = collection(&temp);

        let guard = begin(&collection).expect("starts");

        assert_eq!(guard.repaired(), [delete_temporary("upload.part")]);
        assert!(guard.warnings().is_empty());
        assert!(!temp.root().join(".antiq/tmp/upload.part").exists());
        drop(guard);
        let again = begin(&collection).expect("starts again");
        assert!(again.repaired().is_empty(), "the second run finds it done");
    }

    #[test]
    fn a_failing_repair_reaches_the_caller_as_a_warning() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/tmp/x", "x");
        let tmp = temp.root().join(".antiq/tmp");
        let _restore = crate::filesystem::RestoreModes(vec![tmp.clone()]);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let probe = std::fs::File::create(tmp.join("probe"));
        assert!(
            probe.is_err(),
            "the test needs a user that mode 0555 locks out"
        );
        let collection = collection(&temp);

        let guard = begin(&collection).expect("a failing repair never fails the run");

        assert!(guard.repaired().is_empty());
        assert!(
            guard.warnings()[0]
                .message
                .starts_with("cannot delete a temporary file .antiq/tmp/x: "),
            "{:?}",
            guard.warnings()
        );
    }

    fn crashed(temp: &mut TempCollection) {
        temp.tree().file(".antiq/tmp/upload.part", "x");
        temp.tree()
            .file(".antiq/doomed/2026-11-04T090211/gone/f", "x");
        temp.tree()
            .file(".antiq/trash/2026-11-04T090212/back/f", "x");
        temp.tree().dir(".antiq/pieces/empty");
    }

    #[test]
    fn a_dry_run_lists_the_repairs_a_real_run_then_does_and_changes_nothing() {
        let mut temp = TempCollection::minimal();
        crashed(&mut temp);
        let collection = collection(&temp);
        let before = snapshot(temp.root());

        let started = start(&collection, &clock(), true).expect("starts");

        let Started::DryRun {
            holder,
            repairs,
            warnings,
        } = started
        else {
            panic!("a dry run");
        };
        assert!(holder.is_none() && warnings.is_empty(), "{warnings:?}");
        assert_eq!(snapshot(temp.root()), before, "nothing was changed");
        let lines: Vec<String> = repairs.iter().map(Repair::describe).collect();
        assert_eq!(
            lines,
            [
                "delete .antiq/tmp/upload.part",
                "delete .antiq/doomed/2026-11-04T090211/gone",
                "remove the empty folder .antiq/doomed/2026-11-04T090211",
                "move .antiq/trash/2026-11-04T090212/back back to back",
                "remove the empty folder .antiq/trash/2026-11-04T090212",
                "remove the empty folder .antiq/pieces/empty",
            ]
        );
        let guard = begin(&collection).expect("starts");
        assert_eq!(
            guard.repaired(),
            repairs,
            "the run does what the dry run listed"
        );
    }

    #[test]
    fn a_dry_run_whose_live_move_cannot_work_lists_live_and_the_run_reports_recovered() {
        let mut temp = TempCollection::minimal();
        temp.tree()
            .file(".antiq/trash/2026-11-04T090212/up/down/f", "x");
        let piece = RelPath::parse("up/down").expect("a path");
        let stamp = "2026-11-04T090212".parse().expect("a stamp");
        for kind in VersionFile::ALL {
            let name = file_name(&piece, &stamp, kind);
            temp.tree()
                .file(&format!(".antiq/pieces/up/down/{name}"), "x");
        }
        temp.tree().dir("up");
        let up = temp.root().join("up");
        let _restore = crate::filesystem::RestoreModes(vec![up.clone()]);
        std::fs::set_permissions(&up, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        assert!(
            std::fs::File::create(up.join("probe")).is_err(),
            "the test needs a user that mode 0555 locks out"
        );
        let collection = collection(&temp);

        let Started::DryRun { repairs, .. } = start(&collection, &clock(), true).expect("starts")
        else {
            panic!("a dry run");
        };
        let guard = begin(&collection).expect("starts");

        let destination = |repairs: &[Repair]| {
            repairs
                .iter()
                .find_map(|repair| match repair {
                    Repair::GiveBack { destination, .. } => Some(*destination),
                    _ => None,
                })
                .expect("a give back")
        };
        assert_eq!(destination(&repairs), crate::repair::Destination::Live);
        assert_eq!(
            destination(guard.repaired()),
            crate::repair::Destination::Recovered
        );
    }

    #[test]
    fn a_dry_run_while_another_run_holds_the_lock_names_it_and_reads_nothing() {
        let mut temp = TempCollection::minimal();
        crashed(&mut temp);
        let collection = collection(&temp);
        let _holder = crate::testing::hold_lock(temp.root());
        // Planning would warn about these, so a read shows as a warning.
        let blocked = [
            temp.root().join(".antiq/trash"),
            temp.root().join(".antiq/tmp"),
        ];
        let _restore = crate::filesystem::RestoreModes(blocked.to_vec());
        for dir in &blocked {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        }
        assert!(
            std::fs::read_dir(&blocked[0]).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        let started = start(&collection, &clock(), true).expect("starts");

        let Started::DryRun {
            holder,
            repairs,
            warnings,
        } = started
        else {
            panic!("a dry run");
        };
        assert_eq!(holder.expect("the holder is named").pid, std::process::id());
        assert!(repairs.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
