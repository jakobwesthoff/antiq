// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The extraction that `unearth` and `fork` share.

#![allow(dead_code, reason = "the restore core that uses these follows")]

use crate::archive::extract::ExtractionFlush;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::{Durable, RealDurable};
use crate::filesystem::tree::TreeRoot;
use crate::listing::{self, Listing, ListingEntry, ListingError, ListingType};
use crate::markers::RESTORED_FILE;
use crate::pin::PIN_FILE;
use crate::reference::{self, CandidateSet, ReferenceError};
use crate::relpath::{Name, RelPath};
use crate::versions::VersionsError;
use crate::versions::damage::{self, Check, Condition, Places};
use crate::versions::naming::{self, Candidate, Lookup};
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StoreVersion};
use jiff::tz::TimeZone;
use rustix::io::Errno;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// Why a restore failed.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("{0}")]
    Usage(String),
    #[error("cannot find the piece: {0}")]
    Reference(#[from] ReferenceError),
    #[error("cannot read the store: {0}")]
    Versions(#[from] VersionsError),
    #[error("cannot read the listing: {0}")]
    Listing(#[from] ListingError),
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("`{input}` matches no version of {piece}; there are {}", stamps.join(", "))]
    NoSuchVersion {
        piece: String,
        input: String,
        stamps: Vec<String>,
    },
    #[error("`{input}` matches several versions of {piece}: {}", stamps.join(", "))]
    AmbiguousVersion {
        piece: String,
        input: String,
        stamps: Vec<String>,
    },
    #[error("version {stamp} of {piece} has no archive; `forget` removes what is left of it")]
    Incomplete { piece: String, stamp: String },
    #[error(
        "version {stamp} of {piece} is damaged: {}. `fork` extracts it with a warning and keeps it, `rebuild` rebuilds its listing and checksum file from the archive, or the missing file can be copied back from the target",
        reasons.join("; ")
    )]
    Damaged {
        piece: String,
        stamp: String,
        reasons: Vec<String>,
    },
    #[error(
        "`{reference}` matches no piece with an archive; {piece} holds only incomplete versions, and `forget` removes what is left of them"
    )]
    OnlyIncomplete { reference: String, piece: String },
    #[error("version {stamp} of {piece} has a listing of a newer format than this antiq reads")]
    FormatTooNew { piece: String, stamp: String },
    #[error("version {stamp} of {piece} holds `{name}` at its top, and it is not a file")]
    MarkerNotAFile {
        piece: String,
        stamp: String,
        name: String,
    },
}

impl HasKind for RestoreError {
    fn kind(&self) -> ErrorKind {
        match self {
            RestoreError::Usage(_) => ErrorKind::Usage,
            RestoreError::Reference(error) => error.kind(),
            RestoreError::Listing(error) => error.kind(),
            RestoreError::FormatTooNew { .. } => ErrorKind::FormatTooNew,
            RestoreError::OnlyIncomplete { .. } => ErrorKind::UnknownReference,
            _ => ErrorKind::Other,
        }
    }
}

// =========================================================
// Choosing the piece and the version
// =========================================================

/// The version to bring back, with everything the checks on it read.
#[derive(Debug)]
pub(crate) struct Chosen {
    pub piece: RelPath,
    pub stamp: Stamp,
    pub version: StoreVersion,
    pub listing: Listing,
}

