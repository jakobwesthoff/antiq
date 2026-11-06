// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The path remote: a directory on this machine, usually the mount point
//! of a share.

use crate::fsx::FsxError;
use crate::fsx::device::is_mount_point;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::pack::hashing::hex;
use crate::relpath::RelPath;
use crate::remote::marker::{self, Marker};
use crate::remote::{
    Assessment, Created, Durable, Placement, Reachability, RealDurable, Remote, RemoteError,
    Transfer, Upload,
};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The parts of the path remote that a test replaces to reach conditions an
/// unprivileged test cannot create. The default is the real thing.
#[derive(Clone)]
pub(crate) struct Seams {
    /// Whether another filesystem is mounted at the path.
    pub mount: Arc<MountTest>,
    /// How the flushes of what was written are done.
    pub durable: Arc<dyn Durable>,
    /// How a file is opened to be read back for its checksum.
    pub read_back: Arc<ReadBack>,
}

type MountTest = dyn Fn(&Path) -> Result<bool, FsxError> + Send + Sync;
type ReadBack = dyn Fn(&Path) -> io::Result<File> + Send + Sync;

impl Default for Seams {
    fn default() -> Seams {
        Seams {
            mount: Arc::new(is_mount_point),
            durable: Arc::new(RealDurable),
            read_back: Arc::new(open_uncached),
        }
    }
}

/// Opens a file so that what is read comes from the remote and not from
/// this machine's cache of it. A network mount may otherwise answer from
/// the pages it kept when the file was written, which would make the
/// check of ADR 0043 read back nothing. Where no way of asking for that is
/// known, the read fails instead of falling back to a cached read.
#[cfg(target_os = "macos")]
fn open_uncached(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    rustix::fs::fcntl_nocache(&file, true)?;
    Ok(file)
}

#[cfg(target_os = "linux")]
fn open_uncached(path: &Path) -> io::Result<File> {
    use rustix::fs::{Advice, fadvise};
    let file = File::open(path)?;
    // Advisory: clean pages of the file are dropped, which they are after
    // the flush that follows every write.
    fadvise(&file, 0, None, Advice::DontNeed)?;
    Ok(file)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_uncached(_path: &Path) -> io::Result<File> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// What a remote root looks like. This is the one place that turns what is
/// found in the file system into a reachability.
#[derive(Debug)]
pub(crate) enum RootState {
    /// The root does not exist.
    Missing,
    /// Looking at the root failed the way it does when the server of a
    /// network mount is gone.
    ServerGone(String),
    /// The root cannot be used and waiting will not change that.
    Unusable(String),
    /// The root exists and holds no marker.
    NoMarker {
        mount_point: bool,
    },
    Marker(Marker),
}

/// What a failed stat of the root means. A missing root and the errors a
/// stale network mount gives are away; every other error is broken.
pub(crate) fn root_error_state(error: &io::Error) -> RootState {
    use rustix::io::Errno;
    if error.kind() == io::ErrorKind::NotFound {
        return RootState::Missing;
    }
    let server_gone = [
        Errno::NOTCONN,
        Errno::HOSTDOWN,
        Errno::HOSTUNREACH,
        Errno::NETUNREACH,
        Errno::TIMEDOUT,
    ];
    match error.raw_os_error() {
        Some(code) if server_gone.iter().any(|errno| errno.raw_os_error() == code) => {
            RootState::ServerGone(error.to_string())
        }
        _ => RootState::Unusable(error.to_string()),
    }
}

/// Looks at the root without changing anything. A root that is a symlink
/// to a directory counts as that directory, as it does for the mount test
/// and the marker read.
pub(crate) fn inspect(root: &Path, seams: &Seams) -> RootState {
    match fs::metadata(root) {
        Err(error) => return root_error_state(&error),
        Ok(found) if !found.is_dir() => return RootState::Unusable("it is not a directory".into()),
        Ok(_) => {}
    }
    match fs::read(root.join(marker::FILE_NAME)) {
        Ok(bytes) => match Marker::parse(&bytes) {
            Ok(found) => RootState::Marker(found),
            Err(error) => RootState::Unusable(error.to_string()),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => match (seams.mount)(root) {
            Ok(mount_point) => RootState::NoMarker { mount_point },
            Err(error) => RootState::Unusable(error.to_string()),
        },
        Err(error) => RootState::Unusable(format!("the marker cannot be read: {error}")),
    }
}

/// Maps what [`inspect`] found to a reachability for the remote whose
/// marker says `expected`. The reason names the root.
pub(crate) fn assess(root: &Path, state: &RootState, expected: ObjectId) -> Assessment {
    let root = root.display();
    match state {
        RootState::Missing => Assessment::new(Reachability::Away, format!("{root} does not exist")),
        RootState::ServerGone(error) => Assessment::new(
            Reachability::Away,
            format!("{root} cannot be reached: {error}"),
        ),
        RootState::NoMarker { mount_point: false } => Assessment::new(
            Reachability::Away,
            format!(
                "{root} holds no {} and is not a mount point",
                marker::FILE_NAME
            ),
        ),
        RootState::NoMarker { mount_point: true } => Assessment::new(
            Reachability::Broken,
            format!(
                "{root} is a mount point without the marker {}",
                marker::FILE_NAME
            ),
        ),
        RootState::Unusable(reason) => {
            Assessment::new(Reachability::Broken, format!("{root}: {reason}"))
        }
        RootState::Marker(found) if found.id == expected => {
            Assessment::new(Reachability::Reachable, "")
        }
        RootState::Marker(found) => Assessment::new(
            Reachability::Broken,
            format!(
                "{root} holds the marker of another remote ({}), not {expected}",
                found.id
            ),
        ),
    }
}

/// Looks at `root` and maps the result.
pub(crate) fn assess_root(root: &Path, expected: ObjectId, seams: &Seams) -> Assessment {
    assess(root, &inspect(root, seams), expected)
}

/// A remote in a directory.
pub struct PathRemote {
    root: PathBuf,
    expected: ObjectId,
    seams: Seams,
}

impl PathRemote {
    pub fn new(root: PathBuf, expected: ObjectId) -> PathRemote {
        PathRemote::with_seams(root, expected, Seams::default())
    }

    pub(crate) fn with_seams(root: PathBuf, expected: ObjectId, seams: Seams) -> PathRemote {
        PathRemote {
            root,
            expected,
            seams,
        }
    }

    /// Checks the marker, which every operation does first, and turns a
    /// remote that cannot be used into the error that says so.
    fn require(&self) -> Result<(), RemoteError> {
        self.assess().into_result()
    }

    fn assess(&self) -> Assessment {
        assess_root(&self.root, self.expected, &self.seams)
    }

    fn object_dir(&self, id: ObjectId) -> PathBuf {
        self.root.join("objects").join(id.to_string())
    }

    /// Makes the object's directory and `objects/` if they are missing,
    /// and flushes the root and `objects/` whether or not it had to make
    /// anything. A run that was killed after it made a directory but
    /// before the flush leaves one that the next run finds in place, so
    /// only flushing every time makes the entry durable before the files
    /// that depend on it are.
    fn ensure_object_dir(&self, id: ObjectId) -> Result<PathBuf, RemoteError> {
        let objects = self.root.join("objects");
        let dir = self.object_dir(id);
        for made in [&objects, &dir] {
            match fs::create_dir(made) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error("create", made)(error)),
            }
        }
        self.seams.durable.dir(&self.root)?;
        self.seams.durable.dir(&objects)?;
        Ok(dir)
    }

    /// Renames without replacing and flushes the directory afterwards.
    fn rename_in(&self, dir: &Path, temp: &str, name: &str) -> Result<Placement, RemoteError> {
        let tree = TreeRoot::open(dir).map_err(io_error("open", dir))?;
        // The names are file names, not escaped paths: a `%` in one is a
        // `%`.
        let relative = |text: &str| {
            RelPath::from_bytes(text.as_bytes().to_vec()).expect("a plain name is a valid path")
        };
        match tree.rename(&relative(temp), &tree, &relative(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(Placement::TargetExists);
            }
            Err(error) => return Err(io_error("rename", &dir.join(temp))(error)),
        }
        self.seams.durable.dir(dir)?;
        Ok(Placement::Placed)
    }

    /// The final name of an upload is taken. That is fine when the file
    /// there is the one that was to be put (spec "Storage"), and a conflict
    /// otherwise. The directory is flushed in the first case, since the
    /// entry of a file found in place may be one an earlier failed flush
    /// left undurable.
    fn accept_existing(&self, dir: &Path, upload: &Upload<'_>) -> Result<Transfer, RemoteError> {
        let present = self.checksum_of(&dir.join(upload.final_name))?;
        if present != upload.sha256 {
            return Err(RemoteError::PayloadConflict {
                name: upload.final_name.to_string(),
            });
        }
        self.seams.durable.dir(dir)?;
        Ok(Transfer {
            bytes: 0,
            already_present: true,
        })
    }

    /// Removes a file; one that is not there counts as removed and changes
    /// nothing, so nothing is flushed for it.
    fn remove_in(&self, dir: &Path, name: &str) -> Result<(), RemoteError> {
        let path = dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {
                self.seams.durable.dir(dir)?;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove", &path)(error)),
        }
    }

    fn checksum_of(&self, path: &Path) -> Result<String, RemoteError> {
        let mut file = (self.seams.read_back)(path).map_err(io_error("open", path))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            let read = file.read(&mut buffer).map_err(io_error("read", path))?;
            if read == 0 {
                return Ok(hex(&hasher.finalize()));
            }
            hasher.update(&buffer[..read]);
        }
    }
}

