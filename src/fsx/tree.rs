// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Access to a directory tree through a directory handle.
//!
//! A path syscall fails once the whole path is longer than the platform's
//! limit (1,024 bytes on macOS), although every name in it is short. A
//! [`TreeRoot`] works around that: it holds the root directory open and
//! reaches an entry by opening its parent directories one component at a
//! time, each relative to the previous one, so no call ever sees more than
//! one name. Every component is opened with `O_NOFOLLOW`, so a symlink
//! inside the tree is never traversed.
//!
//! Each call opens all the directories above its entry again, so its cost
//! grows with the depth of the path. A walker that goes down a tree
//! descends with [`TreeRoot::sub`], which holds the directory it has
//! reached open, and passes only the names below it.
//!
//! Two path conventions hold for every method. The paths a method takes
//! are relative to the root it is called on. The paths a method returns
//! ([`TreeEntry::path`], [`DirEntry::path`]) are relative to the root the
//! chain of `sub` calls started from, so that the same entry has the same
//! path wherever it was reached from. [`TreeRoot::prefix`] is the path of
//! a root below that original root.

use crate::relpath::{Name, RelPath};
use crate::tree::{EntryKind, TreeEntry};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{
    AtFlags, Dir, Mode, OFlags, RawMode, RenameFlags, Timespec, Timestamps, UTIME_OMIT, chmodat,
    fcntl_getfl, fcntl_setfl, fstat, linkat, mkdirat, open, openat, readlinkat, renameat,
    renameat_with, statat, symlinkat, unlinkat, utimensat,
};
use rustix::io::Errno;
use std::fs::File;
use std::io;
use std::path::Path;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// How every directory on the way to an entry is opened. Walking through a
/// directory needs only permission to search it, so it is opened for
/// searching and not for reading: macOS has `O_SEARCH` for that, Linux
/// `O_PATH`. Where neither exists the directory is opened for reading.
#[cfg(target_os = "macos")]
const SEARCH: OFlags = OFlags::from_bits_retain(libc::O_SEARCH as _)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(target_os = "linux")]
const SEARCH: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const SEARCH: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// How the root is opened: like a directory on the way, except that
/// symlinks in the path to the root itself are followed.
const ROOT: OFlags = SEARCH.difference(OFlags::NOFOLLOW);

/// How a directory is opened to list it, which does need permission to
/// read it.
const LISTING: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A directory held open, through which entries below it are reached.
#[derive(Debug)]
pub struct TreeRoot {
    fd: OwnedFd,
    /// Where this root lies below the root the chain of `sub` calls
    /// started from; the root path for a root made by `open`.
    prefix: RelPath,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Name,
    /// The path of the entry, relative to the original root (see the
    /// module documentation).
    pub path: RelPath,
    /// The kind the directory reports; `Unknown` when it reports none.
    pub kind: EntryKind,
}

/// A directory handle that is either the root, borrowed, or one opened on
/// the way down.
enum Handle<'a> {
    Root(BorrowedFd<'a>),
    Opened(OwnedFd),
}

impl AsFd for Handle<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Handle::Root(fd) => *fd,
            Handle::Opened(fd) => fd.as_fd(),
        }
    }
}

impl TreeRoot {
    /// Opens `path` as a root. Symlinks in `path` itself are followed: the
    /// root is a directory antiq chose (the collection, a staging
    /// directory), not something found inside a piece.
    pub fn open(path: &Path) -> io::Result<TreeRoot> {
        let fd = open(path, ROOT, Mode::empty())?;
        Ok(TreeRoot {
            fd,
            prefix: RelPath::root(),
        })
    }

    /// The directory at `path`, relative to this root, as a root of its
    /// own. It holds that directory open, so what it reaches costs nothing
    /// for the directories above, and it keeps working when they can no
    /// longer be entered.
    pub fn sub(&self, path: &RelPath) -> io::Result<TreeRoot> {
        Ok(TreeRoot {
            fd: self.open_dir(path, SEARCH)?,
            prefix: self.prefix.concat(path),
        })
    }

    /// Where this root lies below the root the chain of `sub` calls
    /// started from; the root path for a root made by `open`.
    pub fn prefix(&self) -> &RelPath {
        &self.prefix
    }

    /// `lstat` of the entry at `path`, relative to this root, with the
    /// target of a symlink. The root path stands for the root directory
    /// itself. The returned `path` is relative to the original root.
    pub fn lstat(&self, path: &RelPath) -> io::Result<TreeEntry> {
        let (dir, name) = self.split_or_root(path)?;
        let (raw, link_target) = read_entry(
            || read_stat(dir.as_fd(), name),
            || Ok(readlinkat(dir.as_fd(), name, Vec::new())?.into_bytes()),
        )?;
        let kind = EntryKind::from_mode(raw.mode);
        Ok(TreeEntry {
            path: self.prefix.concat(path),
            kind,
            size: raw.size,
            allocated: raw.allocated,
            mtime_ns: raw.mtime_ns,
            ctime_ns: raw.ctime_ns,
            inode: raw.inode,
            device: raw.device,
            mode: raw.mode,
            nlink: raw.nlink,
            link_target,
            birth_ns: raw.birth_ns,
            mount_root: raw.mount_root,
        })
    }

    /// The target of the symlink at `path`, relative to this root.
    pub fn read_link(&self, path: &RelPath) -> io::Result<Vec<u8>> {
        let (dir, name) = self.split(path)?;
        Ok(readlinkat(dir.as_fd(), name, Vec::new())?.into_bytes())
    }

    /// The entries of the directory at `path`, relative to this root, in
    /// the order the filesystem gives them. Each entry's `path` is
    /// relative to the original root.
    pub fn read_dir(&self, path: &RelPath) -> io::Result<Vec<DirEntry>> {
        let directory = self.prefix.concat(path);
        let mut entries = Vec::new();
        for entry in Dir::new(self.open_dir(path, LISTING)?)? {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name = Name::from_bytes(bytes.to_vec())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let kind = EntryKind::from_mode(mode_of(entry.file_type().as_raw_mode()));
            let path = directory.join(&name);
            entries.push(DirEntry { name, path, kind });
        }
        Ok(entries)
    }

    /// Opens the regular file at `path`, relative to this root, for
    /// reading. Anything else is refused: a symlink by the filesystem, any
    /// other kind of entry with [`TreeError::NotRegular`]. The open itself
    /// never waits, so a fifo that replaced the file cannot stall it, and
    /// the file it returns blocks like any other.
    pub fn open_read(&self, path: &RelPath) -> io::Result<File> {
        let (dir, name) = self.split(path)?;
        // `NOCTTY` keeps a terminal device from becoming the controlling
        // terminal of the process.
        let flags =
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY;
        let file = File::from(openat(dir.as_fd(), name, flags, Mode::empty())?);
        let kind = EntryKind::from_mode(mode_of(fstat(&file)?.st_mode));
        if kind != EntryKind::File {
            return Err(TreeError::NotRegular(kind).into());
        }
        fcntl_setfl(&file, fcntl_getfl(&file)? - OFlags::NONBLOCK)?;
        Ok(file)
    }

    /// The directory at `path`, relative to this root, as an open file, so
    /// that the entries it holds can be made durable.
    pub(crate) fn open_dir_file(&self, path: &RelPath) -> io::Result<File> {
        Ok(File::from(self.open_dir(path, SEARCH)?))
    }

