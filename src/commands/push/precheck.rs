// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Step 4 of `push`: what the target holds decides whether the push goes
//! on, stops as unreachable, or is refused.

use super::outcome::{Replacement, TakeOver};
use super::{Mismatch, PushError};
use crate::external::summary::Said;
use crate::filesystem::device;
use crate::id::ObjectId;
use crate::target::marker::{FILE_NAME, Marker, MarkerError};
use crate::target::rsync::{self, Call, Options, Outcome, Rsync, RsyncError};
use crate::target::url::TargetUrl;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

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
    pub on_mismatch: Mismatch,
}

/// What the table says to do.
#[derive(Debug)]
pub(super) enum Decision {
    Unreachable(String),
    Proceed(Proceed),
    Refuse(PushError),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Proceed {
    /// The marker is to be created before the mirror.
    pub write_marker: bool,
    /// The marker of this collection names another host or path and is to
    /// be sent again before the mirror.
    pub take_over: Option<TakeOver>,
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
            take_over: None,
            note: None,
        }),
        Probe::Missing => refuse(format!(
            "the target directory {shown} does not exist; create it, or push with --init to create the last level"
        )),
        Probe::Empty if flags.init => Decision::Proceed(Proceed {
            write_marker: true,
            take_over: None,
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
    match (same_place, flags.on_mismatch) {
        (true, Mismatch::Force) => Decision::Proceed(Proceed {
            write_marker: false,
            take_over: None,
            note: Some("--force changes nothing here: the marker names this host and path".into()),
        }),
        (true, _) => Decision::Proceed(Proceed {
            write_marker: false,
            take_over: None,
            note: None,
        }),
        (false, Mismatch::Force) => Decision::Proceed(Proceed {
            write_marker: false,
            take_over: None,
            note: Some(format!(
                "pushing although the marker names host `{}` and path `{}`; the marker stays as it is",
                marker.host, marker.path
            )),
        }),
        // The marker names this collection (the ID was compared above), so
        // the machine or the place is the only thing that differs.
        (false, Mismatch::TakeOver) => {
            let replace = |from: &str, to: &str| {
                (from != to).then(|| Replacement {
                    from: from.to_string(),
                    to: to.to_string(),
                })
            };
            Decision::Proceed(Proceed {
                write_marker: false,
                take_over: Some(TakeOver {
                    host: replace(&marker.host, &this.host),
                    path: replace(&marker.path, &this.path),
                }),
                note: None,
            })
        }
        (false, Mismatch::Refuse) => Decision::Refuse(PushError::MarkerMismatch {
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

/// What the inspection of a path target found: the probe, the canonical
/// path every rsync call uses from here on, and the device and inode that
/// path had, `None` when there was no directory to look at.
pub(super) type Inspected = (Probe, PathBuf, Option<(u64, u64)>);

/// Refuses a target directory that is no longer the one that was
/// inspected. A mount point or a link swapped in between would send the
/// run, which deletes, to another place; a small window remains between
/// this check and rsync's own open.
pub(super) fn still_the_same(
    canonical: &Path,
    inspected: (u64, u64),
    identity: &dyn Fn(&Path) -> io::Result<(u64, u64)>,
) -> Result<(), PushError> {
    let now = identity(canonical).map_err(|error| PushError::Target {
        problem: format!("the target {} cannot be read: {error}", canonical.display()),
    })?;
    if now == inspected {
        Ok(())
    } else {
        Err(PushError::Target {
            problem: format!(
                "the target {} is another directory than the one that was inspected",
                canonical.display()
            ),
        })
    }
}

/// Looks at the path target `root` on this machine. Returns what was found
/// and the canonical path, which the rsync calls use from here on.
pub(super) fn inspect_path(root: &Path, collection: &Path) -> Result<Inspected, PushError> {
    let identity = |path: &Path| {
        let meta = fs::metadata(path)?;
        Ok((meta.dev(), meta.ino()))
    };
    let mount_root = |path: &Path| device::is_mount_point(path).map_err(io::Error::other);
    let is_empty = |path: &Path| Ok(fs::read_dir(path)?.next().transpose()?.is_none());
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
) -> Result<Inspected, PushError> {
    match inspect(root, collection, io) {
        Ok(found) => Ok(found),
        Err(Stop::Unreachable(reason)) => {
            Ok((Probe::Unreachable(reason), root.to_path_buf(), None))
        }
        Err(Stop::Refuse(error)) => Err(error),
    }
}

fn inspect(root: &Path, collection: &Path, io: &Io) -> Result<Inspected, Stop> {
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
            return Ok((Probe::Missing, root.to_path_buf(), None));
        }
        Err(error) => return Err(failed(&error, root)),
    };
    not_inside(root, &canonical, collection_identity, io)?;
    let directory = not_holding(&canonical, collection, io)?;

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
    let shown = canonical.display().to_string();
    let probe = match read_marker(&canonical).map_err(|error| marker_stop(&error, &shown))? {
        Some(probe) => probe,
        None if empty => Probe::Empty,
        None => Probe::NotEmpty,
    };
    Ok((probe, canonical, Some(directory)))
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
fn not_holding(canonical: &Path, collection: &Path, io: &Io) -> Result<(u64, u64), Stop> {
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
    Ok(target)
}

/// The most of a marker file that is read; a real one is a line of JSON.
const MARKER_LIMIT: u64 = 64 * 1024;

/// The refusal for a marker that cannot be read.
fn marker_unreadable(error: &io::Error, shown: &str) -> PushError {
    PushError::Target {
        problem: format!("the marker {FILE_NAME} in {shown} is not readable: {error}"),
    }
}

/// How a failed read of the marker of a path target counts: the five errors
/// of a share that is gone are unreachable, every other error is a
/// refusal.
fn marker_stop(error: &io::Error, shown: &str) -> Stop {
    if is_gone(error) {
        Stop::Unreachable(format!("the target {shown} cannot be reached: {error}"))
    } else {
        Stop::Refuse(marker_unreadable(error, shown))
    }
}

/// Reads `.antiq-target` of the directory `dir`, `None` when there is
/// none.
/// The file is looked at without following links, so that a link cannot
/// stand in for a marker.
fn read_marker(dir: &Path) -> Result<Option<Probe>, io::Error> {
    let path = dir.join(FILE_NAME);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.is_file() {
        return Ok(Some(Probe::MarkerUnreadable(
            "it is not a regular file".into(),
        )));
    }
    let mut bytes = Vec::new();
    fs::File::open(&path).and_then(|file| file.take(MARKER_LIMIT).read_to_end(&mut bytes))?;
    Ok(Some(match Marker::parse(&bytes) {
        Ok(marker) => Probe::Marker(marker),
        Err(MarkerError::TooNew { found }) => Probe::MarkerTooNew(found),
        Err(MarkerError::Invalid { reason }) => Probe::MarkerUnreadable(reason),
    }))
}

// =========================================================
// An ssh target
// =========================================================

/// Runs an rsync call. The real one is [`Call::run`]; a test hands in one
/// that answers.
pub(super) type Runner<'a> = &'a dyn Fn(&Call) -> Result<Outcome, RsyncError>;

/// What the inspection of an ssh target needs.
pub(super) struct Remote<'a> {
    pub rsync: &'a Rsync,
    pub options: &'a Options,
    pub url: &'a TargetUrl,
    /// The target as the messages name it.
    pub shown: &'a str,
    /// The directory in which the fetched marker gets a directory of its own.
    pub scratch: &'a Path,
}

/// One entry of an `rsync --list-only` listing.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    /// The first character of the permissions: `-`, `d`, `l`, ...
    kind: char,
    /// The size in bytes; `None` when the listing prints it in a form that
    /// is not a plain number (`-h`).
    size: Option<u64>,
    name: String,
}

/// The entries of a listing. A line that does not have the shape of an
/// entry counts as an entry of unknown kind, so that it cannot make a
/// directory look empty.
fn parse_listing(stdout: &[u8]) -> Vec<Entry> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let unknown = || Entry {
                kind: '?',
                size: None,
                name: line.to_string(),
            };
            // permissions, size, date and time, each padded with spaces;
            // the name follows after one more space and may hold spaces.
            let mut rest = line;
            let mut size = None;
            for field in 0..4 {
                rest = rest.trim_start();
                match rest.find(char::is_whitespace) {
                    Some(end) => {
                        // Thousands separators are rsync's; a `-h` listing
                        // prints `1.50K`, which stays unparsed.
                        if field == 1 {
                            size = rest[..end].replace(',', "").parse().ok();
                        }
                        rest = &rest[end..];
                    }
                    None => return unknown(),
                }
            }
            let Some(name) = rest.strip_prefix(' ') else {
                return unknown();
            };
            let name = name.split(" -> ").next().unwrap_or(name);
            Entry {
                kind: line.chars().next().unwrap_or('?'),
                size,
                name: name.to_string(),
            }
        })
        .collect()
}