/// The directory of an object where an interrupted upload leaves what it
/// has.
const PARTIAL_DIR: &str = ".antiq-partial";

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> RemoteError {
    let path = path.display().to_string();
    move |source| RemoteError::Io {
        action,
        path,
        source,
    }
}

fn create_or_truncate(path: &Path) -> Result<File, RemoteError> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(io_error("create", path))
}

/// Every name the remote is given is one component of an object's
/// directory. The names come from the metadata of other machines, so a
/// name that reaches outside the directory is refused.
fn plain_name(name: &str) -> Result<(), RemoteError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(RemoteError::InvalidName { name: name.into() });
    }
    Ok(())
}

impl Remote for PathRemote {
    fn reachability(&self) -> Result<Assessment, RemoteError> {
        Ok(self.assess())
    }

    fn begin_phase(&self) -> Result<(), RemoteError> {
        self.require()
    }

    fn create_exclusive(&self, id: ObjectId, name: &str) -> Result<Created, RemoteError> {
        self.require()?;
        plain_name(name)?;
        let dir = self.ensure_object_dir(id)?;
        let path = dir.join(name);
        let created = OpenOptions::new().write(true).create_new(true).open(&path);
        match created {
            Ok(file) => {
                self.seams.durable.file(&file, &path)?;
                self.seams.durable.dir(&dir)?;
                Ok(Created::Yes)
            }
            // The claim of another machine may not be durable yet on the
            // server either, so its entry is flushed here as well.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.seams.durable.dir(&dir)?;
                Ok(Created::AlreadyExists)
            }
            Err(error) => Err(io_error("create", &path)(error)),
        }
    }

    fn write_temp(&self, id: ObjectId, temp: &str, bytes: &[u8]) -> Result<(), RemoteError> {
        self.require()?;
        plain_name(temp)?;
        let dir = self.ensure_object_dir(id)?;
        let path = dir.join(temp);
        let mut file = create_or_truncate(&path)?;
        file.write_all(bytes).map_err(io_error("write", &path))?;
        self.seams.durable.file(&file, &path)?;
        Ok(())
    }

    fn rename_into_place(
        &self,
        id: ObjectId,
        temp: &str,
        name: &str,
    ) -> Result<Placement, RemoteError> {
        self.require()?;
        plain_name(temp)?;
        plain_name(name)?;
        self.rename_in(&self.object_dir(id), temp, name)
    }

    fn put_payload(&self, upload: &Upload<'_>) -> Result<Transfer, RemoteError> {
        self.require()?;
        plain_name(upload.temp)?;
        plain_name(upload.final_name)?;
        let dir = self.ensure_object_dir(upload.id)?;
        let target = dir.join(upload.final_name);
        if target.try_exists().map_err(io_error("stat", &target))? {
            return self.accept_existing(&dir, upload);
        }

        let mut source = File::open(upload.source).map_err(io_error("open", upload.source))?;
        let temp = dir.join(upload.temp);
        let mut out = create_or_truncate(&temp)?;
        let bytes = io::copy(&mut source, &mut out).map_err(io_error("write", &temp))?;
        self.seams.durable.file(&out, &temp)?;
        drop(out);
        match self.rename_in(&dir, upload.temp, upload.final_name)? {
            Placement::Placed => Ok(Transfer {
                bytes,
                already_present: false,
            }),
            // Another machine put the same name in place while this copy
            // was written.
            Placement::TargetExists => {
                let _ = fs::remove_file(&temp);
                self.accept_existing(&dir, upload)
            }
        }
    }

    fn delete(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
        self.require()?;
        plain_name(name)?;
        self.remove_in(&self.object_dir(id), name)
    }

    fn delete_partial(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
        self.require()?;
        plain_name(name)?;
        self.remove_in(&self.object_dir(id).join(PARTIAL_DIR), name)
    }

    fn checksum(&self, id: ObjectId, name: &str) -> Result<String, RemoteError> {
        self.require()?;
        plain_name(name)?;
        self.checksum_of(&self.object_dir(id).join(name))
    }

    fn flush(&self) -> Result<(), RemoteError> {
        Ok(())
    }

    fn finish(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";
    const OTHER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a ULID")
    }

    /// A remote root with a matching marker.
    fn remote_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp dir");
        fs::write(
            root.path().join(marker::FILE_NAME),
            Marker::new(id(ID)).to_bytes(),
        )
        .expect("the marker is written");
        root
    }

    fn seams_with_mount(answer: Result<bool, ()>) -> Seams {
        Seams {
            mount: Arc::new(move |_| {
                answer.map_err(|()| FsxError::Io {
                    action: "stat",
                    path: "the root".into(),
                    source: io::Error::from_raw_os_error(5),
                })
            }),
            ..Seams::default()
        }
    }

    fn assess_at(root: &Path) -> Assessment {
        assess_root(root, id(ID), &Seams::default())
    }

    fn assert_state(found: &Assessment, expected: Reachability, root: &Path) {
        assert_eq!(found.reachability, expected, "{}", found.reason);
        if expected != Reachability::Reachable {
            assert!(
                found.reason.contains(&root.display().to_string()),
                "the reason names the root: {}",
                found.reason
            );
        }
    }

    #[test]
    fn a_matching_marker_is_reachable() {
        let root = remote_root();
        let found = assess_at(root.path());
        assert_eq!(found.reachability, Reachability::Reachable);
        assert_eq!(found.reason, "");
    }

    #[test]
    fn a_missing_root_is_away() {
        let parent = tempfile::tempdir().expect("temp dir");
        let root = parent.path().join("absent");
        assert_state(&assess_at(&root), Reachability::Away, &root);
    }

    #[test]
    fn a_root_without_a_marker_that_is_no_mount_point_is_away() {
        let root = tempfile::tempdir().expect("temp dir");
        let found = assess_root(root.path(), id(ID), &seams_with_mount(Ok(false)));
        assert_state(&found, Reachability::Away, root.path());
    }

    #[test]
    fn a_mount_point_without_a_marker_is_broken() {
        let root = tempfile::tempdir().expect("temp dir");
        let found = assess_root(root.path(), id(ID), &seams_with_mount(Ok(true)));
        assert_state(&found, Reachability::Broken, root.path());
        assert!(found.reason.contains("mount point"), "{}", found.reason);
    }

    #[test]
    fn a_failing_mount_test_is_broken() {
        let root = tempfile::tempdir().expect("temp dir");
        let found = assess_root(root.path(), id(ID), &seams_with_mount(Err(())));
        assert_state(&found, Reachability::Broken, root.path());
    }

    #[test]
    fn the_marker_of_another_remote_is_broken() {
        let root = tempfile::tempdir().expect("temp dir");
        fs::write(
            root.path().join(marker::FILE_NAME),
            Marker::new(id(OTHER)).to_bytes(),
        )
        .expect("write");
        let found = assess_at(root.path());
        assert_state(&found, Reachability::Broken, root.path());
        assert!(found.reason.contains(OTHER), "{}", found.reason);
    }

    #[test]
    fn a_marker_that_is_a_directory_is_broken() {
        let root = tempfile::tempdir().expect("temp dir");
        fs::create_dir(root.path().join(marker::FILE_NAME)).expect("dir");
        assert_state(&assess_at(root.path()), Reachability::Broken, root.path());
    }

    #[test]
    fn a_marker_that_cannot_be_read_is_broken() {
        let root = remote_root();
        let marker_path = root.path().join(marker::FILE_NAME);
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read(&marker_path).is_err(),
            "the test needs a user for whom mode 000 denies reading; run it as a non-root user"
        );
        assert_state(&assess_at(root.path()), Reachability::Broken, root.path());
    }

    #[test]
    fn a_marker_that_is_no_json_is_broken() {
        let root = tempfile::tempdir().expect("temp dir");
        fs::write(root.path().join(marker::FILE_NAME), "not json").expect("write");
        assert_state(&assess_at(root.path()), Reachability::Broken, root.path());
    }

    #[test]
    fn a_marker_of_a_newer_format_is_broken_and_says_so() {
        let root = tempfile::tempdir().expect("temp dir");
        let text = format!("{{\"format\":2,\"id\":\"{ID}\",\"layout\":1}}");
        fs::write(root.path().join(marker::FILE_NAME), text).expect("write");
        let found = assess_at(root.path());
        assert_state(&found, Reachability::Broken, root.path());
        assert!(found.reason.contains("newer"), "{}", found.reason);
    }

    #[test]
    fn a_root_that_is_a_file_is_broken() {
        let parent = tempfile::tempdir().expect("temp dir");
        let root = parent.path().join("file");
        fs::write(&root, "x").expect("write");
        assert_state(&assess_at(&root), Reachability::Broken, &root);
    }

    #[test]
    fn a_root_below_a_file_is_broken() {
        let parent = tempfile::tempdir().expect("temp dir");
        let file = parent.path().join("file");
        fs::write(&file, "x").expect("write");
        let root = file.join("remote");
        assert_state(&assess_at(&root), Reachability::Broken, &root);
    }

    #[test]
    fn a_root_below_a_directory_that_cannot_be_searched_is_broken() {
        let parent = tempfile::tempdir().expect("temp dir");
        let closed = parent.path().join("closed");
        fs::create_dir_all(closed.join("remote")).expect("dirs");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).expect("chmod");
        let root = closed.join("remote");
        let denied = fs::metadata(&root).is_err();
        let found = assess_at(&root);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 000 denies searching; run it as a non-root user"
        );
        assert_state(&found, Reachability::Broken, &root);
    }

    #[test]
    fn a_root_that_is_a_symlink_to_a_remote_is_reachable() {
        let root = remote_root();
        let parent = tempfile::tempdir().expect("temp dir");
        let link = parent.path().join("nas");
        std::os::unix::fs::symlink(root.path(), &link).expect("symlink");
        assert_eq!(assess_at(&link).reachability, Reachability::Reachable);
    }

    #[test]
    fn the_errors_of_a_gone_server_are_away_and_every_other_error_is_broken() {
        let away = [
            rustix::io::Errno::NOTCONN,
            rustix::io::Errno::HOSTDOWN,
            rustix::io::Errno::HOSTUNREACH,
            rustix::io::Errno::NETUNREACH,
            rustix::io::Errno::TIMEDOUT,
        ];
        for errno in away {
            let error = io::Error::from_raw_os_error(errno.raw_os_error());
            assert!(
                matches!(root_error_state(&error), RootState::ServerGone(_)),
                "{errno:?}"
            );
        }
        for errno in [
            rustix::io::Errno::STALE,
            rustix::io::Errno::IO,
            rustix::io::Errno::ACCESS,
        ] {
            let error = io::Error::from_raw_os_error(errno.raw_os_error());
            assert!(
                matches!(root_error_state(&error), RootState::Unusable(_)),
                "{errno:?}"
            );
        }
        let missing = io::Error::from_raw_os_error(rustix::io::Errno::NOENT.raw_os_error());
        assert!(matches!(root_error_state(&missing), RootState::Missing));
        let without_code = io::Error::other("no errno");
        assert!(matches!(
            root_error_state(&without_code),
            RootState::Unusable(_)
        ));
    }

    #[test]
    fn a_gone_server_is_away_and_the_reason_names_the_root() {
        let error = io::Error::from_raw_os_error(rustix::io::Errno::HOSTDOWN.raw_os_error());
        let root = Path::new("/Volumes/nas/antiq");
        let found = assess(root, &root_error_state(&error), id(ID));
        assert_state(&found, Reachability::Away, root);
    }

    #[test]
    fn a_phase_begins_only_at_a_reachable_remote() {
        let root = remote_root();
        let remote = PathRemote::new(root.path().to_path_buf(), id(ID));
        remote.begin_phase().expect("the remote is reachable");
        fs::write(
            root.path().join(marker::FILE_NAME),
            Marker::new(id(OTHER)).to_bytes(),
        )
        .expect("write");
        let error = remote.begin_phase().expect_err("the marker is another");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
    }

    #[test]
    fn a_phase_does_not_begin_at_an_away_remote() {
        let parent = tempfile::tempdir().expect("temp dir");
        let remote = PathRemote::new(parent.path().join("absent"), id(ID));
        let error = remote.begin_phase().expect_err("no root");
        assert!(matches!(error, RemoteError::Away { .. }), "{error}");
    }

    #[test]
    fn the_trait_object_answers_reachability_and_finishes() {
        let root = remote_root();
        let remote: Box<dyn Remote> = Box::new(PathRemote::new(root.path().to_path_buf(), id(ID)));
        let found = remote.reachability().expect("an answer");
        assert_eq!(found.reachability, Reachability::Reachable);
        remote.finish();
    }
}

