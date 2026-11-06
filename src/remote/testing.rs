// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fault-injecting remote for tests.
//!
//! [`FaultRemote`] wraps a [`PathRemote`] and can fail the Nth call of an
//! operation, run a callback before or after a call so that a test can act
//! as another machine in the middle of a commit (ADR 0030), and report
//! every call it saw. Calls are counted from 1.

use super::marker::{self, Marker};
use super::path::PathRemote;
use super::{
    Assessment, Created, KnownVersions, MetaFile, Placement, Remote, RemoteError, RemoteFile,
    Transfer, Upload,
};
use crate::id::ObjectId;
use jiff::Timestamp;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The ID the remote root of the test fixtures carries in its marker.
pub const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

/// An object ID that is fixed per number, so that a test can name objects.
pub fn object(number: u8) -> ObjectId {
    ObjectId::from_ulid(ulid::Ulid::from_parts(
        1_600_000_000_000 + u64::from(number),
        u128::from(number),
    ))
}

/// A remote root with the marker of [`REMOTE_ID`].
pub fn remote_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temp dir");
    let marker = Marker::new(REMOTE_ID.parse().expect("a ULID"));
    std::fs::write(root.path().join(marker::FILE_NAME), marker.to_bytes())
        .expect("the marker is written");
    root
}

/// A path remote on `root` as the machine `machine` sees it. Two of them
/// on one root are two machines sharing a remote.
pub fn path_remote(root: &Path, machine: &str) -> PathRemote {
    PathRemote::new(
        root.to_path_buf(),
        REMOTE_ID.parse().expect("a ULID"),
        machine,
    )
}

/// One method of [`Remote`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    Reachability,
    BeginPhase,
    Clock,
    PullMetadata,
    ListObjects,
    ListDir,
    ListPartial,
    ReadFile,
    GetPayload,
    CreateExclusive,
    WriteTemp,
    RenameIntoPlace,
    PutPayload,
    Delete,
    DeletePartial,
    Checksum,
    Flush,
    Finish,
}

/// How an injected failure reads to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// An I/O error, which has kind `other`.
    Io,
    Away,
    Broken,
}

impl Fault {
    fn into_error(self, op: Op, name: &str) -> RemoteError {
        let what = format!("injected fault in {op:?} {name}");
        match self {
            Fault::Io => RemoteError::Io {
                action: "inject",
                path: what,
                source: std::io::Error::from_raw_os_error(5),
            },
            Fault::Away => RemoteError::Away {
                message: what,
                overdue: false,
            },
            Fault::Broken => RemoteError::Broken(what),
        }
    }
}

/// A call the remote saw: the method and the file name it was given, empty
/// where the method takes none. A rename logs the final name, a put the
/// final name, a temp write its temp name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub op: Op,
    pub name: String,
}

impl Call {
    pub fn new(op: Op, name: &str) -> Call {
        Call {
            op,
            name: name.to_string(),
        }
    }
}

type Callback = Arc<dyn Fn(&PathRemote) + Send + Sync>;

#[derive(Default)]
struct State {
    counts: BTreeMap<Op, usize>,
    calls: Vec<Call>,
    faults: Vec<(Op, usize, Fault)>,
    before: Vec<(Op, usize, Callback)>,
    after: Vec<(Op, usize, Callback)>,
    clock: Option<Timestamp>,
}

/// What [`FaultRemote::enter`] found for one call.
struct Entered {
    before: Vec<Callback>,
    after: Vec<Callback>,
    fault: Option<Fault>,
}

/// A path remote that fails and runs callbacks when a test says so.
pub struct FaultRemote {
    inner: PathRemote,
    state: Mutex<State>,
}

impl FaultRemote {
    pub fn new(inner: PathRemote) -> FaultRemote {
        FaultRemote {
            inner,
            state: Mutex::new(State::default()),
        }
    }

    /// Makes the `nth` call of `op` fail with `fault` without doing it.
    pub fn fail(&self, op: Op, nth: usize, fault: Fault) {
        self.lock().faults.push((op, nth, fault));
    }

    /// Runs `callback` with the wrapped remote right before the `nth` call
    /// of `op`.
    pub fn before(
        &self,
        op: Op,
        nth: usize,
        callback: impl Fn(&PathRemote) + Send + Sync + 'static,
    ) {
        self.lock().before.push((op, nth, Arc::new(callback)));
    }

    /// Runs `callback` with the wrapped remote right after the `nth` call of
    /// `op` succeeded.
    pub fn after(
        &self,
        op: Op,
        nth: usize,
        callback: impl Fn(&PathRemote) + Send + Sync + 'static,
    ) {
        self.lock().after.push((op, nth, Arc::new(callback)));
    }

