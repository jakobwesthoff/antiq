// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Step 4 of `push`: what the target holds decides whether the push goes
//! on, stops as unreachable, or is refused.

use super::PushError;
use crate::filesystem::device;
use crate::id::ObjectId;
use crate::target::marker::{FILE_NAME, Marker, MarkerError};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// What was found at the target.
#[derive(Debug)]
pub(super) enum Probe {
    /// The target could not be reached; the text says why.
    Unreachable(String),
    /// The directory does not exist.
    Missing,
    /// The directory exists, holds nothing and has no marker.
    Empty,
    /// The directory holds files and no marker.
    NotEmpty,
    Marker(Marker),
    /// `.antiq-target` is there and cannot be used; the text says why.
    MarkerUnreadable(String),
    /// `.antiq-target` has a `format` this antiq does not know.
    MarkerTooNew(u64),
}

/// What this collection and machine put into a marker.
#[derive(Debug)]
pub(super) struct Identity {
    pub id: ObjectId,
    pub host: String,
    pub path: String,
}

/// The command-line flags that change the table.
#[derive(Debug, Clone, Copy)]
pub(super) struct Flags {
    pub init: bool,
    pub force: bool,
}

/// What the table says to do.
#[derive(Debug)]
pub(super) enum Decision {
    Unreachable(String),
    // TODO: the marker run (step 5) and the output notes read it.
    #[allow(dead_code)]
    Proceed(Proceed),
    Refuse(PushError),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Proceed {
    /// The marker is to be sent before the mirror.
    pub write_marker: bool,
    /// What the output tells about the way the push was cleared.
    pub note: Option<String>,
}

/// The table of the spec, one row per arm. `ssh` is whether the target is
/// reached over ssh; `shown` is the target as the messages name it.
pub(super) fn decide(
    probe: Probe,
    ssh: bool,
    this: &Identity,
    flags: Flags,
    shown: &str,
) -> Decision {
    let refuse = |problem: String| Decision::Refuse(PushError::Target { problem });
    match probe {
        Probe::Unreachable(reason) => Decision::Unreachable(reason),
        // A path target that is not there is a share that is not mounted,
        // which `--init` must not paper over. An ssh target is created by
        // the marker run, so `--init` is how it comes to exist.
        Probe::Missing if !ssh => Decision::Unreachable(format!(
            "the target directory {shown} does not exist; create it on the mounted volume"
        )),
        Probe::Missing if flags.init => Decision::Proceed(Proceed {
            write_marker: true,
            note: None,
        }),
        Probe::Missing => refuse(format!(
            "the target directory {shown} does not exist; create it, or push with --init to create the last level"
        )),
        Probe::Empty if flags.init => Decision::Proceed(Proceed {
            write_marker: true,
            note: None,
        }),
        Probe::Empty => Decision::Unreachable(format!(
            "the target directory {shown} is empty and has no {FILE_NAME}; run `antiq push --init` to use it"
        )),
        Probe::NotEmpty => refuse(format!(
            "the target directory {shown} is not empty and has no {FILE_NAME}; --init is for an empty directory"
        )),
        Probe::MarkerUnreadable(why) => refuse(format!(
            "the marker {FILE_NAME} in {shown} is not readable: {why}"
        )),
        Probe::MarkerTooNew(found) => {
            Decision::Refuse(PushError::MarkerFormat(MarkerError::TooNew { found }))
        }
        Probe::Marker(marker) => decide_marker(&marker, this, flags, shown),
    }
}

fn decide_marker(marker: &Marker, this: &Identity, flags: Flags, shown: &str) -> Decision {
    if marker.id != this.id {
        return Decision::Refuse(PushError::Target {
            problem: format!(
                "the target {shown} belongs to another collection ({}); this one is {}",
                marker.id, this.id
            ),
        });
    }
    if flags.init {
        return Decision::Refuse(PushError::Target {
            problem: format!(
                "the target {shown} already has a {FILE_NAME}; --init is for an empty directory"
            ),
        });
    }
    let same_place = marker.host == this.host && marker.path == this.path;
    match (same_place, flags.force) {
        (true, false) => Decision::Proceed(Proceed {
            write_marker: false,
            note: None,
        }),
        (true, true) => Decision::Proceed(Proceed {
            write_marker: false,
            note: Some("--force changes nothing here: the marker names this host and path".into()),
        }),
        (false, true) => Decision::Proceed(Proceed {
            write_marker: false,
            note: Some(format!(
                "pushing although the marker names host `{}` and path `{}`; the marker stays as it is",
                marker.host, marker.path
            )),
        }),
        (false, false) => Decision::Refuse(PushError::MarkerMismatch {
            marker_host: marker.host.clone(),
            marker_path: marker.path.clone(),
            host: this.host.clone(),
            path: this.path.clone(),
        }),
    }
}

/// Why the inspection of a path target ends before it has a probe.
#[derive(Debug)]
pub(super) enum Stop {
    Unreachable(String),
    Refuse(PushError),
}

/// Whether `error` says the volume is gone, not that something is wrong
/// with what is on it.
fn is_gone(error: &io::Error) -> bool {
    use rustix::io::Errno;
    [
        Errno::NOTCONN,
        Errno::HOSTDOWN,
        Errno::HOSTUNREACH,
        Errno::NETUNREACH,
        Errno::TIMEDOUT,
    ]
    .iter()
    .any(|errno| error.raw_os_error() == Some(errno.raw_os_error()))
}

/// How a failed call on `path` counts: the five errors of a share that is
/// gone are unreachable, every other error is a refusal.
pub(super) fn failed(error: &io::Error, path: &Path) -> Stop {
    if is_gone(error) {
        Stop::Unreachable(format!(
            "the target {} cannot be reached: {error}",
            path.display()
        ))
    } else {
        Stop::Refuse(PushError::Target {
            problem: format!("the target {} cannot be read: {error}", path.display()),
        })
    }
}

/// The file system calls the inspection of a path target makes, apart
/// from the marker file, which it reads itself.
pub(super) struct Io<'a> {
    pub canonicalize: &'a dyn Fn(&Path) -> io::Result<PathBuf>,
    /// Device and inode, which stay the same under every spelling of a path
    /// and under a volume's name equality.
    pub identity: &'a dyn Fn(&Path) -> io::Result<(u64, u64)>,
    pub mount_root: &'a dyn Fn(&Path) -> io::Result<bool>,
    pub is_empty: &'a dyn Fn(&Path) -> io::Result<bool>,
}