/// Finds the piece `given` names in the store and the version of it to
/// bring back, and refuses a version that cannot be: one without an
/// archive, a damaged one, one of a newer format, and one whose top holds
/// a marker's name as anything but a file.
pub(crate) fn choose_version(
    collection: &Collection,
    places: &Places,
    cwd: &Path,
    given: &OsStr,
    version: Option<&str>,
    zone: &TimeZone,
) -> Result<Chosen, RestoreError> {
    if given.is_empty() {
        return Err(RestoreError::Usage("the piece is empty".to_string()));
    }
    if version == Some("") {
        return Err(RestoreError::Usage("the version is empty".to_string()));
    }

    let scan = store::scan(places.pieces_dir)?;
    let piece = match reference::resolve_piece(
        collection,
        cwd,
        given,
        &CandidateSet::store(collection, &scan),
    ) {
        Ok(piece) => piece,
        Err(ReferenceError::Unknown { reference }) => {
            // A folder that holds only sidecars is no piece to unearth, but
            // the person who names it is told what to do with it.
            let wider = CandidateSet::store_and_sidecar_only(collection, &scan);
            return Err(
                match reference::resolve_piece(collection, cwd, given, &wider) {
                    Ok(piece) => RestoreError::OnlyIncomplete {
                        reference,
                        piece: piece.to_string(),
                    },
                    Err(_) => ReferenceError::Unknown { reference }.into(),
                },
            );
        }
        Err(other) => return Err(other.into()),
    };
    let mut versions = scan
        .pieces
        .iter()
        .find(|stored| stored.path == piece)
        .map(|stored| stored.versions.clone())
        .expect("the resolved piece is one of the scan");
    versions.sort_by_key(|candidate| std::cmp::Reverse(candidate.stamp));

    let version = match version {
        None => versions
            .iter()
            .find(|candidate| !candidate.is_incomplete())
            .expect("a piece of the store has a version with an archive"),
        Some(input) => {
            let candidates: Vec<Candidate> = versions
                .iter()
                .map(|candidate| Candidate {
                    stamp: candidate.stamp,
                    incomplete: candidate.is_incomplete(),
                })
                .collect();
            let stamps = |stamps: &[Stamp]| stamps.iter().map(Stamp::to_string).collect();
            match naming::find_version(input, &candidates, zone) {
                Lookup::Found(stamp) => versions
                    .iter()
                    .find(|candidate| candidate.stamp == stamp)
                    .expect("a found stamp is one of the candidates"),
                Lookup::NoMatch => {
                    return Err(RestoreError::NoSuchVersion {
                        piece: piece.to_string(),
                        input: input.to_string(),
                        stamps: stamps(&candidates.iter().map(|c| c.stamp).collect::<Vec<_>>()),
                    });
                }
                Lookup::Several(found) => {
                    return Err(RestoreError::AmbiguousVersion {
                        piece: piece.to_string(),
                        input: input.to_string(),
                        stamps: stamps(&found),
                    });
                }
            }
        }
    };
    let (piece_text, stamp_text) = (piece.to_string(), version.stamp.to_string());
    if version.is_incomplete() {
        return Err(RestoreError::Incomplete {
            piece: piece_text,
            stamp: stamp_text,
        });
    }

    let inspection = damage::inspect(places, &piece, version, Check::Full)?;
    match inspection.condition {
        Condition::Sound => {}
        Condition::Incomplete | Condition::BeingStored => {
            return Err(RestoreError::Incomplete {
                piece: piece_text,
                stamp: stamp_text,
            });
        }
        Condition::NewerFormat => {
            return Err(RestoreError::FormatTooNew {
                piece: piece_text,
                stamp: stamp_text,
            });
        }
        Condition::Damaged(damages) => {
            return Err(RestoreError::Damaged {
                piece: piece_text,
                stamp: stamp_text,
                reasons: damages.iter().map(ToString::to_string).collect(),
            });
        }
    }

    let store_root = TreeRoot::open(places.pieces_dir).map_err(|source| RestoreError::Read {
        path: places.pieces_dir.display().to_string(),
        source,
    })?;
    let listing_name = version
        .listings
        .first()
        .expect("a sound version has a listing");
    let listing_path = piece.join(
        &Name::from_bytes(listing_name.as_encoded_bytes().to_vec())
            .expect("a scanned file name is a valid name"),
    );
    let file = store_root
        .open_read(&listing_path)
        .map_err(|source| RestoreError::Read {
            path: listing_path.to_string(),
            source,
        })?;
    let listing = listing::read_full(file)?;
    if let Some(name) = marker_not_a_file(&listing.entries) {
        return Err(RestoreError::MarkerNotAFile {
            piece: piece_text,
            stamp: stamp_text,
            name,
        });
    }

    Ok(Chosen {
        piece,
        stamp: version.stamp,
        version: version.clone(),
        listing,
    })
}

