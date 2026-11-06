// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The path remote: a directory on this machine, usually the mount point
//! of a share.

use crate::fsx::FsxError;
use crate::fsx::device::is_mount_point;
use crate::id::ObjectId;
use crate::remote::marker::{self, Marker};
use crate::remote::{Assessment, Reachability, Remote, RemoteError};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The parts of the path remote that a test replaces to reach conditions an
/// unprivileged test cannot create. The default is the real thing.
#[derive(Clone)]
pub(crate) struct Seams {
    /// Whether another filesystem is mounted at the path.
    pub mount: Arc<MountTest>,
}

type MountTest = dyn Fn(&Path) -> Result<bool, FsxError> + Send + Sync;

impl Default for Seams {
    fn default() -> Seams {
        Seams {
            mount: Arc::new(is_mount_point),
        }
    }
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
}

impl Remote for PathRemote {
    fn reachability(&self) -> Result<Assessment, RemoteError> {
        Ok(self.assess())
    }

    fn begin_phase(&self) -> Result<(), RemoteError> {
        self.require()
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
