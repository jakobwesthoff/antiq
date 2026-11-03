# Tar from Rust: packing, verifying and extracting

Researched 2026-11-03 on macOS arm64, APFS (case-insensitive), Rust
1.97.0, with `tar` 0.4.46, `xz2` and `filetime` 0.2.29 in a probe crate,
GNU tar 1.35 (`gtar`) and bsdtar 3.5.3 with libarchive 3.7.4. Crate
figures come from the crates.io API on that day.

Marks: **measured** (run on this machine), **read** (documentation or
source, named), **reported** (issue tracker, linked).

## Requirements

| # | Requirement |
|---|---|
| R1 | Tar written in-process, streamed into a compressor |
| R2 | Byte-deterministic: entries sorted by path bytes, pinned mode, fixed uid/gid/uname/gname, no per-run extras; unchanged files give unchanged bytes |
| R3 | Per-file SHA-256 in the same read pass as packing |
| R4 | Long paths, non-UTF-8 names, symlinks stored and never followed, hard links, empty directories, modes |
| R5 | A decision on macOS xattrs such as `com.apple.provenance` |
| R6 | Strict verify: type, size, content hash, mtime, mode, link target against the disk; equal path sets |
| R7 | Safe extraction into staging: refuse escaping members, restore mtimes and modes |
| R8 | Read archives made by GNU tar and bsdtar, including the 313 archives `archive.sh` made |
| R9 | Licence compatible with an MPL-2.0 binary; macOS and Linux |

## Option A: the `tar` crate

- Pure Rust, MIT OR Apache-2.0. Dependencies `filetime`, `libc` and
  `xattr` (default feature, system calls only).
- First release 2014-11-11, 0.4.46 on 2026-05-18; 275,967,083
  downloads, 72,372,014 recent; 3,620 reverse dependencies. The
  repository is now `composefs/tar-rs`, with 167 open issues.
- RustSec: RUSTSEC-2026-0067 (`unpack_in` chmod follows symlinks) and
  RUSTSEC-2026-0068 (PAX size ignored when the base header size is
  non-zero), both fixed in 0.4.45; RUSTSEC-2021-0080 and
  RUSTSEC-2018-0002, older link handling bugs (**read**, rustsec.org,
  advisory-db). All four concern extraction.
- API: `Builder<W: Write>` streams without seeking; `append_data`
  writes a header and the data, with GNU long names (`L`); `append_link`
  handles long link targets (`K`). `Archive<R: Read>::entries()` reads
  as a stream; `Entry::unpack_in(dst)` extracts one member (**read**,
  0.4.46 source).

### Determinism

`HeaderMode::Deterministic` pins mtime to 1153704088, uid/gid to 0 and
reduces the mode to `0755` (directories and files with the owner execute
bit) or `0644` (**read**, `src/header.rs` around line 797). A
`chmod 600` therefore does not change the archive (**measured**); modes
cannot be verified, and a restored `0600` file comes back `0644`.

Hand-built headers keep the mode: `Header::new_gnu()`,
`set_mode(mode & 0o7777)`, uid and gid 0, empty uname and gname,
`set_mtime`, `set_entry_type`, `set_size`, then `append_data` or
`append_link`. The builder adds no pax records.

Test tree of 18 members (a 124-character directory chain, a long
symlink target, absolute, relative and dangling symlinks, `0600` and
`0755` files, an empty file, an empty directory), tar SHA-256 prefixes
(**measured**):

| Packer | Original | After `cp -R` (new mtimes) | After `cp -Rp` |
|---|---|---|---|
| hand-built GNU headers, fixed mtime | `4b10f50a` | `4b10f50a` | `4b10f50a` |
| hand-built GNU headers, real mtime | `f488dba6` | `6acfcdc0` | `f488dba6` |
| `HeaderMode::Deterministic` | `2459cd9e` | `2459cd9e` | `2459cd9e` |

- With a fixed mtime, `touch` leaves the bytes unchanged and `chmod 600`
  changes them (`4b10f50a` to `6cfbecb9`).
- With real mtimes, the bytes change when a file's mtime changes, as
  after `cp -R`, and stay the same after `cp -Rp`.
- Streaming into `xz2::write::XzEncoder` at preset 9 gave the same
  `.tar.xz` on a re-run; `xz -t` and `gtar tf` accepted it.
- Sorting by full path bytes orders `dir`, `dir-y`, `dir/x` (`-` is
  0x2d, `/` is 0x2f). Parents come before children, but the order is
  not GNU `--sort=name`, which sorts per directory.

### Per-file hash in the same pass