/// The first entry at the top of the piece that has a marker's name and is
/// not what that marker is: `.antiq-restored` must be a file, and
/// `.antiq-pin` a file or a symlink. A hard link is extracted as a file.
pub(crate) fn marker_not_a_file(entries: &[ListingEntry]) -> Option<String> {
    entries.iter().find_map(|entry| {
        let mut components = entry.path.components();
        let (Some(name), None) = (components.next(), components.next()) else {
            return None;
        };
        let is_file = matches!(entry.kind, ListingType::File | ListingType::Hardlink);
        let fits = if name == RESTORED_FILE.as_bytes() {
            is_file
        } else if name == PIN_FILE.as_bytes() {
            is_file || entry.kind == ListingType::Symlink
        } else {
            return None;
        };
        (!fits).then(|| String::from_utf8_lossy(name).into_owned())
    })
}

// =========================================================
// Flushing the extracted tree
// =========================================================

/// The operating system families that flush differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum System {
    MacOs,
    Linux,
    Other,
}

impl System {
    fn current() -> System {
        if cfg!(target_os = "macos") {
            System::MacOs
        } else if cfg!(target_os = "linux") {
            System::Linux
        } else {
            System::Other
        }
    }
}

/// How the extracted tree is made durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    /// Every file and directory is flushed on its own.
    PerItem,
    /// A cheap flush per item where the platform needs one, and one
    /// barrier over the whole volume at the end.
    Barrier,
}

/// The kernel `(major, minor)` of a `uname` release such as
/// `5.15.0-91-generic`.
pub(crate) fn parse_kernel(release: &str) -> Option<(u32, u32)> {
    let mut numbers = release.split('.');
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Picks the method from the platform and from what the one try of the
/// barrier answered. An answer that is no refusal is an error.
///
/// macOS refuses the full flush with `ENOTSUP` on filesystems that cannot
/// do it. On Linux `syncfs` reports the write errors of the whole
/// filesystem from kernel 5.8 on only; before that it can return success
/// for data that failed to reach the disk, so older kernels flush per
/// item. `EINVAL` and `ENOSYS` mean the call is unknown.
pub(crate) fn choose_method(
    os: System,
    kernel: Option<(u32, u32)>,
    probe: &io::Result<()>,
) -> io::Result<Method> {
    let refusals: &[Errno] = match os {
        System::MacOs => &[Errno::NOTSUP],
        System::Linux => &[Errno::INVAL, Errno::NOSYS],
        System::Other => return Ok(Method::PerItem),
    };
    let accepted = match probe {
        Ok(()) => true,
        Err(error)
            if refusals
                .iter()
                .any(|refusal| error.raw_os_error() == Some(refusal.raw_os_error())) =>
        {
            false
        }
        Err(error) => {
            return Err(match error.raw_os_error() {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(error.kind(), error.to_string()),
            });
        }
    };
    let kernel_reports_errors =
        os != System::Linux || kernel.is_some_and(|kernel| kernel >= (5, 8));
    Ok(if accepted && kernel_reports_errors {
        Method::Barrier
    } else {
        Method::PerItem
    })
}

type Barrier = Box<dyn Fn(&File) -> io::Result<()> + Send + Sync>;

/// What the platform provides; tests put their own in its place.
pub(crate) struct Platform {
    pub os: System,
    pub kernel: Option<(u32, u32)>,
    /// Flushes everything the volume of the file holds.
    pub barrier: Barrier,
    /// The flush of one item in [`Method::PerItem`].
    pub durable: Box<dyn Durable>,
}

impl Platform {
    pub(crate) fn real() -> Platform {
        Platform {
            os: System::current(),
            kernel: kernel_version(),
            barrier: Box::new(real_barrier),
            durable: Box::new(RealDurable),
        }
    }
}

#[cfg(target_os = "linux")]
fn kernel_version() -> Option<(u32, u32)> {
    parse_kernel(rustix::system::uname().release().to_str().ok()?)
}

#[cfg(not(target_os = "linux"))]
fn kernel_version() -> Option<(u32, u32)> {
    None
}

/// `syncfs` on Linux. `sync_all` is `F_FULLFSYNC` on macOS, which asks
/// the drive to empty its cache for everything written before, not only
/// for this file.
#[cfg(target_os = "linux")]
fn real_barrier(file: &File) -> io::Result<()> {
    Ok(rustix::fs::syncfs(file)?)
}

#[cfg(not(target_os = "linux"))]
fn real_barrier(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Makes the files and directories of an extraction durable, per item or
/// with one barrier at the end.
pub(crate) struct TreeFlush {
    method: Method,
    os: System,
    probe: File,
    probe_path: PathBuf,
    barrier: Barrier,
    durable: Box<dyn Durable>,
}

impl TreeFlush {
    pub(crate) fn begin(extract_dir: &Path) -> io::Result<TreeFlush> {
        TreeFlush::begin_with(extract_dir, Platform::real())
    }

    pub(crate) fn begin_with(extract_dir: &Path, platform: Platform) -> io::Result<TreeFlush> {
        let probe_path = extract_dir.join(".flush-probe");
        let probe = File::create(&probe_path)?;
        // Opened before the extraction starts, so that on Linux the
        // descriptor's error state is older than every write it must report.
        let tried = (platform.barrier)(&probe);
        let method = choose_method(platform.os, platform.kernel, &tried)?;
        Ok(TreeFlush {
            method,
            os: platform.os,
            probe,
            probe_path,
            barrier: platform.barrier,
            durable: platform.durable,
        })
    }

    pub(crate) fn method(&self) -> Method {
        self.method
    }

    /// Runs the barrier of [`Method::Barrier`]; [`Method::PerItem`] has
    /// nothing left to do. The probe file is removed either way.
    pub(crate) fn finish(self) -> io::Result<()> {
        match self.method {
            Method::Barrier => (self.barrier)(&self.probe),
            Method::PerItem => Ok(()),
        }
    }
}

impl Drop for TreeFlush {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.probe_path);
    }
}

