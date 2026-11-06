// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The run lock in .antiq/lock.
//!
//! A run that changes state holds an exclusive `flock` on the lock file
//! for as long as its [`RunLock`] lives, and writes who it is into the
//! file. The lock belongs to a collection on a local disk; the machine
//! name in the file says whose run it is and is not used to decide
//! anything.

use crate::clock::{Clock, whole_seconds};
use crate::error::{ErrorKind, HasKind};
use jiff::Timestamp;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::Path;

/// Errors of the run lock.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum LockError {
    #[diagnostic(
        code(antiq::lock::held),
        help("Wait until that run ends, then run the command again.")
    )]
    #[error(
        "another antiq run holds the lock (pid {pid}, started {started}), and runs that change an antiq collection take turns"
    )]
    Held { pid: u32, started: Timestamp },
    /// Another run holds the lock but has not written who it is yet.
    #[diagnostic(
        code(antiq::lock::held_unknown),
        help("Wait until that run ends, then run the command again.")
    )]
    #[error(
        "another antiq run holds the lock but has not written who it is yet, and runs that change an antiq collection take turns"
    )]
    HeldUnknown,
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
}

impl HasKind for LockError {
    fn kind(&self) -> ErrorKind {
        match self {
            LockError::Held { .. } | LockError::HeldUnknown => ErrorKind::LockHeld,
            LockError::Io { .. } => ErrorKind::Other,
        }
    }
}

/// Who holds the lock, as written into the lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub machine: String,
    pub started: Timestamp,
}

/// Proof that this process holds the run lock. Dropping it releases the
/// lock.
///
/// Only [`acquire`] makes one: the field is private.
///
/// ```compile_fail,E0451
/// use antiq::lock::RunLock;
///
/// // The lock file would have to be held for the proof to mean anything.
/// let _forged = RunLock { file: todo!() };
/// ```
#[derive(Debug)]
#[must_use = "the lock is released when this is dropped"]
pub struct RunLock {
    file: File,
}

impl Drop for RunLock {
    /// Empties the lock file while the lock is still held, so that the
    /// pid of a run that ended cleanly cannot show up as a live holder
    /// once another process has taken it. The lock itself goes with the
    /// file.
    fn drop(&mut self) {
        // Best effort: a lock file that cannot be emptied still unlocks.
        let _ = self.file.set_len(0);
    }
}

/// Takes the exclusive lock on `lock_file`, creating the file if needed,
/// and writes the pid, `machine` and the time into it. Fails at once with
/// [`LockError::Held`] when another run holds the lock.
pub fn acquire(lock_file: &Path, machine: &str, clock: &dyn Clock) -> Result<RunLock, LockError> {
    // Neither truncated nor written before the lock is ours: the holder's
    // pid has to stay readable for the run that is turned away.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_file)
        .map_err(io_error("open", lock_file))?;
    if !lock_outcome(file.try_lock(), lock_file)? {
        return Err(describe_holder(&mut file, lock_file)?);
    }

    let me = Holder {
        pid: std::process::id(),
        machine: machine.to_owned(),
        started: whole_seconds(clock.now()),
    };
    let text = serde_json::to_string(&me).expect("a holder is plain data and always serialises");
    file.set_len(0).map_err(io_error("truncate", lock_file))?;
    file.write_all(text.as_bytes())
        .map_err(io_error("write", lock_file))?;
    Ok(RunLock { file })
}

/// What `File::try_lock` said: `true` for a lock taken, `false` for one
/// that another description holds.
fn lock_outcome(result: Result<(), TryLockError>, lock_file: &Path) -> Result<bool, LockError> {
    match result {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(io_error("lock", lock_file)(error)),
    }
}

/// The error for a lock that is held: with the holder's pid and start
/// when its file has them.
fn describe_holder(file: &mut File, lock_file: &Path) -> Result<LockError, LockError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(io_error("read", lock_file))?;
    Ok(match serde_json::from_slice::<Holder>(&bytes) {
        Ok(holder) => LockError::Held {
            pid: holder.pid,
            started: holder.started,
        },
        Err(_) => LockError::HeldUnknown,
    })
}

