// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Atomic file writes.
//!
//! Callers hold the run lock; see the module docs of [`crate::fsx`].

use super::{FsxError, io_error};
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
}