impl TreeFlush {
    /// The flush per item that the barrier relies on: macOS needs the
    /// data handed to the drive with `fsync` before the full flush can
    /// cover it, Linux needs nothing as `syncfs` writes it all.
    fn cheap(&self, file: &File) -> io::Result<()> {
        match self.os {
            System::MacOs => Ok(rustix::fs::fsync(file)?),
            System::Linux | System::Other => Ok(()),
        }
    }
}

fn into_io(error: FilesystemError) -> io::Error {
    match error {
        FilesystemError::Io { source, .. } => source,
        other => io::Error::other(other),
    }
}

impl ExtractionFlush for TreeFlush {
    fn file(&self, file: &File, path: &RelPath) -> io::Result<()> {
        match self.method {
            Method::PerItem => self
                .durable
                .file(file, Path::new(&path.to_string()))
                .map_err(into_io),
            Method::Barrier => self.cheap(file),
        }
    }

    fn directory(&self, directory: &File, path: &RelPath) -> io::Result<()> {
        match self.method {
            Method::PerItem => self
                .durable
                .dir(directory, Path::new(&path.to_string()))
                .map_err(into_io),
            Method::Barrier => self.cheap(directory),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::atomic::recording::{Flush, Recording};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn errno(errno: Errno) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(errno.raw_os_error()))
    }

    #[test]
    fn the_kernel_comes_from_the_release() {
        assert_eq!(parse_kernel("5.8.0"), Some((5, 8)));
        assert_eq!(parse_kernel("5.15.0-91-generic"), Some((5, 15)));
        assert_eq!(parse_kernel("4.19.2"), Some((4, 19)));
        assert_eq!(parse_kernel("6.1"), Some((6, 1)));
    }

    #[test]
    fn a_release_without_numbers_has_no_kernel() {
        assert_eq!(parse_kernel("garbage"), None);
        assert_eq!(parse_kernel(""), None);
        assert_eq!(parse_kernel("5"), None);
        assert_eq!(parse_kernel("5.x.1"), None);
    }