/// Lists the target over ssh and, when it holds a marker, fetches it.
pub(super) fn inspect_ssh(remote: &Remote, run: Runner) -> Result<Probe, PushError> {
    let listing = run(&remote.rsync.list(remote.options, remote.url)).map_err(PushError::Rsync)?;
    let stderr = String::from_utf8_lossy(&listing.stderr);
    match listing.code {
        Some(0) => {}
        // rsync passes on ssh's 255, or reports exit 12 for the closed
        // connection when it notices that before it has reaped ssh.
        Some(code)
            if code == 255 || (code == 12 && stderr.contains("connection unexpectedly closed")) =>
        {
            let gone = stderr.lines().any(|line| {
                line.starts_with("ssh: connect to host")
                    || line.starts_with("ssh: Could not resolve hostname")
                    || line.contains("timed out")
            });
            return if gone {
                Ok(Probe::Unreachable(format!(
                    "the target {} cannot be reached: {}",
                    remote.shown,
                    stderr.trim()
                )))
            } else {
                Err(listing_failed(remote, &listing))
            };
        }
        Some(23) if says_missing(&stderr) => return Ok(Probe::Missing),
        _ => return Err(listing_failed(remote, &listing)),
    }

    let entries = parse_listing(&listing.stdout);
    // The entry of the directory itself proves that rsync listed it. A
    // listing without one, such as the empty output of a quiet rsync, is
    // not an empty directory.
    let listed_itself = entries
        .iter()
        .filter(|entry| entry.kind == 'd' && entry.name == ".")
        .count();
    if listed_itself != 1 {
        return Err(PushError::Target {
            problem: format!(
                "the listing of the target {} did not list the directory itself, so antiq cannot tell what it holds",
                remote.shown
            ),
        });
    }
    let marker = entries.iter().find(|entry| entry.name == FILE_NAME);
    match marker {
        Some(entry) if entry.kind == '-' && entry.size.is_some_and(|size| size > MARKER_LIMIT) => {
            Ok(Probe::MarkerUnreadable("it is larger than 64 KiB".into()))
        }
        Some(entry) if entry.kind == '-' => fetch_marker(remote, run),
        Some(_) => Ok(Probe::MarkerUnreadable("it is not a regular file".into())),
        None if entries.iter().all(|entry| entry.name == ".") => Ok(Probe::Empty),
        None => Ok(Probe::NotEmpty),
    }
}