/// Looks at the path target `root` on this machine. Returns what was found
/// and the canonical path, which the rsync calls use from here on.
pub(super) fn inspect_path(root: &Path, collection: &Path) -> Result<(Probe, PathBuf), PushError> {
    let identity = |path: &Path| {
        let meta = fs::metadata(path)?;
        Ok((meta.dev(), meta.ino()))
    };
    let mount_root = |path: &Path| device::is_mount_point(path).map_err(io::Error::other);
    let is_empty = |path: &Path| match fs::read_dir(path)?.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(error)) => Err(error),
    };
    inspect_path_with(
        root,
        collection,
        &Io {
            canonicalize: &|path| fs::canonicalize(path),
            identity: &identity,
            mount_root: &mount_root,
            is_empty: &is_empty,
        },
    )
}

pub(super) fn inspect_path_with(
    root: &Path,
    collection: &Path,
    io: &Io,
) -> Result<(Probe, PathBuf), PushError> {
    match inspect(root, collection, io) {
        Ok(found) => Ok(found),
        Err(Stop::Unreachable(reason)) => Ok((Probe::Unreachable(reason), root.to_path_buf())),
        Err(Stop::Refuse(error)) => Err(error),
    }
}

fn inspect(root: &Path, collection: &Path, io: &Io) -> Result<(Probe, PathBuf), Stop> {
    let collection_identity =
        (io.identity)(collection).map_err(|error| failed(&error, collection))?;

    let canonical = match (io.canonicalize)(root) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // A target that is not there can still lie inside the
            // collection, where its directory would be created.
            let existing = nearest_existing(root, io)?;
            if let Some(existing) = existing {
                not_inside(root, &existing, collection_identity, io)?;
            }
            return Ok((Probe::Missing, root.to_path_buf()));
        }
        Err(error) => return Err(failed(&error, root)),
    };
    not_inside(root, &canonical, collection_identity, io)?;
    not_holding(&canonical, collection, io)?;

    match (io.mount_root)(&canonical) {
        Ok(false) => {}
        Ok(true) => {
            return Err(Stop::Refuse(PushError::Target {
                problem: format!(
                    "the target {} is the root of a mounted volume; use a folder on the volume, not its root",
                    canonical.display()
                ),
            }));
        }
        Err(error) => return Err(failed(&error, &canonical)),
    }

    // The directory is read before the marker, so that a directory that
    // cannot be listed is reported as such and not as a marker that is
    // missing.
    let empty = (io.is_empty)(&canonical).map_err(|error| failed(&error, &canonical))?;
    let probe = match read_marker(&canonical)? {
        Some(probe) => probe,
        None if empty => Probe::Empty,
        None => Probe::NotEmpty,
    };
    Ok((probe, canonical))
}

