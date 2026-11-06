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
use path::PathRemote;
use serde::Serialize;
use std::fs::File;
use std::path::Path;
use url::{RemoteUrl, UrlError};

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

/// The answer to "can the remote be used right now", with the reason when
/// it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub reachability: Reachability,
    /// Says what is wrong, naming the place; empty while the remote is
    /// reachable.
    pub reason: String,
}

impl Assessment {
    pub fn new(reachability: Reachability, reason: impl Into<String>) -> Assessment {
        Assessment {
            reachability,
            reason: reason.into(),
        }
    }

    /// Ok while the remote is reachable, else the error that says why not.
    pub fn into_result(self) -> Result<(), RemoteError> {
        match self.reachability {
            Reachability::Reachable => Ok(()),
            Reachability::Away => Err(RemoteError::Away {
                message: self.reason,
                overdue: false,
            }),
            Reachability::Broken => Err(RemoteError::Broken(self.reason)),
        }
    }
}

/// Everything a remote does. Each implementation provides every operation
/// (ADR 0015), and each one checks that the remote is the one the config
/// names before it reads or writes (ADR 0031).
pub trait Remote: Send + Sync {
    /// Whether the remote can be used now. It looks and changes nothing.
    fn reachability(&self) -> Result<Assessment, RemoteError>;

    /// Called at the start of each remote phase of a run.
    fn begin_phase(&self) -> Result<(), RemoteError>;

    /// Called once when the run is done with the remote.
    fn finish(&self);
}

/// Opens the remote the config of `collection` names.
pub fn open(collection: &Collection) -> Result<Box<dyn Remote>, RemoteError> {
    let config = collection
        .config()
        .remotes
        .first()
        .ok_or(RemoteError::NoRemote)?;
    match RemoteUrl::parse(&config.url)? {
        RemoteUrl::Path { root } => Ok(Box::new(PathRemote::new(root, config.id))),
        RemoteUrl::Ssh { .. } => Err(RemoteError::Unsupported("an ssh remote".into())),
    }
}

/// Looks at the remote of a collection without changing anything. A remote
/// that cannot be opened, such as an ssh remote before this version can
/// open it, cannot be used and is reported as broken.
pub fn probe_state(collection: &Collection) -> RemoteState {
    let Some(config) = collection.config().remotes.first() else {
        return RemoteState::None;
    };
    match RemoteUrl::parse(&config.url) {
        Ok(RemoteUrl::Path { root }) => {
            path::assess_root(&root, config.id, &path::Seams::default())
                .reachability
                .into()
        }
        Ok(RemoteUrl::Ssh { .. }) | Err(_) => RemoteState::Broken,
    }
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

    // =====================================================
    // Opening and probing the configured remote
    // =====================================================

    const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

    fn collection_with(url: &str, machine: Option<&str>) -> Collection {
        let mut config = crate::config::Config::new(
            crate::testing::tree::COLLECTION_ID
                .parse()
                .expect("the fixture ID is a ULID"),
        );
        config.machine.name = machine.map(String::from);
        config.remotes.push(crate::config::RemoteConfig {
            url: url.into(),
            id: REMOTE_ID.parse().expect("a ULID"),
            host_key_alias: None,
        });
        Collection::new(std::path::PathBuf::from("/collection-for-tests"), config)
    }

    fn remote_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp dir");
        let marker = marker::Marker::new(REMOTE_ID.parse().expect("a ULID"));
        std::fs::write(root.path().join(marker::FILE_NAME), marker.to_bytes())
            .expect("the marker is written");
        root
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .expect("the directory is listed")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn without_a_remote_the_state_is_none() {
        assert_eq!(probe_state(&Collection::for_tests()), RemoteState::None);
    }

    #[test]
    fn a_path_remote_with_its_marker_is_reachable() {
        let root = remote_root();
        let collection = collection_with(root.path().to_str().expect("UTF-8"), Some("laptop"));
        assert_eq!(probe_state(&collection), RemoteState::Reachable);
    }

    #[test]
    fn a_missing_root_is_away() {
        let parent = tempfile::tempdir().expect("temp dir");
        let url = parent.path().join("absent");
        let collection = collection_with(url.to_str().expect("UTF-8"), Some("laptop"));
        assert_eq!(probe_state(&collection), RemoteState::Away);
    }

    #[test]
    fn an_ssh_remote_cannot_be_used_yet_and_is_broken() {
        let collection = collection_with("ssh://nas/srv/antiq", Some("laptop"));
        assert_eq!(probe_state(&collection), RemoteState::Broken);
    }

    #[test]
    fn a_url_that_does_not_parse_is_broken() {
        let collection = collection_with("relative/path", Some("laptop"));
        assert_eq!(probe_state(&collection), RemoteState::Broken);
    }

    #[test]
    fn probing_leaves_the_remote_as_it_was() {
        let root = remote_root();
        let before = names(root.path());
        let collection = collection_with(root.path().to_str().expect("UTF-8"), Some("laptop"));
        probe_state(&collection);
        assert_eq!(names(root.path()), before);
    }

    #[test]
    fn a_machine_name_that_cannot_be_resolved_does_not_make_the_remote_broken() {
        let root = remote_root();
        let collection = collection_with(root.path().to_str().expect("UTF-8"), Some(""));
        assert!(collection.machine_name().is_err());
        assert_eq!(probe_state(&collection), RemoteState::Reachable);
    }

    #[test]
    fn opening_without_a_remote_says_so() {
        let error = open(&Collection::for_tests()).err().expect("no remote");
        assert!(matches!(error, RemoteError::NoRemote), "{error}");
    }

    #[test]
    fn opening_an_ssh_url_is_unsupported() {
        let collection = collection_with("ssh://nas/srv/antiq", Some("laptop"));
        let error = open(&collection).err().expect("not supported yet");
        assert!(matches!(error, RemoteError::Unsupported(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn opening_a_url_that_does_not_parse_is_an_error() {
        let collection = collection_with("relative/path", Some("laptop"));
        let error = open(&collection).err().expect("no URL");
        assert!(matches!(error, RemoteError::Url(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn an_opened_path_remote_answers_for_its_root() {
        let root = remote_root();
        let collection = collection_with(root.path().to_str().expect("UTF-8"), Some("laptop"));
        let remote = open(&collection).expect("a path remote opens");
        let found = remote.reachability().expect("an answer");
        assert_eq!(found.reachability, Reachability::Reachable);
    }

    #[test]
    fn an_assessment_becomes_the_error_that_says_why() {
        assert!(
            Assessment::new(Reachability::Reachable, "")
                .into_result()
                .is_ok()
        );
        let away = Assessment::new(Reachability::Away, "no root").into_result();
        assert!(matches!(
            away,
            Err(RemoteError::Away { ref message, overdue: false }) if message == "no root"
        ));
        let broken = Assessment::new(Reachability::Broken, "wrong marker").into_result();
        assert!(
            matches!(broken, Err(RemoteError::Broken(ref message)) if message == "wrong marker")
        );
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
    #[error(transparent)]
    Url(#[from] UrlError),
}

impl HasKind for RemoteError {
    fn kind(&self) -> ErrorKind {
        match self {
            RemoteError::Url(_) => ErrorKind::Other,
            RemoteError::NoRemote => ErrorKind::NoRemote,
            RemoteError::Away { .. } => ErrorKind::RemoteAway,
            RemoteError::Broken(_) => ErrorKind::RemoteBroken,
            RemoteError::Unsupported(_) => ErrorKind::Unsupported,
        }
    }
}
