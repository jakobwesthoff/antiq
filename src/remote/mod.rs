// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Remotes.

pub mod away;
pub mod cleanup;
pub mod commit;
pub mod marker;
pub mod path;
pub mod policy;
pub mod rsync;
pub mod ssh;
#[cfg(test)]
pub mod testing;
pub mod url;

use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::fsx::FsxError;
use crate::fsx::atomic::{flush_dir, flush_file};
use serde::Serialize;
use std::fs::File;
use std::path::Path;

/// How the path remote makes what it wrote durable. The real one flushes
/// with `sync_all` and falls back to plain `fsync` where a filesystem
/// refuses that (`fsx::atomic`); tests put a recorder in its place to see
/// in which order the flushes and the renames happen.
#[allow(
    dead_code,
    reason = "the path remote, in the next commits, is the caller"
)]
pub(crate) trait Durable: Send + Sync {
    /// Makes the data of an open file durable. `path` names it in errors.
    fn file(&self, file: &File, path: &Path) -> Result<(), FsxError>;
    /// Makes the entries of a directory durable.
    fn dir(&self, dir: &Path) -> Result<(), FsxError>;
}

#[allow(
    dead_code,
    reason = "the path remote, in the next commits, is the caller"
)]
pub(crate) struct RealDurable;

impl Durable for RealDurable {
    fn file(&self, file: &File, path: &Path) -> Result<(), FsxError> {
        flush_file(file, path)
    }

    fn dir(&self, dir: &Path) -> Result<(), FsxError> {
        flush_dir(dir)
    }
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

    type Observer = Box<dyn Fn(&Flush, Option<&File>) + Send + Sync>;

    #[derive(Default)]
    pub(crate) struct Recording {
        flushes: Mutex<Vec<Flush>>,
        observer: Mutex<Option<Observer>>,
        fail_files: Mutex<bool>,
        fail_dirs: Mutex<bool>,
    }

    impl Recording {
        pub(crate) fn new() -> Recording {
            Recording::default()
        }

        /// Called with every flush before it is recorded, so a test can
        /// read the files as they are at that moment.
        pub(crate) fn observe(
            &self,
            observer: impl Fn(&Flush, Option<&File>) + Send + Sync + 'static,
        ) {
            *self.observer.lock().expect("no test panics holding it") = Some(Box::new(observer));
        }

        pub(crate) fn fail_files(&self) {
            *self.fail_files.lock().expect("no test panics holding it") = true;
        }

        pub(crate) fn fail_dirs(&self) {
            *self.fail_dirs.lock().expect("no test panics holding it") = true;
        }

        pub(crate) fn flushes(&self) -> Vec<Flush> {
            self.flushes
                .lock()
                .expect("no test panics holding it")
                .clone()
        }

        fn record(&self, flush: Flush, file: Option<&File>) {
            if let Some(observer) = &*self.observer.lock().expect("no test panics holding it") {
                observer(&flush, file);
            }
            self.flushes
                .lock()
                .expect("no test panics holding it")
                .push(flush);
        }
    }

    fn eio(path: &Path) -> FsxError {
        FsxError::Io {
            action: "flush",
            path: path.display().to_string(),
            source: std::io::Error::from_raw_os_error(5),
        }
    }

    impl Durable for Recording {
        fn file(&self, file: &File, path: &Path) -> Result<(), FsxError> {
            self.record(Flush::File(path.to_path_buf()), Some(file));
            if *self.fail_files.lock().expect("no test panics holding it") {
                return Err(eio(path));
            }
            Ok(())
        }

        fn dir(&self, dir: &Path) -> Result<(), FsxError> {
            self.record(Flush::Dir(dir.to_path_buf()), None);
            if *self.fail_dirs.lock().expect("no test panics holding it") {
                return Err(eio(dir));
            }
            Ok(())
        }
    }
}

/// What `status` reports about the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteState {
    None,
    Reachable,
    Away,
    Broken,
}

/// The three answers to "can the remote be used right now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    Reachable,
    Away,
    Broken,
}

impl From<Reachability> for RemoteState {
    fn from(reachability: Reachability) -> RemoteState {
        match reachability {
            Reachability::Reachable => RemoteState::Reachable,
            Reachability::Away => RemoteState::Away,
            Reachability::Broken => RemoteState::Broken,
        }
    }
}

/// Looks at the remote of a collection without changing anything.
pub fn probe_state(_collection: &Collection) -> RemoteState {
    RemoteState::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remote_state_serialises_in_lower_case() {
        let names: Vec<String> = [
            RemoteState::None,
            RemoteState::Reachable,
            RemoteState::Away,
            RemoteState::Broken,
        ]
        .iter()
        .map(|state| serde_json::to_string(state).expect("a unit variant serialises"))
        .collect();
        assert_eq!(
            names,
            [r#""none""#, r#""reachable""#, r#""away""#, r#""broken""#]
        );
    }

    #[test]
    fn a_reachability_is_the_state_of_an_attached_remote() {
        let states = [
            (Reachability::Reachable, RemoteState::Reachable),
            (Reachability::Away, RemoteState::Away),
            (Reachability::Broken, RemoteState::Broken),
        ];
        for (reachability, state) in states {
            assert_eq!(RemoteState::from(reachability), state);
        }
    }

    #[test]
    fn no_remote_is_probed_yet() {
        assert_eq!(probe_state(&Collection::for_tests()), RemoteState::None);
    }
}

/// Errors of remotes.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("no remote is attached")]
    NoRemote,
    /// The remote cannot be reached right now. `overdue` says it has been
    /// away for longer than `away_warn_days` while remote work is pending.
    #[error("{message}")]
    Away { message: String, overdue: bool },
    #[error("{0}")]
    Broken(String),
    #[error("{0} is not supported yet")]
    Unsupported(String),
}

impl HasKind for RemoteError {
    fn kind(&self) -> ErrorKind {
        match self {
            RemoteError::NoRemote => ErrorKind::NoRemote,
            RemoteError::Away { .. } => ErrorKind::RemoteAway,
            RemoteError::Broken(_) => ErrorKind::RemoteBroken,
            RemoteError::Unsupported(_) => ErrorKind::Unsupported,
        }
    }
}
