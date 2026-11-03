# Filesystem primitives from Rust

Researched 2026-11-03 on macOS (Darwin 27.0.0, arm64) with Rust 1.97.0:
an APFS system volume, three SMB shares of the NAS mounted through the
macOS client (smbfs), one NFS mount. No Linux host was available, so
every Linux statement is read from documentation. Crate figures come
from the crates.io API on that day.

Marks: **measured** (run on this machine), **read** (documentation or
source, named), **reported** (third party).

Probed: std 1.97, fs4 1.1.0, rustix 1.1.6, nix 0.31.3, tempfile 3.27.0,
atomic-write-file 0.3.1, walkdir 2.5.0, jwalk 0.9.0, remove_dir_all
1.0.0, sysinfo 0.40.1, libc 0.2.191.

## Requirements

| # | Requirement |
|---|---|
| R1 | Exclusive advisory lock on `.antiq/lock` for a whole run, pid in the file, a clear message when held |
| R2 | Atomic write: temp file in the same directory, fsync, rename, fsync of the directory |
| R3 | Exclusive create (`O_EXCL`) of `meta.<N+1>`, also on an SMB mount |
| R4 | Rename a folder into `.antiq/trash/`, detect EXDEV, delete large trees |
| R5 | lstat-only walk collecting type, size, mtime_ns, ctime, inode, mode, link target, birth time; 20,000+ files; non-UTF-8 names |
| R6 | Birth time on APFS and Linux |
| R7 | Mount point test (`st_dev`) and filesystem type (apfs, smbfs, nfs) |
| R8 | Free disk space before packing |

## 1. Locking

- `File::lock` and `File::try_lock` are stable since Rust 1.89 (**read**,
  `std/src/fs.rs`). On macOS and Linux they call `flock`; `try_lock`
  returns `TryLockError::WouldBlock` when another process holds the lock
  (**read**, `std/src/sys/fs/unix.rs`; **measured**).
- Std opens files with `O_CLOEXEC`, so `ssh` and `rsync` children do not
  inherit the lock (**read**).
- Two processes on APFS: the second got `WouldBlock` and read the
  holder's pid from the file. After `kill -9` of the holder the next run
  got the lock at once (**measured**). A failed `try_lock` therefore
  means a live holder; the pid in the file can be stale.
- Pid pattern: open without truncating, `try_lock`, then `set_len(0)`
  and write the pid. Truncating before the lock would erase the
  holder's pid.
- smbfs, one client: same results (**measured**). Apple's client takes
  an SMB byte-range lock over the whole file for `flock` (**read**,
  apple-oss-distributions/smb, `kernel/smbfs/smbfs_vnops.c`,
  `smbfs_vnop_advlock`). Not tested with two machines.
- Linux: on NFS, `flock` is emulated with whole-file `fcntl` locks, and
  the mount option `local_lock` can make it local only; on CIFS before
  5.5 `flock` is not sent to the server (**read**, man7.org `flock(2)`,
  `nfs(5)`).
- `fs4` 1.1.0 calls the same `flock` through rustix (**read**, source);
  `fd-lock` 4.0.4 adds a guard type around it; `fs2` 0.4.3 was last
  released in 2018.

## 2. Atomic writes and fsync

On macOS std implements `sync_all` and `sync_data` with
`fcntl(F_FULLFSYNC)` (**read**, `std/src/sys/fs/unix.rs`), and smbfs
does not support it:

| Call | APFS | smbfs |
|---|---|---|
| `File::sync_all()` | ok, 5 to 13 ms | `ENOTSUP` |
| `File::sync_data()` | ok | `ENOTSUP` |
| `fcntl(F_FULLFSYNC)` | ok | `ENOTSUP` |
| `libc::fsync` on a file | ok | ok |
| `File::open(dir).sync_all()` | ok | `ENOTSUP` |
| `libc::fsync` on a directory | ok | ok |
| `rename` over an existing file | replaces | replaces |

(**measured**). A write helper therefore falls back to plain `fsync`
on `ENOTSUP`; `rustix::fs::fsync` is plain `fsync` on macOS and
`rustix` also offers `fcntl_fullfsync` (**read**, rustix 1.1.6
`src/fs/fcntl_apple.rs`). Whether a successful `fsync` on smbfs means
the server has the data on disk is not known.

