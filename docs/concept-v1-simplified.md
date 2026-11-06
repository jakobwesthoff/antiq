# antiq v1, simplified: concept

Status: draft for review. It replaces the multi-machine design of
`docs/specs/spec-v1.md` once agreed; the spec and the ADRs are then
rewritten to match (ADRs stay rewritable until the first release).
Sections marked **Proposal** are not decided yet; the open questions
are collected at the end.

## Goal

antiq archives the folders of a collection that nobody has touched for
a while, finds them again by name or by the files they hold, brings
them back, and mirrors the whole collection to one backup target.

- All work happens on one machine. The target is a plain copy of the
  collection: copying it back restores everything, folders and
  archives alike.
- Everything antiq writes can be understood and used without antiq:
  archives are `tar.zst`, listings are checksum files that `shasum -c`
  reads, and names say what a file is.
- There is no multi-machine support, no version conflicts, no claims,
  no versioned metadata documents and no tiers. A second machine that
  needs an archive copies it from the target by hand. A later version
  may add more; the layout does not block that.

## Concepts

- **Collection:** a directory with a `.antiq/` folder.
- **Piece:** a folder of the collection that antiq manages, chosen by
  `patterns` and `exclude`. Its path relative to the collection is its
  identity: archiving the same path again adds a version of the same
  piece.
- **Live:** a piece that exists as a folder.
- **Archived version:** one archive of a piece, made at one moment.
  A piece can have several versions side by side.
- **Pinned:** a live piece with a `.antiq-pin` file, never archived.
- **Target:** the backup location, a path (a mounted NAS) or
  `user@host:path` reached over ssh by rsync.

## Store layout

```text
<collection>/
  projects/scanner/                     a live piece (a plain folder)
  .antiq/
    config.toml
    pieces/
      old/client-site/
        old-client-site--2026-09-03T101500.tar.zst
        old-client-site--2026-09-03T101500.tar.zst.sha256
        old-client-site--2026-09-03T101500.sha256.zst
```

- The folder under `.antiq/pieces/` is the piece's path.
- File names carry the normalized piece path and the archive time.
  Normalizing replaces every character outside `[A-Za-z0-9._]` with
  `-` and collapses repeated `-`. The time is the moment of archiving
  in UTC, written `YYYY-MM-DDTHHMMSS`. A second archive of the same
  path within the same second gets a counter suffix (`…T101500-2`).
- `.tar.zst.sha256` holds the SHA-256 of the archive file in
  `shasum -c` format.
- `.sha256.zst` is the listing (below), zstd-compressed.
- There is no state file: everything antiq needs follows from the
  folders, the archive files and the `.antiq-pin` files.

## Listing format

A listing is a SHA-256 checksum file with antiq's metadata in comment
lines. `shasum -a 256 -c` and GNU `sha256sum -c` both skip lines that
start with `#` (verified 2026-11-04, macOS shasum and GNU coreutils
9.12, also with `--strict`). A comment after the path on the same line
does not work: both tools read it as part of the file name.

```text
# antiq listing 1
# piece    projects/scanner
# archived 2026-11-04T09:02:11Z
# archive  projects-scanner--2026-11-04T090211.tar.zst
#
# dir   0755  2026-10-30T17:40:00Z                 -  src/
# file  0644  2026-10-30T17:44:02Z              4211  src/main.rs
3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b  src/main.rs
# link  0755  2026-10-30T17:41:00Z                 -  src/latest -> main.rs
```

- One aligned comment line per entry: type, octal mode, UTC time with
  `T`, the exact size in bytes (or `-`), and the path last. No field
  contains a space, so a parser splits on runs of spaces; the widths
  are fixed and generous so the columns line up.
- A regular file's comment line is followed by its standard checksum
  line. Directories end in `/`; links show `-> target`.
- Paths with characters that the checksum format cannot carry are
  escaped as GNU `sha256sum` does, in the checksum lines and in the
  comment lines alike.

## Commands

