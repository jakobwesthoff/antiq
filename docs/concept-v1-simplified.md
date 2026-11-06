# antiq v1, simplified: concept

Status: draft for review. It replaces the multi-machine design of
`docs/specs/spec-v1.md` once agreed; the spec and the ADRs are then
rewritten to match (ADRs stay rewritable until the first release).
Sections marked **Proposal** have not been confirmed one by one yet.

## Goal

antiq archives the folders of a collection that nobody has touched for
a while, finds them again by name or by the files they hold, brings
them back, and mirrors the whole collection to one backup target.

- All work happens on one machine. The target is a plain copy of the
  collection: copying it back restores everything, live folders and
  versions alike.
- Everything antiq writes can be understood and used without antiq:
  archive files are `tar.zst`, listings are checksum files that `shasum -c`
  reads, and names say what a file is.
- There is no multi-machine support, no version conflicts, no claims,
  no versioned metadata documents and no tiers. A second machine that
  needs a version copies its files from the target by hand. A later version
  may add more; the layout does not block that.

## Concepts

- **Collection:** a directory with a `.antiq/` folder.
- **Piece:** a folder of the collection that antiq manages, chosen by
  `patterns` and `exclude`. Its path relative to the collection is its
  identity: archiving the same path again adds a version of the same
  piece.
- **Live:** a piece that exists as a folder.
- **Version:** one archived state of a piece, made at one moment. A
  piece can have several versions side by side. User-facing text calls
  it a version throughout; "archive" is only the verb.
- **Archive file:** the `.tar.zst` of a version. The word appears only
  where the files on disk matter.
- **Pinned:** a live piece with a `.antiq-pin` file, never archived.
- **Target:** the backup location, a path (a mounted NAS) or
  `user@host:path` reached over ssh by rsync.

## Selection, walk and idle check

These rules carry over from the current spec and code unchanged, except
where noted.

- **Patterns:** glob patterns select the pieces, default `*`; `**` is
  allowed. When matches nest, the shallowest match is the piece.
- **Dot entries:** a folder whose name starts with a dot is a piece
  only when a pattern names that dot literally.
- **Exclude:** literal paths, not patterns, that are never pieces,
  together with everything below them; another spelling the volume
  treats as the same name is excluded as well.
- **Symlinks:** never pieces, and no walk follows a link. Links inside
  a piece are archived as links, and rsync copies them as links. A link
  on the target points to whatever its path names there; the user
  documentation says so.
- **Mount points:** a piece that contains another device is never
  archived; `status` names the mount point.
- **Nested collections:** a piece that contains a `.antiq/` directory
  is never archived; `status` names it.
- **Unreadable entries:** a piece with an entry the walk cannot read is
  never archived; `status` names the first such entry.
- **Ignored paths:** what the `ignore` lines of the config and the
  `.antiqignore` files match is not packed, not listed and does not
  count for the idle check; `archive` deletes it with the folder. The
  default is `ignore = [".DS_Store"]`; the separate junk list goes. A
  typical use is `node_modules/`, which can be fetched again.
- **Timeless paths:** what the `timeless` lines of the config and the
  `.antiq-timeless` files match is packed but does not count for the idle
  check, such as `.git/`, which changes on every `git fetch`. Default
  empty.
- **Rule syntax** for both kinds is gitignore's. Config lines are
  anchored at each piece's top, a file's lines at its directory. A
  pattern without a `/` except at its end matches at any depth below;
  a `/` at the start or in the middle anchors it, so `docs/*.log` in
  `a/.antiqignore` matches `a/docs/x.log` but not `a/b/docs/x.log`
  (that needs `**/docs/*.log`). The nearest file wins, then those
  further out, then the config. Rule files themselves are packed.
- **Future times:** a file time in the future counts like any other, so
  the piece is not due before that time has passed; `status` lists such
  files so the user can fix them. (The current spec ignores times more
  than a day ahead; that rule goes.)
- **Empty pieces:** a piece without a countable file is never archived.

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
- File names carry the normalized piece path and the time of archiving.
  Normalizing replaces every character outside `[A-Za-z0-9._]` with
  `-` and collapses repeated `-`. The time is the moment of archiving
  in UTC, written `YYYY-MM-DDTHHMMSS`. A second version of the same
  path within the same second gets a counter suffix (`…T101500-2`).
- `.tar.zst.sha256` holds the SHA-256 of the archive file in
  `shasum -c` format.