- `atomic-write-file` 0.3.1 (BSD-3-Clause, 6.2M downloads): temp file,
  fsync, rename, directory fsync, always replacing (**read**, source).
  Works on APFS; on smbfs `commit()` failed with `ENOTSUP` and left its
  temp file behind (**measured**).
- `tempfile` 3.27.0: `persist` is a rename; `persist_noclobber` uses
  `RENAME_NOREPLACE` and falls back to hard link plus unlink (**read**,
  `src/file/imp/unix.rs`). On smbfs it failed with `ENOTSUP` for a new
  target, and `link()` also fails with `ENOTSUP` (**measured**). An SMB
  mount offers no rename that refuses an existing target.

## 3. Exclusive create

- `OpenOptions::create_new(true)` returns `ErrorKind::AlreadyExists`
  when the file exists (**measured**, APFS and smbfs).
- Race test, 4 processes with 8 threads each creating the same name, 30
  rounds: exactly one winner in every round on APFS and on smbfs, no
  other errors (**measured**, one client).
- Apple's client sends `O_EXCL` as the SMB create disposition
  `FILE_CREATE`, which the server decides (**read**, `smbfs_vnops.c`).
  Not tested from two machines; the NAS's Samba settings were not
  examined.
- The file exists under its final name from the moment of creation, so
  a reader on another machine can see `meta.<N+1>` empty or partly
  written. Write-then-rename-exclusively is not available on smbfs
  (section 2).

## 4. Trash and deleting trees

- `fs::rename` across devices fails with `ErrorKind::CrossesDevices`
  (EXDEV, errno 18) (**measured**, APFS to smbfs).
- Directory rename works on smbfs; renaming onto a non-empty directory
  gives `ENOTEMPTY` (**measured**).
- `std::fs::remove_dir_all` did not follow a symlink out of the tree
  (**measured**). It does not change permissions: a tree with a `0555`
  directory failed with `PermissionDenied` and was left half deleted on
  APFS (**measured**). A delete helper retries after `chmod u+rwx`.
- The `remove_dir_all` crate 1.0.0 re-exports std on non-Windows
  (**read**, README) and failed the same way (**measured**).
- Not tested: files with the macOS immutable flag (`uchg`).

## 5. Walking a tree

Each walk took one `symlink_metadata` per entry and collected type,
size, mtime_ns, ctime_ns, inode, mode, link target and `created()`; warm
cache, release build (**measured**):

| Tree | Entries | `read_dir` + lstat | walkdir | jwalk |
|---|---|---|---|---|
| `~/playground` | 57,057 | 194 ms | 187 ms | 133 ms |
| `~/.cargo/registry` | 152,352 | 822 ms | 674 ms (1.15 s with a second lstat) | 377 ms |

- `walkdir` and `jwalk` return lstat data from `DirEntry::metadata()`
  when not following links, which is their default (**read**, walkdir
  `src/dent.rs`, jwalk `src/core/dir_entry.rs`). A second lstat per
  entry cost about 1.7 times as much.
- `walkdir` 2.5.0 (739M downloads, 7,155 reverse dependencies) has
  `same_file_system(true)`. `jwalk` 0.9.0 is parallel through rayon and
  crossbeam.
- APFS refuses non-UTF-8 names (`EILSEQ`), so none was walked
  (**measured**). Names stay `OsString` through the walk.

## 6. Birth time

- `Metadata::created()` gives nanoseconds on APFS (**measured**).
- On smbfs, a file created through the mount reported a birth time equal
  to its mtime, at 100 ns granularity (**measured**).
- On the NFS mount `created()` returned a value; whether it is real is
  unknown.
- On Linux, `rustix::fs::statx` returns `stx_btime` through raw system
  calls, so it also works in static musl builds, where std's `created()`
  returns `Unsupported` (**read**, rustix `src/fs/statx.rs`). Not run.

## 7. Mount point and filesystem type

| Path | `st_dev` differs from parent | `statfs` `f_fstypename` |
|---|---|---|
| home directory | no | apfs |
| `/System/Volumes/Data` | no, although it is a mount point | apfs |
| SMB shares under `/Volumes` | yes | smbfs |
| NFS mount | yes | nfs |