/// The first of the folders above `path`, nearest first, that exists, in
/// its canonical spelling.
fn nearest_existing(path: &Path, io: &Io) -> Result<Option<PathBuf>, Stop> {
    for ancestor in path.ancestors().skip(1) {
        match (io.canonicalize)(ancestor) {
            Ok(existing) => return Ok(Some(existing)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(failed(&error, ancestor)),
        }
    }
    Ok(None)
}

/// Refuses a target that is the collection or below it. Every folder on
/// the way up from `canonical` is compared by device and inode.
fn not_inside(
    shown: &Path,
    canonical: &Path,
    collection_identity: (u64, u64),
    io: &Io,
) -> Result<(), Stop> {
    for folder in canonical.ancestors() {
        let identity = (io.identity)(folder).map_err(|error| failed(&error, folder))?;
        if identity == collection_identity {
            return Err(Stop::Refuse(PushError::Target {
                problem: format!(
                    "the target {} is the collection or lies inside it",
                    shown.display()
                ),
            }));
        }
    }
    Ok(())
}

/// Refuses a target that holds the collection.
fn not_holding(canonical: &Path, collection: &Path, io: &Io) -> Result<(), Stop> {
    let target = (io.identity)(canonical).map_err(|error| failed(&error, canonical))?;
    for folder in collection.ancestors() {
        let identity = (io.identity)(folder).map_err(|error| failed(&error, folder))?;
        if identity == target {
            return Err(Stop::Refuse(PushError::Target {
                problem: format!(
                    "the collection lies inside the target {}",
                    canonical.display()
                ),
            }));
        }
    }
    Ok(())
}

/// The most of a marker file that is read; a real one is a line of JSON.
const MARKER_LIMIT: u64 = 64 * 1024;

/// Reads `.antiq-target` of the directory `dir`, `None` when there is none.
/// The file is looked at without following links, so that a link cannot
/// stand in for a marker.
fn read_marker(dir: &Path) -> Result<Option<Probe>, Stop> {
    let path = dir.join(FILE_NAME);
    let unreadable = |error: io::Error| {
        Stop::Refuse(PushError::Target {
            problem: format!(
                "the marker {FILE_NAME} in {} is not readable: {error}",
                dir.display()
            ),
        })
    };
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    if !meta.is_file() {
        return Ok(Some(Probe::MarkerUnreadable(
            "it is not a regular file".into(),
        )));
    }
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .and_then(|file| file.take(MARKER_LIMIT).read_to_end(&mut bytes))
        .map_err(unreadable)?;
    Ok(Some(match Marker::parse(&bytes) {
        Ok(marker) => Probe::Marker(marker),
        Err(MarkerError::TooNew { found }) => Probe::MarkerTooNew(found),
        Err(MarkerError::Invalid { reason }) => Probe::MarkerUnreadable(reason),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";
    const OTHER_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

    fn this() -> Identity {
        Identity {
            id: ID.parse().expect("a ULID"),
            host: "Studio".into(),
            path: "/srv/coll".into(),
        }
    }

    fn marker(id: &str, host: &str, path: &str) -> Probe {
        Probe::Marker(Marker::new(
            id.parse().expect("a ULID"),
            host.into(),
            path.into(),
        ))
    }

    const NONE: Flags = Flags {
        init: false,
        force: false,
    };
    const INIT: Flags = Flags {
        init: true,
        force: false,
    };
    const FORCE: Flags = Flags {
        init: false,
        force: true,
    };

    fn decided(probe: Probe, ssh: bool, flags: Flags) -> Decision {
        decide(probe, ssh, &this(), flags, "/mnt/nas/antiq")
    }

    fn refusal(decision: Decision) -> String {
        match decision {
            Decision::Refuse(error) => format!("{error}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn unreachable(decision: Decision) -> String {
        match decision {
            Decision::Unreachable(reason) => reason,
            other => panic!("expected unreachable, got {other:?}"),
        }
    }

    fn proceeds(decision: Decision) -> Proceed {
        match decision {
            Decision::Proceed(proceed) => proceed,
            other => panic!("expected to proceed, got {other:?}"),
        }
    }

    #[test]
    fn an_unreachable_probe_stays_unreachable_with_its_reason() {
        let reason = unreachable(decided(Probe::Unreachable("no route".into()), false, NONE));
        assert_eq!(reason, "no route");
        unreachable(decided(Probe::Unreachable("no route".into()), true, INIT));
    }

    #[test]
    fn a_missing_path_target_is_unreachable_also_with_init() {
        for flags in [NONE, INIT, FORCE] {
            let reason = unreachable(decided(Probe::Missing, false, flags));
            assert!(reason.contains("/mnt/nas/antiq"), "{reason}");
            assert!(reason.contains("does not exist"), "{reason}");
        }
    }

    #[test]
    fn a_missing_ssh_target_is_refused_unless_init_creates_it() {
        let message = refusal(decided(Probe::Missing, true, NONE));
        assert!(message.contains("does not exist"), "{message}");
        refusal(decided(Probe::Missing, true, FORCE));
        assert_eq!(
            proceeds(decided(Probe::Missing, true, INIT)),
            Proceed {
                write_marker: true,
                note: None
            }
        );
    }

    #[test]
    fn an_empty_directory_is_unreachable_with_a_hint_unless_init() {
        for flags in [NONE, FORCE] {
            let reason = unreachable(decided(Probe::Empty, false, flags));
            assert!(reason.contains("push --init"), "{reason}");
        }
        assert!(proceeds(decided(Probe::Empty, false, INIT)).write_marker);
        assert!(proceeds(decided(Probe::Empty, true, INIT)).write_marker);
    }

    #[test]
    fn a_directory_with_files_and_no_marker_is_always_refused() {
        for flags in [NONE, INIT, FORCE] {
            let message = refusal(decided(Probe::NotEmpty, false, flags));
            assert!(message.contains("not empty"), "{message}");
        }
    }

    #[test]
    fn a_marker_that_cannot_be_used_is_refused_with_force_too() {
        for flags in [NONE, FORCE] {
            let message = refusal(decided(
                Probe::MarkerUnreadable("bad json".into()),
                false,
                flags,
            ));
            assert!(message.contains("not readable"), "{message}");
            assert!(message.contains("bad json"), "{message}");
        }
        match decided(Probe::MarkerTooNew(2), false, FORCE) {
            Decision::Refuse(PushError::MarkerFormat(_)) => {}
            other => panic!("expected a format refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_marker_of_another_collection_is_refused_with_force_and_init_too() {
        for flags in [NONE, INIT, FORCE] {
            let message = refusal(decided(
                marker(OTHER_ID, "Studio", "/srv/coll"),
                false,
                flags,
            ));
            assert!(message.contains("another collection"), "{message}");
            assert!(message.contains(OTHER_ID), "{message}");
        }
    }

    #[test]
    fn init_is_refused_where_a_marker_of_this_collection_exists() {
        let message = refusal(decided(marker(ID, "Studio", "/srv/coll"), false, INIT));
        assert!(message.contains("already"), "{message}");
    }

    #[test]
    fn a_marker_of_another_host_or_path_names_both_unless_forced() {
        for (host, path) in [("Laptop", "/srv/coll"), ("Studio", "/old/coll")] {
            let message = refusal(decided(marker(ID, host, path), false, NONE));
            assert!(
                message.contains(host) && message.contains(path),
                "{message}"
            );
            assert!(
                message.contains("Studio") && message.contains("/srv/coll"),
                "{message}"
            );

            let forced = proceeds(decided(marker(ID, host, path), false, FORCE));
            assert!(!forced.write_marker);
            let note = forced.note.expect("a forced push is noted");
            assert!(note.contains(host) && note.contains(path), "{note}");
        }
    }

    #[test]
    fn a_matching_marker_proceeds_and_force_adds_a_note() {
        let plain = proceeds(decided(marker(ID, "Studio", "/srv/coll"), false, NONE));
        assert_eq!(
            plain,
            Proceed {
                write_marker: false,
                note: None
            }
        );
        let forced = proceeds(decided(marker(ID, "Studio", "/srv/coll"), true, FORCE));
        assert!(!forced.write_marker);
        assert!(forced.note.is_some_and(|note| note.contains("--force")));
    }

    fn error(errno: rustix::io::Errno) -> io::Error {
        io::Error::from_raw_os_error(errno.raw_os_error())
    }

    #[test]
    fn the_five_errors_of_a_gone_share_are_unreachable() {
        use rustix::io::Errno;
        for errno in [
            Errno::NOTCONN,
            Errno::HOSTDOWN,
            Errno::HOSTUNREACH,
            Errno::NETUNREACH,
            Errno::TIMEDOUT,
        ] {
            match failed(&error(errno), Path::new("/mnt/nas")) {
                Stop::Unreachable(reason) => {
                    assert!(reason.contains("/mnt/nas"), "{errno}: {reason}");
                }
                other => panic!("{errno}: expected unreachable, got {other:?}"),
            }
        }
    }

    #[test]
    fn any_other_error_is_a_refusal_naming_the_path() {
        use rustix::io::Errno;
        for errno in [Errno::ACCESS, Errno::IO] {
            match failed(&error(errno), Path::new("/mnt/nas")) {
                Stop::Refuse(PushError::Target { problem }) => {
                    assert!(problem.contains("/mnt/nas"), "{problem}");
                    assert!(problem.contains("cannot be read"), "{problem}");
                }
                other => panic!("{errno}: expected a refusal, got {other:?}"),
            }
        }
    }

    /// An `Io` that sees an existing, empty, ordinary directory, with
    /// single parts replaced by the test.
    type Call<T> = Box<dyn Fn(&Path) -> io::Result<T>>;

    struct Seen {
        canonicalize: Call<PathBuf>,
        identity: Call<(u64, u64)>,
        mount_root: Call<bool>,
        is_empty: Call<bool>,
    }

    impl Seen {
        fn plain() -> Seen {
            Seen {
                canonicalize: Box::new(|path| Ok(path.to_path_buf())),
                identity: Box::new(|path| Ok((1, path.as_os_str().len() as u64))),
                mount_root: Box::new(|_| Ok(false)),
                is_empty: Box::new(|_| Ok(true)),
            }
        }

        fn inspect(&self, root: &str) -> Result<Probe, PushError> {
            let io = Io {
                canonicalize: &self.canonicalize,
                identity: &self.identity,
                mount_root: &self.mount_root,
                is_empty: &self.is_empty,
            };
            inspect_path_with(Path::new(root), Path::new("/srv/coll"), &io).map(|(probe, _)| probe)
        }
    }

    fn refused(result: Result<Probe, PushError>) -> String {
        match result {
            Err(PushError::Target { problem }) => problem,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_unreachable_error_of_the_canonical_path_is_unreachable() {
        let mut seen = Seen::plain();
        seen.canonicalize = Box::new(|_| Err(error(rustix::io::Errno::NOTCONN)));
        assert!(matches!(
            seen.inspect("/mnt/nas/antiq"),
            Ok(Probe::Unreachable(_))
        ));
    }

    #[test]
    fn another_error_of_the_canonical_path_is_a_refusal() {
        let mut seen = Seen::plain();
        seen.canonicalize = Box::new(|_| Err(error(rustix::io::Errno::ACCESS)));
        assert!(refused(seen.inspect("/mnt/nas/antiq")).contains("/mnt/nas/antiq"));
    }

    #[test]
    fn an_error_while_looking_for_the_existing_part_of_a_missing_path_counts_too() {
        let mut seen = Seen::plain();
        seen.canonicalize = Box::new(|path| {
            if path == Path::new("/mnt/nas") {
                Err(error(rustix::io::Errno::TIMEDOUT))
            } else {
                Err(error(rustix::io::Errno::NOENT))
            }
        });
        assert!(matches!(
            seen.inspect("/mnt/nas/antiq"),
            Ok(Probe::Unreachable(_))
        ));
    }

    #[test]
    fn a_missing_path_is_missing_when_an_ancestor_exists() {
        let mut seen = Seen::plain();
        seen.canonicalize = Box::new(|path| {
            if path == Path::new("/mnt") {
                Ok(path.to_path_buf())
            } else {
                Err(error(rustix::io::Errno::NOENT))
            }
        });
        assert!(matches!(seen.inspect("/mnt/nas/antiq"), Ok(Probe::Missing)));
    }

    #[test]
    fn a_path_with_no_existing_folder_above_it_is_missing() {
        let mut seen = Seen::plain();
        seen.canonicalize = Box::new(|_| Err(error(rustix::io::Errno::NOENT)));
        assert!(matches!(seen.inspect("/mnt/nas/antiq"), Ok(Probe::Missing)));
    }

    #[test]
    fn an_error_of_the_identity_check_is_a_refusal() {
        let mut seen = Seen::plain();
        seen.identity = Box::new(|_| Err(error(rustix::io::Errno::IO)));
        assert!(refused(seen.inspect("/mnt/nas/antiq")).contains("cannot be read"));
    }

    #[test]
    fn an_error_of_the_mount_check_is_a_refusal() {
        let mut seen = Seen::plain();
        seen.mount_root = Box::new(|_| Err(error(rustix::io::Errno::IO)));
        assert!(refused(seen.inspect("/mnt/nas/antiq")).contains("cannot be read"));
    }

    #[test]
    fn an_unreachable_error_of_the_listing_is_unreachable() {
        let mut seen = Seen::plain();
        seen.is_empty = Box::new(|_| Err(error(rustix::io::Errno::HOSTDOWN)));
        assert!(matches!(
            seen.inspect("/mnt/nas/antiq"),
            Ok(Probe::Unreachable(_))
        ));
    }

    #[test]
    fn an_empty_and_a_filled_directory_are_told_apart() {
        let mut seen = Seen::plain();
        assert!(matches!(seen.inspect("/mnt/nas/antiq"), Ok(Probe::Empty)));
        seen.is_empty = Box::new(|_| Ok(false));
        assert!(matches!(
            seen.inspect("/mnt/nas/antiq"),
            Ok(Probe::NotEmpty)
        ));
    }

    #[test]
    fn the_collection_and_what_lies_below_it_are_no_target() {
        for root in ["/srv/coll", "/srv/coll/sub/dir"] {
            let seen = Seen::plain();
            assert!(refused(seen.inspect(root)).contains("collection"), "{root}");
        }
    }

    #[test]
    fn a_missing_path_below_the_collection_is_no_target_either() {
        let mut seen = Seen::plain();
        seen.canonicalize = Box::new(|path| {
            if path == Path::new("/srv/coll") {
                Ok(path.to_path_buf())
            } else {
                Err(error(rustix::io::Errno::NOENT))
            }
        });
        assert!(refused(seen.inspect("/srv/coll/sub/dir")).contains("collection"));
    }

    #[test]
    fn a_target_that_holds_the_collection_is_refused() {
        for root in ["/srv", "/"] {
            let seen = Seen::plain();
            assert!(
                refused(seen.inspect(root)).contains("inside the target"),
                "{root}"
            );
        }
    }

    #[test]
    fn a_mount_root_is_refused() {
        let mut seen = Seen::plain();
        seen.mount_root = Box::new(|_| Ok(true));
        assert!(refused(seen.inspect("/mnt/nas")).contains("mount"));
    }
}