- `.sha256.zst` is the listing (below), zstd-compressed.
- There is no state file: everything antiq needs follows from the
  folders, the files of the versions, and the `.antiq-pin` and
  `.antiq-unearthed` files.

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
collection ID and the patterns given. It does not look for the layout
of `archive.sh`; its old archive files are brought in by hand with
`import`.
Afterwards it prints how many folders the next `archive` would archive,
if that takes no more than a few lines on top of the code `status` and
`archive -n` already run; otherwise it prints only that the collection
was created.

### `archive [<piece>]`

Without an argument, archives every live piece that is due. With a
piece, archives that one now, however recently it changed; a pinned
piece is refused until it is unpinned. **Proposal for the flow:**

1. The pieces are the folders matched by `patterns` (default `*`, each
   folder directly under the collection), minus `exclude`.
2. A piece is due when its newest mtime, not counting ignored and
   timeless paths, is older than `idle_days`. Pinned pieces are never due.
3. It is packed in `.antiq/tmp/`: the `.tar.zst`, the listing and the
   `.tar.zst.sha256`.
4. It is verified: the whole archive file is read back and compared with
   the listing and the folder. A folder that changes meanwhile is
   skipped.
5. The three files are moved into `.antiq/pieces/<path>/` and flushed.
6. Only then is the folder removed, through a trash inside `.antiq/`,
   so an interruption loses nothing.
7. One output line per folder, a summary at the end.

### `unearth <piece> [--version <time>]`

Brings a version of a piece back as a folder and **removes that
version** once the folder is complete and verified. The folder is an ordinary
live folder again. Archiving it later creates a new version.

- `<piece>` is a path, or a search word that matches exactly one
  piece; anything else is an error that lists the candidates. Without
  `--version` the newest version is taken; `--version` takes a
  timestamp or a unique prefix of one.
- There is no reclaim: an unearthed folder that is archived again,
  changed or not, becomes a new version.
- The unearthed files keep their old modification times. So that the
  folder is not due again at once, `unearth` writes `.antiq-unearthed`
  at its top, holding the time of the unearth. The idle clock of that
  folder starts at the later of this time and its newest file time.
  The file does not count as a change and is not packed into a later
  version.

### `fork <piece> [--version <time>] [--name <new path>]`

Extracts a working copy of a version, which stays unchanged. Without
`--name` the copy goes to the original path. Archived later, the copy
becomes a new version (same path) or a new piece (new path).

### `forget <piece> <version>`

Deletes exactly one version (its three files). Deleting every
version of a piece takes `--all-versions --force`. The next `push`
mirrors the deletion to the target.

### `import <folder> [--as <path>] [--time <time>]`

Packs a folder from outside the collection directly as a version of
`<path>` (default: the folder's name). The version time is
the newest mtime in the folder, or `--time`. The source folder is left
in place. This is also the way to bring in old archive files: unpack them
with their original timestamps somewhere temporary, then import each
folder.

### `list [query]`

One row per version, newest first within a piece; the query
matches piece paths fuzzily.

```text
$ antiq list scan
PIECE              VERSION              SIZE    FILES
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
13 folders · 214 versions (38.2 GB) · archive after 30 days idle

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

rsync runs with `--fsync`, so the target flushes each finished file.
Beyond that antiq relies on rsync's own check: rsync computes a
whole-file checksum of each transferred file while writing it and
compares it with the sender's. antiq reads nothing back. This needs
rsync 3.x; the `rsync` macOS ships is openrsync, which has no
`--fsync`. Nothing records when the last push ran.

`push` copies live folders whole, ignored paths such as
`node_modules/` included; the ignore rules apply to versions only.

`push` has no brake: whatever is deleted locally, by antiq or by hand,
is deleted on the target by the next push. The target is a mirror, not
a backup with history, and the user documentation says so. That is why
the command is called `push` and not `backup`.

`--if-reachable` makes `push` exit 0 when the target cannot be reached,
for scheduled runs. Scheduling runs the two commands one after the
other, such as `antiq archive && antiq push --if-reachable`; there is
no command that does both.

## Configuration

The configuration is per collection, in `.antiq/config.toml`.
**Proposal** for its keys:

```toml
[collection]
id = "01JBX…"                  # written at init, also in the target marker
patterns = ["*"]
exclude = []
ignore = [".DS_Store"]
timeless = []
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
- **Paused branches** (i31 version conflicts, i33 tiers, i42 decision
  fixes, i44 forget draft) are deleted unmerged. The parts of i42 that
  still apply (removing the unused error kind, the folder lookup fix)
  are carried over.

## Documentation after agreement

Once the concept is agreed, it becomes the new `docs/specs/spec-v1.md`
and this document goes. ADRs about parts that no longer exist are
deleted, the others are rewritten to the new model; numbers stay, gaps
are fine. The todos for removed parts are closed in the same step.