    /// Creates the directory `path`, relative to this root.
    pub fn create_dir(&self, path: &RelPath, mode: u32) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(mkdirat(dir.as_fd(), name, mode_bits(mode))?)
    }

    /// Creates the file `path`, relative to this root, for writing; fails
    /// if the name exists.
    pub fn create_file(&self, path: &RelPath, mode: u32) -> io::Result<File> {
        let (dir, name) = self.split(path)?;
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(File::from(openat(
            dir.as_fd(),
            name,
            flags,
            mode_bits(mode),
        )?))
    }

    /// Creates the symlink `path`, relative to this root, holding `target`
    /// as given.
    pub fn create_symlink(&self, path: &RelPath, target: &[u8]) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(symlinkat(target, dir.as_fd(), name)?)
    }

    /// Gives the file at `existing` a second name, `new`; both are
    /// relative to this root.
    pub fn hard_link(&self, existing: &RelPath, new: &RelPath) -> io::Result<()> {
        let (from, from_name) = self.split(existing)?;
        let (to, to_name) = self.split(new)?;
        let no_flags = AtFlags::empty();
        linkat(from.as_fd(), from_name, to.as_fd(), to_name, no_flags)?;
        Ok(())
    }

    /// Sets the permission bits of the file or directory at `path`,
    /// relative to this root. A symlink is never followed. On Linux a
    /// symlink is refused as `Unsupported`, since the kernel cannot change
    /// the mode of a symlink itself; on macOS the symlink's own mode
    /// changes.
    pub fn set_mode(&self, path: &RelPath, mode: u32) -> io::Result<()> {
        let (dir, name) = self.split_or_root(path)?;
        change_mode(dir.as_fd(), name, mode_bits(mode))
    }

    /// Sets the modification time, in nanoseconds since the epoch, of the
    /// entry at `path`, relative to this root: the time of a symlink is
    /// its own, not its target's. The access time stays.
    pub fn set_times(&self, path: &RelPath, mtime_ns: i128) -> io::Result<()> {
        let (dir, name) = self.split_or_root(path)?;
        let seconds = i64::try_from(mtime_ns.div_euclid(NANOS_PER_SECOND))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let nanoseconds = mtime_ns.rem_euclid(NANOS_PER_SECOND) as i64;
        let times = Timestamps {
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            last_modification: Timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds as _,
            },
        };
        let own_time = AtFlags::SYMLINK_NOFOLLOW;
        utimensat(dir.as_fd(), name, &times, own_time)?;
        Ok(())
    }

    /// Moves the entry at `from` to `to` in `to_root`, each relative to its
    /// own root. `to_root` may be this root. An existing target is never
    /// replaced: the rename fails with `AlreadyExists`.
    ///
    /// The refusal is atomic with the rename where the filesystem supports
    /// a no-replace rename. On one that does not (smbfs, for example) the
    /// target is looked at first and renamed to afterwards, and in the
    /// window between the two another process could create it. antiq
    /// moves entries under the run lock.
    pub fn rename(&self, from: &RelPath, to_root: &TreeRoot, to: &RelPath) -> io::Result<()> {
        let (source, source_name) = self.split(from)?;
        let (target, target_name) = to_root.split(to)?;
        rename_no_replace(
            source.as_fd(),
            source_name,
            target.as_fd(),
            target_name,
            renameat_no_replace,
        )
    }

    /// Removes the file, symlink or other non-directory entry at `path`,
    /// relative to this root.
    pub fn remove_file(&self, path: &RelPath) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(unlinkat(dir.as_fd(), name, AtFlags::empty())?)
    }

    /// Removes the empty directory at `path`, relative to this root.
    pub fn remove_dir(&self, path: &RelPath) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(unlinkat(dir.as_fd(), name, AtFlags::REMOVEDIR)?)
    }

    fn open_dir(&self, path: &RelPath, flags: OFlags) -> io::Result<OwnedFd> {
        let (dir, name) = self.split_or_root(path)?;
        Ok(openat(dir.as_fd(), name, flags, Mode::empty())?)
    }

    /// Opens the directories above the last component of `path` and
    /// returns the innermost with that last name. The root path has no last
    /// component.
    fn split<'a>(&'a self, path: &'a RelPath) -> io::Result<(Handle<'a>, &'a [u8])> {
        let mut names: Vec<&[u8]> = path.components().collect();
        let Some(last) = names.pop() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root has no parent directory",
            ));
        };
        let mut handle = Handle::Root(self.fd.as_fd());
        for name in names {
            let next = openat(handle.as_fd(), name, SEARCH, Mode::empty())?;
            handle = Handle::Opened(next);
        }
        Ok((handle, last))
    }

    /// Like `split`, but the root path is the root directory itself,
    /// named `.` in its own directory.
    fn split_or_root<'a>(&'a self, path: &'a RelPath) -> io::Result<(Handle<'a>, &'a [u8])> {
        if path.is_root() {
            return Ok((Handle::Root(self.fd.as_fd()), b"."));
        }
        self.split(path)
    }
}

fn mode_bits(mode: u32) -> Mode {
    Mode::from_bits_truncate(mode as RawMode)
}

/// `fchmodat` that cannot follow a symlink, also when the entry is
/// replaced by one between a check and the call: macOS refuses to follow
/// one itself.
#[cfg(not(target_os = "linux"))]
fn change_mode(dir: BorrowedFd<'_>, name: &[u8], mode: Mode) -> io::Result<()> {
    chmodat(dir, name, mode, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(())
}

/// The syscall number of `fchmodat2`, which takes the flags that
/// `fchmodat` ignores. Linux has it since 6.6; the libc wrappers and
/// rustix 1.1.6 have no function for it.
#[cfg(target_os = "linux")]
const SYS_FCHMODAT2: libc::c_long = 452;

/// `fchmodat2` without following a symlink, replaceable by a test.
#[cfg(target_os = "linux")]
type Fchmodat2 = fn(BorrowedFd<'_>, &[u8], Mode) -> Result<(), Errno>;

#[cfg(target_os = "linux")]
fn fchmodat2_no_follow(dir: BorrowedFd<'_>, name: &[u8], mode: Mode) -> Result<(), Errno> {
    use std::os::fd::AsRawFd;

    let name = std::ffi::CString::new(name).map_err(|_| Errno::INVAL)?;
    // SAFETY: `fchmodat2` reads a directory descriptor, a NUL-terminated
    // string that outlives the call, a mode and a flag. Every argument is
    // passed as the `long` that `syscall` reads it as.
    let result = unsafe {
        libc::syscall(
            SYS_FCHMODAT2,
            dir.as_raw_fd() as libc::c_long,
            name.as_ptr() as libc::c_long,
            mode.bits() as libc::c_long,
            libc::AT_SYMLINK_NOFOLLOW as libc::c_long,
        )
    };
    match result {
        0 => Ok(()),
        _ => Err(Errno::from_io_error(&io::Error::last_os_error()).unwrap_or(Errno::INVAL)),
    }
}

#[cfg(target_os = "linux")]
fn change_mode(dir: BorrowedFd<'_>, name: &[u8], mode: Mode) -> io::Result<()> {
    change_mode_with(dir, name, mode, fchmodat2_no_follow, "/proc/self/fd")
}

/// Linux changes a mode without following a symlink in one of two ways.
/// `fchmodat2` with `AT_SYMLINK_NOFOLLOW` does it in one call and refuses a
/// symlink with `EOPNOTSUPP`. Where that call does not exist (`ENOSYS` on a
/// kernel before 6.6, `EINVAL`, or `EPERM` from a seccomp profile that does
/// not know it) the entry is opened without following a symlink, `fstat`
/// tells a symlink apart on the open handle, and the mode is set through
/// the handle's link in `proc_dir` (`/proc/self/fd`), which names the very
/// inode that was checked. That needs `/proc`.
#[cfg(target_os = "linux")]
fn change_mode_with(
    dir: BorrowedFd<'_>,
    name: &[u8],
    mode: Mode,
    fchmodat2: Fchmodat2,
    proc_dir: &str,
) -> io::Result<()> {
    use rustix::fs::CWD;
    use std::os::fd::AsRawFd;

    let symlink_refused = || {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "the mode of a symlink cannot be changed",
        )
    };
    match fchmodat2(dir, name, mode) {
        Ok(()) => return Ok(()),
        Err(Errno::OPNOTSUPP) => return Err(symlink_refused()),
        Err(Errno::NOSYS | Errno::INVAL | Errno::PERM) => {}
        Err(other) => return Err(other.into()),
    }
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let entry = openat(dir, name, flags, Mode::empty())?;
    if EntryKind::from_mode(mode_of(fstat(&entry)?.st_mode)) == EntryKind::Symlink {
        return Err(symlink_refused());
    }
    let through_proc = format!("{proc_dir}/{}", entry.as_raw_fd());
    match chmodat(CWD, through_proc, mode, AtFlags::empty()) {
        Err(Errno::NOENT) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "changing a mode without following symlinks needs /proc or Linux 6.6",
        )),
        done => Ok(done?),
    }
}