/// Whether the failing line of an exit 23 says that the directory is not
/// there: rsync's `change_dir` and `link_stat` lines end in `(2)`, the
/// `stat:` line of openrsync in `No such file or directory`.
fn says_missing(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim_end();
        ((line.contains("change_dir") || line.contains("link_stat")) && line.ends_with("(2)"))
            || (line.contains("stat:") && line.ends_with("No such file or directory"))
    })
}

fn listing_failed(remote: &Remote, outcome: &Outcome) -> PushError {
    PushError::ListingFailed {
        problem: format!(
            "listing the target {} failed: {}",
            remote.shown,
            rsync::ended(outcome)
        ),
        said: Said::of("rsync", &outcome.stderr),
    }
}

/// Fetches the marker of the ssh target into a directory of its own and
/// reads it there.
fn fetch_marker(remote: &Remote, run: Runner) -> Result<Probe, PushError> {
    let scratch = Scratch::create(remote.scratch)?;
    let outcome = run(&remote
        .rsync
        .fetch_marker(remote.options, remote.url, scratch.path()))
    .map_err(PushError::Rsync)?;
    if outcome.code != Some(0) {
        return Ok(Probe::MarkerUnreadable(format!(
            "fetching it failed: {}",
            rsync::ended(&outcome)
        )));
    }
    Ok(read_marker(scratch.path())
        .map_err(|error| marker_unreadable(&error, remote.shown))?
        .unwrap_or_else(|| Probe::MarkerUnreadable("the fetch left no file".into())))
}