#[cfg(test)]
mod writing {
    use super::*;
    use crate::remote::recording::{Flush, Recording};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";
    const OTHER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn id() -> ObjectId {
        ID.parse().expect("a ULID")
    }

    fn sha(bytes: &[u8]) -> String {
        crate::pack::hashing::hex(&Sha256::digest(bytes))
    }

    /// A remote with a matching marker, a recorder in place of the
    /// flushes, and a directory of local files to upload from.
    struct Fixture {
        root: tempfile::TempDir,
        local: tempfile::TempDir,
        durable: Arc<Recording>,
        remote: PathRemote,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("temp dir");
        fs::write(
            root.path().join(marker::FILE_NAME),
            Marker::new(id()).to_bytes(),
        )
        .expect("the marker is written");
        let durable = Arc::new(Recording::new());
        let seams = Seams {
            durable: durable.clone(),
            ..Seams::default()
        };
        let remote = PathRemote::with_seams(root.path().to_path_buf(), id(), seams);
        Fixture {
            root,
            local: tempfile::tempdir().expect("temp dir"),
            durable,
            remote,
        }
    }

    impl Fixture {
        fn objects(&self) -> PathBuf {
            self.root.path().join("objects")
        }

        fn object_dir(&self) -> PathBuf {
            self.objects().join(ID)
        }