#[allow(
    clippy::useless_conversion,
    reason = "`st_mode` is 16 bits wide on macOS and 32 on Linux"
)]
fn mode_of(mode: RawMode) -> u32 {
    u32::from(mode)
}

/// What a [`TreeRoot`] reports about an entry that its caller can tell
/// apart from an error of the filesystem. It travels inside an
/// `io::Error`; [`TreeError::of`] finds it there.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// The entry changed while it was being read, twice in a row.
    #[error("the entry changed while it was read")]
    Changed,
    /// An entry that is not a regular file was opened for reading.
    #[error("only a regular file can be read, not a {0:?}")]
    NotRegular(EntryKind),
}

impl TreeError {
    /// The tree error inside `error`, if it holds one.
    pub fn of(error: &io::Error) -> Option<&TreeError> {
        error.get_ref()?.downcast_ref()
    }
}

impl From<TreeError> for io::Error {
    fn from(error: TreeError) -> io::Error {
        io::Error::other(error)
    }
}

/// A rename that fails instead of replacing an existing target.
type RenameAttempt = fn(BorrowedFd<'_>, &[u8], BorrowedFd<'_>, &[u8]) -> io::Result<()>;

fn renameat_no_replace(
    from: BorrowedFd<'_>,
    from_name: &[u8],
    to: BorrowedFd<'_>,
    to_name: &[u8],
) -> io::Result<()> {
    let flags = RenameFlags::NOREPLACE;
    Ok(renameat_with(from, from_name, to, to_name, flags)?)
}

/// Renames with `attempt`; on a filesystem that cannot refuse to replace
/// (`ENOTSUP`, `EINVAL` for the flag), looks at the target and renames
/// plainly if it is not there.
fn rename_no_replace(
    from: BorrowedFd<'_>,
    from_name: &[u8],
    to: BorrowedFd<'_>,
    to_name: &[u8],
    attempt: RenameAttempt,
) -> io::Result<()> {
    let error = match attempt(from, from_name, to, to_name) {
        Err(error) => error,
        done => return done,
    };
    let flag_unsupported = [Errno::NOTSUP, Errno::INVAL]
        .iter()
        .any(|errno| error.raw_os_error() == Some(errno.raw_os_error()));
    if !flag_unsupported {
        return Err(error);
    }
    match statat(to, to_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
        Err(Errno::NOENT) => Ok(renameat(from, from_name, to, to_name)?),
        Err(other) => Err(other.into()),
    }
}

/// Stats an entry and, for a symlink, reads its target. The two are
/// separate lookups, so a symlink replaced in between would mix the fields
/// of one with the target of the other. After reading the target the entry
/// is stat'd again, and it counts as unchanged when its type, inode and
/// change time are the same (the size is left out: it need not equal the
/// target's length on every filesystem). A change, or an entry that is no
/// longer a symlink when its target is read (`EINVAL`), gets one more
/// attempt; after a second one the entry is reported as
/// [`TreeError::Changed`].
fn read_entry(
    mut stat: impl FnMut() -> io::Result<RawStat>,
    mut read_link: impl FnMut() -> io::Result<Vec<u8>>,
) -> io::Result<(RawStat, Option<Vec<u8>>)> {
    for _ in 0..2 {
        let first = stat()?;
        if EntryKind::from_mode(first.mode) != EntryKind::Symlink {
            return Ok((first, None));
        }
        let target = match read_link() {
            Ok(target) => target,
            Err(error) if error.raw_os_error() == Some(Errno::INVAL.raw_os_error()) => continue,
            Err(error) => return Err(error),
        };
        let second = stat()?;
        let unchanged = first.mode == second.mode
            && first.inode == second.inode
            && first.ctime_ns == second.ctime_ns;
        if unchanged {
            return Ok((first, Some(target)));
        }
    }
    Err(TreeError::Changed.into())
}

/// The fields of an `lstat`, in the units a [`TreeEntry`] uses.
struct RawStat {
    mode: u32,
    size: u64,
    allocated: u64,
    mtime_ns: i128,
    ctime_ns: i128,
    inode: u64,
    device: u64,
    nlink: u64,
    birth_ns: Option<i128>,
    mount_root: bool,
}

fn nanoseconds(seconds: i64, nanos: u32) -> i128 {
    i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanos)
}

#[cfg(target_os = "linux")]
fn read_stat(dir: BorrowedFd<'_>, name: &[u8]) -> io::Result<RawStat> {
    use rustix::fs::{StatxFlags, makedev, statx};

    let stat = statx(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS | StatxFlags::BTIME,
    )?;
    let has_birth_time = stat.stx_mask & StatxFlags::BTIME.bits() != 0;
    Ok(RawStat {
        mode: u32::from(stat.stx_mode),
        size: stat.stx_size,
        allocated: stat.stx_blocks * 512,
        mtime_ns: nanoseconds(stat.stx_mtime.tv_sec, stat.stx_mtime.tv_nsec),
        ctime_ns: nanoseconds(stat.stx_ctime.tv_sec, stat.stx_ctime.tv_nsec),
        inode: stat.stx_ino,
        device: makedev(stat.stx_dev_major, stat.stx_dev_minor),
        nlink: u64::from(stat.stx_nlink),
        birth_ns: has_birth_time
            .then(|| nanoseconds(stat.stx_btime.tv_sec, stat.stx_btime.tv_nsec)),
        mount_root: mount_root_from_attributes(stat.stx_attributes_mask, stat.stx_attributes),
    })
}

#[cfg(not(target_os = "linux"))]
fn read_stat(dir: BorrowedFd<'_>, name: &[u8]) -> io::Result<RawStat> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    #[cfg(target_os = "macos")]
    let mount_root = EntryKind::from_mode(u32::from(stat.st_mode)) == EntryKind::Directory
        && mount_root_of(dir, name)?;
    #[cfg(not(target_os = "macos"))]
    let mount_root = false;
    let nanos = |nsec: i64| u32::try_from(nsec).unwrap_or(0);
    Ok(RawStat {
        mode: u32::from(stat.st_mode),
        size: stat.st_size as u64,
        allocated: stat.st_blocks as u64 * 512,
        mtime_ns: nanoseconds(stat.st_mtime, nanos(stat.st_mtime_nsec)),
        ctime_ns: nanoseconds(stat.st_ctime, nanos(stat.st_ctime_nsec)),
        inode: stat.st_ino,
        device: u64::from(stat.st_dev as u32),
        nlink: u64::from(stat.st_nlink),
        birth_ns: birth_time(&stat),
        mount_root,
    })
}

/// Whether the attributes `statx` reported say the entry is the root of a
/// mount. `mask` lists the attributes the kernel knows: before Linux 5.8
/// it leaves this one out, and an attribute it did not report says nothing.
#[cfg(target_os = "linux")]
fn mount_root_from_attributes(
    mask: rustix::fs::StatxAttributes,
    attributes: rustix::fs::StatxAttributes,
) -> bool {
    let mount_root = rustix::fs::StatxAttributes::MOUNT_ROOT;
    mask.contains(mount_root) && attributes.contains(mount_root)
}

