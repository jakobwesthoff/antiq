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
use crate::id::ObjectId;
use crate::machine::MachineError;
use crate::report::Warning;
use jiff::Timestamp;
use path::PathRemote;
use rsync::RsyncError;
use serde::Serialize;
use ssh::SshRemote;
use ssh::session::SessionError;
use std::collections::BTreeMap;
use std::fs::File;
use std::num::NonZeroU32;
use std::path::Path;
use url::{RemoteUrl, UrlError};

/// How the path remote makes what it wrote durable. The real one flushes
/// with `sync_all` and falls back to plain `fsync` where a filesystem
/// refuses that (`fsx::atomic`); tests put a recorder in its place to see
/// in which order the flushes and the renames happen.
pub(crate) trait Durable: Send + Sync {
    /// Makes the data of an open file durable. `path` names it in errors.
    fn file(&self, file: &File, path: &Path) -> Result<(), FsxError>;
    /// Makes the entries of a directory durable.
    fn dir(&self, dir: &Path) -> Result<(), FsxError>;
}

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
        /// Directory flushes from this one on (counted from 0) fail.
        fail_dirs_from: Mutex<Option<usize>>,
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

        /// Makes the directory flushes fail from the `n`th one on, counted
        /// from 0, so a test can let the flushes of the setup pass and fail
        /// the one it is about.
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
            let earlier = self
                .flushes()
                .iter()
                .filter(|flush| matches!(flush, Flush::Dir(_)))
                .count();
            self.record(Flush::Dir(dir.to_path_buf()), None);
            let from = *self
                .fail_dirs_from
                .lock()
                .expect("no test panics holding it");
            if from.is_some_and(|from| earlier >= from) {
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

    /// The remote's own clock, which the ages of its files are measured
    /// against (spec "Committing a version"). The path remote writes a
    /// probe file and reads its modification time.
    fn clock(&self) -> Result<Timestamp, RemoteError>;

    /// The metadata versions of every object that are newer than the ones
    /// `known` lists, in one call: claims, temp names and other files are
    /// not versions and are left out. The bytes are not parsed.
    fn pull_metadata(&self, known: &KnownVersions) -> Result<Vec<MetaFile>, RemoteError>;

    /// The objects that have a directory, in ID order.
    fn list_objects(&self) -> Result<Vec<ObjectId>, RemoteError>;

    /// The entries of an object's directory, by name; none when the object
    /// has no directory.
    fn list_dir(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError>;

    /// The entries of an object's `.antiq-partial` directory, by name.
    fn list_partial(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError>;

    /// The whole content of a small file, such as a listing or a metadata
    /// version.
    fn read_file(&self, id: ObjectId, name: &str) -> Result<Vec<u8>, RemoteError>;

    /// Copies a file of the object to `to`, which is created or rewritten.
    /// The caller flushes it, as it has to move it into place anyway.
    fn get_payload(&self, id: ObjectId, name: &str, to: &Path) -> Result<Transfer, RemoteError>;

    /// Creates the empty file `name` in the object's directory, and fails
    /// to when it exists. The claim of a metadata version is made this
    /// way (ADR 0030). The path remote makes the new entry durable at
    /// once; the ssh remote does at the next [`Remote::flush`].
    fn create_exclusive(&self, id: ObjectId, name: &str) -> Result<Created, RemoteError>;

    /// Writes `bytes` under the temp name `temp` in the object's
    /// directory. A file of that name is rewritten from the start. The
    /// path remote makes the bytes durable at once; the ssh remote makes
    /// them durable at the next [`Remote::flush`].
    fn write_temp(&self, id: ObjectId, temp: &str, bytes: &[u8]) -> Result<(), RemoteError>;

    /// Renames `temp` to `name` without replacing a file that has that
    /// name. The path remote makes the object's directory durable at once;
    /// the ssh remote does at the next [`Remote::flush`].
    fn rename_into_place(
        &self,
        id: ObjectId,
        temp: &str,
        name: &str,
    ) -> Result<Placement, RemoteError>;

    /// Puts a payload or listing in place under its final name. A final
    /// name that is taken accepts the put only when the file there has the
    /// SHA-256 of the upload (spec "Storage").
    fn put_payload(&self, upload: &Upload<'_>) -> Result<Transfer, RemoteError>;

    /// Removes a file of the object's directory. A file that is not there
    /// counts as removed.
    fn delete(&self, id: ObjectId, name: &str) -> Result<(), RemoteError>;

    /// Removes a file of the object's `.antiq-partial` directory, where an
    /// interrupted upload leaves what it has. A file that is not there
    /// counts as removed.
    fn delete_partial(&self, id: ObjectId, name: &str) -> Result<(), RemoteError>;

    /// The SHA-256 of a file as lower-case hex, computed from what the
    /// remote holds and not from what this machine has cached of it
    /// (ADR 0043).
    fn checksum(&self, id: ObjectId, name: &str) -> Result<String, RemoteError>;

    /// Makes everything the remote has written so far durable on its own
    /// side. The path remote flushes every file as it writes it; the ssh
    /// remote runs `sync` on the far side.
    fn flush(&self) -> Result<(), RemoteError>;

    /// What the remote noticed while it worked and the user should know,
    /// collected since the last call. The default has nothing to report.
    fn take_warnings(&self) -> Vec<Warning> {
        Vec::new()
    }

    /// Called once when the run is done with the remote.
    fn finish(&self);
}

/// The highest committed metadata version this machine has of each object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownVersions(BTreeMap<ObjectId, u32>);

impl KnownVersions {
    pub fn new() -> KnownVersions {
        KnownVersions::default()
    }

    pub fn insert(&mut self, id: ObjectId, number: u32) {
        self.0.insert(id, number);
    }

    /// The highest version known of the object; 0 when it is unknown.
    pub fn highest(&self, id: ObjectId) -> u32 {
        self.0.get(&id).copied().unwrap_or(0)
    }
}

/// One committed metadata version as the remote holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaFile {
    pub id: ObjectId,
    pub number: NonZeroU32,
    pub bytes: Vec<u8>,
}

/// An entry of an object's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub name: String,
    pub size: u64,
    /// The modification time the remote reports.
    pub modified: Timestamp,
    pub is_dir: bool,
}

/// What [`Remote::create_exclusive`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    Yes,
    /// The file was there already.
    AlreadyExists,
}