/// The run that holds the lock, read from the lock file without trying
/// the lock: `None` when the file is missing, empty, unreadable as a
/// holder, or names a process that no longer exists.
///
/// Two cases escape this. A holder that has taken the lock but not yet
/// written to the file shows as no holder. A process that died without
/// releasing, whose pid another process has taken since, shows as a live
/// holder.
pub fn holder(lock_file: &Path) -> Result<Option<Holder>, LockError> {
    let bytes = match std::fs::read(lock_file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read", lock_file)(error)),
    };
    let alive = |holder: &Holder| process_exists(holder.pid);
    Ok(serde_json::from_slice::<Holder>(&bytes).ok().filter(alive))
}

/// Whether a process with this pid exists. A process of another user
/// answers with a refusal, which also says it exists.
fn process_exists(pid: u32) -> bool {
    process_exists_with(pid, &test_kill_process)
}

/// `signal_check` is the seam for the answer of the system: a test cannot
/// rely on a process of another user to exist wherever it runs.
fn process_exists_with(pid: u32, signal_check: &dyn Fn(Pid) -> Result<(), Errno>) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    match signal_check(pid) {
        Ok(()) => true,
        Err(errno) => errno == Errno::PERM,
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> LockError {
    let path = path.display().to_string();
    move |source| LockError::Io {
        action,
        path,
        source,
    }
}

/// Runs `attempt` again for a while as long as `held` calls its result a
/// lock that is held. A parallel test that forks keeps a copy of the lock
/// file's descriptor between its `fork` and its `exec`, so a lock that a
/// test has just released can still read as held for a moment.
#[cfg(test)]
pub(crate) fn retry_while_held<T>(held: impl Fn(&T) -> bool, attempt: impl FnMut() -> T) -> T {
    retry_with(100, std::time::Duration::from_millis(20), held, attempt)
}

#[cfg(test)]
fn retry_with<T>(
    tries: usize,
    pause: std::time::Duration,
    held: impl Fn(&T) -> bool,
    mut attempt: impl FnMut() -> T,
) -> T {
    let mut result = attempt();
    for _ in 0..tries {
        if !held(&result) {
            break;
        }
        std::thread::sleep(pause);
        result = attempt();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::error::Error;
    use crate::testing::tree::Tree;
    use std::fs;
    use std::process::Command;

    fn clock() -> FixedClock {
        FixedClock::new("2026-11-03T10:00:00.750Z".parse().expect("a timestamp"))
    }

    fn started() -> Timestamp {
        "2026-11-03T10:00:00Z".parse().expect("a timestamp")
    }

    fn lock_path(tree: &mut Tree) -> std::path::PathBuf {
        tree.dir(".antiq");
        tree.path(".antiq/lock")
    }

    /// Takes the lock once it is free. A process that another test thread
    /// forked while this one held the lock keeps the descriptor until it
    /// has executed, so a release is not instant for a moment.
    fn acquire_when_free(path: &Path) -> RunLock {
        let attempts = std::iter::repeat_with(|| {
            std::thread::yield_now();
            acquire(path, "m", &clock())
        });
        let mut free = attempts.take(100_000).filter_map(Result::ok);
        free.next().expect("the lock becomes free")
    }

    #[test]
    fn the_lock_file_names_pid_machine_and_whole_second_start() {
        let mut tree = Tree::new();
        let path = lock_path(&mut tree);
        let _lock = acquire(&path, "studio", &clock()).expect("the lock is free");
        let text = fs::read_to_string(&path).expect("the file exists");
        let written: Holder = serde_json::from_str(&text).expect("the file is a holder");
        assert_eq!(
            written,
            Holder {
                pid: std::process::id(),
                machine: "studio".into(),
                started: started(),
            }
        );
    }

    #[test]
    fn a_second_run_is_refused_and_told_who_holds_the_lock() {
        let mut tree = Tree::new();
        let path = lock_path(&mut tree);
        let _first = acquire(&path, "studio", &clock()).expect("the lock is free");
        let before = fs::read(&path).expect("exists");
        let error = acquire(&path, "laptop", &clock()).expect_err("held");
        assert!(
            matches!(
                &error,
                LockError::Held { pid, started: at } if *pid == std::process::id() && *at == started()
            ),
            "{error}"
        );
        assert_eq!(fs::read(&path).expect("exists"), before);
    }

    #[test]
    fn the_lock_is_free_again_and_the_file_empty_after_the_run() {
        let mut tree = Tree::new();
        let path = lock_path(&mut tree);
        drop(acquire(&path, "studio", &clock()).expect("free"));
        assert_eq!(fs::read(&path).expect("exists"), b"");
        assert_eq!(holder(&path).expect("reads"), None);
        let _again = acquire_when_free(&path);
    }

    #[test]
    fn a_longer_stale_file_leaves_no_trailing_bytes() {
        let mut tree = Tree::new();
        let path = lock_path(&mut tree);
        fs::write(&path, "x".repeat(500)).expect("written");
        let _lock = acquire(&path, "m", &clock()).expect("free");
        let text = fs::read_to_string(&path).expect("exists");
        serde_json::from_str::<Holder>(&text).expect("only the holder is in the file");
    }

    #[test]
    fn a_holder_that_has_not_written_yet_is_a_lock_held_without_details() {
        let mut tree = Tree::new();
        let path = lock_path(&mut tree);
        let taken = File::create(&path).expect("created");
        taken.try_lock().expect("free");
        let error = acquire(&path, "m", &clock()).expect_err("held");
        assert!(matches!(error, LockError::HeldUnknown), "{error}");
        let wrapped = Error::from(error);
        assert_eq!(wrapped.kind(), ErrorKind::LockHeld);
        assert!(wrapped.details().is_empty());
    }

    #[test]
    fn a_lock_error_other_than_contention_is_an_io_error() {
        let enolck = io::Error::from_raw_os_error(Errno::NOLCK.raw_os_error());
        let error = lock_outcome(Err(TryLockError::Error(enolck)), Path::new("/x/lock"))
            .expect_err("fails");
        assert!(
            matches!(error, LockError::Io { action: "lock", .. }),
            "{error}"
        );
        assert!(error.to_string().contains("/x/lock"), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(lock_outcome(Ok(()), Path::new("/x")).expect("taken"));
        let contended = lock_outcome(Err(TryLockError::WouldBlock), Path::new("/x"));
        assert!(!contended.expect("a plain refusal"));
    }

    #[test]
    fn a_held_result_is_tried_again_until_it_is_free() {
        let mut calls = 0;
        let result = retry_with(
            10,
            std::time::Duration::ZERO,
            |held: &bool| *held,
            || {
                calls += 1;
                calls < 4
            },
        );
        assert!(!result);
        assert_eq!(calls, 4);
    }

    #[test]
    fn a_result_that_stays_held_is_returned_after_the_tries() {
        let mut calls = 0;
        let result = retry_with(
            10,
            std::time::Duration::ZERO,
            |held: &bool| *held,
            || {
                calls += 1;
                true
            },
        );
        assert!(result);
        assert_eq!(calls, 11);
    }

    #[test]
    fn a_free_result_is_returned_at_once() {
        let result = retry_while_held(|held: &bool| *held, || false);
        assert!(!result);
    }

    #[test]
    fn the_lock_file_cannot_be_opened_in_a_missing_directory() {
        let tree = Tree::new();
        let error = acquire(&tree.path("missing/lock"), "m", &clock()).expect_err("fails");
        assert!(
            matches!(error, LockError::Io { action: "open", .. }),
            "{error}"
        );
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    /// A child that inherited the file would keep the lock after the run
    /// ended. Watching for that with a spawned process is not reliable:
    /// a process forked by another test thread while the lock was held
    /// keeps the descriptor until it has executed, so the flag is checked.
    #[test]
    fn the_lock_file_is_closed_in_child_processes() {
        let mut tree = Tree::new();
        let path = lock_path(&mut tree);
        let lock = acquire(&path, "m", &clock()).expect("free");
        let flags = rustix::io::fcntl_getfd(&lock.file).expect("the flags are readable");
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }

    #[test]
    fn nobody_holds_a_missing_lock_file() {
        let tree = Tree::new();
        assert_eq!(holder(&tree.path("lock")).expect("reads"), None);
    }

    #[test]
    fn an_empty_lock_file_shows_no_holder_also_while_a_run_is_about_to_write() {
        let mut tree = Tree::new();
        let path = tree.file("lock", "");
        assert_eq!(holder(&path).expect("reads"), None);
    }

    #[test]
    fn a_lock_file_that_is_no_holder_shows_none() {
        let mut tree = Tree::new();
        let path = tree.file("lock", "not json");
        assert_eq!(holder(&path).expect("reads"), None);
    }

    #[test]
    fn a_running_holder_is_shown_with_its_three_fields() {
        let mut tree = Tree::new();
        let path = lock_path(&mut tree);
        let _lock = acquire(&path, "studio", &clock()).expect("free");
        let shown = holder(&path).expect("reads").expect("a holder");
        assert_eq!(shown.pid, std::process::id());
        assert_eq!(shown.machine, "studio");
        assert_eq!(shown.started, started());
        let _ = acquire(&path, "m", &clock()).expect_err("holder() took no lock of its own");
    }

    #[test]
    fn a_holder_whose_process_is_gone_is_not_shown() {
        let mut tree = Tree::new();
        let mut child = Command::new("true").spawn().expect("true starts");
        let pid = child.id();
        child.wait().expect("the child ends");
        let content = Holder {
            pid,
            machine: "m".into(),
            started: started(),
        };
        let path = tree.file("lock", serde_json::to_string(&content).expect("json"));
        assert_eq!(holder(&path).expect("reads"), None);
    }

    #[test]
    fn a_held_lock_file_that_is_not_utf8_is_held_without_details() {
        let mut tree = Tree::new();
        let path = tree.file("lock", b"\xff\xfe not text");
        let taken = File::open(&path).expect("opens");
        taken.try_lock().expect("free");
        let error = acquire(&path, "m", &clock()).expect_err("held");
        assert!(matches!(error, LockError::HeldUnknown), "{error}");
    }

    #[test]
    fn a_lock_file_that_is_not_utf8_shows_no_holder() {
        let mut tree = Tree::new();
        let path = tree.file("lock", b"\xff\xfe not text");
        assert_eq!(holder(&path).expect("reads"), None);
    }

    #[test]
    fn a_refused_signal_still_means_the_process_exists() {
        let answering = |errno: Option<Errno>| move |_: Pid| errno.map_or(Ok(()), Err);
        assert!(process_exists_with(7, &answering(None)));
        assert!(process_exists_with(7, &answering(Some(Errno::PERM))));
        assert!(!process_exists_with(7, &answering(Some(Errno::SRCH))));
    }

    /// The system's own answer for a process of another user: launchd is
    /// owned by root.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_of_another_user_counts_as_existing() {
        assert!(
            !rustix::process::getuid().is_root(),
            "the test needs a non-root user, who is refused a signal to pid 1"
        );
        let refused = test_kill_process(Pid::from_raw(1).expect("1 is a pid"))
            .expect_err("the test needs pid 1 to belong to another user");
        assert_eq!(refused, Errno::PERM);
        assert!(process_exists(1));
    }

    #[test]
    fn a_pid_that_does_not_fit_is_no_process() {
        assert!(!process_exists(0));
        assert!(!process_exists(u32::MAX));
        assert!(process_exists(std::process::id()));
    }

    #[test]
    fn a_lock_file_that_cannot_be_read_is_an_io_error() {
        let mut tree = Tree::new();
        let directory = tree.dir("lock");
        let error = holder(&directory).expect_err("a directory is no lock file");
        assert!(
            matches!(error, LockError::Io { action: "read", .. }),
            "{error}"
        );
    }
}