/// Whether the mount point names of an entry and of the directory holding
/// it differ, which is how macOS tells a mount root on the device of its
/// parent.
#[cfg(target_os = "macos")]
fn mount_names_differ(own: &[u8], parent: &[u8]) -> bool {
    own != parent
}

/// What the failure of opening a directory to compare its mount point name
/// means. A directory the process may not search cannot be entered either,
/// so it counts as no mount root; one that was replaced since its `lstat`
/// has changed; anything else, a shortage of descriptors included, is
/// reported.
#[cfg(target_os = "macos")]
fn mount_root_after_failed_open(errno: Errno) -> io::Result<bool> {
    match errno {
        Errno::ACCESS => Ok(false),
        Errno::NOTDIR | Errno::LOOP => Err(TreeError::Changed.into()),
        other => Err(other.into()),
    }
}

/// The name of the mount point of the filesystem `fd` lives on.
#[cfg(target_os = "macos")]
fn mount_point_name(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let stat = rustix::fs::fstatfs(fd)?;
    let name = stat.f_mntonname.iter();
    Ok(name
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect())
}

/// Whether the directory `name` of `dir` is the root of a mount that
/// shares the device of its parent. The directory is opened for searching
/// only, which changes nothing about it. The root of a `TreeRoot` is named
/// `.`, and its parent is then `..`. The two `fstatfs` calls and the
/// `lstat` before them are not one atomic step.
#[cfg(target_os = "macos")]
fn mount_root_of(dir: BorrowedFd<'_>, name: &[u8]) -> io::Result<bool> {
    let beside = if name == b"." { b".." } else { name };
    let opened = match openat(dir, beside, SEARCH, Mode::empty()) {
        Ok(fd) => fd,
        Err(errno) => return mount_root_after_failed_open(errno),
    };
    let (own, parent) = if name == b"." {
        (mount_point_name(dir)?, mount_point_name(opened.as_fd())?)
    } else {
        (mount_point_name(opened.as_fd())?, mount_point_name(dir)?)
    };
    Ok(mount_names_differ(&own, &parent))
}