        fn names(&self) -> Vec<String> {
            let mut found: Vec<String> = fs::read_dir(self.object_dir())
                .expect("the object directory is listed")
                .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
                .collect();
            found.sort();
            found
        }

        fn local(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.local.path().join(name);
            fs::write(&path, bytes).expect("the local file is written");
            path
        }

        /// Puts the marker of another remote in place of ours.
        fn replace_marker(&self) {
            fs::write(
                self.root.path().join(marker::FILE_NAME),
                Marker::new(OTHER.parse().expect("a ULID")).to_bytes(),
            )
            .expect("write");
        }

        fn put(&self, bytes: &[u8]) -> Result<Transfer, RemoteError> {
            let source = self.local("p", bytes);
            let sha256 = sha(bytes);
            self.remote
                .put_payload(&Upload::new(id(), ".p.part", "p.tar.zst", &source, &sha256))
        }
    }

    fn bad_names() -> [&'static str; 6] {
        ["", ".", "..", "a/b", "/abs", "a\0b"]
    }

    fn assert_invalid(result: Result<impl std::fmt::Debug, RemoteError>, name: &str) {
        assert!(
            matches!(result, Err(RemoteError::InvalidName { .. })),
            "{name:?}: {result:?}"
        );
    }

    fn assert_broken(result: Result<impl std::fmt::Debug, RemoteError>) {
        assert!(matches!(result, Err(RemoteError::Broken(_))), "{result:?}");
    }