    #[test]
    fn macos_takes_the_barrier_when_the_full_flush_works() {
        assert_eq!(
            choose_method(System::MacOs, None, &Ok(())).unwrap(),
            Method::Barrier
        );
    }

    #[test]
    fn macos_flushes_per_item_when_the_full_flush_is_not_supported() {
        let refused = errno(Errno::NOTSUP);
        assert_eq!(
            choose_method(System::MacOs, None, &refused).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn linux_takes_the_barrier_from_kernel_5_8_on() {
        assert_eq!(
            choose_method(System::Linux, Some((5, 8)), &Ok(())).unwrap(),
            Method::Barrier
        );
        assert_eq!(
            choose_method(System::Linux, Some((6, 1)), &Ok(())).unwrap(),
            Method::Barrier
        );
    }

    #[test]
    fn linux_flushes_per_item_below_kernel_5_8_or_without_a_kernel() {
        assert_eq!(
            choose_method(System::Linux, Some((5, 7)), &Ok(())).unwrap(),
            Method::PerItem
        );
        assert_eq!(
            choose_method(System::Linux, Some((4, 19)), &Ok(())).unwrap(),
            Method::PerItem
        );
        assert_eq!(
            choose_method(System::Linux, None, &Ok(())).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn linux_flushes_per_item_when_syncfs_is_refused() {
        for refusal in [Errno::INVAL, Errno::NOSYS] {
            assert_eq!(
                choose_method(System::Linux, Some((5, 15)), &errno(refusal)).unwrap(),
                Method::PerItem,
                "{refusal:?}"
            );
        }
    }

    #[test]
    fn other_systems_flush_per_item() {
        assert_eq!(
            choose_method(System::Other, Some((9, 9)), &Ok(())).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn a_probe_error_that_is_no_refusal_is_returned() {
        let error = choose_method(System::MacOs, None, &errno(Errno::IO)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
        let error = choose_method(System::Linux, Some((5, 15)), &errno(Errno::NOTSUP)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::NOTSUP.raw_os_error()));
    }

    /// A platform whose barrier counts its calls.
    fn counting(
        os: System,
        kernel: Option<(u32, u32)>,
        probe: io::Result<()>,
    ) -> (Platform, Arc<AtomicUsize>, Arc<Recording>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let recording = Arc::new(Recording::new());
        let seen = Arc::clone(&calls);
        let probe = std::sync::Mutex::new(Some(probe));
        let platform = Platform {
            os,
            kernel,
            barrier: Box::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                probe
                    .lock()
                    .expect("no test panics holding it")
                    .take()
                    .unwrap_or(Ok(()))
            }),
            durable: Box::new(Shared(Arc::clone(&recording))),
        };
        (platform, calls, recording)
    }

    /// A [`Durable`] that a test can keep a handle on.
    struct Shared(Arc<Recording>);

    impl Durable for Shared {
        fn file(&self, file: &File, path: &Path) -> Result<(), FilesystemError> {
            self.0.file(file, path)
        }
        fn dir(&self, dir: &File, path: &Path) -> Result<(), FilesystemError> {
            self.0.dir(dir, path)
        }
    }

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid test path")
    }

    #[test]
    fn begin_tries_the_barrier_once_and_takes_it_when_it_works() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, calls, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        assert_eq!(flush.method(), Method::Barrier);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn begin_falls_back_to_per_item_flushes_when_the_barrier_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, recording) = counting(System::MacOs, None, errno(Errno::NOTSUP));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        assert_eq!(flush.method(), Method::PerItem);
        let file = File::open(dir.path()).unwrap();
        flush.file(&file, &rel("a/b")).unwrap();
        flush.directory(&file, &rel("a")).unwrap();
        assert_eq!(
            recording.flushes(),
            vec![
                Flush::File(PathBuf::from("a/b")),
                Flush::Dir(PathBuf::from("a"))
            ]
        );
    }

    #[test]
    fn the_barrier_mode_flushes_nothing_per_item_through_the_durable() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, recording) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        let file = File::open(dir.path()).unwrap();
        flush.file(&file, &rel("a")).unwrap();
        flush.directory(&file, &rel("")).unwrap();
        assert!(recording.flushes().is_empty());
    }

    #[test]
    fn finish_runs_exactly_one_more_barrier_in_barrier_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, calls, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        flush.finish().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn finish_runs_no_barrier_in_per_item_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, calls, _) = counting(System::MacOs, None, errno(Errno::NOTSUP));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        flush.finish().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failing_barrier_is_returned_by_finish() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let platform = Platform {
            os: System::MacOs,
            kernel: None,
            barrier: Box::new(move |_| {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    errno(Errno::IO)
                }
            }),
            durable: Box::new(RealDurable),
        };
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        let error = flush.finish().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
    }

    #[test]
    fn a_per_item_error_keeps_its_errno() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, recording) = counting(System::MacOs, None, errno(Errno::NOTSUP));
        recording.fail_files_from(0);
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        let file = File::open(dir.path()).unwrap();
        let error = flush.file(&file, &rel("a")).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
    }

    #[test]
    fn the_probe_file_is_gone_after_finish_and_after_a_drop() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        flush.finish().unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        let (platform, _, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        drop(flush);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_real_platform_takes_the_barrier_on_a_local_volume() {
        let dir = tempfile::tempdir().unwrap();
        let flush = TreeFlush::begin(dir.path()).unwrap();
        assert_eq!(flush.method(), Method::Barrier);
        flush.finish().unwrap();
    }
}