#[cfg(target_os = "macos")]
fn birth_time(stat: &rustix::fs::Stat) -> Option<i128> {
    Some(nanoseconds(
        stat.st_birthtime,
        u32::try_from(stat.st_birthtime_nsec).unwrap_or(0),
    ))
}

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
fn birth_time(_stat: &rustix::fs::Stat) -> Option<i128> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::deep_path;
    use crate::testing::tree::Tree;
    use rustix::io::Errno;
    use std::fs::Permissions;
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    #[test]
    fn an_ordinary_entry_is_no_mount_root() {
        let mut tree = Tree::new();
        tree.file("d/f", "x");
        tree.symlink("d/l", "f");
        tree.fifo("d/q");
        let root = root_of(&tree);
        for entry in ["d", "d/f", "d/l", "d/q"] {
            let found = root.lstat(&path(entry)).expect("stat");
            assert!(!found.mount_root, "{entry}");
        }
        assert!(!root.lstat(&RelPath::root()).expect("stat").mount_root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_data_volume_is_a_mount_root_below_a_parent_on_its_device() {
        let parent = std::fs::metadata("/System/Volumes").expect("the parent exists");
        let data = std::fs::metadata("/System/Volumes/Data").expect("exists");
        assert_eq!(
            std::os::unix::fs::MetadataExt::dev(&data),
            std::os::unix::fs::MetadataExt::dev(&parent),
            "the test needs the data volume to share its parent's device"
        );
        let volumes = TreeRoot::open(Path::new("/System/Volumes")).expect("opens");
        assert!(volumes.lstat(&path("Data")).expect("stat").mount_root);
        let data = TreeRoot::open(Path::new("/System/Volumes/Data")).expect("opens");
        assert!(data.lstat(&RelPath::root()).expect("stat").mount_root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_directory_that_cannot_be_searched_is_no_mount_root_and_still_has_a_stat() {
        let mut tree = Tree::new();
        tree.file("locked/f", "x");
        let locked = tree.path("locked");
        let _unlock = Unlock(vec![locked.clone()]);
        std::fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("chmod");
        let refused = std::fs::read_dir(&locked).expect_err("the test needs a non-root user");
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let found = root_of(&tree).lstat(&path("locked")).expect("stat");
        assert_eq!(found.kind, EntryKind::Directory);
        assert!(!found.mount_root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mount_point_names_differ_when_they_are_not_the_same_bytes() {
        assert!(mount_names_differ(b"/System/Volumes/Data", b"/"));
        assert!(!mount_names_differ(b"/", b"/"));
        assert!(!mount_names_differ(b"", b""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_failed_open_answers_by_its_errno() {
        assert!(!mount_root_after_failed_open(Errno::ACCESS).expect("no mount root"));
        for errno in [Errno::NOTDIR, Errno::LOOP] {
            let error = mount_root_after_failed_open(errno).expect_err("changed");
            assert_eq!(TreeError::of(&error), Some(&TreeError::Changed), "{errno}");
        }
        for errno in [Errno::NOENT, Errno::MFILE, Errno::NFILE, Errno::IO] {
            let error = mount_root_after_failed_open(errno).expect_err("fails");
            assert_eq!(error.raw_os_error(), Some(errno.raw_os_error()), "{errno}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_mount_root_attribute_counts_only_when_the_kernel_reports_it() {
        use rustix::fs::StatxAttributes as Attributes;
        let root = Attributes::MOUNT_ROOT;
        let none = Attributes::empty();
        assert!(mount_root_from_attributes(root, root), "reported and set");
        assert!(!mount_root_from_attributes(root, none), "reported, not set");
        assert!(!mount_root_from_attributes(none, root), "an old kernel");
        assert!(!mount_root_from_attributes(none, none));
    }

    /// Restores the permissions of directories a test locked.
    struct Unlock(Vec<std::path::PathBuf>);

    impl Drop for Unlock {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::set_permissions(path, Permissions::from_mode(0o755));
            }
        }
    }

    fn name(text: &str) -> Name {
        Name::from_bytes(text.as_bytes().to_vec()).expect("valid name")
    }

    fn root_of(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("the root opens")
    }

    fn raw_error(error: &io::Error) -> Option<i32> {
        error.raw_os_error()
    }

    /// Every directory of `deep_path(levels)`, created one by one.
    fn create_chain(root: &TreeRoot, levels: usize) -> RelPath {
        let deep = deep_path(levels);
        let mut so_far = RelPath::root();
        for component in deep.components() {
            let component = Name::from_bytes(component.to_vec()).expect("valid name");
            so_far = so_far.join(&component);
            root.create_dir(&so_far, 0o755)
                .expect("the directory is created");
        }
        deep
    }

    #[test]
    fn a_file_far_past_the_path_limit_can_be_created_read_listed_and_removed() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let deep = create_chain(&root, 25);

        // The setup is deep enough only if a path syscall on the whole
        // path fails.
        let whole = tree
            .root()
            .join(std::ffi::OsStr::from_bytes(deep.as_bytes()));
        let refused = std::fs::symlink_metadata(&whole).expect_err("the path is too long");
        assert_eq!(raw_error(&refused), Some(Errno::NAMETOOLONG.raw_os_error()));

        let file = deep.join(&name("note"));
        root.create_file(&file, 0o640)
            .expect("the file is created")
            .write_all(b"hello")
            .expect("the file is written");

        let entry = root.lstat(&file).expect("the file is stat'd");
        assert_eq!(entry.kind, EntryKind::File);
        assert_eq!(entry.size, 5);
        assert_eq!(entry.mode & 0o777, 0o640);
        assert_eq!(entry.nlink, 1);
        assert_eq!(entry.path, file);

        let mut content = String::new();
        root.open_read(&file)
            .expect("the file opens")
            .read_to_string(&mut content)
            .expect("the file is read");
        assert_eq!(content, "hello");

        let listed = root.read_dir(&deep).expect("the directory is listed");
        let names: Vec<&[u8]> = listed.iter().map(|e| e.name.as_bytes()).collect();
        assert_eq!(names, [&b"note"[..]]);
        assert_eq!(listed[0].kind, EntryKind::File);

        root.remove_file(&file).expect("the file is removed");
        let mut up = deep;
        while !up.is_root() {
            root.remove_dir(&up).expect("the directory is removed");
            let bytes = up.as_bytes();
            let cut = bytes.iter().rposition(|b| *b == b'/').unwrap_or(0);
            up = RelPath::from_bytes(bytes[..cut].to_vec()).expect("the parent is valid");
        }
        assert!(root.read_dir(&RelPath::root()).expect("listed").is_empty());
    }

    #[test]
    fn links_can_be_created_and_read_at_depth() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let deep = create_chain(&root, 25);

        let file = deep.join(&name("file"));
        root.create_file(&file, 0o644).expect("the file is created");
        let symlink = deep.join(&name("symlink"));
        root.create_symlink(&symlink, b"file\xff")
            .expect("the symlink is created");
        let hard = deep.join(&name("hard"));
        root.hard_link(&file, &hard)
            .expect("the hard link is created");

        let entry = root.lstat(&symlink).expect("the symlink is stat'd");
        assert_eq!(entry.kind, EntryKind::Symlink);
        assert_eq!(entry.link_target.as_deref(), Some(&b"file\xff"[..]));
        assert_eq!(
            root.read_link(&symlink).expect("the link is read"),
            b"file\xff"
        );

        let (file_entry, hard_entry) = (
            root.lstat(&file).expect("stat"),
            root.lstat(&hard).expect("stat"),
        );
        assert_eq!(file_entry.inode, hard_entry.inode);
        assert_eq!(file_entry.nlink, 2);
    }

    #[test]
    fn a_directory_without_permissions_can_be_made_accessible_and_removed() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let deep = create_chain(&root, 25);
        let locked = deep.join(&name("locked"));
        root.create_dir(&locked, 0o755)
            .expect("the directory is created");

        root.set_mode(&locked, 0o000).expect("the mode is set");
        assert_eq!(root.lstat(&locked).expect("stat").mode & 0o777, 0);
        assert!(
            root.read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        root.set_mode(&locked, 0o755).expect("the mode is restored");
        root.remove_dir(&locked).expect("the directory is removed");
    }

    #[cfg(target_os = "linux")]
    mod linux_modes {
        use super::*;

        fn not_implemented(_: BorrowedFd<'_>, _: &[u8], _: Mode) -> Result<(), Errno> {
            Err(Errno::NOSYS)
        }

        fn invalid(_: BorrowedFd<'_>, _: &[u8], _: Mode) -> Result<(), Errno> {
            Err(Errno::INVAL)
        }

        fn filtered(_: BorrowedFd<'_>, _: &[u8], _: Mode) -> Result<(), Errno> {
            Err(Errno::PERM)
        }

        fn not_supported(_: BorrowedFd<'_>, _: &[u8], _: Mode) -> Result<(), Errno> {
            Err(Errno::OPNOTSUPP)
        }

        fn denied(_: BorrowedFd<'_>, _: &[u8], _: Mode) -> Result<(), Errno> {
            Err(Errno::ACCESS)
        }

        /// Sets 0600 on `name` in `tree` with `fchmodat2` replaced by
        /// `attempt` and `/proc/self/fd` by `proc_dir`.
        fn chmod_600(
            tree: &Tree,
            name: &str,
            attempt: Fchmodat2,
            proc_dir: &str,
        ) -> io::Result<()> {
            let root = root_of(tree);
            let relative = path(name);
            let (dir, name) = root.split(&relative).expect("the name splits");
            let mode = Mode::from_bits_truncate(0o600);
            change_mode_with(dir.as_fd(), name, mode, attempt, proc_dir)
        }

        fn mode_of_file(tree: &Tree, name: &str) -> u32 {
            std::fs::metadata(tree.path(name)).expect("exists").mode() & 0o777
        }

        #[test]
        fn without_proc_the_failure_says_what_is_missing_and_not_that_the_entry_is_gone() {
            let mut tree = Tree::new();
            tree.file_with("file", "x", 0o644, 1_000_000_000);
            let error = chmod_600(&tree, "file", not_implemented, "/no/such/proc")
                .expect_err("there is no way to change the mode");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("/proc"), "{error}");
            assert_eq!(mode_of_file(&tree, "file"), 0o644);
        }

        #[test]
        fn a_kernel_without_fchmodat2_falls_back_to_proc() {
            for attempt in [not_implemented, invalid, filtered] {
                let mut tree = Tree::new();
                tree.file_with("file", "x", 0o644, 1_000_000_000);
                chmod_600(&tree, "file", attempt, "/proc/self/fd").expect("the mode is set");
                assert_eq!(mode_of_file(&tree, "file"), 0o600);
            }
        }

        #[test]
        fn another_error_of_fchmodat2_is_passed_on_without_a_fallback() {
            let mut tree = Tree::new();
            tree.file_with("file", "x", 0o644, 1_000_000_000);
            let error = chmod_600(&tree, "file", denied, "/proc/self/fd").expect_err("denied");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(mode_of_file(&tree, "file"), 0o644);
        }

        #[test]
        fn a_symlink_is_refused_whichever_way_the_mode_is_set() {
            let mut tree = Tree::new();
            tree.file_with("target", "x", 0o644, 1_000_000_000);
            tree.symlink("link", "target");
            for attempt in [not_supported, not_implemented] {
                let error =
                    chmod_600(&tree, "link", attempt, "/proc/self/fd").expect_err("refused");
                assert_eq!(error.kind(), io::ErrorKind::Unsupported);
                assert!(error.to_string().contains("symlink"), "{error}");
            }
            assert_eq!(mode_of_file(&tree, "target"), 0o644);
        }
    }

    #[test]
    fn the_mode_of_the_root_itself_can_be_set() {
        let tree = Tree::new();
        let root = root_of(&tree);
        root.set_mode(&RelPath::root(), 0o750)
            .expect("the mode is set");
        assert_eq!(
            root.lstat(&RelPath::root()).expect("stat").mode & 0o777,
            0o750
        );
    }

    #[test]
    fn the_time_of_a_symlink_is_set_without_touching_its_target() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let deep = create_chain(&root, 25);
        let file = deep.join(&name("file"));
        let link = deep.join(&name("link"));
        root.create_file(&file, 0o644).expect("the file is created");
        root.create_symlink(&link, b"file")
            .expect("the symlink is created");

        let first = 1_000_000_000_i128 * 1_000_000_000;
        let second = 1_500_000_000_i128 * 1_000_000_000 + 123;
        root.set_times(&file, first)
            .expect("the file's time is set");
        root.set_times(&link, second)
            .expect("the link's time is set");

        assert_eq!(root.lstat(&file).expect("stat").mtime_ns, first);
        assert_eq!(root.lstat(&link).expect("stat").mtime_ns, second);
    }

    #[test]
    fn a_time_before_the_epoch_can_be_set() {
        let tree = Tree::new();
        let root = root_of(&tree);
        root.create_file(&path("old"), 0o644)
            .expect("the file is created");
        let before = -86_400_i128 * 1_000_000_000 - 5;
        root.set_times(&path("old"), before)
            .expect("the time is set");
        assert_eq!(root.lstat(&path("old")).expect("stat").mtime_ns, before);
    }

    #[test]
    fn a_final_symlink_is_not_followed_when_opening() {
        let mut tree = Tree::new();
        tree.file("target", "x");
        tree.symlink("link", "target");
        let root = root_of(&tree);
        let error = root
            .open_read(&path("link"))
            .expect_err("the symlink is refused");
        assert_eq!(raw_error(&error), Some(Errno::LOOP.raw_os_error()));
    }

    #[test]
    fn a_fifo_is_refused_at_once_and_not_waited_for() {
        let mut tree = Tree::new();
        tree.fifo("queue");
        let root = root_of(&tree);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let opened = root.open_read(&path("queue")).map(|_| ());
            sender.send(opened).expect("the test is still listening");
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("opening a fifo blocks");
        let error = result.expect_err("a fifo is not read");
        assert_eq!(
            TreeError::of(&error),
            Some(&TreeError::NotRegular(EntryKind::Fifo))
        );
    }

    #[test]
    fn a_directory_is_not_opened_for_reading() {
        let mut tree = Tree::new();
        tree.dir("dir");
        let error = root_of(&tree)
            .open_read(&path("dir"))
            .expect_err("a directory is not read");
        assert_eq!(
            TreeError::of(&error),
            Some(&TreeError::NotRegular(EntryKind::Directory))
        );
    }

    #[test]
    fn a_file_opened_for_reading_blocks_like_any_other() {
        let mut tree = Tree::new();
        tree.file("file", "x");
        let file = root_of(&tree)
            .open_read(&path("file"))
            .expect("the file opens");
        let flags = rustix::fs::fcntl_getfl(&file).expect("the flags are read");
        assert!(!flags.contains(OFlags::NONBLOCK), "{flags:?}");
    }

    fn raw(mode: u32, inode: u64, ctime_ns: i128) -> RawStat {
        RawStat {
            mode,
            size: 0,
            allocated: 0,
            mtime_ns: 0,
            ctime_ns,
            inode,
            device: 1,
            nlink: 1,
            birth_ns: None,
            mount_root: false,
        }
    }

    const FILE: u32 = 0o100644;
    const SYMLINK: u32 = 0o120777;

    /// `read_entry` over scripted answers: the stats in order, then the
    /// link reads in order.
    fn scripted(
        stats: Vec<io::Result<RawStat>>,
        links: Vec<io::Result<Vec<u8>>>,
    ) -> io::Result<(RawStat, Option<Vec<u8>>)> {
        let (mut stats, mut links) = (stats.into_iter(), links.into_iter());
        read_entry(
            || stats.next().expect("no more stats were scripted"),
            || links.next().expect("no more link reads were scripted"),
        )
    }

    #[test]
    fn a_file_is_stat_ed_once_and_its_link_is_not_read() {
        let (entry, target) = scripted(vec![Ok(raw(FILE, 7, 1))], vec![]).expect("read");
        assert_eq!(entry.inode, 7);
        assert_eq!(target, None);
    }

    #[test]
    fn a_symlink_that_did_not_change_is_read_with_its_target() {
        let stats = vec![Ok(raw(SYMLINK, 7, 1)), Ok(raw(SYMLINK, 7, 1))];
        let (entry, target) = scripted(stats, vec![Ok(b"target".to_vec())]).expect("read");
        assert_eq!(entry.inode, 7);
        assert_eq!(target.as_deref(), Some(&b"target"[..]));
    }

    #[test]
    fn a_symlink_replaced_during_the_read_is_read_again() {
        for changed in [raw(SYMLINK, 8, 1), raw(SYMLINK, 7, 2), raw(FILE, 7, 1)] {
            let stats = vec![
                Ok(raw(SYMLINK, 7, 1)),
                Ok(changed),
                Ok(raw(SYMLINK, 9, 3)),
                Ok(raw(SYMLINK, 9, 3)),
            ];
            let links = vec![Ok(b"old".to_vec()), Ok(b"new".to_vec())];
            let (entry, target) = scripted(stats, links).expect("the second attempt holds");
            assert_eq!(entry.inode, 9);
            assert_eq!(target.as_deref(), Some(&b"new"[..]));
        }
    }

    #[test]
    fn a_symlink_that_keeps_changing_is_reported_as_changed() {
        let stats = vec![
            Ok(raw(SYMLINK, 1, 1)),
            Ok(raw(SYMLINK, 2, 1)),
            Ok(raw(SYMLINK, 3, 1)),
            Ok(raw(SYMLINK, 4, 1)),
        ];
        let links = vec![Ok(b"a".to_vec()), Ok(b"b".to_vec())];
        let error = scripted(stats, links).map(|_| ()).expect_err("it changed");
        assert_eq!(TreeError::of(&error), Some(&TreeError::Changed));
    }

    #[test]
    fn a_link_that_stopped_being_one_is_read_again() {
        let stats = vec![Ok(raw(SYMLINK, 7, 1)), Ok(raw(FILE, 8, 2))];
        let links = vec![Err(Errno::INVAL.into())];
        let (entry, target) = scripted(stats, links).expect("the replacement is a file");
        assert_eq!(entry.inode, 8);
        assert_eq!(target, None);
    }

    #[test]
    fn another_error_while_reading_the_link_is_passed_on() {
        let stats = vec![Ok(raw(SYMLINK, 7, 1))];
        let links = vec![Err(Errno::ACCESS.into())];
        let error = scripted(stats, links).map(|_| ()).expect_err("denied");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_size_of_a_symlink_is_the_length_of_its_target_on_the_local_filesystem() {
        let mut tree = Tree::new();
        tree.symlink("link", "some/target");
        let entry = root_of(&tree).lstat(&path("link")).expect("stat");
        assert_eq!(entry.size, "some/target".len() as u64);
        assert_eq!(entry.link_target.as_deref(), Some(&b"some/target"[..]));
    }

    #[test]
    fn errors_of_the_filesystem_are_not_tree_errors() {
        let error = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(TreeError::of(&error), None);
    }

    #[test]
    fn a_file_is_created_only_when_it_does_not_exist() {
        let tree = Tree::new();
        let root = root_of(&tree);
        root.create_file(&path("once"), 0o644)
            .expect("the first create works");
        let error = root
            .create_file(&path("once"), 0o644)
            .expect_err("the second fails");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn a_symlink_inside_the_path_is_not_followed() {
        let mut tree = Tree::new();
        tree.file("real/inner/file", "x");
        tree.symlink("link", "real");
        let root = root_of(&tree);
        assert!(root.lstat(&path("link/inner")).is_err());
        assert!(root.read_dir(&path("link")).is_err());
        assert!(root.create_file(&path("link/new"), 0o644).is_err());
    }

    #[test]
    fn a_root_reached_through_a_symlink_works() {
        let mut tree = Tree::new();
        tree.file("real/file", "x");
        let link = tree.symlink("link", tree.path("real"));
        let root = TreeRoot::open(&link).expect("the root opens through the symlink");
        assert_eq!(
            root.lstat(&path("file")).expect("stat").kind,
            EntryKind::File
        );
    }

    #[test]
    fn lstat_reports_a_symlink_as_a_symlink_also_to_a_directory() {
        let mut tree = Tree::new();
        tree.dir("real");
        tree.symlink("link", "real");
        let entry = root_of(&tree).lstat(&path("link")).expect("stat");
        assert_eq!(entry.kind, EntryKind::Symlink);
        assert_eq!(entry.link_target.as_deref(), Some(&b"real"[..]));
    }

    #[test]
    fn a_missing_path_is_not_found() {
        let tree = Tree::new();
        let error = root_of(&tree)
            .lstat(&path("missing/entry"))
            .expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_root_is_a_directory() {
        let tree = Tree::new();
        let entry = root_of(&tree).lstat(&RelPath::root()).expect("stat");
        assert_eq!(entry.kind, EntryKind::Directory);
        assert!(entry.path.is_root());
    }

    #[test]
    fn nothing_can_be_created_or_removed_at_the_root_path() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let top = RelPath::root();
        assert_eq!(
            root.create_dir(&top, 0o755).expect_err("refused").kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            root.create_file(&top, 0o644)
                .map(|_| ())
                .expect_err("refused")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            root.remove_dir(&top).expect_err("refused").kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            root.remove_file(&top).expect_err("refused").kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            root.create_symlink(&top, b"x").expect_err("refused").kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            root.hard_link(&path("a"), &top)
                .expect_err("refused")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_sparse_file_reports_less_allocated_than_its_size() {
        let mut tree = Tree::new();
        tree.sparse("hole", 1 << 20);
        let entry = root_of(&tree).lstat(&path("hole")).expect("stat");
        assert_eq!(entry.size, 1 << 20);
        assert!(
            entry.allocated < entry.size,
            "{} bytes allocated",
            entry.allocated
        );
    }

    #[test]
    fn a_fifo_is_reported_as_a_fifo() {
        let mut tree = Tree::new();
        tree.fifo("queue");
        let entry = root_of(&tree).lstat(&path("queue")).expect("stat");
        assert_eq!(entry.kind, EntryKind::Fifo);
    }

    #[test]
    fn the_time_of_a_file_is_the_one_it_was_given() {
        let mut tree = Tree::new();
        tree.file_with("file", "x", 0o600, 1_000_000_000);
        let entry = root_of(&tree).lstat(&path("file")).expect("stat");
        assert_eq!(entry.mtime_ns, 1_000_000_000_i128 * 1_000_000_000);
    }

    /// Every field of the entry at `relative` against what the standard
    /// library reports for the same path.
    fn assert_matches_std(tree: &Tree, relative: &str) {
        let full = tree.path(relative);
        let std = std::fs::symlink_metadata(&full).expect("std stats the entry");
        let entry = root_of(tree).lstat(&path(relative)).expect("stat");
        let nanoseconds =
            |seconds: i64, nanos: i64| i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanos);
        assert_eq!(entry.mode, std.mode(), "{relative}: mode");
        assert_eq!(entry.size, std.len(), "{relative}: size");
        assert_eq!(entry.allocated, std.blocks() * 512, "{relative}: allocated");
        assert_eq!(entry.inode, std.ino(), "{relative}: inode");
        assert_eq!(entry.device, std.dev(), "{relative}: device");
        assert_eq!(entry.nlink, std.nlink(), "{relative}: nlink");
        assert_eq!(
            entry.mtime_ns,
            nanoseconds(std.mtime(), std.mtime_nsec()),
            "{relative}: mtime"
        );
        assert_eq!(
            entry.ctime_ns,
            nanoseconds(std.ctime(), std.ctime_nsec()),
            "{relative}: ctime"
        );
        // Where the filesystem has no birth time, std has none either.
        let created = std.created().ok().map(|time| {
            let since_epoch = time.duration_since(std::time::UNIX_EPOCH);
            let nanos = since_epoch.expect("a birth time after 1970").as_nanos();
            i128::try_from(nanos).expect("fits")
        });
        assert!(
            created.is_none() || entry.birth_ns == created,
            "{relative}: birth"
        );
    }

    #[test]
    fn every_field_of_a_stat_is_the_one_std_reports() {
        let mut tree = Tree::new();
        tree.file_with("file", "some content", 0o640, 1_000_000_000);
        tree.dir("dir/inner");
        tree.symlink("link", "file");
        tree.hard_link("file", "hard");
        tree.sparse("hole", 1 << 20);
        for relative in ["file", "dir", "dir/inner", "link", "hard", "hole", ""] {
            assert_matches_std(&tree, relative);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_a_birth_time() {
        let mut tree = Tree::new();
        tree.file("file", "x");
        let entry = root_of(&tree).lstat(&path("file")).expect("stat");
        assert!(entry.birth_ns.is_some_and(|ns| ns > 0));
    }

    #[test]
    fn a_subtree_can_be_opened_as_a_root_of_its_own() {
        let mut tree = Tree::new();
        tree.file("a/b/file", "x");
        let sub = root_of(&tree).sub(&path("a/b")).expect("the subtree opens");
        assert_eq!(
            sub.lstat(&path("file")).expect("stat").kind,
            EntryKind::File
        );
    }

    #[test]
    fn a_sub_root_takes_paths_relative_to_itself_and_reports_paths_relative_to_the_original() {
        let mut tree = Tree::new();
        tree.file("a/b/f", "x");
        let root = root_of(&tree);
        assert!(root.prefix().is_root());
        let sub = root.sub(&path("a/b")).expect("the subtree opens");
        assert_eq!(sub.prefix(), &path("a/b"));
        assert_eq!(sub.lstat(&path("f")).expect("stat").path, path("a/b/f"));
    }

    #[test]
    fn the_root_of_a_sub_root_is_reported_as_the_path_it_was_opened_at() {
        let mut tree = Tree::new();
        tree.dir("a/b");
        let sub = root_of(&tree).sub(&path("a/b")).expect("the subtree opens");
        let entry = sub.lstat(&RelPath::root()).expect("stat");
        assert_eq!(entry.path, path("a/b"));
        assert_eq!(entry.kind, EntryKind::Directory);
    }

    #[test]
    fn a_sub_root_of_a_sub_root_carries_both_prefixes() {
        let mut tree = Tree::new();
        tree.file("a/b/c/f", "x");
        let root = root_of(&tree);
        let inner = root
            .sub(&path("a"))
            .and_then(|a| a.sub(&path("b/c")))
            .expect("the subtrees open");
        assert_eq!(inner.prefix(), &path("a/b/c"));
        assert_eq!(inner.lstat(&path("f")).expect("stat").path, path("a/b/c/f"));
    }

    #[test]
    fn a_listing_through_a_sub_root_names_each_entry_relative_to_the_original() {
        let mut tree = Tree::new();
        tree.file("a/b/x", "1");
        let sub = root_of(&tree).sub(&path("a")).expect("the subtree opens");
        let listed = sub.read_dir(&path("b")).expect("listed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name.as_bytes(), b"x");
        assert_eq!(listed[0].path, path("a/b/x"));
        let from_root = root_of(&tree).read_dir(&RelPath::root()).expect("listed");
        assert_eq!(from_root[0].path, path("a"));
    }

    #[test]
    fn a_sub_root_stays_usable_when_an_ancestor_can_no_longer_be_entered() {
        let mut tree = Tree::new();
        tree.file("a/b/c/f", "x");
        let sub = root_of(&tree).sub(&path("a/b")).expect("the subtree opens");
        let ancestor = tree.path("a");
        let _unlock = Unlock(vec![ancestor.clone()]);
        std::fs::set_permissions(&ancestor, Permissions::from_mode(0o000)).expect("mode is set");
        assert!(
            std::fs::metadata(ancestor.join("b")).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let entry = sub.lstat(&path("c/f")).expect("the file is still reached");
        assert_eq!(entry.path, path("a/b/c/f"));
    }

    /// A tree with `search_only/inner/file`, where `search_only` can be
    /// entered and written but not listed (mode 0311).
    fn search_only_tree() -> (Tree, Unlock) {
        let mut tree = Tree::new();
        tree.file("search_only/inner/file", "x");
        let locked = tree.path("search_only");
        std::fs::set_permissions(&locked, Permissions::from_mode(0o311)).expect("mode is set");
        assert!(
            std::fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 0311 keeps from listing"
        );
        (tree, Unlock(vec![locked]))
    }

    #[test]
    fn entries_below_a_directory_that_can_only_be_entered_are_reached() {
        let (tree, _unlock) = search_only_tree();
        let root = root_of(&tree);
        let entry = root
            .lstat(&path("search_only/inner/file"))
            .expect("the file is stat'd");
        assert_eq!(entry.kind, EntryKind::File);
        root.remove_file(&path("search_only/inner/file"))
            .expect("the file is removed");
    }

    #[test]
    fn a_root_that_can_only_be_entered_is_opened_and_used() {
        let (tree, _unlock) = search_only_tree();
        let root = TreeRoot::open(&tree.path("search_only")).expect("the root opens");
        assert_eq!(
            root.lstat(&path("inner/file")).expect("stat").kind,
            EntryKind::File
        );
        root.create_dir(&path("inner/new"), 0o755)
            .expect("a directory is created");
    }

    #[test]
    fn a_directory_that_can_only_be_entered_cannot_be_listed() {
        let (tree, _unlock) = search_only_tree();
        let root = root_of(&tree);
        let error = root
            .read_dir(&path("search_only"))
            .expect_err("listing needs read permission");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_symlink_in_place_of_a_file_never_gets_its_target_changed() {
        let mut tree = Tree::new();
        let target = tree.file_with("target", "x", 0o644, 1_000_000_000);
        tree.symlink("link", &target);
        let root = root_of(&tree);
        let _ = root.set_mode(&path("link"), 0o600);
        let mode = std::fs::metadata(&target).expect("exists").mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn a_symlink_swapped_in_while_the_mode_is_set_never_gets_its_target_changed() {
        let mut tree = Tree::new();
        let outside = tree.file_with("outside/secret", "s", 0o600, 1_000_000_000);
        tree.dir("inside");
        let inside = tree.path("inside");
        let root = TreeRoot::open(&inside).expect("the root opens");
        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let (inside, outside, stop) = (inside.clone(), outside.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::write(inside.join("file.tmp"), "x");
                    let _ = std::fs::rename(inside.join("file.tmp"), inside.join("x"));
                    let _ = std::os::unix::fs::symlink(&outside, inside.join("link.tmp"));
                    let _ = std::fs::rename(inside.join("link.tmp"), inside.join("x"));
                }
            })
        };
        let changed = (0..20_000).any(|_| {
            let _ = root.set_mode(&path("x"), 0o777);
            let mode = std::fs::metadata(&outside).expect("exists").mode() & 0o777;
            mode != 0o600
        });
        stop.store(true, Ordering::Relaxed);
        swapper.join().expect("the swapper ends");
        assert!(!changed, "set_mode changed a file outside the tree");
    }

    #[test]
    fn an_entry_is_renamed_from_one_root_into_another() {
        let mut tree = Tree::new();
        tree.file("from/dir/file", "x");
        tree.dir("to");
        let from = TreeRoot::open(&tree.path("from")).expect("the root opens");
        let to = TreeRoot::open(&tree.path("to")).expect("the root opens");
        from.rename(&path("dir"), &to, &path("moved"))
            .expect("the directory is moved");
        assert!(from.lstat(&path("dir")).is_err());
        assert_eq!(
            to.lstat(&path("moved/file")).expect("stat").kind,
            EntryKind::File
        );
    }

    #[test]
    fn an_existing_target_is_not_replaced() {
        let mut tree = Tree::new();
        tree.file("one", "first");
        tree.file("two", "second");
        let root = root_of(&tree);
        let error = root
            .rename(&path("one"), &root, &path("two"))
            .expect_err("the target exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(tree.path("two")).expect("readable"),
            "second"
        );
        assert!(tree.path("one").exists());
    }

    #[test]
    fn an_entry_can_be_renamed_at_depth() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let deep = create_chain(&root, 25);
        let before = deep.join(&name("before"));
        let after = deep.join(&name("after"));
        root.create_file(&before, 0o644)
            .expect("the file is created");
        root.rename(&before, &root, &after)
            .expect("the file is renamed");
        assert!(root.lstat(&before).is_err());
        assert_eq!(root.lstat(&after).expect("stat").kind, EntryKind::File);
    }

    /// `rename_no_replace` of `from` to `to` in one root, with `attempt`
    /// in place of the real no-replace rename.
    fn seamed_rename(
        root: &TreeRoot,
        from: &str,
        to: &str,
        attempt: RenameAttempt,
    ) -> io::Result<()> {
        let (from, to) = (path(from), path(to));
        let (dir, from_name) = root.split(&from).expect("the source splits");
        let (_, to_name) = root.split(&to).expect("the target splits");
        rename_no_replace(dir.as_fd(), from_name, dir.as_fd(), to_name, attempt)
    }

    /// What a filesystem without no-replace renames answers.
    fn flag_not_supported(
        _from: BorrowedFd<'_>,
        _from_name: &[u8],
        _to: BorrowedFd<'_>,
        _to_name: &[u8],
    ) -> io::Result<()> {
        Err(Errno::NOTSUP.into())
    }

    /// An error that has nothing to do with the flag.
    fn denied(
        _from: BorrowedFd<'_>,
        _from_name: &[u8],
        _to: BorrowedFd<'_>,
        _to_name: &[u8],
    ) -> io::Result<()> {
        Err(Errno::ACCESS.into())
    }

    #[test]
    fn without_no_replace_support_a_missing_target_is_checked_for_and_renamed_to() {
        let mut tree = Tree::new();
        tree.file("one", "x");
        let root = root_of(&tree);
        seamed_rename(&root, "one", "two", flag_not_supported).expect("the file is renamed");
        assert!(root.lstat(&path("one")).is_err());
        assert!(root.lstat(&path("two")).is_ok());
    }

    #[test]
    fn without_no_replace_support_an_existing_target_is_still_refused() {
        let mut tree = Tree::new();
        tree.file("one", "x");
        tree.symlink("two", "nowhere");
        let root = root_of(&tree);
        let error = seamed_rename(&root, "one", "two", flag_not_supported)
            .expect_err("the target exists, also as a dangling symlink");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(root.lstat(&path("one")).is_ok());
    }

    #[test]
    fn without_no_replace_support_a_target_that_cannot_be_looked_at_is_an_error() {
        let mut tree = Tree::new();
        tree.file("one", "x");
        let not_a_directory = std::fs::File::open(tree.path("one")).expect("opens");
        let root = root_of(&tree);
        let one = path("one");
        let (dir, from_name) = root.split(&one).expect("the source splits");
        let error = rename_no_replace(
            dir.as_fd(),
            from_name,
            not_a_directory.as_fd(),
            b"two",
            flag_not_supported,
        )
        .expect_err("a file has no entries");
        assert_eq!(error.raw_os_error(), Some(Errno::NOTDIR.raw_os_error()));
    }

    #[test]
    fn another_error_of_the_rename_is_not_mistaken_for_a_missing_flag() {
        let mut tree = Tree::new();
        tree.file("one", "x");
        let root = root_of(&tree);
        let error = seamed_rename(&root, "one", "two", denied).expect_err("the error is passed on");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(root.lstat(&path("two")).is_err());
    }

    #[test]
    fn the_root_itself_cannot_be_renamed() {
        let tree = Tree::new();
        let root = root_of(&tree);
        let error = root
            .rename(&RelPath::root(), &root, &path("elsewhere"))
            .expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_directory_is_listed_with_the_kinds_of_its_entries() {
        let mut tree = Tree::new();
        tree.file("dir/file", "x");
        tree.dir("dir/sub");
        tree.symlink("dir/link", "file");
        let mut listed: Vec<(String, EntryKind)> = root_of(&tree)
            .read_dir(&path("dir"))
            .expect("listed")
            .iter()
            .map(|e| (e.name.to_string(), e.kind))
            .collect();
        listed.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            listed,
            [
                ("file".to_string(), EntryKind::File),
                ("link".to_string(), EntryKind::Symlink),
                ("sub".to_string(), EntryKind::Directory),
            ]
        );
    }

    #[test]
    fn a_path_that_is_not_a_directory_cannot_be_listed() {
        let mut tree = Tree::new();
        tree.file("file", "x");
        let error = root_of(&tree)
            .read_dir(&path("file"))
            .expect_err("not a directory");
        assert_eq!(raw_error(&error), Some(Errno::NOTDIR.raw_os_error()));
    }
}