/// What [`Remote::rename_into_place`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Placed,
    /// A file of the final name was there already and is untouched.
    TargetExists,
}

/// A file to put on the remote.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Upload<'a> {
    pub id: ObjectId,
    /// The name it is written under until it is complete.
    pub temp: &'a str,
    /// The name it has once it is in place.
    pub final_name: &'a str,
    /// The local file.
    pub source: &'a Path,
    /// SHA-256 of the file as lower-case hex.
    pub sha256: &'a str,
    /// The name of the current live package, which a remote with delta
    /// transfer starts the new one from. The path remote copies in full
    /// and ignores it.
    pub seed: Option<&'a str>,
}

impl<'a> Upload<'a> {
    /// An upload without a seed.
    pub fn new(
        id: ObjectId,
        temp: &'a str,
        final_name: &'a str,
        source: &'a Path,
        sha256: &'a str,
    ) -> Upload<'a> {
        Upload {
            id,
            temp,
            final_name,
            source,
            sha256,
            seed: None,
        }
    }
}

/// What a put or a get moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Transfer {
    /// Bytes written to the destination.
    pub bytes: u64,
    /// The final name held this very file already, so nothing was written.
    pub already_present: bool,
}

/// Opens the remote the config of `collection` names.
pub fn open(collection: &Collection) -> Result<Box<dyn Remote>, RemoteError> {
    let config = collection
        .config()
        .remotes
        .first()
        .ok_or(RemoteError::NoRemote)?;
    match RemoteUrl::parse(&config.url)? {
        RemoteUrl::Path { root } => {
            let machine = collection.machine_name()?;
            Ok(Box::new(PathRemote::new(root, config.id, &machine)))
        }
        RemoteUrl::Ssh {
            user,
            host,
            port,
            root,
        } => {
            let target = ssh::SshTarget {
                user,
                host,
                port,
                root,
            };
            let machine = &collection.config().machine;
            Ok(Box::new(SshRemote::connect(&target, config, machine)?))
        }
    }
}