#[cfg(test)]
mod choice_tests {
    use super::*;
    use crate::error::Error;
    use crate::testing::tree::{TempCollection, Tree};
    use crate::testing::versions::{Store, StoredVersion};
    use crate::versions::names::VersionFile;

    const OLD: &str = "2026-11-03T081500";
    const NEW: &str = "2026-11-04T090211";

    fn source() -> Tree {
        let mut tree = Tree::new();
        tree.file("projects/scanner/main.rs", "fn main() {}");
        tree.file("projects/scanner/src/lib.rs", "pub fn f() {}");
        tree.file("tools/scan-old/run.sh", "echo");
        tree
    }

    struct World {
        collection: TempCollection,
        store: Store,
    }

    impl World {
        fn new() -> World {
            World {
                collection: TempCollection::minimal(),
                store: Store::new(),
            }
        }

        fn add(&mut self, piece: &str, stamp: &str) -> StoredVersion {
            self.store.add_version(&source(), piece, stamp)
        }

        fn choose(&self, given: &str, version: Option<&str>) -> Result<Chosen, RestoreError> {
            self.choose_in(given, version, &TimeZone::UTC)
        }

        fn choose_in(
            &self,
            given: &str,
            version: Option<&str>,
            zone: &TimeZone,
        ) -> Result<Chosen, RestoreError> {
            let collection =
                Collection::find(self.collection.root()).expect("the collection is found");
            choose_version(
                &collection,
                &self.store.places(),
                self.collection.root(),
                OsStr::new(given),
                version,
                zone,
            )
        }
    }

    fn scanner() -> World {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world.add("projects/scanner", NEW);
        world
    }

    fn chosen_stamp(result: Result<Chosen, RestoreError>) -> String {
        result.expect("a version is chosen").stamp.to_string()
    }

    fn message(result: Result<Chosen, RestoreError>) -> String {
        result.expect_err("the choice is refused").to_string()
    }

    #[test]
    fn a_path_names_the_piece_and_the_newest_version_is_taken() {
        let chosen = scanner().choose("projects/scanner", None).expect("chosen");
        assert_eq!(chosen.piece.to_string(), "projects/scanner");
        assert_eq!(chosen.stamp.to_string(), NEW);
        assert_eq!(chosen.listing.entries.len(), 3);
    }

    #[test]
    fn a_search_word_names_the_piece() {
        assert_eq!(chosen_stamp(scanner().choose("scanner", None)), NEW);
    }

