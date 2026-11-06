// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The extraction that `unearth` and `fork` share.

#![allow(dead_code, reason = "the restore core that uses these follows")]

use crate::archive::extract::ExtractionFlush;
use crate::error::HasKind;
use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::{Durable, RealDurable};
use crate::relpath::RelPath;
use rustix::io::Errno;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// Why a restore failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {}

impl HasKind for RestoreError {}

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
pub(crate) fn choose(
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
        let method = choose(platform.os, platform.kernel, &tried)?;
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
            choose(System::MacOs, None, &Ok(())).unwrap(),
            Method::Barrier
        );
    }

    #[test]
    fn macos_flushes_per_item_when_the_full_flush_is_not_supported() {
        let refused = errno(Errno::NOTSUP);
        assert_eq!(
            choose(System::MacOs, None, &refused).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn linux_takes_the_barrier_from_kernel_5_8_on() {
        assert_eq!(
            choose(System::Linux, Some((5, 8)), &Ok(())).unwrap(),
            Method::Barrier
        );
        assert_eq!(
            choose(System::Linux, Some((6, 1)), &Ok(())).unwrap(),
            Method::Barrier
        );
    }

    #[test]
    fn linux_flushes_per_item_below_kernel_5_8_or_without_a_kernel() {
        assert_eq!(
            choose(System::Linux, Some((5, 7)), &Ok(())).unwrap(),
            Method::PerItem
        );
        assert_eq!(
            choose(System::Linux, Some((4, 19)), &Ok(())).unwrap(),
            Method::PerItem
        );
        assert_eq!(
            choose(System::Linux, None, &Ok(())).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn linux_flushes_per_item_when_syncfs_is_refused() {
        for refusal in [Errno::INVAL, Errno::NOSYS] {
            assert_eq!(
                choose(System::Linux, Some((5, 15)), &errno(refusal)).unwrap(),
                Method::PerItem,
                "{refusal:?}"
            );
        }
    }

    #[test]
    fn other_systems_flush_per_item() {
        assert_eq!(
            choose(System::Other, Some((9, 9)), &Ok(())).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn a_probe_error_that_is_no_refusal_is_returned() {
        let error = choose(System::MacOs, None, &errno(Errno::IO)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
        let error = choose(System::Linux, Some((5, 15)), &errno(Errno::NOTSUP)).unwrap_err();
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