A `Read` wrapper handed to `append_data` computed each file's SHA-256 as
the builder consumed it, and a `Write` wrapper the stream's hash
(**measured**). No second read of the files.

### Gaps antiq fills itself

- **Hard links.** The builder does not detect them (issue
  <https://github.com/composefs/tar-rs/issues/119>, **reported**); each
  link is stored as a full file. A `(dev, ino)` map that emits
  `EntryType::Link` through `append_link` works; `gtar` lists the
  result as a hard link (**measured**). On APFS, `cp -R` turns hard
  links into separate files.
- **Xattrs.** The builder writes none; `com.apple.provenance` and
  `user.*` attributes are dropped (**measured**). Extracting xattrs is
  off by default (**read**, `src/archive.rs`). Storing them would mean
  pax `SCHILY.xattr.*` records through `append_pax_extensions`; not
  tested.
- **Directory mtimes.** Extraction sets mtimes on files and symlinks
  only (**read**, `src/entry.rs`; **measured**). Setting directory
  mtimes deepest first after the last member restored them.
- **Directory modes.** Modes are applied as stored, so a directory with
  mode `0644` loses its search bit and later members fail
  (**measured**). Owner `rwx` during extraction and the real mode in the
  final pass avoids it.
- **Hostile members.** Results for crafted archives, with `dst/` next
  to `outside/` (**measured**):

  | Member | `Archive::unpack` | `unpack_in` per entry |
  |---|---|---|
  | absolute `/tmp/x` | `/` stripped, written to `dst/tmp/x` | same |
  | `../escaped.txt` | skipped silently | returns `false` |
  | symlink `a -> ../outside`, then `a/pwn.txt` | error, symlink stays in `dst` | same |
  | symlink `a -> /etc` | created | same |
  | symlink `a -> ../outside`, then directory `a` | error, nothing outside | same |
  | hard link to `../outside/secret`, then file `h` | error, `outside/secret` unchanged | same |

  Nothing was written outside `dst`. Stripping and skipping are silent,
  so refusing an archive takes antiq's own check before `unpack_in`.
  Extraction has to stop at the first refused member: carrying on
  extracted the children of a refused symlink as plain directories
  (**measured**). The crate's guard compares canonicalised paths, which
  is check-then-use and holds in a staging directory no other process
  writes to (**read**, `src/entry.rs`, `validate_inside_dst`).

A probe extractor with these checks refused or stopped all six hostile
archives with nothing written outside the target, and round-tripped the
test tree with identical modes and mtimes for files, symlinks and
directories (**measured**).

### Strict verify

One pass over `entries()` and one over the disk, comparing type, mode
(`& 0o7777`), mtime, size, content SHA-256, symlink target bytes, hard
link target by `(dev, ino)`, and members missing on either side: 0
differences on a fresh tree; an edited, a deleted and an added file, a
changed mode and a repointed symlink were all reported (**measured**).

### Existing archives

All 313 `~/playground/archive/*.tar.xz` were read to the end through
`xz -dc` into a pipe, with 0 errors and 123,220 entries: 107,207
regular files, 14,456 directories, 1,148 hard links in 11 archives, 409
symlinks in 13 archives. All have GNU headers; 207 contain paths longer
than 100 bytes (GNU long names); none has pax records, non-UTF-8 paths
or `._` members. Each has one top-level folder named after the unit,
owner `jakob/staff` (**measured**).

### Other tools' output

- `bsdtar -cf` on macOS adds a `._name` AppleDouble member for every
  file with xattrs, plus `LIBARCHIVE.xattr.*` and `SCHILY.xattr.*` pax
  records; the crate lists the `._` files as ordinary members
  (**measured**).
- `gtar --format=pax` writes `atime` and `ctime` records for every entry
  (**measured**, listing). Whether they differ between runs was not
  tested.

### Names

- APFS refuses non-UTF-8 names (`open()` with bytes `\xff\xfe` fails
  with EILSEQ) (**measured**). On Unix the crate handles paths as bytes
  (**read**, `path2bytes`); not run on Linux.
- APFS here treats `café` in NFC and NFD as one name, and `README` and
  `Readme` as one (**measured**). An archive with both forms collides
  when extracted on macOS; a strict verify of the staged tree reports
  the missing member.

## Option B: async forks

- `astral-tokio-tar` 0.7.0 (2026-08-28; 35,730,419 downloads,
  16,716,835 recent; 46 reverse dependencies): adds an option that
  refuses symlinks pointing outside the target and one that skips
  permissions; same `HeaderMode` as `tar`. RustSec lists five
  advisories (RUSTSEC-2025-0110, 2026-0066, 2026-0112, 2026-0113,
  2026-0145), three of them PAX desynchronisation (**read**).
- `tokio-tar` 0.3.1 (2023-07-14): unmaintained, RUSTSEC-2025-0111
  unfixed. `async-tar` 0.6.1 (2026-06-01): no RustSec page.
- All need an async runtime; read, not run.

## Option C: libarchive through bindings

- `libarchive` 0.1.1 and `libarchive3-sys` 0.1.2: last released 2016.
- `libarchive2-sys` 0.2.0 (2025-10-16): 20,234 downloads.
- `compress-tools` 0.16.1 (2026-04-23, 193,967 downloads): reads and
  extracts through libarchive; no writer.
- libarchive (BSD-2-Clause) is what macOS `tar` uses; bsdtar 3.5.3 has
  no `--mtime` and no `--sort`.

## Option D: the `tar` executables

- GNU tar with `--sort=name --mtime=@N --owner=0 --group=0
  --numeric-owner --format=gnu` is deterministic across copies. It is
  not part of macOS; `/usr/bin/tar` is bsdtar, which needs a pre-sorted
  list through `-T` and cannot pin mtimes. Two flag sets.
- A subprocess cannot hash each file in the same pass; strict verify
  needs `tar -d` (GNU only, no content hash) or extraction.
- GNU tar is GPL; running it from an MPL-2.0 binary is not linking.

## Comparison

| Requirement | `tar` crate | `astral-tokio-tar` | libarchive crates | executables |
|---|---|---|---|---|
| R1 in-process writer | yes | yes, async | no writer | no |
| R2 deterministic | hand-built headers | same API | no pinning in bsdtar | GNU tar only |
| R3 hash in the same pass | yes | yes | no | no |
| R4 long names, links | long names yes; hard links own code | same | yes | yes |
| R5 xattrs | none written | same | possible | differs per tar |
| R6 strict verify | own code over `entries()` | same | own code | GNU `tar -d` |
| R7 safe extraction | guard plus own refusal and directory pass | external-symlink option | libarchive options | tar defaults |
| R8 old archives | 313 of 313 | not run | yes | yes |
| R9 licence | MIT/Apache | MIT/Apache | BSD-2 C | GPL executable |

| Option | Latest | First release | Downloads (recent) | Reverse deps | RustSec |
|---|---|---|---|---|---|
| `tar` 0.4.46 | 2026-05-18 | 2014-11-11 | 276M (72.4M) | 3,620 | 4, all fixed |
| `astral-tokio-tar` 0.7.0 | 2026-08-28 | 2025-02-05 | 35.7M (16.7M) | 46 | 5 |
| `tokio-tar` 0.3.1 | 2023-07-14 | 2020-01-09 | 19.6M (3.3M) | 28 | 1 unfixed |
| `async-tar` 0.6.1 | 2026-06-01 | 2019-12-24 | 9.2M (1.7M) | not fetched | none |
| `compress-tools` 0.16.1 | 2026-04-23 | 2019-04-25 | 194k (27k) | not fetched | none |

## Assessment

The `tar` crate fits: it is the only option that writes in-process,
read all 313 existing archives, and lets antiq hash every file in the
packing pass. Constraints from the measurements:

1. Hand-built GNU headers, not `HeaderMode::Deterministic`: mode
   `& 0o7777`, uid/gid 0, empty names, no pax records.
2. Real mtimes in the headers. Each entry's header carries its own
   mtime, so a file whose mtime did not change keeps its bytes; only
   touched or copied files change their header. A fixed mtime would
   make the archive unable to restore or verify mtimes, which the idle
   clock depends on.
3. Hard-link detection by `(dev, ino)` in antiq.
4. An extractor loop over `entries()` with antiq's own checks before
   `unpack_in`, stopping at the first refusal, owner `rwx` on
   directories until a final pass that sets directory modes and mtimes
   deepest first. `tar >= 0.4.45`.
5. No xattrs, matching the existing archives.
6. Special files (fifo, socket, device) skipped and reported.

## Not verified

- Non-UTF-8 names on Linux; extraction on a case-sensitive volume.
- Release-mode speed and the multi-threaded xz encoder.
- An xattr round trip through pax records.
- Files that change size while being read: `append_data` writes exactly
  the size in the header, so the packer has to handle a short or long
  read.
- Restoring symlink mtimes on Linux.
- The async forks, read only.

## Open points for the spec

- Whether a symlink with an absolute or upward target counts as a member
  that escapes the unit. The existing archives hold 409 symlinks; the
  crate refuses writes through them either way.