(**measured**). Comparing `st_dev` with the parent catches network
mounts and external disks; `statfs`'s `f_mntonname` gives the exact
mount point. `libc::statfs`, `nix::sys::statfs` and
`rustix::fs::statfs` gave the same type name on macOS. Linux has no
`f_fstypename`; `f_type` magic numbers identify the type, and nix
exports constants such as `NFS_SUPER_MAGIC` and `SMB_SUPER_MAGIC`
(**read**, nix `src/sys/statfs.rs`).

## 8. Free space

`statvfs` `f_bavail * f_frsize` gave 314,873,352,192 bytes for `/`
through rustix, nix and `fs4::available_space` (**measured**).
`sysinfo` 0.40.1 reported 362 GB available for `/` at the same time
(**measured**). Hard links are not supported on smbfs (`ENOTSUP`).

## Crate facts

| Crate | Latest | Released | Downloads (recent) | Reverse deps | Licence |
|---|---|---|---|---|---|
| fs4 | 1.1.0 | 2026-04-28 | 78.2M (20.6M) | 442 | MIT OR Apache-2.0 |
| fd-lock | 4.0.4 | 2025-03-10 | 71.7M (15.5M) | 165 | MIT OR Apache-2.0 |
| fs2 | 0.4.3 | 2018-01-06 | 99.5M (25.1M) | 1,452 | MIT/Apache-2.0 |
| rustix | 1.1.6 | 2026-10-27 | 1,343M (335M) | not fetched | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| nix | 0.31.3 | 2026-05-11 | 937M (230M) | not fetched | MIT |
| atomic-write-file | 0.3.1 | 2026-08-11 | 6.2M (1.25M) | 78 | BSD-3-Clause |
| tempfile | 3.27.0 | 2026-03-11 | 954M (235M) | 39,539 | MIT OR Apache-2.0 |
| walkdir | 2.5.0 | 2024-03-01 | 739M (190M) | 7,155 | Unlicense/MIT |
| jwalk | 0.9.0 | 2026-08-05 | 13.2M (2.6M) | 196 | MIT |
| remove_dir_all | 1.0.0 | 2024-11-22 | 135M (11.0M) | 84 | MIT OR Apache-2.0 |
| sysinfo | 0.40.1 | 2026-10-30 | 237M (58.6M) | not fetched | MIT |

RustSec has no entries for most of these; `nix` has RUSTSEC-2021-0119
(2021) and `remove_dir_all` RUSTSEC-2023-0018 for its old 0.5 line
(**read**, rustsec.org).

## Assessment

- Lock: std `File::try_lock`, no extra crate. `.antiq/` is local; a
  warning when it sits on smbfs or NFS.
- Atomic writes: a small helper on std: `create_new` temp in the same
  directory, write, `sync_all` with a fallback to `fsync` on `ENOTSUP`,
  rename, the same for the directory. `atomic-write-file` is fine for
  local state files only.
- Exclusive create of `meta.<N+1>`: `create_new` held on smbfs. The
  open problem is a reader seeing a partly written file under the final
  name, and SMB offers no exclusive rename. The commit protocol has to
  account for it, for example by treating an unparseable or
  checksum-failing `meta.N` as not committed, or by creating an empty
  claim file exclusively first. Neither was tested.
- Trash: `fs::rename`, matching `ErrorKind::CrossesDevices`; a delete
  helper that retries with `chmod` on `PermissionDenied`.
- Walk: `walkdir` with one lstat per entry; `jwalk` is faster on large
  trees but brings rayon and crossbeam.
- Birth time: std `created()`; `rustix::fs::statx` only for static musl
  builds.
- Mount point and type: `st_dev` comparison, `statfs` on macOS, `f_type`
  on Linux, through `rustix` or `nix`.
- Free space: `statvfs` through `rustix` or `fs4`, not `sysinfo`.

## Not verified

- Anything on Linux.
- Locking and `O_EXCL` on smbfs from two machines; NFS writes.
- The NAS's Samba settings (oplocks, case sensitivity, `strict sync`).
- Whether `fsync` returning 0 on smbfs means the data is on the
  server's disk.
- Deleting very large trees; immutable flags.

## Open points for the design

- How the commit protocol handles a partly written `meta.<N+1>` on a
  path remote over SMB.
- How non-UTF-8 names (Linux only) are written to listings and
  manifests.