static SCRATCH_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory that exists for one inspection and is removed after it.
pub(super) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub(super) fn create(parent: &Path) -> Result<Scratch, PushError> {
        let scratch_error = |source| PushError::Scratch {
            path: parent.to_path_buf(),
            source,
        };
        fs::create_dir_all(parent).map_err(scratch_error)?;
        let name = format!(
            "push-{}-{}",
            std::process::id(),
            SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = parent.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(scratch_error)?;
        Ok(Scratch { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // What stays behind in `.antiq/tmp/` is removed by the next run's
        // repairs.
        let _ = fs::remove_dir_all(&self.path);
    }
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
        on_mismatch: Mismatch::Refuse,
    };
    const INIT: Flags = Flags {
        init: true,
        on_mismatch: Mismatch::Refuse,
    };
    const FORCE: Flags = Flags {
        init: false,
        on_mismatch: Mismatch::Force,
    };
    const TAKE_OVER: Flags = Flags {
        init: false,
        on_mismatch: Mismatch::TakeOver,
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
                take_over: None,
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
                take_over: None,
                note: None
            }
        );
        let forced = proceeds(decided(marker(ID, "Studio", "/srv/coll"), true, FORCE));
        assert!(!forced.write_marker);
        assert!(forced.note.is_some_and(|note| note.contains("--force")));
    }

    fn replacement(from: &str, to: &str) -> Option<Replacement> {
        Some(Replacement {
            from: from.into(),
            to: to.into(),
        })
    }

    #[test]
    fn a_take_over_replaces_the_host_and_the_path_that_differ() {
        let cases = [
            (
                ("Laptop", "/old/coll"),
                replacement("Laptop", "Studio"),
                replacement("/old/coll", "/srv/coll"),
            ),
            (
                ("Laptop", "/srv/coll"),
                replacement("Laptop", "Studio"),
                None,
            ),
            (
                ("Studio", "/old/coll"),
                None,
                replacement("/old/coll", "/srv/coll"),
            ),
        ];
        for ((host, path), host_change, path_change) in cases {
            let proceed = proceeds(decided(marker(ID, host, path), false, TAKE_OVER));
            assert!(!proceed.write_marker);
            assert_eq!(proceed.note, None);
            assert_eq!(
                proceed.take_over,
                Some(TakeOver {
                    host: host_change,
                    path: path_change
                })
            );
        }
    }

    #[test]
    fn a_take_over_of_a_marker_that_names_this_place_changes_nothing() {
        let proceed = proceeds(decided(marker(ID, "Studio", "/srv/coll"), false, TAKE_OVER));
        assert_eq!(proceed.take_over, None);
        assert_eq!(proceed.note, None);
    }

    #[test]
    fn a_take_over_never_replaces_the_marker_of_another_collection() {
        let other = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
        let message = refusal(decided(marker(other, "Laptop", "/old"), false, TAKE_OVER));
        assert!(message.contains("another collection"), "{message}");
    }

    #[test]
    fn a_take_over_does_not_make_up_for_a_missing_marker() {
        let reason = unreachable(decided(Probe::Empty, false, TAKE_OVER));
        assert!(reason.contains("push --init"), "{reason}");
        let reason = unreachable(decided(Probe::Missing, false, TAKE_OVER));
        assert!(reason.contains("does not exist"), "{reason}");
        let refused = refusal(decided(Probe::NotEmpty, false, TAKE_OVER));
        assert!(refused.contains("not empty"), "{refused}");
        let unreadable = Probe::MarkerUnreadable("x".into());
        let refused = refusal(decided(unreadable, false, TAKE_OVER));
        assert!(refused.contains("not readable"), "{refused}");
    }

    #[test]
    fn init_is_refused_over_an_existing_marker_also_with_a_take_over() {
        let both = Flags {
            init: true,
            on_mismatch: Mismatch::TakeOver,
        };
        let message = refusal(decided(marker(ID, "Laptop", "/old"), false, both));
        assert!(message.contains("already"), "{message}");
    }

    fn error(errno: rustix::io::Errno) -> io::Error {
        io::Error::from_raw_os_error(errno.raw_os_error())
    }

    #[test]
    fn a_marker_path_that_cannot_be_looked_at_is_an_error_not_a_missing_marker() {
        let temp = tempfile::tempdir().expect("a temp directory");
        let file = temp.path().join("file");
        fs::write(&file, "x").expect("the file is written");

        let error = read_marker(&file).expect_err("a file has no marker below it");

        assert_ne!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_marker_read_that_fails_with_a_gone_share_error_is_unreachable() {
        use rustix::io::Errno;
        for errno in [
            Errno::NOTCONN,
            Errno::HOSTDOWN,
            Errno::HOSTUNREACH,
            Errno::NETUNREACH,
            Errno::TIMEDOUT,
        ] {
            match marker_stop(&error(errno), "/mnt/nas") {
                Stop::Unreachable(reason) => {
                    assert!(reason.contains("/mnt/nas"), "{errno}: {reason}");
                }
                other => panic!("{errno}: expected unreachable, got {other:?}"),
            }
        }
        match marker_stop(&error(rustix::io::Errno::ACCESS), "/mnt/nas") {
            Stop::Refuse(refusal) => {
                assert!(refusal.to_string().contains("not readable"), "{refusal}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
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
    type Seam<T> = Box<dyn Fn(&Path) -> io::Result<T>>;

    struct Seen {
        canonicalize: Seam<PathBuf>,
        identity: Seam<(u64, u64)>,
        mount_root: Seam<bool>,
        is_empty: Seam<bool>,
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
            inspect_path_with(Path::new(root), Path::new("/srv/coll"), &io)
                .map(|(probe, _, _)| probe)
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
    fn a_target_directory_that_changed_is_refused_and_the_same_one_passes() {
        let canonical = Path::new("/mnt/nas/antiq");
        still_the_same(canonical, (1, 5), &|_| Ok((1, 5))).expect("the same");
        match still_the_same(canonical, (1, 5), &|_| Ok((2, 5))) {
            Err(PushError::Target { problem }) => {
                assert!(problem.contains("another directory"), "{problem}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        match still_the_same(canonical, (1, 5), &|_| Err(error(rustix::io::Errno::NOENT))) {
            Err(PushError::Target { problem }) => assert!(problem.contains("cannot be read")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_mount_root_is_refused() {
        let mut seen = Seen::plain();
        seen.mount_root = Box::new(|_| Ok(true));
        assert!(refused(seen.inspect("/mnt/nas")).contains("mount"));
    }

    // -----------------------------------------------------
    // An ssh target
    // -----------------------------------------------------

    use crate::external::RunLog;
    use crate::target::rsync::{Options, Outcome};
    use crate::testing::fake_rsync::FakeRsync;
    use std::cell::RefCell;

    const RSYNC_3_5_1: &str =
        "rsync  version 3.5.1  protocol version 33\nCopyright (C) 1996-2026\n";
    const DOT: &str = "drwxr-xr-x          4,096 2026/11/04 20:07:08 .\n";

    fn entry(kind: char, size: Option<u64>, name: &str) -> Entry {
        Entry {
            kind,
            size,
            name: name.into(),
        }
    }

    #[test]
    fn a_listing_is_taken_apart_into_kind_and_name() {
        let listing = format!(
            "{DOT}-rw-r--r--             12 2026/11/04 20:07:08 .antiq-target\n\
             drwxr-xr-x             64 2026/11/04 20:07:08 my folder\n\
             lrwxr-xr-x              5 2026/11/04 20:07:08 link -> elsewhere\n\
             -rw-r--r--      1,234,567 2026/11/04 20:07:08  odd  name \n\
             -rw-r--r--          1.50K 2026/11/04 20:07:08 human\n"
        );
        assert_eq!(
            parse_listing(listing.as_bytes()),
            [
                entry('d', Some(4096), "."),
                entry('-', Some(12), ".antiq-target"),
                entry('d', Some(64), "my folder"),
                entry('l', Some(5), "link"),
                entry('-', Some(1_234_567), " odd  name "),
                entry('-', None, "human"),
            ]
        );
    }

    #[test]
    fn a_line_of_another_shape_counts_as_an_entry() {
        assert_eq!(
            parse_listing(b"surprise\n\n"),
            [entry('?', None, "surprise")]
        );
        assert_eq!(parse_listing(b"a b c d"), [entry('?', None, "a b c d")]);
        assert_eq!(parse_listing(b"a b c d\t"), [entry('?', None, "a b c d\t")]);
        assert_eq!(parse_listing(b""), []);
    }

    #[test]
    fn exit_23_says_missing_only_for_the_lines_of_the_spec() {
        for line in [
            "rsync: [sender] change_dir \"/srv/antiq\" failed: No such file or directory (2)",
            "rsync: [sender] link_stat \"/srv/antiq/.\" failed: No such file or directory (2)",
            "openrsync: (l)stat: /srv/antiq: No such file or directory",
        ] {
            assert!(says_missing(&format!("{line}\n")), "{line}");
        }
        for line in [
            "rsync: [sender] change_dir \"/srv\" failed: Permission denied (13)",
            "rsync: [sender] some other failure (2) happened",
            "rsync error: some files/attrs were not transferred (code 23) at main.c(1)",
        ] {
            assert!(!says_missing(&format!("{line}\n")), "{line}");
        }
    }

    /// What the stand-in runner is told to answer, by call.
    struct Script {
        answers: Vec<Outcome>,
        calls: RefCell<Vec<Vec<String>>>,
        /// Written as the marker when a call fetches one.
        fetched: Option<&'static str>,
    }

    fn outcome(code: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
        Outcome::finished(code, stdout, stderr)
    }

    impl Script {
        fn new(answers: Vec<Outcome>) -> Script {
            Script {
                answers,
                calls: RefCell::new(Vec::new()),
                fetched: None,
            }
        }

        fn run(&self, call: &Call) -> Result<Outcome, RsyncError> {
            let args: Vec<String> = call
                .args()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let number = self.calls.borrow().len();
            if let (Some(text), true) = (
                self.fetched,
                args.iter().any(|a| a.ends_with("/.antiq-target")),
            ) {
                let into = args.last().expect("the directory comes last");
                fs::write(Path::new(into).join(".antiq-target"), text).expect("written");
            }
            self.calls.borrow_mut().push(args);
            Ok(self.answers[number.min(self.answers.len() - 1)].clone())
        }
    }

    struct Ssh {
        _fake: FakeRsync,
        rsync: Rsync,
        url: TargetUrl,
        scratch: tempfile::TempDir,
    }

    fn ssh() -> Ssh {
        let fake = FakeRsync::new(RSYNC_3_5_1);
        let rsync = Rsync::probe(&fake.path(), &RunLog::discard()).expect("the stand-in passes");
        Ssh {
            _fake: fake,
            rsync,
            url: TargetUrl::parse("backup@nas:/srv/antiq").expect("a target"),
            scratch: tempfile::tempdir().expect("a temp dir"),
        }
    }

    impl Ssh {
        fn inspect(&self, script: &Script) -> Result<Probe, PushError> {
            let options = Options {
                fsync: true,
                rsync_path: None,
                rsync_args: Vec::new(),
            };
            inspect_ssh(
                &Remote {
                    rsync: &self.rsync,
                    options: &options,
                    url: &self.url,
                    shown: "backup@nas:/srv/antiq",
                    scratch: self.scratch.path(),
                },
                &|call| script.run(call),
            )
        }
    }

    fn unreachable_text(result: Result<Probe, PushError>) -> String {
        match result {
            Ok(Probe::Unreachable(reason)) => reason,
            other => panic!("expected unreachable, got {other:?}"),
        }
    }

    #[test]
    fn ssh_lines_of_a_target_that_cannot_be_reached_are_unreachable() {
        for line in [
            "ssh: connect to host nas port 22: Connection refused",
            "ssh: Could not resolve hostname nas: Name or service not known",
            "ssh: connect to host nas port 22: Operation timed out",
            "Connection timed out during banner exchange",
        ] {
            let script = Script::new(vec![outcome(Some(255), "", &format!("{line}\n"))]);
            let reason = unreachable_text(ssh().inspect(&script));
            assert!(reason.contains(line), "{reason}");
            assert!(reason.contains("backup@nas:/srv/antiq"), "{reason}");
        }
    }

    #[test]
    fn a_closed_connection_with_an_ssh_line_counts_like_a_255() {
        // rsync reports exit 12 instead of ssh's 255 when it notices the
        // closed connection before it has reaped ssh.
        let closed = "rsync: connection unexpectedly closed (0 bytes received so far) [Receiver]\n\
                      rsync error: error in rsync protocol data stream (code 12) at io.c(303)\n";
        let refused = format!("ssh: connect to host nas port 22: Connection refused\n{closed}");
        let script = Script::new(vec![outcome(Some(12), "", &refused)]);
        let reason = unreachable_text(ssh().inspect(&script));
        assert!(reason.contains("Connection refused"), "{reason}");

        for stderr in [
            closed,
            "ssh: connect to host nas port 22: Connection refused\n",
        ] {
            let script = Script::new(vec![outcome(Some(12), "", stderr)]);
            assert!(
                matches!(ssh().inspect(&script), Err(PushError::ListingFailed { .. })),
                "{stderr}"
            );
        }
    }

    #[test]
    fn any_other_255_is_a_refusal_with_the_line() {
        for line in [
            "Host key verification failed.",
            "someone@nas: Permission denied (publickey).",
        ] {
            let script = Script::new(vec![outcome(Some(255), "", &format!("{line}\n"))]);
            match ssh().inspect(&script) {
                Err(PushError::ListingFailed { problem, said }) => {
                    assert!(
                        said.groups.iter().any(|group| group.line == line),
                        "{said:?}"
                    );
                    assert!(problem.contains("255"), "{problem}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn exit_23_is_a_missing_directory_only_with_the_not_found_line() {
        let missing =
            "rsync: [sender] change_dir \"/srv/antiq\" failed: No such file or directory (2)\n";
        let script = Script::new(vec![outcome(Some(23), "", missing)]);
        assert!(matches!(ssh().inspect(&script), Ok(Probe::Missing)));

        let denied = "rsync: [sender] change_dir \"/srv\" failed: Permission denied (13)\n";
        let script = Script::new(vec![outcome(Some(23), "", denied)]);
        match ssh().inspect(&script) {
            Err(PushError::ListingFailed { said, .. }) => {
                assert!(said.groups[0].line.contains("Permission denied"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_timeout_or_a_signal_of_the_listing_is_a_refusal() {
        for (code, expected) in [(Some(30), "exit 30"), (None, "signal")] {
            let mut ended = outcome(code, "", "Timeout in data send/receive\n");
            ended.signal = code.is_none().then_some(9);
            let script = Script::new(vec![ended]);
            match ssh().inspect(&script) {
                Err(PushError::ListingFailed { problem, said }) => {
                    assert!(problem.contains(expected), "{problem}");
                    assert_eq!(said.groups[0].line, "Timeout in data send/receive");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_rsync_that_cannot_start_is_the_rsync_error() {
        let ssh = ssh();
        let options = Options {
            fsync: true,
            rsync_path: None,
            rsync_args: Vec::new(),
        };
        let result = inspect_ssh(
            &Remote {
                rsync: &ssh.rsync,
                options: &options,
                url: &ssh.url,
                shown: "x",
                scratch: ssh.scratch.path(),
            },
            &|_| {
                Err(RsyncError::Spawn {
                    path: "rsync".into(),
                    cause: io::Error::other("gone"),
                })
            },
        );
        assert!(matches!(result, Err(PushError::Rsync(_))), "{result:?}");
    }

    #[test]
    fn a_listing_with_only_the_dot_is_empty_and_one_with_files_is_not() {
        let script = Script::new(vec![outcome(Some(0), DOT, "")]);
        assert!(matches!(ssh().inspect(&script), Ok(Probe::Empty)));
        let more = format!("{DOT}-rw-r--r--  1 2026/11/04 20:07:08 stray.txt\n");
        let script = Script::new(vec![outcome(Some(0), &more, "")]);
        assert!(matches!(ssh().inspect(&script), Ok(Probe::NotEmpty)));
    }

    #[test]
    fn a_listing_without_the_directory_itself_is_a_refusal_and_never_empty() {
        // Without the entry of the directory, silence (rsync's `-q` in the
        // user's options) cannot be told from an empty directory.
        for stdout in [
            "",
            "\n",
            "-rw-r--r--  12 2026/11/04 20:07:08 stray.txt\n",
            "-rw-r--r--  12 2026/11/04 20:07:08 .\n",
            "not a listing line\n",
        ] {
            let script = Script::new(vec![outcome(Some(0), stdout, "")]);
            match ssh().inspect(&script) {
                Err(PushError::Target { problem }) => {
                    assert!(problem.contains("backup@nas:/srv/antiq"), "{problem}");
                    assert!(problem.contains("not list the directory"), "{problem}");
                }
                other => panic!("expected a refusal for {stdout:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_calls_list_the_directory_and_fetch_the_marker_into_a_directory_of_their_own() {
        let listing = format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 .antiq-target\n");
        let mut script = Script::new(vec![
            outcome(Some(0), &listing, ""),
            outcome(Some(0), "", ""),
        ]);
        script.fetched = Some(
            "{\"format\":1,\"id\":\"01BX5ZZKBKACTAV9WEVGEMMVRY\",\"host\":\"Studio\",\"path\":\"/srv/coll\"}\n",
        );
        let ssh = ssh();

        let probe = ssh.inspect(&script).expect("inspected");

        assert!(matches!(probe, Probe::Marker(_)), "{probe:?}");
        let calls = script.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[0].contains(&"--list-only".to_string()),
            "{:?}",
            calls[0]
        );
        assert_eq!(calls[0].last().expect("a target"), "backup@nas:/srv/antiq/");
        assert!(!calls[0].contains(&"--fsync".to_string()), "{:?}", calls[0]);
        assert_eq!(
            calls[1][calls[1].len() - 2],
            "backup@nas:/srv/antiq/.antiq-target"
        );
        let into = calls[1].last().expect("a directory");
        assert!(
            into.starts_with(ssh.scratch.path().to_str().expect("UTF-8")),
            "{into}"
        );
        assert!(!Path::new(into).exists(), "the directory is removed again");
    }

    #[test]
    fn a_marker_over_64_kib_in_the_listing_is_not_fetched() {
        let listing = format!("{DOT}-rw-r--r--  65,537 2026/11/04 20:07:08 .antiq-target\n");
        let script = Script::new(vec![outcome(Some(0), &listing, "")]);
        match ssh().inspect(&script).expect("inspected") {
            Probe::MarkerUnreadable(why) => assert!(why.contains("64 KiB"), "{why}"),
            other => panic!("expected an unreadable marker, got {other:?}"),
        }
        assert_eq!(script.calls.borrow().len(), 1);
    }

    #[test]
    fn a_marker_of_64_kib_or_of_a_size_that_does_not_parse_is_fetched() {
        for size in ["65,536", "1.50K", "12"] {
            let listing = format!("{DOT}-rw-r--r--  {size} 2026/11/04 20:07:08 .antiq-target\n");
            let script = Script::new(vec![
                outcome(Some(0), &listing, ""),
                outcome(Some(0), "", ""),
            ]);
            ssh().inspect(&script).expect("inspected");
            assert_eq!(script.calls.borrow().len(), 2, "{size}");
        }
    }

    #[test]
    fn a_marker_that_is_not_a_regular_file_in_the_listing_is_not_fetched() {
        let listing = format!("{DOT}drwxr-xr-x  64 2026/11/04 20:07:08 .antiq-target\n");
        let script = Script::new(vec![outcome(Some(0), &listing, "")]);
        let probe = ssh().inspect(&script).expect("inspected");
        assert!(matches!(probe, Probe::MarkerUnreadable(_)), "{probe:?}");
        assert_eq!(script.calls.borrow().len(), 1);
    }

    #[test]
    fn a_fetch_that_fails_or_leaves_nothing_usable_is_an_unreadable_marker() {
        let listing = format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 .antiq-target\n");
        let cases = [
            (
                outcome(Some(23), "", "rsync: link_stat failed\n"),
                None,
                "failed: rsync ended with exit 23",
            ),
            (outcome(Some(0), "", ""), None, "left no file"),
            (outcome(Some(0), "", ""), Some("not json\n"), ""),
        ];
        for (fetch, written, expected) in cases {
            let mut script = Script::new(vec![outcome(Some(0), &listing, ""), fetch]);
            script.fetched = written;
            match ssh().inspect(&script).expect("inspected") {
                Probe::MarkerUnreadable(why) => {
                    assert!(!why.is_empty() && why.contains(expected), "{why}");
                }
                other => panic!("expected an unreadable marker, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_marker_of_a_newer_format_is_told_apart_when_fetched() {
        let listing = format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 .antiq-target\n");
        let mut script = Script::new(vec![
            outcome(Some(0), &listing, ""),
            outcome(Some(0), "", ""),
        ]);
        script.fetched = Some("{\"format\":2}\n");
        assert!(matches!(ssh().inspect(&script), Ok(Probe::MarkerTooNew(2))));
    }

    #[test]
    fn a_fetch_that_cannot_start_is_the_rsync_error() {
        let listing = format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 .antiq-target\n");
        let ssh = ssh();
        let options = Options {
            fsync: true,
            rsync_path: None,
            rsync_args: Vec::new(),
        };
        let first = RefCell::new(true);
        let result = inspect_ssh(
            &Remote {
                rsync: &ssh.rsync,
                options: &options,
                url: &ssh.url,
                shown: "x",
                scratch: ssh.scratch.path(),
            },
            &|_| {
                if first.replace(false) {
                    Ok(outcome(Some(0), &listing, ""))
                } else {
                    Err(RsyncError::Spawn {
                        path: "rsync".into(),
                        cause: io::Error::other("gone"),
                    })
                }
            },
        );
        assert!(matches!(result, Err(PushError::Rsync(_))), "{result:?}");
    }

    #[test]
    fn a_scratch_directory_that_cannot_be_made_is_an_error() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let file = temp.path().join("file");
        fs::write(&file, "x").expect("written");
        match Scratch::create(&file.join("below")) {
            Err(PushError::Scratch { path, .. }) => assert_eq!(path, file.join("below")),
            other => panic!("expected a scratch error, got {:?}", other.map(|_| ())),
        }

        use std::os::unix::fs::PermissionsExt;
        let locked = temp.path().join("locked");
        fs::create_dir(&locked).expect("made");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("locked");
        assert!(
            fs::File::create(locked.join("probe")).is_err(),
            "the test runs as a user who cannot write there"
        );
        let result = Scratch::create(&locked);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlocked");
        assert!(matches!(result, Err(PushError::Scratch { .. })));
    }
}