    #[test]
    fn an_ambiguous_word_keeps_its_candidates() {
        let mut world = scanner();
        world.add("tools/scan-old", NEW);
        let error = world.choose("scan", None).expect_err("two pieces match");
        assert_eq!(error.kind(), ErrorKind::AmbiguousReference);
        let details = Error::from(error).details();
        let candidates = details["candidates"].as_array().expect("candidates");
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn an_unknown_piece_has_its_own_kind() {
        let error = scanner()
            .choose("nothing-like-it", None)
            .expect_err("unknown");
        assert_eq!(error.kind(), ErrorKind::UnknownReference);
    }

    #[test]
    fn an_empty_piece_and_an_empty_version_are_usage_errors() {
        let world = scanner();
        let error = world.choose("", None).expect_err("empty piece");
        assert_eq!(error.kind(), ErrorKind::Usage);
        let error = world
            .choose("projects/scanner", Some(""))
            .expect_err("empty version");
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn a_version_is_named_by_its_stamp() {
        let world = scanner();
        assert_eq!(chosen_stamp(world.choose("scanner", Some(OLD))), OLD);
    }

    #[test]
    fn a_version_is_named_by_a_prefix_of_its_utc_stamp() {
        let world = scanner();
        assert_eq!(
            chosen_stamp(world.choose("scanner", Some("2026-11-03T08"))),
            OLD
        );
    }

    #[test]
    fn a_version_is_named_by_its_local_date_and_time() {
        let world = scanner();
        let berlin = TimeZone::get("Europe/Berlin").expect("the zone exists");
        // 09:02:11 UTC is 10:02:11 in Berlin in November.
        let by_date = world.choose_in("scanner", Some("2026-11-04"), &berlin);
        assert_eq!(chosen_stamp(by_date), NEW);
        let by_time = world.choose_in("scanner", Some("2026-11-04 10:02"), &berlin);
        assert_eq!(chosen_stamp(by_time), NEW);
    }

    #[test]
    fn several_matches_list_every_stamp() {
        let text = message(scanner().choose("scanner", Some("2026-11")));
        assert!(text.contains(OLD) && text.contains(NEW), "{text}");
    }

    #[test]
    fn no_match_lists_the_versions_there_are() {
        let error = scanner()
            .choose("scanner", Some("1999"))
            .expect_err("no match");
        assert!(
            matches!(error, RestoreError::NoSuchVersion { .. }),
            "{error:?}"
        );
        let text = error.to_string();
        assert!(text.contains(OLD) && text.contains(NEW), "{text}");
    }

    #[test]
    fn an_incomplete_version_named_by_stamp_is_refused_naming_forget() {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        let error = world.choose("scanner", Some(NEW)).expect_err("no archive");
        assert!(
            matches!(error, RestoreError::Incomplete { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("forget"), "{error}");
    }

    #[test]
    fn a_piece_with_only_incomplete_versions_is_unknown_and_the_error_names_forget() {
        let mut world = World::new();
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        for given in ["projects/scanner", "scanner"] {
            let error = world.choose(given, None).expect_err("no archive");
            assert!(
                matches!(error, RestoreError::OnlyIncomplete { .. }),
                "{given}: {error:?}"
            );
            assert_eq!(error.kind(), ErrorKind::UnknownReference);
            assert!(error.to_string().contains("forget"), "{error}");
        }
    }

    #[test]
    fn a_word_that_matches_nothing_even_among_incomplete_pieces_stays_unknown() {
        let mut world = World::new();
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        let error = world.choose("nothing-like-it", None).expect_err("unknown");
        assert!(
            matches!(
                error,
                RestoreError::Reference(ReferenceError::Unknown { .. })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn the_newest_version_with_an_archive_is_taken_over_a_newer_incomplete_one() {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        assert_eq!(chosen_stamp(world.choose("scanner", None)), OLD);
    }

    #[test]
    fn a_damaged_newest_version_is_refused_and_the_older_one_is_not_taken() {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Listing);
        let error = world.choose("scanner", None).expect_err("damaged");
        assert!(matches!(error, RestoreError::Damaged { .. }), "{error:?}");
    }

    fn assert_damaged(damage: impl FnOnce(&StoredVersion), expected: &str) {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        damage(&version);
        let error = world.choose("scanner", None).expect_err("damaged");
        let RestoreError::Damaged { reasons, .. } = &error else {
            panic!("expected damage, got {error:?}");
        };
        assert!(
            reasons.iter().any(|reason| reason.contains(expected)),
            "{reasons:?} lacks {expected:?}"
        );
        let text = error.to_string();
        for remedy in ["`fork`", "`rebuild`", "copied back"] {
            assert!(text.contains(remedy), "{text} lacks {remedy}");
        }
    }

    #[test]
    fn a_missing_listing_is_damage() {
        assert_damaged(|v| v.remove(VersionFile::Listing), "listing is missing");
    }

    #[test]
    fn a_missing_checksum_file_is_damage() {
        assert_damaged(
            |v| v.remove(VersionFile::Checksum),
            "checksum file is missing",
        );
    }

    #[test]
    fn a_truncated_archive_is_damage() {
        assert_damaged(
            |v| v.truncate(VersionFile::Archive, 10),
            "archive has 10 bytes",
        );
    }

    #[test]
    fn a_cut_listing_is_damage() {
        assert_damaged(|v| v.cut_listing(), "listing is damaged");
    }

    #[test]
    fn a_doubled_slot_names_every_file() {
        let mut twin = None;
        assert_damaged(
            |v| twin = Some(v.add_twin(VersionFile::Archive, "other")),
            "2 files for the archive",
        );
        assert!(twin.is_some());
    }

    #[test]
    fn a_listing_of_a_newer_format_has_its_own_kind() {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        version.edit_listing(|text| text.replacen("# antiq listing 1", "# antiq listing 2", 1));
        let error = world.choose("scanner", None).expect_err("newer format");
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
        assert!(
            matches!(error, RestoreError::FormatTooNew { .. }),
            "{error:?}"
        );
    }

    fn entry(path: &str, kind: ListingType) -> ListingEntry {
        ListingEntry {
            path: RelPath::parse(path).expect("a valid path"),
            kind,
            size: 0,
            mtime: 0,
            mode: 0o644,
            link: None,
            sha256: None,
        }
    }

    #[test]
    fn a_marker_name_at_the_top_must_be_what_the_marker_is() {
        let refused = |path: &str, kind| marker_not_a_file(&[entry(path, kind)]);
        assert_eq!(
            refused(".antiq-restored", ListingType::Dir).as_deref(),
            Some(".antiq-restored")
        );
        assert_eq!(
            refused(".antiq-restored", ListingType::Symlink).as_deref(),
            Some(".antiq-restored")
        );
        assert_eq!(
            refused(".antiq-pin", ListingType::Dir).as_deref(),
            Some(".antiq-pin")
        );
        assert_eq!(refused(".antiq-restored", ListingType::File), None);
        assert_eq!(refused(".antiq-pin", ListingType::File), None);
        assert_eq!(refused(".antiq-pin", ListingType::Symlink), None);
        assert_eq!(refused(".antiq-pin", ListingType::Hardlink), None);
        assert_eq!(refused("sub/.antiq-pin", ListingType::Dir), None);
        assert_eq!(refused("other", ListingType::Dir), None);
    }

    #[test]
    fn a_version_whose_top_holds_a_marker_name_as_a_directory_is_refused() {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        // A directory entry for the marker's name, with the counts of the
        // head left as they are: directories count for neither.
        version.edit_listing(|text| {
            let line = text
                .lines()
                .find(|line| line.ends_with("  src/"))
                .expect("the listing has a directory line")
                .to_string();
            text.replacen(&line, &line.replace("src/", ".antiq-pin/"), 1)
        });
        let error = world.choose("scanner", None).expect_err("a marker name");
        assert!(
            matches!(error, RestoreError::MarkerNotAFile { .. }),
            "{error:?}"
        );
    }
}