    /// Makes [`Remote::clock`] answer `now`.
    pub fn set_clock(&self, now: Timestamp) {
        self.lock().clock = Some(now);
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// How many calls of `op` there were.
    pub fn count(&self, op: Op) -> usize {
        self.lock().counts.get(&op).copied().unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("no test panics holding the state")
    }

    fn enter(&self, op: Op, name: &str) -> Entered {
        let mut state = self.lock();
        state.calls.push(Call::new(op, name));
        let nth = {
            let count = state.counts.entry(op).or_insert(0);
            *count += 1;
            *count
        };
        let matching = |hooks: &[(Op, usize, Callback)]| -> Vec<Callback> {
            hooks
                .iter()
                .filter(|(hooked, at, _)| *hooked == op && *at == nth)
                .map(|(_, _, callback)| Arc::clone(callback))
                .collect()
        };
        Entered {
            before: matching(&state.before),
            after: matching(&state.after),
            fault: state
                .faults
                .iter()
                .find(|(faulted, at, _)| *faulted == op && *at == nth)
                .map(|(_, _, fault)| *fault),
        }
    }

    /// Runs one call: the callbacks are run without the state locked, so
    /// they can call the wrapped remote and this one's setters.
    fn run<T>(
        &self,
        op: Op,
        name: &str,
        call: impl FnOnce(&PathRemote) -> Result<T, RemoteError>,
    ) -> Result<T, RemoteError> {
        let entered = self.enter(op, name);
        for callback in &entered.before {
            callback(&self.inner);
        }
        if let Some(fault) = entered.fault {
            return Err(fault.into_error(op, name));
        }
        let outcome = call(&self.inner);
        if outcome.is_ok() {
            for callback in &entered.after {
                callback(&self.inner);
            }
        }
        outcome
    }
}

impl Remote for FaultRemote {
    fn reachability(&self) -> Result<Assessment, RemoteError> {
        self.run(Op::Reachability, "", |inner| inner.reachability())
    }

    fn begin_phase(&self) -> Result<(), RemoteError> {
        self.run(Op::BeginPhase, "", |inner| inner.begin_phase())
    }

    fn clock(&self) -> Result<Timestamp, RemoteError> {
        self.run(Op::Clock, "", |inner| match self.lock().clock {
            Some(now) => Ok(now),
            None => inner.clock(),
        })
    }

    fn pull_metadata(&self, known: &KnownVersions) -> Result<Vec<MetaFile>, RemoteError> {
        self.run(Op::PullMetadata, "", |inner| inner.pull_metadata(known))
    }

    fn list_objects(&self) -> Result<Vec<ObjectId>, RemoteError> {
        self.run(Op::ListObjects, "", |inner| inner.list_objects())
    }

    fn list_dir(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
        self.run(Op::ListDir, "", |inner| inner.list_dir(id))
    }

    fn list_partial(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
        self.run(Op::ListPartial, "", |inner| inner.list_partial(id))
    }

    fn read_file(&self, id: ObjectId, name: &str) -> Result<Vec<u8>, RemoteError> {
        self.run(Op::ReadFile, name, |inner| inner.read_file(id, name))
    }

    fn get_payload(&self, id: ObjectId, name: &str, to: &Path) -> Result<Transfer, RemoteError> {
        self.run(Op::GetPayload, name, |inner| {
            inner.get_payload(id, name, to)
        })
    }

    fn create_exclusive(&self, id: ObjectId, name: &str) -> Result<Created, RemoteError> {
        self.run(Op::CreateExclusive, name, |inner| {
            inner.create_exclusive(id, name)
        })
    }

    fn write_temp(&self, id: ObjectId, temp: &str, bytes: &[u8]) -> Result<(), RemoteError> {
        self.run(Op::WriteTemp, temp, |inner| {
            inner.write_temp(id, temp, bytes)
        })
    }

    fn rename_into_place(
        &self,
        id: ObjectId,
        temp: &str,
        name: &str,
    ) -> Result<Placement, RemoteError> {
        self.run(Op::RenameIntoPlace, name, |inner| {
            inner.rename_into_place(id, temp, name)
        })
    }

    fn put_payload(&self, upload: &Upload<'_>) -> Result<Transfer, RemoteError> {
        self.run(Op::PutPayload, upload.final_name, |inner| {
            inner.put_payload(upload)
        })
    }

    fn delete(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
        self.run(Op::Delete, name, |inner| inner.delete(id, name))
    }

    fn delete_partial(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
        self.run(Op::DeletePartial, name, |inner| {
            inner.delete_partial(id, name)
        })
    }

    fn checksum(&self, id: ObjectId, name: &str) -> Result<String, RemoteError> {
        self.run(Op::Checksum, name, |inner| inner.checksum(id, name))
    }

    fn flush(&self) -> Result<(), RemoteError> {
        self.run(Op::Flush, "", |inner| inner.flush())
    }

