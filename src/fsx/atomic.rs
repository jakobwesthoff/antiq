// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Atomic file writes.
//!
//! Callers hold the run lock; see the module docs of [`crate::fsx`].

use super::tree::TreeRoot;
use super::{FsxError, failure, io_error};
use crate::relpath::{Name, RelPath};
use crate::tree::EntryKind;
use rustix::io::Errno;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// One way of making a file's data durable.
type Flush<'a> = &'a dyn Fn(&File) -> io::Result<()>;

/// Makes the file's data durable: the full flush of the platform
/// (`F_FULLFSYNC` on macOS), and when the filesystem refuses that with
/// `ENOTSUP` (smbfs does), a plain `fsync`.
///
/// `sync` and `fsync` are the two calls, passed in so that tests can make
/// the first one fail. Only the raw errno `ENOTSUP` falls back: on macOS
/// `ErrorKind::Unsupported` stands for a different errno.
fn flush_with(file: &File, sync: Flush<'_>, fsync: Flush<'_>) -> io::Result<()> {
    match sync(file) {
        Err(error) if error.raw_os_error() == Some(Errno::NOTSUP.raw_os_error()) => fsync(file),
        other => other,
    }
}

fn real_sync(file: &File) -> io::Result<()> {
    file.sync_all()
}

fn real_fsync(file: &File) -> io::Result<()> {
    Ok(rustix::fs::fsync(file)?)
}

/// Makes the data of an open file durable. `path` names the file in the
/// error.
pub(crate) fn flush_file(file: &File, path: &Path) -> Result<(), FsxError> {
    flush_with(file, &real_sync, &real_fsync).map_err(io_error("flush", path))
}

/// Makes the entries of a directory durable.
pub(crate) fn flush_dir(dir: &Path) -> Result<(), FsxError> {
    flush_dir_with(dir, &real_sync, &real_fsync)
}

fn flush_dir_with(dir: &Path, sync: Flush<'_>, fsync: Flush<'_>) -> Result<(), FsxError> {
    let handle = File::open(dir).map_err(io_error("flush directory", dir))?;
    flush_with(&handle, sync, fsync).map_err(io_error("flush directory", dir))
}

/// How the store operations make what they wrote durable. The real one
/// flushes with [`flush_file`]; tests put a recorder in its place
/// ([`recording::Recording`]) to see in which order the flushes and the
/// renames happen and what the disk holds at each flush.
///
/// Both methods take the open handle, so a path deeper than the system
/// limit is flushed like any other. `path` only names the target in the
/// error and in the record.
#[allow(
    dead_code,
    reason = "used by the versions store (move_in and the other operations)"
)]
pub(crate) trait Durable: Send + Sync {
    /// Makes the data of an open file durable.
    fn file(&self, file: &File, path: &Path) -> Result<(), FsxError>;
    /// Makes the entries of an open directory durable.
    fn dir(&self, dir: &File, path: &Path) -> Result<(), FsxError>;
}

/// The flushing of the platform ([`flush_file`]).
#[allow(
    dead_code,
    reason = "used by the versions store (move_in and the other operations)"
)]
pub(crate) struct RealDurable;

impl Durable for RealDurable {
    fn file(&self, file: &File, path: &Path) -> Result<(), FsxError> {
        flush_file(file, path)
    }

    fn dir(&self, dir: &File, path: &Path) -> Result<(), FsxError> {
        flush_with(dir, &real_sync, &real_fsync).map_err(io_error("flush directory", path))
    }
}

/// How an error or a record names `path` below `root`: the path itself,
/// or `.` for the root.
#[allow(
    dead_code,
    reason = "used by the versions store (move_in and the other operations)"
)]
pub(crate) fn named(root: &TreeRoot, path: &RelPath) -> std::path::PathBuf {
    let shown = root.prefix().concat(path);
    if shown.is_root() {
        return std::path::PathBuf::from(".");
    }
    std::path::PathBuf::from(shown.to_string())
}