Every command prints readable text on a terminal and has `--json`.
Every command that changes something has `-n`/`--dry-run`. Only `push`
talks to the target.

### `init`

Makes a directory a collection: writes `.antiq/config.toml` with a new
collection ID and the patterns given.

### `archive`

Archives every live piece that is due. **Proposal for the flow:**

1. The pieces are the folders matched by `patterns` (default `*`, each
   folder directly under the collection), minus `exclude`.
2. A piece is due when its newest mtime, not counting files matched by
   the `ignore` lines (gitignore syntax, for example `node_modules/`),
   is older than `idle_days`. Pinned pieces are never due.
3. It is packed in `.antiq/tmp/`: the `.tar.zst`, the listing and the
   `.tar.zst.sha256`.
4. It is verified: the whole archive is read back and compared with
   the listing and the folder. A folder that changes meanwhile is
   skipped.
5. The three files are moved into `.antiq/pieces/<path>/` and flushed.
6. Only then is the folder removed, through a trash inside `.antiq/`,
   so an interruption loses nothing.
7. One output line per folder, a summary at the end.

### `unearth <piece> [--version <time>]`

Brings an archived piece back as a folder and **removes that archive**
once the folder is complete and verified. The folder is an ordinary
live folder again. Archiving it later creates a new version.

- `<piece>` is a path, or a search word that matches exactly one
  piece; anything else is an error that lists the candidates. Without
  `--version` the newest version is taken; `--version` takes a
  timestamp or a unique prefix of one.
- There is no reclaim: an unearthed folder that is archived again,
  changed or not, becomes a new version.

### `fork <piece> [--version <time>] [--name <new path>]`

Extracts a working copy while the archive stays unchanged. Without
`--name` the copy goes to the original path. Archived later, the copy
becomes a new version (same path) or a new piece (new path).

### `forget <piece> <version>`

Deletes exactly one archived version (its three files). Deleting every
version of a piece takes `--all-versions --force`. The next `push`
mirrors the deletion to the target.

### `import <folder> [--as <path>] [--time <time>]`

