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

use crate::relpath::{Name, RelPath};
use crate::tree::{EntryKind, TreeEntry};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{
    AtFlags, Dir, Mode, OFlags, RawMode, RenameFlags, Timespec, Timestamps, UTIME_OMIT, chmodat,
    linkat, mkdirat, open, openat, readlinkat, renameat, renameat_with, statat, symlinkat,
    unlinkat, utimensat,
};
use rustix::io::Errno;
use std::fs::File;
use std::io;
use std::path::Path;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// How every directory on the way to an entry is opened.
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A directory held open, through which entries below it are reached.
#[derive(Debug)]
pub struct TreeRoot {
    fd: OwnedFd,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Name,
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
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = open(path, flags, Mode::empty())?;
        Ok(TreeRoot { fd })
    }

    /// The directory at `path` as a root of its own.
    pub fn sub(&self, path: &RelPath) -> io::Result<TreeRoot> {
        Ok(TreeRoot {
            fd: self.open_dir(path)?,
        })
    }

    /// `lstat` of the entry, with the target of a symlink. The root path
    /// stands for the root directory.
    pub fn lstat(&self, path: &RelPath) -> io::Result<TreeEntry> {
        let (dir, name) = self.split_or_root(path)?;
        let raw = read_stat(dir.as_fd(), name)?;
        let kind = EntryKind::from_mode(raw.mode);
        let link_target = match kind {
            EntryKind::Symlink => Some(readlinkat(dir.as_fd(), name, Vec::new())?.into_bytes()),
            _ => None,
        };
        Ok(TreeEntry {
            path: path.clone(),
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
        })
    }

    pub fn read_link(&self, path: &RelPath) -> io::Result<Vec<u8>> {
        let (dir, name) = self.split(path)?;
        Ok(readlinkat(dir.as_fd(), name, Vec::new())?.into_bytes())
    }

    /// The entries of a directory, in the order the filesystem gives them.
    pub fn read_dir(&self, path: &RelPath) -> io::Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        for entry in Dir::new(self.open_dir(path)?)? {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name = Name::from_bytes(bytes.to_vec())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let kind = EntryKind::from_mode(RawMode::from(entry.file_type().as_raw_mode()).into());
            entries.push(DirEntry { name, kind });
        }
        Ok(entries)
    }

    /// Opens a file for reading. The open never waits for a writer, so a
    /// fifo that replaced the file cannot stall it; a symlink is refused.
    pub fn open_read(&self, path: &RelPath) -> io::Result<File> {
        let (dir, name) = self.split(path)?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        Ok(File::from(openat(dir.as_fd(), name, flags, Mode::empty())?))
    }

    pub fn create_dir(&self, path: &RelPath, mode: u32) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(mkdirat(dir.as_fd(), name, mode_bits(mode))?)
    }

    /// Creates a file for writing; fails if the name exists.
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

    /// Creates a symlink holding `target` as given.
    pub fn create_symlink(&self, path: &RelPath, target: &[u8]) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(symlinkat(target, dir.as_fd(), name)?)
    }

    /// Gives the file at `existing` a second name, `new`.
    pub fn hard_link(&self, existing: &RelPath, new: &RelPath) -> io::Result<()> {
        let (from, from_name) = self.split(existing)?;
        let (to, to_name) = self.split(new)?;
        let no_flags = AtFlags::empty();
        linkat(from.as_fd(), from_name, to.as_fd(), to_name, no_flags)?;
        Ok(())
    }

    /// Sets the permission bits of a file or directory. A symlink is
    /// refused: Linux has no way to change a symlink's mode.
    pub fn set_mode(&self, path: &RelPath, mode: u32) -> io::Result<()> {
        let (dir, name) = self.split_or_root(path)?;
        if EntryKind::from_mode(read_stat(dir.as_fd(), name)?.mode) == EntryKind::Symlink {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the mode of a symlink cannot be changed",
            ));
        }
        // Never a flag: Linux refuses `SYMLINK_NOFOLLOW` here, and symlinks
        // were turned away above.
        let no_flags = AtFlags::empty();
        chmodat(dir.as_fd(), name, mode_bits(mode), no_flags)?;
        Ok(())
    }

    /// Sets the modification time, in nanoseconds since the epoch, of the
    /// entry itself: the time of a symlink is its own, not its target's.
    /// The access time stays.
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

    /// Moves the entry at `from` to `to` in `to_root`, which may be this
    /// root. An existing target is never replaced: the rename fails with
    /// `AlreadyExists`.
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

    /// Removes a file, a symlink or any other entry that is not a
    /// directory.
    pub fn remove_file(&self, path: &RelPath) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(unlinkat(dir.as_fd(), name, AtFlags::empty())?)
    }

    /// Removes an empty directory.
    pub fn remove_dir(&self, path: &RelPath) -> io::Result<()> {
        let (dir, name) = self.split(path)?;
        Ok(unlinkat(dir.as_fd(), name, AtFlags::REMOVEDIR)?)
    }

    fn open_dir(&self, path: &RelPath) -> io::Result<OwnedFd> {
        let (dir, name) = self.split_or_root(path)?;
        Ok(openat(dir.as_fd(), name, DIRECTORY, Mode::empty())?)
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
            let next = openat(handle.as_fd(), name, DIRECTORY, Mode::empty())?;
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
    })
}

#[cfg(not(target_os = "linux"))]
fn read_stat(dir: BorrowedFd<'_>, name: &[u8]) -> io::Result<RawStat> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
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
    })
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
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
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
    fn the_mode_of_a_symlink_is_not_changed() {
        let tree = Tree::new();
        let root = root_of(&tree);
        root.create_symlink(&path("link"), b"nowhere")
            .expect("the symlink is created");
        let error = root
            .set_mode(&path("link"), 0o600)
            .expect_err("a symlink has no mode here");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
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
    fn opening_a_fifo_does_not_wait_for_a_writer() {
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
        result.expect("the fifo opens");
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
    fn times_and_owner_fields_come_from_the_stat() {
        let mut tree = Tree::new();
        tree.file_with("file", "x", 0o600, 1_000_000_000);
        let entry = root_of(&tree).lstat(&path("file")).expect("stat");
        assert_eq!(entry.mtime_ns, 1_000_000_000_i128 * 1_000_000_000);
        assert!(
            entry.ctime_ns > entry.mtime_ns,
            "ctime is the time of the change just made"
        );
        assert!(entry.inode > 0);
        assert!(entry.device > 0);
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