    // =====================================================
    // create_exclusive
    // =====================================================

    #[test]
    fn a_claim_is_created_once() {
        let fx = fixture();
        let first = fx.remote.create_exclusive(id(), "meta.2.claim");
        assert_eq!(first.expect("created"), Created::Yes);
        let second = fx.remote.create_exclusive(id(), "meta.2.claim");
        assert_eq!(second.expect("answered"), Created::AlreadyExists);
        assert_eq!(fx.names(), ["meta.2.claim"]);
    }

    #[test]
    fn a_new_claim_makes_the_directories_and_then_the_file_durable() {
        let fx = fixture();
        fx.remote
            .create_exclusive(id(), "meta.2.claim")
            .expect("created");
        assert_eq!(
            fx.durable.flushes(),
            [
                Flush::Dir(fx.root.path().to_path_buf()),
                Flush::Dir(fx.objects()),
                Flush::File(fx.object_dir().join("meta.2.claim")),
                Flush::Dir(fx.object_dir()),
            ]
        );
    }

    #[test]
    fn a_claim_that_exists_flushes_the_directory_but_no_file() {
        let fx = fixture();
        fx.remote
            .create_exclusive(id(), "meta.2.claim")
            .expect("created");
        let before = fx.durable.flushes().len();
        fx.remote
            .create_exclusive(id(), "meta.2.claim")
            .expect("answered");
        assert_eq!(
            fx.durable.flushes()[before..],
            [
                Flush::Dir(fx.root.path().to_path_buf()),
                Flush::Dir(fx.objects()),
                Flush::Dir(fx.object_dir()),
            ]
        );
    }

    #[test]
    fn a_claim_in_a_directory_that_cannot_be_written_is_an_error() {
        let fx = fixture();
        fs::create_dir_all(fx.object_dir()).expect("dirs");
        fs::set_permissions(fx.object_dir(), fs::Permissions::from_mode(0o555)).expect("chmod");
        let denied = File::create(fx.object_dir().join("probe")).is_err();
        let result = fx.remote.create_exclusive(id(), "meta.2.claim");
        fs::set_permissions(fx.object_dir(), fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 555 denies writing; run it as a non-root user"
        );
        assert!(matches!(result, Err(RemoteError::Io { .. })), "{result:?}");
    }

    #[test]
    fn a_remote_that_cannot_be_written_cannot_make_its_objects_directory() {
        let fx = fixture();
        fs::set_permissions(fx.root.path(), fs::Permissions::from_mode(0o555)).expect("chmod");
        let denied = File::create(fx.root.path().join("probe")).is_err();
        let result = fx.remote.create_exclusive(id(), "meta.2.claim");
        fs::set_permissions(fx.root.path(), fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 555 denies writing; run it as a non-root user"
        );
        match result {
            Err(RemoteError::Io { action, path, .. }) => {
                assert_eq!(action, "create");
                assert!(path.ends_with("objects"), "{path}");
            }
            other => panic!("expected the objects directory to fail: {other:?}"),
        }
    }

    #[test]
    fn a_claim_name_that_is_no_file_name_is_refused() {
        let fx = fixture();
        for name in bad_names() {
            assert_invalid(fx.remote.create_exclusive(id(), name), name);
        }
        assert!(!fx.objects().exists());
    }

    #[test]
    fn a_claim_at_a_remote_with_another_marker_is_refused_and_creates_nothing() {
        let fx = fixture();
        fx.replace_marker();
        assert_broken(fx.remote.create_exclusive(id(), "meta.2.claim"));
        assert!(!fx.objects().exists());
    }

    // =====================================================
    // write_temp
    // =====================================================