Packs a folder from outside the collection directly as an archived
version of `<path>` (default: the folder's name). The version time is
the newest mtime in the folder, or `--time`. The source folder is left
in place. This is also the way to bring in old archives: unpack them
with their original timestamps somewhere temporary, then import each
folder.

### `list [query]`

One row per archived version, newest first within a piece; the query
matches piece paths fuzzily.

```text
$ antiq list scan
PATH               ARCHIVED             SIZE    FILES
projects/scanner   2026-11-04 09:02     1.2 GB  48 211
projects/scanner   2026-09-03 10:15     1.1 GB  47 980
tools/scan-old     2025-02-11 18:40    40 MB      812
```

### `grep <pattern>`

Searches the paths of every archived file, as `<piece>/<path>`, in all
listings, with a regular expression (`regex::bytes` over each decoded
listing, the files in parallel on all cores but two). Measured in
`docs/research/listing-grep.md`: about 0.2-0.26 s for 300 versions of
50 000 entries with compressed listings, warm cache, 14 threads.

On a terminal the hits are grouped by piece, each in-piece path once
with the versions that hold it, the match highlighted:

```text
projects/scanner                       3 versions, newest 2026-11-04
  src/main.rs                          all 3 versions
  tools/bench/main.rs                  2026-09-03, 2026-05-14
tools/scan-old                         1 version, 2025-02-11
  cli/src/main.rs                      2025-02-11

4 files in 2 pieces · antiq unearth <piece> [--version <date>]
```

A match wholly inside the piece path gives one row, "piece name
matches", with the version and file counts and no file list. In a pipe
every hit and version is one tab-separated line `piece  version  path`
(`-` as the path for a name match).

### `status`

Local only. One row per piece that has a state, sorted by path, the
state coloured in the first column: `due` (archived on the next run),
`soon` (due within `soon_days`, default 3), `pinned`. Plain live
folders are only counted in the header.

```text
13 folders · 214 archived (38.2 GB) · archive after 30 days idle

STATE  PIECE                                 IDLE  WHAT
due    experiments/rust-gpu                   33d  archives next run
due    old/client-site                       141d  archives next run
pinned projects/antiq                        120d  kept by .antiq-pin
soon   tools/scan-old                         28d  archives in 2 days
```

### `pin <piece>` / `unpin <piece>`

Create or remove `.antiq-pin` in a live piece.

### `push`

Mirrors the collection to the target. **Proposal for the flow:**

1. Check the marker on the target. A marker of another collection
   aborts, another host warns, a missing marker on an empty target is
   created, a missing marker on a non-empty target aborts.
2. Run one `rsync -a --delete --delete-delay <collection>/ <target>/`,
   leaving out `.antiq/tmp/`, the trash and the lock. With
   `--delete-delay`, deletions happen only at the end, so an
   interrupted run removes nothing that was on the target before.
3. `-n` summarizes rsync's `--itemize-changes` per piece.

The name is `push` and not `backup`: the target is an exact mirror, so
what is deleted locally is deleted there too.

## Configuration

The configuration is per collection, in `.antiq/config.toml`.
**Proposal** for its keys:

```toml
[collection]
id = "01JBX…"                  # written at init, also in the target marker
patterns = ["*"]
exclude = []
ignore = ["node_modules/", "target/"]
idle_days = 30
soon_days = 3

[target]
url = "nas.local:/volume1/antiq/projects"
rsync = "/opt/homebrew/bin/rsync"   # optional, else from PATH
```

## Repairs at the start of a run

There is no `reconcile` command. Every run that changes something first
cleans up what an interrupted `archive` or `unearth` left behind:
temporary files in `.antiq/tmp/` and folders in the trash.

## Existing stores

Stores in the old layout (`.antiq/objects/`) are not converted; they
exist only in test collections, since v1 is unreleased.

## What remains from the built code

The modules named below exist on main today (`src/`). How each fares,
**as a proposal for the rework plan**:

- **Kept, adapted to the new store:**
  - the selection and idle check (`select.rs`, `idle.rs`, `walk.rs`);
  - packing and verifying (`pack/`);
  - the lock and the run guard (`lock.rs`, `run.rs`);
  - pins (`pin.rs`);
  - the listing writer and reader (`listing.rs`, new format);
  - configuration (`config.rs`, fewer keys);
  - the CLI framework and its JSON output (`cli/`, `report.rs`);
  - the filesystem layer (`fsx/`).
- **Rewritten:**
  - `archive`, `unearth`, `fork`, `import` (moved to the new store and
    semantics);
  - `status` (new layout).
- **New:**
  - `list`, `grep`, `forget`, `push` (rsync mirror and marker).
- **Removed:**
  - the versioned metadata and commit protocol (`meta.rs`, the claim
    parts of `store.rs`, `remote/commit`), `sync` and its phases (live
    packages, tiers), `resolve` and the conflict and divergence
    handling, the tombstone, prune and the brake as far as only they
    use it, the remote abstraction and the own ssh layer as far as rsync
    replaces them, the picker (`cli/picker/`), and `find` with its
    member index.
- **Paused branches** (i31 version conflicts, i33 tiers, i44 forget
  draft) are not merged. The parts of i42 that still apply (removing
  the unused error kind, the folder lookup fix) are carried over.

## Open questions

1. How `push` makes sure the archives arrived intact: whether rsync's
   own whole-file check (with `--fsync`) is enough, or whether antiq
   reads anything back.
2. How an unearthed folder keeps from being archived again on the next
   run, since its files keep their old mtimes and there is no state
   file to hold the time of the unearth.
3. Whether `push` stops before deleting much on the target.
4. Whether anything warns when the last push is long ago, and where its
   time is kept without a state file.
5. How scheduled runs work: `archive` and `push` separately, or one
   command.
6. Which detail rules of the walk and the idle check stay
   (`.antiqignore` files, the junk list, future mtimes, empty pieces,
   mount points, symlinks, dot entries).