/// Looks at the remote of a collection without changing anything. A remote
/// that cannot be opened is reported as broken.
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
        Ok(RemoteUrl::Ssh {
            user,
            host,
            port,
            root,
        }) => {
            let target = ssh::SshTarget {
                user,
                host,
                port,
                root,
            };
            let machine = &collection.config().machine;
            match SshRemote::connect(&target, config, machine)
                .and_then(|remote| remote.reachability())
            {
                Ok(assessment) => assessment.reachability.into(),
                Err(_) => RemoteState::Broken,
            }
        }
        Err(_) => RemoteState::Broken,
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
    fn opening_a_path_remote_needs_the_machine_name_for_its_probe_file() {
        let root = remote_root();
        let collection = collection_with(root.path().to_str().expect("UTF-8"), Some(""));
        let error = open(&collection).err().expect("no machine name");
        assert!(matches!(error, RemoteError::Machine(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_path_remote_has_nothing_to_warn_about() {
        let root = remote_root();
        let collection = collection_with(root.path().to_str().expect("UTF-8"), Some("laptop"));
        let remote = open(&collection).expect("a path remote opens");
        assert!(remote.take_warnings().is_empty());
    }

    #[test]
    fn a_version_nobody_knows_of_is_version_zero() {
        let id: ObjectId = REMOTE_ID.parse().expect("a ULID");
        let mut known = KnownVersions::new();
        assert_eq!(known.highest(id), 0);
        known.insert(id, 3);
        assert_eq!(known.highest(id), 3);
        known.insert(id, 4);
        assert_eq!(known.highest(id), 4);
    }

    #[test]
    fn opening_without_a_remote_says_so() {
        let error = open(&Collection::for_tests()).err().expect("no remote");
        assert!(matches!(error, RemoteError::NoRemote), "{error}");
    }

    #[test]
    fn opening_an_ssh_url_with_a_host_that_ssh_reads_as_an_option_is_an_error() {
        let collection = collection_with("ssh://-oProxyCommand/srv/antiq", Some("laptop"));
        let error = open(&collection).err().expect("the host is refused");
        assert!(matches!(error, RemoteError::Url(_)), "{error}");
        assert_eq!(probe_state(&collection), RemoteState::Broken);
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
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Fsx(#[from] FsxError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error("`{name}` is not a file name of an object's directory")]
    InvalidName { name: String },
    #[error("`{name}` exists on the remote with other content")]
    PayloadConflict { name: String },
    /// The ssh session failed in a way that says nothing about whether the
    /// remote is away or broken.
    #[error(transparent)]
    Session(SessionError),
    /// rsync moved a file and ended with a failure that says nothing about
    /// whether the remote is away or broken.
    #[error("{0}")]
    Transfer(String),
    #[error(transparent)]
    Rsync(#[from] RsyncError),
}

impl HasKind for RemoteError {
    fn kind(&self) -> ErrorKind {
        match self {
            RemoteError::Url(_)
            | RemoteError::Io { .. }
            | RemoteError::Fsx(_)
            | RemoteError::Machine(_)
            | RemoteError::InvalidName { .. }
            | RemoteError::PayloadConflict { .. }
            | RemoteError::Session(_)
            | RemoteError::Transfer(_)
            | RemoteError::Rsync(_) => ErrorKind::Other,
            RemoteError::NoRemote => ErrorKind::NoRemote,
            RemoteError::Away { .. } => ErrorKind::RemoteAway,
            RemoteError::Broken(_) => ErrorKind::RemoteBroken,
            RemoteError::Unsupported(_) => ErrorKind::Unsupported,
        }
    }
}