    /// Logged and counted like the others; it cannot fail, so a fault set
    /// for it has no effect.
    fn finish(&self) {
        let entered = self.enter(Op::Finish, "");
        for callback in &entered.before {
            callback(&self.inner);
        }
        self.inner.finish();
        for callback in &entered.after {
            callback(&self.inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ErrorKind, HasKind};
    use jiff::SignedDuration;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn faulty() -> (tempfile::TempDir, FaultRemote) {
        let root = remote_root();
        let remote = FaultRemote::new(path_remote(root.path(), "laptop"));
        (root, remote)
    }

    fn id() -> ObjectId {
        object(1)
    }

    fn names(remote: &dyn Remote) -> Vec<String> {
        remote
            .list_dir(id())
            .expect("lists")
            .into_iter()
            .map(|file| file.name)
            .collect()
    }

    #[test]
    fn the_nth_call_of_an_operation_fails() {
        let (_root, remote) = faulty();
        remote.fail(Op::ListDir, 2, Fault::Io);
        assert!(remote.list_dir(id()).is_ok(), "the first call passes");
        let second = remote.list_dir(id());
        assert!(
            matches!(second, Err(RemoteError::Io { .. })),
            "the second call fails: {second:?}"
        );
        assert!(remote.list_dir(id()).is_ok(), "the third call passes");
    }

    #[test]
    fn an_io_fault_has_the_kind_other() {
        let (_root, remote) = faulty();
        remote.fail(Op::Flush, 1, Fault::Io);
        let error = remote.flush().expect_err("the fault fires");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn an_away_fault_reads_as_away() {
        let (_root, remote) = faulty();
        remote.fail(Op::Flush, 1, Fault::Away);
        let error = remote.flush().expect_err("the fault fires");
        assert_eq!(error.kind(), ErrorKind::RemoteAway);
    }

    #[test]
    fn a_broken_fault_reads_as_broken() {
        let (_root, remote) = faulty();
        remote.fail(Op::Flush, 1, Fault::Broken);
        let error = remote.flush().expect_err("the fault fires");
        assert_eq!(error.kind(), ErrorKind::RemoteBroken);
    }

    #[test]
    fn faults_apply_to_the_named_operation_only() {
        let (_root, remote) = faulty();
        remote.fail(Op::ReadFile, 1, Fault::Io);
        assert!(remote.list_dir(id()).is_ok());
        assert!(remote.flush().is_ok());
        assert!(remote.read_file(id(), "meta.1").is_err());
    }

    #[test]
    fn a_failed_call_does_not_happen() {
        let (_root, remote) = faulty();
        remote.fail(Op::CreateExclusive, 1, Fault::Io);
        assert!(remote.create_exclusive(id(), "meta.1.claim").is_err());
        assert_eq!(names(&remote), Vec::<String>::new());
    }

    #[test]
    fn a_callback_runs_before_the_call_it_names() {
        let (root, remote) = faulty();
        let other = path_remote(root.path(), "studio");
        remote.before(Op::CreateExclusive, 1, move |_| {
            other
                .create_exclusive(id(), "meta.1.claim")
                .expect("the other machine claims");
        });
        let created = remote
            .create_exclusive(id(), "meta.1.claim")
            .expect("the call goes through");
        assert_eq!(created, Created::AlreadyExists);
    }

    #[test]
    fn a_callback_runs_after_the_call_it_names() {
        let (_root, remote) = faulty();
        remote.after(Op::ListDir, 1, |inner| {
            inner
                .create_exclusive(id(), "meta.1.claim")
                .expect("the callback creates a file");
        });
        let listed = remote.list_dir(id()).expect("lists");
        assert!(listed.is_empty(), "the call did not see the file");
        assert_eq!(names(&remote), ["meta.1.claim"]);
    }

    #[test]
    fn a_callback_is_not_run_when_the_call_fails() {
        let (_root, remote) = faulty();
        let ran = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&ran);
        remote.fail(Op::ListDir, 1, Fault::Io);
        remote.after(Op::ListDir, 1, move |_| seen.store(true, Ordering::SeqCst));
        assert!(remote.list_dir(id()).is_err());
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn an_error_of_the_wrapped_remote_passes_through() {
        let (_root, remote) = faulty();
        let ran = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&ran);
        remote.after(Op::ReadFile, 1, move |_| seen.store(true, Ordering::SeqCst));
        let error = remote.read_file(id(), "meta.1").expect_err("no such file");
        assert!(
            matches!(error, RemoteError::Io { action: "read", .. }),
            "{error:?}"
        );
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn finish_runs_its_callbacks_around_the_call() {
        let (_root, remote) = faulty();
        let order = Arc::new(Mutex::new(Vec::new()));
        for (when, hook) in [("before", true), ("after", false)] {
            let order = Arc::clone(&order);
            let callback = move |_: &PathRemote| order.lock().expect("no panic").push(when);
            if hook {
                remote.before(Op::Finish, 1, callback);
            } else {
                remote.after(Op::Finish, 1, callback);
            }
        }
        remote.finish();
        assert_eq!(*order.lock().expect("no panic"), ["before", "after"]);
    }

    #[test]
    fn every_method_is_forwarded_and_logged() {
        let (root, remote) = faulty();
        let source = root.path().join("source");
        std::fs::write(&source, b"payload").expect("a source file");
        let sha256 = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
        let upload = Upload::new(id(), ".p.part", "p.tar.zst", &source, sha256);
        remote.reachability().expect("answers");
        remote.begin_phase().expect("begins");
        remote.pull_metadata(&KnownVersions::new()).expect("pulls");
        remote.put_payload(&upload).expect("puts");
        assert_eq!(remote.list_objects().expect("lists"), [id()]);
        assert_eq!(remote.checksum(id(), "p.tar.zst").expect("sums"), sha256);
        let copy = root.path().join("copy");
        remote.get_payload(id(), "p.tar.zst", &copy).expect("gets");
        assert_eq!(std::fs::read(&copy).expect("a copy"), b"payload");
        assert_eq!(
            remote.read_file(id(), "p.tar.zst").expect("reads"),
            b"payload"
        );
        assert!(remote.list_partial(id()).expect("lists").is_empty());
        remote.delete_partial(id(), "absent").expect("deletes");
        remote.delete(id(), "p.tar.zst").expect("deletes");
        let ops: Vec<Op> = remote.calls().iter().map(|call| call.op).collect();
        assert_eq!(
            ops,
            [
                Op::Reachability,
                Op::BeginPhase,
                Op::PullMetadata,
                Op::PutPayload,
                Op::ListObjects,
                Op::Checksum,
                Op::GetPayload,
                Op::ReadFile,
                Op::ListPartial,
                Op::DeletePartial,
                Op::Delete
            ]
        );
        assert!(names(&remote).is_empty(), "the delete reached the files");
    }

    #[test]
    fn calls_are_logged_in_order_with_the_name_they_were_given() {
        let (_root, remote) = faulty();
        remote.list_dir(id()).expect("lists");
        remote
            .create_exclusive(id(), "meta.1.claim")
            .expect("creates");
        remote
            .write_temp(id(), ".meta.1.laptop.part", b"x")
            .expect("writes");
        remote
            .rename_into_place(id(), ".meta.1.laptop.part", "meta.1")
            .expect("renames");
        remote.flush().expect("flushes");
        assert_eq!(
            remote.calls(),
            [
                Call::new(Op::ListDir, ""),
                Call::new(Op::CreateExclusive, "meta.1.claim"),
                Call::new(Op::WriteTemp, ".meta.1.laptop.part"),
                Call::new(Op::RenameIntoPlace, "meta.1"),
                Call::new(Op::Flush, ""),
            ]
        );
    }

    #[test]
    fn calls_are_counted_per_operation() {
        let (_root, remote) = faulty();
        remote.list_dir(id()).expect("lists");
        remote.list_dir(id()).expect("lists");
        remote.flush().expect("flushes");
        remote.finish();
        assert_eq!(remote.count(Op::ListDir), 2);
        assert_eq!(remote.count(Op::Flush), 1);
        assert_eq!(remote.count(Op::Finish), 1);
        assert_eq!(remote.count(Op::Delete), 0);
    }

    #[test]
    fn a_failed_call_is_logged() {
        let (_root, remote) = faulty();
        remote.fail(Op::Flush, 1, Fault::Io);
        let _ = remote.flush();
        assert_eq!(remote.calls(), [Call::new(Op::Flush, "")]);
    }

    #[test]
    fn the_clock_defaults_to_the_wrapped_remote() {
        let (root, remote) = faulty();
        let wrapped = path_remote(root.path(), "studio")
            .clock()
            .expect("the path remote answers");
        let now = remote.clock().expect("answers");
        assert!(
            now.duration_since(wrapped).abs() < SignedDuration::from_secs(5),
            "{now} against {wrapped}"
        );
    }

    #[test]
    fn the_clock_can_be_set() {
        let (_root, remote) = faulty();
        let set: Timestamp = "2030-01-01T00:00:00Z".parse().expect("a time");
        remote.set_clock(set);
        assert_eq!(remote.clock().expect("answers"), set);
        assert_eq!(remote.count(Op::Clock), 1);
    }

    #[test]
    fn two_machines_on_one_root_see_the_same_files() {
        let (root, remote) = faulty();
        let other = path_remote(root.path(), "studio");
        other
            .create_exclusive(id(), "meta.1.claim")
            .expect("creates");
        assert_eq!(names(&remote), ["meta.1.claim"]);
    }
}