/// Makes the directory `dir` of `root` durable.
#[allow(
    dead_code,
    reason = "used by the versions store (move_in and the other operations)"
)]
pub(crate) fn flush_dir_of(
    durable: &dyn Durable,
    root: &TreeRoot,
    dir: &RelPath,
) -> Result<(), FsxError> {
    let handle = root
        .open_dir_file(dir)
        .map_err(failure("open directory", root, dir))?;
    durable.dir(&handle, &named(root, dir))
}

/// Creates the directories of `chain` below `base` that are missing, and
/// makes every directory of the chain durable together with its parent,
/// also those that existed before: an earlier run may have created one
/// and failed to flush it, and the archive step that follows relies on
/// the whole chain being there after a crash. `base` itself must exist.
///
/// A component that exists as anything but a directory, a symlink to a
/// directory included, is an error.
#[allow(
    dead_code,
    reason = "used by the versions store (move_in and the other operations)"
)]
pub(crate) fn create_dir_chain(
    durable: &dyn Durable,
    base: &TreeRoot,
    chain: &RelPath,
) -> Result<(), FsxError> {
    let mut parent = RelPath::root();
    for component in chain.components() {
        let name = Name::from_bytes(component.to_vec()).expect("a component of a path is a name");
        let path = parent.join(&name);
        ensure_dir(base, &path, &|path| base.create_dir(path, 0o777))?;
        flush_dir_of(durable, base, &path)?;
        flush_dir_of(durable, base, &parent)?;
        parent = path;
    }
    Ok(())
}

