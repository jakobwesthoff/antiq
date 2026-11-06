# File names between Linux and macOS

Researched 2026-11-03. Linux side: a `python:3-slim` container (Debian,
GNU tar 1.35, overlay filesystem) under OrbStack. macOS side: APFS,
case-insensitive, bsdtar 3.5.3 and the `tar` crate 0.4.46 extracting
with `unpack_in` and `Archive::unpack`.

Marks: **measured** (run), **read** (documentation, named),
**inferred** (follows from measured facts, not run as such).

Several machines share one remote (ADR 0017), so a piece archived on
Linux can be unearthed on macOS and the reverse. Names are stored byte
for byte (ADR 0050).

## Linux archive, unearthed on macOS

| Case | bsdtar | `tar` crate |
|---|---|---|
| non-UTF-8 name | "Illegal byte sequence", member skipped, others extracted, exit 1 | `Err` (os error 92); `unpack` stops there, later members missing |
| `README` and `Readme` | one file `Readme` with the later member's content, exit 0, no message | `Ok(true)` for both, one file |
| `café` in NFC and in NFD | one file, the later member's content, exit 0, no message | `Ok(true)` for both, one file |
| `:` in a name; `CON`, trailing space, backslash, control byte | created | created |
| 255-byte name | created | created |
| path of 1,300 bytes | extracted completely | 44 members fail past macOS's `PATH_MAX` of 1,024 |

(**measured**). Non-UTF-8 names fail loudly. Both collisions are
silent: the later member in the archive replaces the earlier one under
one name. `unpack_in` returns `Ok(true)` for such an overwrite.

## macOS archive, unearthed on Linux

- NFD names arrive with their bytes unchanged (**measured**).
- macOS limits a name to 255 UTF-16 code units, Linux to 255 bytes: a
  name of 255 two-byte characters (510 bytes) exists on APFS, while
  names of 256 and 300 bytes failed in the container with "File name too
  long", GNU tar exit 2 (**measured**). On APFS, 127 emoji (254 units)
  create and 128 (256 units) fail, so code points are not the unit
  (**measured**, 2026-11-04).
- Case collisions and non-UTF-8 names cannot originate on macOS.

## How often

| Count | `~/playground` live | 313 archive listings |
|---|---|---|
| names | 57,062 | 123,220 |
| non-ASCII | 0 | 251, in 5 archives |
| non-UTF-8 | 0 | 0 |
| NFD | 0 | 0 |
| case or NFC/NFD collisions in one directory | 0 | 0 |
| names with `:` | 12 | 50 |

(**measured**, read-only). All 313 archives come from macOS, where
collisions cannot occur; no Linux-made tree exists yet.

## Options

- **Strict verify only** (ADR 0033, ADR 0034): the staged tree holds
  one name where the archive holds two, so the path-set comparison
  fails, provided paths are read back from the staged tree as bytes
  (**inferred**, not run). It reports "path sets differ", without
  saying which names collide, and only after the full extraction.
- **Scan the member list before extracting**, with the rules of the
  unearthing machine: invalid names, name and path length limits, and
  collisions after NFD and case folding per directory. It refuses with
  a list before anything is written. The case and Unicode rules can
  come from a probe of the staging volume instead of a table; the probe
  was not built.
- **Warn at archive time**, using the same check with portable rules,
  while the folder still exists. Refusing would block archiving for
  names that only matter on another system.
- **Extract with escaped names and a mapping file**: changes what an
  unearth restores and needs a mapping-aware verify.
- **Normalise names at archive time**: contradicts ADR 0050 and
  ADR 0033.

Other tools (**read**): git sets `core.precomposeUnicode` and
`core.ignoreCase` per repository after probing the volume; rsync's
`--iconv` converts character sets only; GNU tar's `--transform` renames
by expression. The restic and borg pages read say nothing about
collisions on restore.

## Assessment

The silent collisions are the risk; non-UTF-8 names fail loudly. A scan
of the member list before extracting turns them into a refusal with a
named list and also covers the length limits. The strict verify stays
as the backstop. An archive-time warning is cheap with the same check.
Escaped extraction is worth building only once a real case appears.

## What the probe found on other volumes

antiq's probe (`TargetRules::probe`) creates names in a directory of its
own on the staging volume and reads the answers. Disk images made with
`hdiutil` (macOS, 2026-11-04) showed:

- exFAT answers a name that is not UTF-8 with `EINVAL`, where APFS
  answers `EILSEQ` (**measured**). The probe takes both as "the volume
  refuses such names".
- On APFS a symlink target of 1023 bytes is created and one of 1024
  fails with errno 63, "File name too long" (**measured**, 2026-11-04).
  The limit for Linux, 4095, is the path limit of 4096 less one; it was
  not run on Linux.
- On exFAT and on FAT32 (`-fs MS-DOS`) a hard link fails with errno 45,
  "Operation not supported", and a symlink is created (**measured**).
  The probe tries both calls once and takes only `EPERM`, `ENOTSUP` and
  `EOPNOTSUPP` as "not supported".
- HFS+ stores a precomposed name decomposed, so a directory listing
  returns other bytes than the ones written, and the 255-unit limit
  applies to the decomposed form: 127 times `é` creates and 128 times
  fails with errno 63, "File name too long" (**measured**). The probe
  creates a precomposed name, lists the directory and compares the bytes.

## Not verified

- The strict verify on the collision trees.
- An `lstat` check before each `unpack_in`.
- A case-sensitive APFS volume; ext4 itself (the container used
  overlayfs).
- How restic and borg restore colliding names.