    #[test]
    fn a_temp_file_gets_the_bytes() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), ".meta.3.part", b"{}")
            .expect("written");
        assert_eq!(
            fs::read(fx.object_dir().join(".meta.3.part")).expect("exists"),
            b"{}"
        );
    }

    #[test]
    fn a_temp_file_of_an_earlier_attempt_is_rewritten_from_the_start() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), ".meta.3.part", b"a long first attempt")
            .expect("written");
        fx.remote
            .write_temp(id(), ".meta.3.part", b"short")
            .expect("written");
        assert_eq!(
            fs::read(fx.object_dir().join(".meta.3.part")).expect("exists"),
            b"short"
        );
    }

    #[test]
    fn the_temp_file_holds_all_bytes_when_it_is_flushed() {
        let fx = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let temp = fx.object_dir().join(".meta.3.part");
        fx.durable.observe(move |flush, _| {
            if matches!(flush, Flush::File(_)) {
                log.lock()
                    .expect("no panic")
                    .push(fs::read(&temp).expect("the temp file exists"));
            }
        });
        fx.remote
            .write_temp(id(), ".meta.3.part", b"{\"id\":1}")
            .expect("written");
        assert_eq!(*seen.lock().expect("no panic"), [b"{\"id\":1}".to_vec()]);
    }

    #[test]
    fn a_failing_flush_of_a_temp_file_is_an_error() {
        let fx = fixture();
        fx.durable.fail_files();
        let result = fx.remote.write_temp(id(), ".meta.3.part", b"{}");
        assert!(matches!(result, Err(RemoteError::Fsx(_))), "{result:?}");
    }

    #[test]
    fn a_temp_name_that_is_no_file_name_is_refused() {
        let fx = fixture();
        for name in bad_names() {
            assert_invalid(fx.remote.write_temp(id(), name, b"x"), name);
        }
    }

    #[test]
    fn a_temp_file_at_a_remote_with_another_marker_is_refused_and_written_nowhere() {
        let fx = fixture();
        fx.replace_marker();
        assert_broken(fx.remote.write_temp(id(), ".meta.3.part", b"x"));
        assert!(!fx.objects().exists());
    }

    // =====================================================
    // rename_into_place
    // =====================================================

    #[test]
    fn a_temp_file_is_renamed_into_place_and_the_directory_flushed() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), ".meta.3.part", b"{}")
            .expect("written");
        let before = fx.durable.flushes().len();
        let placed = fx.remote.rename_into_place(id(), ".meta.3.part", "meta.3");
        assert_eq!(placed.expect("renamed"), Placement::Placed);
        assert_eq!(fx.names(), ["meta.3"]);
        assert_eq!(
            fx.durable.flushes()[before..],
            [Flush::Dir(fx.object_dir())]
        );
    }

    #[test]
    fn a_name_that_is_taken_is_never_replaced() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), "meta.3", b"theirs")
            .expect("written");
        fx.remote
            .write_temp(id(), ".meta.3.part", b"mine")
            .expect("written");
        let before = fx.durable.flushes().len();
        let placed = fx.remote.rename_into_place(id(), ".meta.3.part", "meta.3");
        assert_eq!(placed.expect("answered"), Placement::TargetExists);
        assert_eq!(
            fs::read(fx.object_dir().join("meta.3")).expect("exists"),
            b"theirs"
        );
        assert_eq!(fx.names(), [".meta.3.part", "meta.3"]);
        assert_eq!(fx.durable.flushes().len(), before, "nothing changed");
    }

    #[test]
    fn a_failing_flush_after_the_rename_leaves_the_file_in_place() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), ".meta.3.part", b"{}")
            .expect("written");
        // The root and `objects/` were flushed by `write_temp`; the next
        // flush is the one after the rename.
        fx.durable.fail_dirs_from(2);
        let result = fx.remote.rename_into_place(id(), ".meta.3.part", "meta.3");
        assert!(matches!(result, Err(RemoteError::Fsx(_))), "{result:?}");
        assert_eq!(fx.names(), ["meta.3"]);
    }

    #[test]
    fn renaming_a_temp_file_that_is_not_there_is_an_error() {
        let fx = fixture();
        fs::create_dir_all(fx.object_dir()).expect("dirs");
        let result = fx.remote.rename_into_place(id(), ".meta.3.part", "meta.3");
        assert!(matches!(result, Err(RemoteError::Io { .. })), "{result:?}");
    }

    #[test]
    fn renaming_in_an_object_that_is_not_there_is_an_error() {
        let fx = fixture();
        let result = fx.remote.rename_into_place(id(), ".meta.3.part", "meta.3");
        assert!(matches!(result, Err(RemoteError::Io { .. })), "{result:?}");
    }

    #[test]
    fn a_rename_with_a_name_that_is_no_file_name_is_refused() {
        let fx = fixture();
        for name in bad_names() {
            assert_invalid(fx.remote.rename_into_place(id(), name, "meta.3"), name);
            assert_invalid(fx.remote.rename_into_place(id(), ".t", name), name);
        }
    }

    #[test]
    fn a_rename_at_a_remote_with_another_marker_is_refused() {
        let fx = fixture();
        fx.replace_marker();
        assert_broken(fx.remote.rename_into_place(id(), ".t", "meta.3"));
    }

    // =====================================================
    // put_payload
    // =====================================================

    #[test]
    fn a_payload_is_copied_under_its_final_name() {
        let fx = fixture();
        let moved = fx.put(b"archive bytes").expect("put");
        assert_eq!(moved.bytes, 13);
        assert!(!moved.already_present);
        assert_eq!(
            fs::read(fx.object_dir().join("p.tar.zst")).expect("exists"),
            b"archive bytes"
        );
        assert_eq!(fx.names(), ["p.tar.zst"]);
    }

    #[test]
    fn the_payload_is_flushed_complete_before_the_rename_and_the_directory_after() {
        let fx = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let (temp, target) = (
            fx.object_dir().join(".p.part"),
            fx.object_dir().join("p.tar.zst"),
        );
        fx.durable.observe(move |flush, _| {
            log.lock().expect("no panic").push((
                flush.clone(),
                target.exists(),
                fs::read(&temp).ok(),
            ));
        });
        fx.put(b"archive bytes").expect("put");
        let seen = seen.lock().expect("no panic").clone();
        let file = seen
            .iter()
            .find(|(flush, ..)| matches!(flush, Flush::File(_)))
            .expect("the file is flushed");
        assert!(
            !file.1,
            "the final name must not exist while the file is flushed"
        );
        assert_eq!(file.2.as_deref(), Some(&b"archive bytes"[..]));
        let last = seen.last().expect("flushes");
        assert_eq!(last.0, Flush::Dir(fx.object_dir()));
        assert!(
            last.1,
            "the payload is in place when the directory is flushed"
        );
        assert_eq!(last.2, None, "the temp name is gone");
    }

    #[test]
    fn a_failing_flush_of_the_payload_leaves_no_final_name() {
        let fx = fixture();
        fx.durable.fail_files();
        let result = fx.put(b"archive bytes");
        assert!(matches!(result, Err(RemoteError::Fsx(_))), "{result:?}");
        assert!(!fx.object_dir().join("p.tar.zst").exists());
    }

    #[test]
    fn a_failing_flush_of_the_directory_leaves_the_payload_in_place() {
        let fx = fixture();
        // The root and `objects/` are flushed first; the third flush is the
        // one after the rename.
        fx.durable.fail_dirs_from(2);
        let result = fx.put(b"archive bytes");
        assert!(matches!(result, Err(RemoteError::Fsx(_))), "{result:?}");
        assert!(fx.object_dir().join("p.tar.zst").exists());
    }

    #[test]
    fn the_same_payload_under_a_taken_name_writes_nothing_and_flushes_the_directory() {
        let fx = fixture();
        fx.put(b"archive bytes").expect("put");
        let before = fx.durable.flushes().len();
        let moved = fx.put(b"archive bytes").expect("put again");
        assert_eq!(moved.bytes, 0);
        assert!(moved.already_present);
        assert_eq!(fx.names(), ["p.tar.zst"]);
        assert!(
            fx.durable.flushes()[before..].contains(&Flush::Dir(fx.object_dir())),
            "the entry of a file found in place may not be durable yet"
        );
    }

    #[test]
    fn other_bytes_under_a_taken_name_are_a_conflict_and_leave_the_file() {
        let fx = fixture();
        fx.put(b"archive bytes").expect("put");
        let source = fx.local("q", b"other bytes");
        let sha256 = sha(b"other bytes");
        let result =
            fx.remote
                .put_payload(&Upload::new(id(), ".p.part", "p.tar.zst", &source, &sha256));
        assert!(
            matches!(result, Err(RemoteError::PayloadConflict { .. })),
            "{result:?}"
        );
        assert_eq!(
            fs::read(fx.object_dir().join("p.tar.zst")).expect("exists"),
            b"archive bytes"
        );
    }

    #[test]
    fn a_temp_file_of_an_earlier_attempt_is_overwritten_by_a_payload() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), ".p.part", b"a long first attempt that was cut off")
            .expect("written");
        fx.put(b"short").expect("put");
        assert_eq!(
            fs::read(fx.object_dir().join("p.tar.zst")).expect("exists"),
            b"short"
        );
        assert_eq!(fx.names(), ["p.tar.zst"]);
    }

    #[test]
    fn a_payload_that_appears_while_ours_is_written_is_accepted_when_it_is_the_same() {
        let fx = fixture();
        let target = fx.object_dir().join("p.tar.zst");
        fx.durable.observe(move |flush, _| {
            if matches!(flush, Flush::File(_)) {
                fs::write(&target, b"archive bytes").expect("the other machine writes");
            }
        });
        let moved = fx.put(b"archive bytes").expect("put");
        assert!(moved.already_present);
        assert_eq!(fx.names(), ["p.tar.zst"]);
    }

    #[test]
    fn a_payload_that_appears_while_ours_is_written_is_a_conflict_when_it_differs() {
        let fx = fixture();
        let target = fx.object_dir().join("p.tar.zst");
        fx.durable.observe(move |flush, _| {
            if matches!(flush, Flush::File(_)) {
                fs::write(&target, b"someone else's bytes").expect("the other machine writes");
            }
        });
        let result = fx.put(b"archive bytes");
        assert!(
            matches!(result, Err(RemoteError::PayloadConflict { .. })),
            "{result:?}"
        );
        assert_eq!(
            fs::read(fx.object_dir().join("p.tar.zst")).expect("exists"),
            b"someone else's bytes"
        );
    }

    #[test]
    fn a_payload_whose_source_is_missing_is_an_error() {
        let fx = fixture();
        let missing = fx.local.path().join("absent");
        let result =
            fx.remote
                .put_payload(&Upload::new(id(), ".p.part", "p.tar.zst", &missing, "00"));
        assert!(matches!(result, Err(RemoteError::Io { .. })), "{result:?}");
    }

    #[test]
    fn a_payload_with_a_name_that_is_no_file_name_is_refused() {
        let fx = fixture();
        let source = fx.local("p", b"x");
        for name in bad_names() {
            let temp = Upload::new(id(), name, "p.tar.zst", &source, "00");
            assert_invalid(fx.remote.put_payload(&temp), name);
            let target = Upload::new(id(), ".p.part", name, &source, "00");
            assert_invalid(fx.remote.put_payload(&target), name);
        }
    }

    #[test]
    fn a_payload_at_a_remote_with_another_marker_is_refused_and_written_nowhere() {
        let fx = fixture();
        fx.replace_marker();
        assert_broken(fx.put(b"x"));
        assert!(!fx.objects().exists());
    }

    // =====================================================
    // delete and delete_partial
    // =====================================================

    #[test]
    fn a_file_is_deleted_and_the_directory_flushed() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), ".old.part", b"x")
            .expect("written");
        let before = fx.durable.flushes().len();
        fx.remote.delete(id(), ".old.part").expect("deleted");
        assert!(fx.names().is_empty());
        assert_eq!(
            fx.durable.flushes()[before..],
            [Flush::Dir(fx.object_dir())]
        );
    }

    #[test]
    fn deleting_a_file_that_is_not_there_changes_and_flushes_nothing() {
        let fx = fixture();
        fs::create_dir_all(fx.object_dir()).expect("dirs");
        fx.remote.delete(id(), "absent").expect("counts as deleted");
        assert!(fx.durable.flushes().is_empty());
    }

    #[test]
    fn deleting_in_an_object_that_is_not_there_creates_nothing() {
        let fx = fixture();
        fx.remote.delete(id(), "absent").expect("counts as deleted");
        assert!(!fx.objects().exists());
        assert!(fx.durable.flushes().is_empty());
    }

    #[test]
    fn deleting_a_directory_is_an_error() {
        let fx = fixture();
        fs::create_dir_all(fx.object_dir().join("sub")).expect("dirs");
        let result = fx.remote.delete(id(), "sub");
        assert!(matches!(result, Err(RemoteError::Io { .. })), "{result:?}");
    }

    #[test]
    fn a_delete_with_a_name_that_is_no_file_name_is_refused() {
        let fx = fixture();
        for name in bad_names() {
            assert_invalid(fx.remote.delete(id(), name), name);
        }
    }

    #[test]
    fn a_delete_at_a_remote_with_another_marker_is_refused_and_deletes_nothing() {
        let fx = fixture();
        fx.remote
            .write_temp(id(), ".old.part", b"x")
            .expect("written");
        fx.replace_marker();
        assert_broken(fx.remote.delete(id(), ".old.part"));
        assert_eq!(fx.names(), [".old.part"]);
    }

    #[test]
    fn a_partial_file_is_deleted_and_its_directory_flushed() {
        let fx = fixture();
        let partial = fx.object_dir().join(".antiq-partial");
        fs::create_dir_all(&partial).expect("dirs");
        fs::write(partial.join("p.tar.zst"), "x").expect("write");
        fx.remote
            .delete_partial(id(), "p.tar.zst")
            .expect("deleted");
        assert!(!partial.join("p.tar.zst").exists());
        assert_eq!(fx.durable.flushes(), [Flush::Dir(partial)]);
    }

    #[test]
    fn deleting_a_partial_file_that_is_not_there_changes_and_flushes_nothing() {
        let fx = fixture();
        fx.remote
            .delete_partial(id(), "absent")
            .expect("counts as deleted");
        assert!(fx.durable.flushes().is_empty());
        assert!(!fx.objects().exists());
    }

    #[test]
    fn a_partial_delete_with_a_name_that_is_no_file_name_is_refused() {
        let fx = fixture();
        for name in bad_names() {
            assert_invalid(fx.remote.delete_partial(id(), name), name);
        }
    }

    #[test]
    fn a_partial_delete_at_a_remote_with_another_marker_is_refused() {
        let fx = fixture();
        fx.replace_marker();
        assert_broken(fx.remote.delete_partial(id(), "p"));
    }

    // =====================================================
    // checksum
    // =====================================================

    #[test]
    fn the_checksum_is_the_sha256_of_the_bytes_on_the_remote() {
        let fx = fixture();
        fx.put(b"archive bytes").expect("put");
        let found = fx.remote.checksum(id(), "p.tar.zst").expect("a checksum");
        assert_eq!(found, sha(b"archive bytes"));
    }

    #[test]
    fn the_checksum_reads_through_the_read_back_seam() {
        let fx = fixture();
        fx.put(b"archive bytes").expect("put");
        let elsewhere = fx.local("elsewhere", b"what the seam opens");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        let seams = Seams {
            read_back: Arc::new(move |path| {
                log.lock().expect("no panic").push(path.to_path_buf());
                File::open(&elsewhere)
            }),
            ..Seams::default()
        };
        let remote = PathRemote::with_seams(fx.root.path().to_path_buf(), id(), seams);
        let found = remote.checksum(id(), "p.tar.zst").expect("a checksum");
        assert_eq!(found, sha(b"what the seam opens"));
        assert_eq!(
            *asked.lock().expect("no panic"),
            [fx.object_dir().join("p.tar.zst")]
        );
    }

    #[test]
    fn a_read_back_that_cannot_open_the_file_is_an_error() {
        let fx = fixture();
        fx.put(b"archive bytes").expect("put");
        let seams = Seams {
            read_back: Arc::new(|_| Err(io::Error::from(io::ErrorKind::Unsupported))),
            ..Seams::default()
        };
        let remote = PathRemote::with_seams(fx.root.path().to_path_buf(), id(), seams);
        let result = remote.checksum(id(), "p.tar.zst");
        assert!(matches!(result, Err(RemoteError::Io { .. })), "{result:?}");
    }

    #[test]
    fn the_checksum_of_a_file_that_is_not_there_is_an_error() {
        let fx = fixture();
        let result = fx.remote.checksum(id(), "absent");
        assert!(matches!(result, Err(RemoteError::Io { .. })), "{result:?}");
    }

    #[test]
    fn the_real_read_back_returns_the_whole_file() {
        let fx = fixture();
        let size = 3 * 1024 * 1024 + 5;
        let path = fx.local("big", &vec![7u8; size]);
        let mut file = open_uncached(&path).expect("this platform can read past the cache");
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut file, &mut bytes).expect("read");
        assert_eq!(bytes.len(), size);
    }

    #[test]
    fn a_checksum_with_a_name_that_is_no_file_name_is_refused() {
        let fx = fixture();
        for name in bad_names() {
            assert_invalid(fx.remote.checksum(id(), name), name);
        }
    }

    #[test]
    fn a_checksum_at_a_remote_with_another_marker_is_refused() {
        let fx = fixture();
        fx.replace_marker();
        assert_broken(fx.remote.checksum(id(), "p"));
    }

    #[test]
    fn flushing_the_remote_needs_nothing_of_the_path_remote() {
        let fx = fixture();
        fx.remote
            .flush()
            .expect("every file is flushed as it is written");
        assert!(fx.durable.flushes().is_empty());
    }
}