/// Makes sure the directory `path` of `base` exists. `create` makes it
/// when `lstat` finds nothing there.
///
/// Pieces are archived in parallel and two of them can share a parent
/// that is new, so a `create` that fails because the name exists now is
/// not an error when what exists is a directory.
fn ensure_dir(
    base: &TreeRoot,
    path: &RelPath,
    create: &dyn Fn(&RelPath) -> io::Result<()>,
) -> Result<(), FsxError> {
    let not_a_directory = || {
        let source = io::Error::from(io::ErrorKind::NotADirectory);
        failure("create directory", base, path)(source)
    };
    match base.lstat(path) {
        Ok(entry) if entry.kind == EntryKind::Directory => Ok(()),
        Ok(_) => Err(not_a_directory()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match create(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match base.lstat(path) {
                Ok(entry) if entry.kind == EntryKind::Directory => Ok(()),
                _ => Err(not_a_directory()),
            },
            Err(error) => Err(failure("create directory", base, path)(error)),
        },
        Err(error) => Err(failure("look at", base, path)(error)),
    }
}

/// Replaces the file at `path` with `bytes`, or creates it.
///
/// The bytes go into a temp file in the same directory, are flushed, and
/// are renamed over the target; then the directory is flushed. A reader
/// sees the old content or the new, never a mixture. The new file gets
/// the default mode (`0666` without the umask); the mode of an old target
/// is not carried over.
///
/// A failure up to and including the rename removes the temp file and
/// leaves the target as it was. When only the final directory flush
/// fails, the target already holds the new bytes: the error then means
/// "written, durability uncertain".
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FsxError> {
    write_atomic_with(path, bytes, &real_sync, &real_fsync)
}

pub(crate) fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    sync: Flush<'_>,
    fsync: Flush<'_>,
) -> Result<(), FsxError> {
    let (dir, name) = split_target(path).map_err(io_error("write", path))?;
    let temp = dir.join(temp_name(name));

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(io_error("create", &temp))?;
    let staged = file
        .write_all(bytes)
        .map_err(io_error("write", &temp))
        .and_then(|()| flush_with(&file, sync, fsync).map_err(io_error("flush", &temp)))
        .and_then(|()| fs::rename(&temp, path).map_err(io_error("replace", path)));
    if let Err(error) = staged {
        // Best effort: the failure being reported matters more than a
        // temp file that cannot be removed either.
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    flush_dir_with(dir, sync, fsync)
}

/// The directory a file is written into and its name.
fn split_target(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path has no file name"))?;
    // A bare name has the empty path as its parent; the directory then is
    // the current one.
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok((dir, name))
}

/// A hidden name next to the target that no other write of this process
/// uses, and that another process's write does not either as the pid is
/// part of it.
fn temp_name(target: &OsStr) -> OsString {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = OsString::from(".");
    name.push(target);
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    name
}

/// A [`Durable`] that records every flush and lets a test look at the
/// state of the disk at that moment or make a flush fail.
#[cfg(test)]
pub(crate) mod recording {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Flush {
        File(PathBuf),
        Dir(PathBuf),
    }

    type Observer = Box<dyn Fn(&Flush, &File) + Send + Sync>;

    #[derive(Default)]
    pub(crate) struct Recording {
        flushes: Mutex<Vec<Flush>>,
        observer: Mutex<Option<Observer>>,
        /// File flushes from this one on (counted from 0) fail.
        fail_files_from: Mutex<Option<usize>>,
        /// Directory flushes from this one on (counted from 0) fail.
        fail_dirs_from: Mutex<Option<usize>>,
    }

    impl Recording {
        pub(crate) fn new() -> Recording {
            Recording::default()
        }

        /// Called with every flush before it is recorded, so a test can
        /// read the files as they are at that moment.
        pub(crate) fn observe(&self, observer: impl Fn(&Flush, &File) + Send + Sync + 'static) {
            *self.observer.lock().expect("no test panics holding it") = Some(Box::new(observer));
        }

        /// Makes the file flushes fail from the `n`th one on, counted from
        /// 0 over the file flushes only.
        pub(crate) fn fail_files_from(&self, n: usize) {
            *self
                .fail_files_from
                .lock()
                .expect("no test panics holding it") = Some(n);
        }

        /// Makes the directory flushes fail from the `n`th one on, counted
        /// from 0 over the directory flushes only, so a test can let the
        /// flushes of its setup pass and fail the one it is about.
        pub(crate) fn fail_dirs_from(&self, n: usize) {
            *self
                .fail_dirs_from
                .lock()
                .expect("no test panics holding it") = Some(n);
        }

        pub(crate) fn flushes(&self) -> Vec<Flush> {
            self.flushes
                .lock()
                .expect("no test panics holding it")
                .clone()
        }

        fn record(
            &self,
            flush: Flush,
            handle: &File,
            fail_from: &Mutex<Option<usize>>,
            same_kind: fn(&Flush) -> bool,
        ) -> Result<(), FsxError> {
            if let Some(observer) = &*self.observer.lock().expect("no test panics holding it") {
                observer(&flush, handle);
            }
            let mut flushes = self.flushes.lock().expect("no test panics holding it");
            let earlier = flushes.iter().filter(|flush| same_kind(flush)).count();
            let path = match &flush {
                Flush::File(path) | Flush::Dir(path) => path.clone(),
            };
            flushes.push(flush);
            let from = *fail_from.lock().expect("no test panics holding it");
            if from.is_some_and(|from| earlier >= from) {
                return Err(FsxError::Io {
                    action: "flush",
                    path: path.display().to_string(),
                    source: io::Error::from_raw_os_error(Errno::IO.raw_os_error()),
                });
            }
            Ok(())
        }
    }

    impl Durable for Recording {
        fn file(&self, file: &File, path: &Path) -> Result<(), FsxError> {
            self.record(
                Flush::File(path.to_path_buf()),
                file,
                &self.fail_files_from,
                |flush| matches!(flush, Flush::File(_)),
            )
        }

        fn dir(&self, dir: &File, path: &Path) -> Result<(), FsxError> {
            self.record(
                Flush::Dir(path.to_path_buf()),
                dir,
                &self.fail_dirs_from,
                |flush| matches!(flush, Flush::Dir(_)),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::Tree;
    use std::cell::RefCell;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Call {
        FileFull,
        FilePlain,
        DirFull,
        DirPlain,
    }

    fn kind_of(file: &File, full: bool) -> Call {
        let is_dir = file.metadata().expect("metadata").is_dir();
        match (is_dir, full) {
            (false, true) => Call::FileFull,
            (false, false) => Call::FilePlain,
            (true, true) => Call::DirFull,
            (true, false) => Call::DirPlain,
        }
    }

    fn notsup() -> io::Error {
        io::Error::from_raw_os_error(Errno::NOTSUP.raw_os_error())
    }

    fn eio() -> io::Error {
        io::Error::from_raw_os_error(Errno::IO.raw_os_error())
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir)
            .expect("the directory is listed")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn a_new_file_gets_the_bytes() {
        let mut tree = Tree::new();
        let dir = tree.dir("d");
        write_atomic(&dir.join("f"), b"hello").expect("the write succeeds");
        assert_eq!(fs::read(dir.join("f")).expect("exists"), b"hello");
    }

    #[test]
    fn an_existing_file_is_replaced_and_no_temp_file_stays() {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "old content, longer than the new one");
        write_atomic(&target, b"new").expect("the write succeeds");
        assert_eq!(fs::read(&target).expect("exists"), b"new");
        assert_eq!(names(target.parent().expect("parent")), ["f"]);
    }

    #[test]
    fn the_target_keeps_its_old_content_while_the_temp_file_is_flushed() {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "old");
        let observed = RefCell::new(None);
        let sync = |file: &File| {
            if !file.metadata().expect("metadata").is_dir() {
                *observed.borrow_mut() = Some((
                    names(target.parent().expect("parent")),
                    fs::read(&target).expect("exists"),
                ));
            }
            Ok(())
        };
        write_atomic_with(&target, b"new", &sync, &real_fsync).expect("the write succeeds");
        let (during, content) = observed.into_inner().expect("the file flush ran");
        assert_eq!(during.len(), 2, "the target and a temp file: {during:?}");
        assert!(during.iter().any(|name| name.starts_with(".f.")));
        assert_eq!(content, b"old");
    }

    #[test]
    fn the_file_is_flushed_before_the_directory_and_after_the_content_is_in_place() {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "old");
        let calls = RefCell::new(Vec::new());
        let sync = |file: &File| {
            let call = kind_of(file, true);
            let content = fs::read(&target).expect("exists");
            calls.borrow_mut().push((call, content));
            Ok(())
        };
        write_atomic_with(&target, b"new", &sync, &real_fsync).expect("the write succeeds");
        assert_eq!(
            calls.into_inner(),
            [
                (Call::FileFull, b"old".to_vec()),
                (Call::DirFull, b"new".to_vec())
            ]
        );
    }

    fn run_with_notsup_on(fail_dir: bool) -> Vec<Call> {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "old");
        let calls = RefCell::new(Vec::new());
        let sync = |file: &File| {
            let call = kind_of(file, true);
            calls.borrow_mut().push(call);
            let is_dir = call == Call::DirFull;
            if is_dir == fail_dir {
                Err(notsup())
            } else {
                Ok(())
            }
        };
        let fsync = |file: &File| {
            calls.borrow_mut().push(kind_of(file, false));
            Ok(())
        };
        write_atomic_with(&target, b"new", &sync, &fsync).expect("the fallback makes it succeed");
        assert_eq!(fs::read(&target).expect("exists"), b"new");
        calls.into_inner()
    }

    #[test]
    fn a_file_flush_refused_with_enotsup_falls_back_to_plain_fsync() {
        assert_eq!(
            run_with_notsup_on(false),
            [Call::FileFull, Call::FilePlain, Call::DirFull]
        );
    }

    #[test]
    fn a_directory_flush_refused_with_enotsup_falls_back_to_plain_fsync() {
        assert_eq!(
            run_with_notsup_on(true),
            [Call::FileFull, Call::DirFull, Call::DirPlain]
        );
    }

    #[test]
    fn an_unsupported_error_without_the_enotsup_errno_does_not_fall_back() {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "old");
        let sync = |_: &File| Err(io::Error::from(io::ErrorKind::Unsupported));
        let fsync = |_: &File| panic!("the fallback must not run");
        let error = write_atomic_with(&target, b"new", &sync, &fsync).expect_err("fails");
        assert!(
            matches!(
                error,
                FsxError::Io {
                    action: "flush",
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(fs::read(&target).expect("exists"), b"old");
    }

    #[test]
    fn a_failing_file_flush_removes_the_temp_file_and_keeps_the_old_content() {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "old");
        let sync = |_: &File| Err(eio());
        let error = write_atomic_with(&target, b"new", &sync, &real_fsync).expect_err("fails");
        assert!(
            matches!(
                error,
                FsxError::Io {
                    action: "flush",
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(fs::read(&target).expect("exists"), b"old");
        assert_eq!(names(target.parent().expect("parent")), ["f"]);
    }

    #[test]
    fn a_failing_directory_flush_reports_an_error_but_the_new_bytes_are_in_place() {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "old");
        let sync = |file: &File| {
            if file.metadata().expect("metadata").is_dir() {
                Err(eio())
            } else {
                Ok(())
            }
        };
        let error = write_atomic_with(&target, b"new", &sync, &real_fsync).expect_err("fails");
        assert!(
            matches!(
                error,
                FsxError::Io {
                    action: "flush directory",
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(fs::read(&target).expect("exists"), b"new");
        assert_eq!(names(target.parent().expect("parent")), ["f"]);
    }

    #[test]
    fn a_missing_directory_fails_and_the_error_names_the_path() {
        let tree = Tree::new();
        let target = tree.path("missing/f");
        let error = write_atomic(&target, b"x").expect_err("fails");
        assert!(error.to_string().contains("missing"), "{error}");
        assert!(
            matches!(
                error,
                FsxError::Io {
                    action: "create",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_path_without_a_file_name_is_refused() {
        let error = write_atomic(Path::new("/"), b"x").expect_err("fails");
        assert!(
            matches!(
                error,
                FsxError::Io {
                    action: "write",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_target_whose_rename_fails_leaves_no_temp_file() {
        let mut tree = Tree::new();
        let directory = tree.dir("d/f");
        let error = write_atomic(&directory, b"x").expect_err("a directory is not replaced");
        assert!(
            matches!(
                error,
                FsxError::Io {
                    action: "replace",
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(names(directory.parent().expect("parent")), ["f"]);
    }

    #[test]
    fn a_bare_name_is_written_into_the_current_directory() {
        let (dir, name) = split_target(Path::new("plain")).expect("splits");
        assert_eq!((dir, name), (Path::new("."), OsStr::new("plain")));
        let (dir, name) = split_target(Path::new("a/plain")).expect("splits");
        assert_eq!((dir, name), (Path::new("a"), OsStr::new("plain")));
    }

    #[test]
    fn the_plain_fsync_of_the_fallback_works_on_files_and_directories() {
        let mut tree = Tree::new();
        let path = tree.file("d/f", "x");
        real_fsync(&File::open(&path).expect("opens")).expect("a file");
        real_fsync(&File::open(path.parent().expect("parent")).expect("opens")).expect("a dir");
    }

    #[test]
    fn flush_file_and_flush_dir_work_on_real_handles() {
        let mut tree = Tree::new();
        let path = tree.file("d/f", "x");
        let file = OpenOptions::new().write(true).open(&path).expect("opens");
        flush_file(&file, &path).expect("a file flushes");
        flush_dir(path.parent().expect("parent")).expect("a directory flushes");
        let error = flush_dir(&tree.path("nope")).expect_err("a missing directory fails");
        assert!(matches!(
            error,
            FsxError::Io {
                action: "flush directory",
                ..
            }
        ));
    }

    #[test]
    fn temp_names_differ_between_calls() {
        let mut tree = Tree::new();
        let target = tree.file("d/f", "x");
        let seen = RefCell::new(Vec::new());
        let sync = |_: &File| {
            seen.borrow_mut()
                .push(names(target.parent().expect("parent")));
            Ok(())
        };
        write_atomic_with(&target, b"1", &sync, &real_fsync).expect("first");
        write_atomic_with(&target, b"2", &sync, &real_fsync).expect("second");
        let seen = seen.into_inner();
        assert_ne!(seen[0], seen[2], "{seen:?}");
    }

    use super::recording::{Flush, Recording};
    use std::os::unix::fs::PermissionsExt;

    fn root_of(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("the root opens")
    }

    fn chain(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    fn dirs(recording: &Recording) -> Vec<String> {
        recording
            .flushes()
            .iter()
            .map(|flush| match flush {
                Flush::Dir(path) => path.display().to_string(),
                Flush::File(path) => format!("file {}", path.display()),
            })
            .collect()
    }

    #[test]
    fn a_missing_chain_is_created_and_each_directory_flushed_with_its_parent() {
        let tree = Tree::new();
        let recording = Recording::new();
        create_dir_chain(&recording, &root_of(&tree), &chain("a/b/c")).expect("created");
        assert!(tree.path("a/b/c").is_dir());
        assert_eq!(dirs(&recording), ["a", ".", "a/b", "a", "a/b/c", "a/b"]);
    }

    #[test]
    fn an_existing_prefix_is_flushed_again() {
        let mut tree = Tree::new();
        tree.dir("a");
        let recording = Recording::new();
        create_dir_chain(&recording, &root_of(&tree), &chain("a/b")).expect("created");
        assert_eq!(dirs(&recording), ["a", ".", "a/b", "a"]);
    }

    #[test]
    fn a_complete_existing_chain_is_flushed_and_left_alone() {
        let mut tree = Tree::new();
        tree.dir("a/b");
        tree.file("a/b/keep", b"x");
        let recording = Recording::new();
        create_dir_chain(&recording, &root_of(&tree), &chain("a/b")).expect("ok");
        assert_eq!(dirs(&recording), ["a", ".", "a/b", "a"]);
        assert!(tree.path("a/b/keep").is_file());
    }

    #[test]
    fn an_empty_chain_does_nothing() {
        let tree = Tree::new();
        let recording = Recording::new();
        create_dir_chain(&recording, &root_of(&tree), &RelPath::root()).expect("ok");
        assert!(recording.flushes().is_empty());
    }

    #[test]
    fn a_file_in_the_chain_is_refused() {
        let mut tree = Tree::new();
        tree.file("a", b"x");
        let recording = Recording::new();
        let error =
            create_dir_chain(&recording, &root_of(&tree), &chain("a/b")).expect_err("a file");
        assert!(error.to_string().contains("create directory a"), "{error}");
        assert!(tree.path("a").is_file());
    }

    #[test]
    fn a_symlink_to_a_directory_in_the_chain_is_refused() {
        let mut tree = Tree::new();
        tree.dir("real");
        tree.symlink("a", "real");
        let recording = Recording::new();
        create_dir_chain(&recording, &root_of(&tree), &chain("a/b")).expect_err("a symlink");
        assert!(!tree.path("real/b").exists());
    }

    #[test]
    fn a_missing_base_is_an_error() {
        let tree = Tree::new();
        let base = TreeRoot::open(tree.root()).expect("opens");
        std::fs::remove_dir(tree.root()).expect("an empty root can go");
        let recording = Recording::new();
        let error = create_dir_chain(&recording, &base, &chain("a")).expect_err("no base");
        assert!(matches!(error, FsxError::Io { .. }), "{error}");
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_an_error() {
        let mut tree = Tree::new();
        let locked = tree.dir("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("mode");
        let _restore = super::super::RestoreModes(vec![locked]);
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let recording = Recording::new();
        let error =
            create_dir_chain(&recording, &root_of(&tree), &chain("locked/b")).expect_err("denied");
        assert!(
            error.to_string().contains("create directory locked/b"),
            "{error}"
        );
        assert!(
            recording.flushes().len() == 2,
            "only locked and its parent were flushed"
        );
    }

    #[test]
    fn a_directory_that_appears_while_it_is_created_is_accepted() {
        let tree = Tree::new();
        let base = root_of(&tree);
        let racing = |path: &RelPath| {
            base.create_dir(path, 0o777)
                .expect("the other piece creates it first");
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        };
        ensure_dir(&base, &chain("a"), &racing).expect("a directory is fine");
        assert!(tree.path("a").is_dir());
    }

    #[test]
    fn a_file_that_appears_while_a_directory_is_created_is_refused() {
        let tree = Tree::new();
        let base = root_of(&tree);
        let racing = |path: &RelPath| {
            drop(
                base.create_file(path, 0o644)
                    .expect("another process creates a file"),
            );
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        };
        let error = ensure_dir(&base, &chain("a"), &racing).expect_err("a file");
        assert!(
            matches!(&error, FsxError::Io { source, .. } if source.kind() == io::ErrorKind::NotADirectory),
            "{error}"
        );
    }

    #[test]
    fn another_failure_to_create_is_passed_on() {
        let tree = Tree::new();
        let base = root_of(&tree);
        let denied = |_: &RelPath| Err(io::Error::from_raw_os_error(Errno::ACCESS.raw_os_error()));
        let error = ensure_dir(&base, &chain("a"), &denied).expect_err("denied");
        assert!(
            matches!(
                error,
                FsxError::Io {
                    action: "create directory",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_failing_flush_of_the_new_directory_stops_the_chain() {
        let tree = Tree::new();
        let recording = Recording::new();
        recording.fail_dirs_from(0);
        create_dir_chain(&recording, &root_of(&tree), &chain("a/b")).expect_err("flush fails");
        assert!(tree.path("a").is_dir());
        assert!(!tree.path("a/b").exists());
    }

    #[test]
    fn a_failing_flush_of_the_parent_stops_the_chain() {
        let tree = Tree::new();
        let recording = Recording::new();
        recording.fail_dirs_from(1);
        create_dir_chain(&recording, &root_of(&tree), &chain("a/b")).expect_err("flush fails");
        assert!(!tree.path("a/b").exists());
        assert_eq!(dirs(&recording), ["a", "."]);
    }

    #[test]
    fn the_recording_sees_the_disk_and_fails_files_from_the_nth() {
        let mut tree = Tree::new();
        let target = tree.file("f", b"x");
        let recording = Recording::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let watched = target.clone();
        recording.observe(move |flush, _| {
            sink.lock()
                .expect("not poisoned")
                .push((flush.clone(), watched.exists()));
        });
        recording.fail_files_from(1);
        let file = File::open(&target).expect("opens");
        recording
            .file(&file, Path::new("f"))
            .expect("the first passes");
        recording
            .file(&file, Path::new("f"))
            .expect_err("the second fails");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            [
                (Flush::File("f".into()), true),
                (Flush::File("f".into()), true)
            ]
        );
    }

    #[test]
    fn the_real_durable_flushes_a_file_and_a_directory() {
        let mut tree = Tree::new();
        let target = tree.file("f", b"x");
        let file = File::open(&target).expect("opens");
        RealDurable.file(&file, &target).expect("flushed");
        let dir = File::open(tree.root()).expect("opens");
        RealDurable.dir(&dir, tree.root()).expect("flushed");
    }
}
