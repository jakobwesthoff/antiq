# antiq v1, simplified: concept

Status: draft for review. It replaces the multi-machine design of
`docs/specs/spec-v1.md` once agreed; the spec and the ADRs are then
rewritten to match (ADRs stay rewritable until the first release).
Sections marked **Proposal** have not been confirmed one by one yet.
Where this concept says a rule of the current spec applies unchanged,
the spec section it names is part of the concept.

## Goal

antiq archives the folders of a collection that nobody has touched for
a while, finds them again by name or by the files they hold, brings
them back, and mirrors the whole collection to one backup target.

- All work happens on one machine. The target is a plain copy of the
  collection: copying it back restores everything, live folders and
  versions alike.
- Everything antiq writes can be understood and used without antiq:
  archive files are `tar.zst`, and a listing is a checksum file that
  `shasum -c` reads once `zstd` has decompressed it. Inside an unpacked
  folder: `zstd -dc <listing> | shasum -a 256 -c -`.
- There is no multi-machine support, no version conflicts, no claims,
  no versioned metadata documents and no tiers. A second machine that
  needs a version copies its files from the target by hand. A later
  version of antiq may add more; the layout does not block that.

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
- **Version stamp:** the UTC time of a version, written
  `YYYY-MM-DDTHHMMSS`, with a counter `-N` when the same piece has
  several versions within one second. Stamp and piece path together
  name a version exactly.
- **Archive file:** the `.tar.zst` of a version. The word appears only
  where the files on disk matter.
- **Pinned:** a live piece with a `.antiq-pin` file at its top, never
  archived.
- **Control files:** `.antiqignore` and `.antiq-timeless` (rule files,
  at any level), `.antiq-pin` and `.antiq-unearthed` (markers, only at
  a piece's top; elsewhere they are ordinary files).
- **Target:** the backup location, a path (a mounted disk or share) or
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
  treats as the same name is excluded as well. A piece that contains
  an existing exclude entry is never archived; `status` names the
  entry.
- **Symlinks:** never pieces, and no walk follows a link. Links inside
  a piece are archived as links, and rsync copies them as links. A link
  on the target points to whatever its path names there; the user
  documentation says so.
- **Mount points:** the rules of spec "Walks" apply unchanged: a piece
  that contains another device or a mount root (bind mounts, btrfs
  subvolumes, macOS firmlinks; read through `statx` on Linux and
  `f_mntonname` on macOS) is never archived; `status` names the mount
  point.
- **Nested collections:** a piece that contains a `.antiq/` directory
  is never archived; `status` names it.
- **Unreadable entries:** a piece with an entry the walk cannot read is
  never archived; `status` names the first such entry. The depth limit
  of 512 levels and the open-file rules of spec "Walks" apply.
- **Ignored paths:** what the `ignore` lines of the config and the
  `.antiqignore` files match is not packed, not listed and does not
  count for the idle check; `archive` deletes it with the folder. The
  default is `ignore = [".DS_Store"]`; the separate junk list goes. A
  typical use is `node_modules/`, which can be fetched again.
- **Timeless paths:** what the `timeless` lines of the config and the
  `.antiq-timeless` files match is packed but does not count for the
  idle check, such as `.git/`, which changes on every `git fetch`.
  Default empty.
- **Rule syntax** for both kinds is gitignore's. Config lines are
  anchored at each piece's top, a file's lines at its directory. A
  pattern without a `/` except at its end matches at any depth below;
  a `/` at the start or in the middle anchors it, so `docs/*.log` in
  `a/.antiqignore` matches `a/docs/x.log` but not `a/b/docs/x.log`
  (that needs `**/docs/*.log`). The nearest file wins, then those
  further out, then the config.
- **Ignored subtrees are still checked.** The walk before packing
  enters ignored paths too, without hashing them, for mount points,
  nested collections, unreadable entries, exclude entries and the depth
  limit, since `archive` deletes them with the folder. Only packing,
  the listing and the idle check leave them out.
- **Control files** never count as activity and never make a piece
  non-empty. The two rule files are packed; `.antiq-unearthed` is not
  packed and is deleted with the folder; `.antiq-pin` never reaches
  packing, since a pinned piece is not archived.
- **Idle clock:** the later of (a) the newest modification time of the
  regular files that are not ignored, not timeless and not control
  files, and (b) the time in `.antiq-unearthed`, if there is one.
  Directories, symlinks and special files do not count. A piece is due
  when its idle clock is older than `idle_days`; a day is 24 hours, and
  a piece exactly that old is not yet due.
- **Future times:** a file time or a marker time in the future counts
  like any other, so the piece is not due before that time has passed;
  `status` lists such files and markers so the user can fix them.
- **Empty pieces:** a piece without a countable file is never archived.
- **Spelling:** the folder under `.antiq/pieces/` holds the spelling of
  a piece's path. A live folder whose path the collection's volume
  treats as the same name (case, Unicode normalization, as probed per
  ADR 0054) is the same piece and is archived into that folder;
  `unearth` and `fork` extract to the stored spelling, `list` shows it,
  and `status` warns when a live spelling differs from the stored one.

## Store layout

```text
<collection>/
  projects/scanner/                     a live piece (a plain folder)
  .antiq/
    config.toml
    lock
    tmp/                                staging for packing and extracting
    trash/                              folders on their way out
    recovered/                          folders given back after a failed removal
    pieces/
      old/client-site/
        old-client-site--2026-09-03T101500.tar.zst
        old-client-site--2026-09-03T101500.tar.zst.sha256
        old-client-site--2026-09-03T101500.sha256.zst
```

- The folder under `.antiq/pieces/` is the piece's path, in its stored
  spelling. A folder there is a piece of the store when it holds at
  least one `.tar.zst`.
- File names are `<name>--<version stamp>` plus the suffix. `<name>` is
  the piece path with every character outside `[A-Za-z0-9._]` replaced
  by `-` and repeated `-` collapsed. It is for people only: nothing
  parses it, it need not be unique, and it is cut from the front at a
  `-` boundary (inside a single over-long component at a character
  boundary) so the longest file name stays within 200 bytes.
- Versions order by time, then by counter as a number; no counter
  counts as 1.
- `.tar.zst.sha256` holds the SHA-256 of the archive file in
  `shasum -c` format. `.sha256.zst` is the listing (below),
  zstd-compressed.
- **A version exists if and only if its `.tar.zst` exists.** The
  `.tar.zst` is moved in last and deleted first. A listing or
  `.tar.zst.sha256` without its `.tar.zst` is a leftover: read-only
  commands skip it, and the repairs of a locked run delete it. A
  `.tar.zst` without its listing or checksum file is a damaged version:
  it is reported by `status` and `list` and never deleted by antiq;
  `unearth` and `fork` can still extract it, with a warning and without
  the comparison against the listing.
- When the last version of a piece goes, its folder under `pieces/` and
  every parent that became empty go with it.
- There is no state file: everything antiq needs follows from the
  folders, the files of the versions, and the control files.

## Listing format

A listing is a SHA-256 checksum file with antiq's metadata in comment
lines. `shasum -a 256 -c` and GNU `sha256sum -c` both skip lines that
start with `#` (verified 2026-11-04, macOS shasum 6.04 and GNU coreutils
9.12, also with `--strict`). A comment after the path on the same line
does not work: both tools read it as part of the file name.

```text
# antiq listing 1
# piece    projects/scanner
# archived 2026-11-04T09:02:11Z
# archive  projects-scanner--2026-11-04T090211.tar.zst
# files    48211
# bytes    1288490188
#
# dir   0755  2026-10-30T17:40:00Z                 -  src/
# file  0644  2026-10-30T17:44:02Z              4211  src/main.rs
3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b  src/main.rs
# link  0755  2026-10-30T17:41:00Z                 -  src/latest	-> main.rs
```

- **Header:** `piece` (the stored spelling), `archived` (the version
  time, the one in the file name), `archive` (the archive file's name),
  `files` and `bytes` (count and total size of the regular files, so
  `list` and `status` read only the head of a listing). A version made
  by `import` adds `imported <time>`, the real time of the import.
- **Entry lines:** four fixed-width fields (type, octal mode, UTC time
  with `T`, the exact size in bytes or `-`), one space, then the path
  to the end of the line, starting at a fixed byte offset. A link line
  separates path and target with one tab: `<path>\t-> <target>`.
- **Types:** `dir` (the path ends in `/`), `file`, `link`, and `hard`
  for a hard link, written `<path>\t=> <target>`; the archive stores it
  as a tar link member. Special files are not archived and have no line
  (spec "Packaging", unchanged).
- **Checksum lines:** every `file` and `hard` line is followed by a
  standard checksum line for its path, so `shasum -c` checks hard links
  like any file.
- **Escaping in checksum lines:** a path containing a newline or a
  backslash gets a leading `\` on the line, with newline written `\n`
  and backslash `\\`; nothing else is escaped, a carriage return or tab
  stays raw. Both macOS `shasum` and GNU `sha256sum` read that (GNU's
  own `\r` escape is not used, since `shasum` fails on it).
- **Escaping in comment lines:** never a leading backslash, since a
  line that starts with `\` is no longer skipped as a comment. Inside
  paths and link targets, backslash, newline, tab and carriage return
  are written `\\`, `\n`, `\t` and `\r`.
- A test listing with such names passes `shasum -a 256 --strict -c` and
  `sha256sum --strict -c` with exit 0.

## Rules for every command

- Every command prints readable text on a terminal and has `--json`.
  Data goes to stdout, messages to stderr. The JSON envelope of the
  current code stays, with the plan's `actions` for dry runs; the
  brake's keys go.
- Every command that changes something has `-n`/`--dry-run`. A dry run
  takes no lock and runs no repairs.
- **Lock:** every command that changes the collection (`archive`,
  `unearth`, `fork`, `forget`, `import`, `pin`, `unpin`) and `push`,
  for its whole rsync run, takes the exclusive lock `.antiq/lock`. A
  held lock fails the command at once, naming the holder. `list`,
  `grep` and `status` take no lock and say when it is held. Without the
  lock on `push`, rsync could scan a piece halfway through an `unearth`
  and leave the target with neither the folder nor the version.
- **Exit codes:** 0 success; 1 error, a held lock included; 2 usage;
  3 partial (some pieces skipped); 4 target unreachable (only `push`,
  and 0 instead under `--if-reachable`).
- **Naming a version** (`unearth`, `fork`, `forget`): text output shows
  local time with seconds, `--json` and pipe output UTC (RFC 3339 in
  JSON, the version stamp in pipes). The input's spelling decides how
  it is matched, and the two are never mixed:
  - with a space (`2026-11-04 10:02:11`), as text shown in local time:
    each version's time is rendered in the machine's current zone and
    the input is prefix-matched against that text;
  - with a `T` (`2026-11-04T090211`, optional `-N`), against the
    version stamps.

  An exact match wins over prefix matches; several matches are an
  error naming each version's stamp. When two versions of one piece
  render alike in local time (the repeated hour when daylight saving
  time ends), text output adds the UTC offset.
- **Repairs:** every locked run that changes something first runs the
  repairs (see "Repairs at the start of a run").

## Commands

### `init`

Makes a directory a collection: writes `.antiq/config.toml` with a new
collection ID and the patterns given, without asking. It does not look
for the layout of `archive.sh`; its old archive files are brought in by
hand with `import`. It refuses, creating nothing and exiting 1, where
`.antiq/` already exists, inside another collection (naming its root),
where `.antiq` is a file or a symlink, and where a folder that would be
a piece holds a `.antiq/` of its own (spec "Collections and pieces",
unchanged).

Whether `init` afterwards prints how many folders the next `archive`
would archive is decided when the rework plan is written: yes if that
takes no more than a few lines on top of the code `status` and
`archive -n` already run, otherwise it prints only that the collection
was created. The spec then states the one behaviour chosen.

### `archive [<piece>]`

Without an argument, archives every live piece that is due. With a
piece, archives that one now, however recently it changed. `<piece>` is
a live piece's path as the patterns select it, or a search word that
matches exactly one live piece; anything else is refused. Only the idle
check is skipped: a pinned piece is refused until it is unpinned, and
the walk refusals and the empty rule still apply.

**Proposal for the flow:**

1. Find the pieces and judge each with the rules of "Selection, walk
   and idle check".
2. Pack the piece in `.antiq/tmp/`: the `.tar.zst`, the listing and the
   `.tar.zst.sha256`.
3. Verify: read the whole archive file back and compare it with the
   listing and the folder. A folder that changes meanwhile is skipped.
4. Move the listing, then the `.tar.zst.sha256`, then the `.tar.zst`
   into `.antiq/pieces/<path>/`, flushing each and the directory.
5. Rename the folder to `.antiq/trash/<version stamp>/<piece path>/`
   (one rename on the same device; a piece on another device counts as
   containing a mount point and never gets here).
6. Walk the trashed tree again and compare it with the record of the
   verify. On any difference rename it back, or to
   `.antiq/recovered/<version stamp>/<piece path>/` if its path is
   taken by then, and list it in `status`; the version stays. A write
   after this walk is lost, as in spec "archive".
7. Delete the trashed tree. A deletion that fails leaves it in the
   trash, where the repairs try again and report it.
8. One output line per folder, a summary at the end.

A crash after step 4 and before step 5 leaves the live folder next to a
complete version of it. The next run may archive the unchanged folder
again, which gives a second, identical version; `forget` removes it.

### `unearth <piece> [--version <time>]`

Brings a version of a piece back as a folder and **removes that
version** once the folder is complete and verified. The folder is an
ordinary live folder again. Archiving it later creates a new version.

- `<piece>` is a path, or a search word that matches exactly one
  piece; anything else is an error that lists the candidates. Without
  `--version` the newest version is taken.
- The target path is refused, and nothing changes, when it is not one
  the patterns select, is excluded, lies in `.antiq/`, inside another
  piece or below a file or symlink, or when anything is at it, a
  dangling symlink included (spec "unearth", unchanged). Member names
  are checked against the target volume as in spec "unearth" (ADR
  0054). Missing parents are created; a failure before the rename
  removes what was created.
- **Order:** extract into `.antiq/tmp/`; verify every entry against the
  listing; write `.antiq-unearthed` at the staged top; check the target
  path; rename into place; flush the parent; delete the version
  (`.tar.zst` first, then the listing and the checksum file, flushing
  between); rewrite `.antiq-unearthed` to the time alone.
- **`.antiq-unearthed`** holds one line,
  `unearthed <UTC time>[ version <stamp>]`, and is read only as a
  regular file at the piece's top. The idle clock starts at its time.
  The version stamp is there only between the rename and the deletion
  of the version: if a crash falls in between, the repairs finish the
  unearth (see there). A marker that cannot be read or parsed fails the
  idle check of that piece with the file named, so the piece is never
  called due by mistake.
- There is no reclaim: an unearthed folder that is archived again,
  changed or not, becomes a new version.

### `fork <piece> [--version <time>] [--name <new path>]`

Extracts a working copy of a version, which stays unchanged. Without
`--name` the copy goes to the original path. The same target rules and
staging as for `unearth` apply; when the original path is taken, the
refusal names `--name` as the way out. `fork` writes `.antiq-unearthed`
with the time alone (no version stamp, so the repairs never treat it as
an unearth). Archived later, the copy becomes a new version (same path)
or a new piece (new path); without edits that is an identical version,
once due or at once with `archive <piece>`.

### `forget <piece> <version>`

Deletes exactly one version: the `.tar.zst` first, then the listing and
the checksum file. Deleting every version of a piece takes
`--all-versions --force`. The next `push` mirrors the deletion to the
target.

### `import <folder> [--as <path>] [--time <time>]`

Packs a folder from outside the collection directly as a version of
`<path>` (default: the folder's name), with steps 1 to 4 of `archive`;
the source folder is left in place.

- The source must not lie inside the collection.
- `<path>` must be one the patterns select, not excluded, not in
  `.antiq/` and not inside another piece.
- The ignore and timeless rules of the collection and the folder's own
  rule files apply, and so do the walk refusals.
- The version time is the newest counted modification time in the
  folder, or `--time`. A version time in the future is refused, naming
  the file.
- This is also the way to bring in old archive files: unpack them with
  their original timestamps somewhere temporary, then import each
  folder.

### `list [query]`

One row per version, newest first within a piece; the query matches
piece paths fuzzily. Each row reads only the head of its listing.
SIZE is the archive file's size on disk; `--json` also carries the
unpacked size and the file count. Damaged versions are shown as such.

```text
$ antiq list scan
PIECE              VERSION                SIZE    FILES
projects/scanner   2026-11-04 10:02:11    1.2 GB  48 211
projects/scanner   2026-09-03 12:15:00    1.1 GB  47 980
tools/scan-old     2025-02-11 19:40:07   40 MB      812
```

**Open:** the user decides the exact layout from rendered examples
before `list` is built.

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

4 files in 2 pieces · antiq unearth <piece> [--version <time>]
```

A match wholly inside the piece path gives one row, "piece name
matches", with the version and file counts and no file list. In a pipe
every hit and version is one tab-separated line `piece  stamp  path`,
with the full version stamp (`-` as the path for a name match).

### `status`

Local only. One row per piece that has a state, sorted by path, the
state coloured in the first column: `due` (archived on the next run),
`soon` (due within `soon_days`, default 3), `pinned`, and the pieces
`archive` refuses with their reason (mount point, nested collection,
unreadable entry, exclude entry, unreadable marker). Plain live folders
are only counted in the header. Below the table: files and markers with
times in the future, damaged versions, folders in `recovered/` or left
in the trash, and live folders whose spelling differs from the stored
one.

```text
13 folders · 214 versions (38.2 GB) · archive after 30 days idle

STATE  PIECE                                 IDLE  WHAT
due    experiments/rust-gpu                   33d  archives next run
due    old/client-site                       141d  archives next run
pinned projects/antiq                        120d  kept by .antiq-pin
soon   tools/scan-old                         28d  archives in 2 days
```

### `pin <piece>` / `unpin <piece>`

Create or remove `.antiq-pin` at the top of a live piece.

### `push`

Mirrors the collection to the target. **Proposal for the flow:**

1. **Check rsync.** The local rsync's `--version` must name rsync 3.2.4
   or newer, the release that added `--fsync` (rsync NEWS.md, section
   3.2.4). openrsync, which macOS ships as `/usr/bin/rsync`, and older
   versions are refused with a message naming the binary and the
   config key `[target] rsync`.
2. **Pre-check the target.** For a path target with the file system,
   for an ssh target with `rsync --list-only <target>/` and its
   message (exit code 23 alone does not tell a missing file from a
   refused permission, so it is never read as "missing"):

   | Situation | Result |
   |---|---|
   | ssh cannot connect (rsync exit 255 with a connect, resolve or timeout message), or a path target answers ENOTCONN, EHOSTDOWN, EHOSTUNREACH, ENETUNREACH or ETIMEDOUT | unreachable |
   | path target directory does not exist | unreachable |
   | ssh target directory does not exist | exit 1, unless `--init` |
   | directory present, no `.antiq-target`, empty | unreachable with a warning naming `push --init`, unless `--init` |
   | directory present, no `.antiq-target`, not empty | exit 1; `--init` is refused |
   | `.antiq-target` of another collection, or not readable | exit 1 |
   | `.antiq-target` of this collection, another host | warning; the host is rewritten after the push |

   Unreachable means exit 0 under `--if-reachable`, otherwise exit 4,
   and the reason is printed in text and `--json` every time. A missing
   directory differs by kind of target because an unmounted share on
   macOS takes its `/Volumes/<name>` directory with it, while an ssh
   host that answers but lacks the directory is misconfigured.
3. **Marker.** `.antiq-target` sits at the target root, outside the
   collection, and holds the collection ID and the host name. It is
   created only by `push --init`, which is refused together with
   `--if-reachable`; `--init` creates a missing target directory when
   its parent exists. `--init` is the user's statement that this empty
   target is the right one; antiq cannot tell an unmounted mount point
   from a real empty directory. The marker is written from a local temp
   file with rsync; `push -n` never writes it.
4. **Mirror.** One rsync run:

   ```text
   rsync -a -x --delete --delete-delay
         --exclude=/.antiq-target --exclude=/.antiq/tmp/
         --exclude=/.antiq/trash/ --exclude=/.antiq/lock
         <collection>/ <target>/
   ```

   with `--fsync` unless `[target] fsync = false`, `--rsync-path` from
   `[target] rsync_path`, the extra options of `[target] rsync_args`,
   and `-e "ssh -o BatchMode=yes"` so a scheduled run never waits for a
   prompt. The excludes are anchored at the collection root, so the
   same names inside a live piece are still copied; the exclude of the
   marker also protects the target's marker from `--delete`. `-x` keeps
   rsync on the collection's device: a mount point inside a live piece
   arrives as an empty directory. With `--delete-delay`, deletions
   happen only at the end, so an interrupted run removes nothing that
   was on the target before.
5. **Second pass.** After a run that deleted anything, `push` runs the
   same command with `-n` and, if it lists transfers, the mirror once
   more. On a case-insensitive target a folder renamed only in case
   (`Foo` to `foo`) is otherwise deleted at the end of the first run
   and copied again only by the next push.
6. **Result.** rsync exit 0 is success; 24 (files vanished during the
   run) is success with a note naming them. Everything else, including
   any failure after the pre-check, is exit 1 with rsync's message, also
   under `--if-reachable`. A receiver that rejects `--fsync` is reported
   as "the target's rsync is too old for --fsync" with a pointer to
   `fsync = false` and `rsync_path`.
7. `-n` runs the checks and summarizes rsync's `--itemize-changes` per
   piece.

With `--fsync` the target flushes each finished file; without it, a
power loss on the target can lose files rsync reported as written.
Beyond that antiq relies on rsync's own check: rsync computes a
whole-file checksum of each file it transfers while writing it and
compares it with the sender's. Files rsync does not transfer, because
size and modification time match, are not compared, so damage on the
target stays unseen until a full comparison (todo
01M6K6WN14S91E2X5TRQYZRHJ5). antiq reads nothing back. Nothing records
when the last push ran.

`push` copies live folders whole, ignored paths such as
`node_modules/` included; the ignore rules apply to versions only.

`push` has no brake: whatever is deleted locally, by antiq or by hand,
is deleted on the target by the next push. The target is a mirror, not
a backup with history, and the user documentation says so. That is why
the command is called `push` and not `backup`.

`--if-reachable` makes `push` exit 0 when the target is unreachable as
defined above, for scheduled runs. Scheduling runs the two commands one
after the other, such as `antiq archive && antiq push --if-reachable`;
there is no command that does both. A held lock is exit 1, also under
`--if-reachable`.

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
url = "nas.local:/volume1/antiq/projects"   # a path or user@host:path
rsync = "/opt/homebrew/bin/rsync"   # optional, else from PATH
rsync_path = "/usr/local/bin/rsync" # optional, the target's rsync (--rsync-path)
fsync = true                        # false when the target's rsync lacks --fsync
rsync_args = []                     # extra rsync options, e.g. for SMB shares
```

The keys of the current config that go: `junk`, `remote_only_days`,
`backup_live`, `live_exclude`, `replace_without_asking`,
`brake_percent`, `grace_hours`, `away_warn_days`, `memory_percent`,
`remote_jobs`, `live_retries`, the `[machine]` table and the
`[[remote]]` list. A config that still has one of them is refused with
the keys named; old stores start fresh anyway.

## Repairs at the start of a run

There is no `reconcile` command. Every locked run that changes
something first:

- deletes what `.antiq/tmp/` holds;
- deletes a listing or `.tar.zst.sha256` in `.antiq/pieces/` whose
  `.tar.zst` is missing, and folders there that became empty;
- for each tree in `.antiq/trash/<stamp>/`: finds its piece path as the
  shortest path below `<stamp>/` for which a version with that stamp
  exists; deletes the tree when that version exists and the tree holds
  nothing its listing lacks (an interrupted deletion only removes
  paths); otherwise renames it back to its path, or to
  `.antiq/recovered/` when the path is taken, and reports it;
- for each live piece whose `.antiq-unearthed` names a version stamp of
  a version of that piece that still exists: deletes that version (an
  unearth that died after its rename) and rewrites the marker to the
  time alone.

The repairs name every version and tree they delete.

## Existing stores

Stores in the old layout (`.antiq/objects/`) are not converted; they
exist only in test collections, since v1 is unreleased.

## What remains from the built code

The modules below exist on main today (`src/`). How each fares,
**as a proposal for the rework plan**:

| Module | Fate | Note |
|---|---|---|
| `select.rs`, `walk.rs`, `idle.rs`, `tree.rs` | kept, adapted | ignore and timeless rules, marker time, control files |
| `pack/` | kept, adapted | without `pack/live.rs`, `pack/manifest.rs` and the live-package constants in `pack/zstd.rs`; listing gains `hard` lines |
| `listing.rs` | rewritten | new format; its file-name helpers move out of `meta.rs` |
| `fsx/` | kept | |
| `lock.rs`, `run.rs` | kept | `run.rs` loses the parts tied to tracking and metadata |
| `pin.rs`, `reference.rs`, `relpath.rs`, `id.rs`, `clock.rs`, `sysmem.rs`, `interact.rs`, `error.rs`, `report.rs`, `plan.rs` | kept, adapted | `plan.rs` loses the brake; `error.rs` its remote and conflict kinds |
| `collection.rs` | adapted | new local layout |
| `config.rs` | rewritten | keys above |
| `machine.rs` | kept | host name for the target marker |
| `repair.rs` | rewritten | the repairs above only |
| `remote/rsync.rs`, `remote/marker.rs`, `remote/url.rs` | kept, adapted | base of `push`, its marker and the target URL |
| `cli/` (without `cli/picker/`), `main.rs`, `lib.rs` | adapted | new command set |
| `ops/archive/`, `ops/unearth/`, `ops/fork.rs`, `ops/import.rs`, `ops/status.rs`, `ops/init.rs`, `ops/pin.rs` | rewritten | new store and semantics; `ops/archive/unearthed.rs` (reclaim) and `ops/unearth/live.rs`, `ops/unearth/remote.rs` go |
| `list`, `grep`, `forget`, `push` | new | |
| `meta.rs`, `store.rs`, `tracking.rs`, `index.rs`, `tombstone.rs`, `brake.rs` | removed | versioned metadata, object store, tracking state, member index |
| `remote/` other than the three above (`commit.rs`, `ssh/`, `path.rs`, `policy.rs`, `away.rs`, `cleanup.rs`, `testing.rs`; `mod.rs` shrinks) | removed | rsync replaces the own remote layer |
| `ops/sync/`, `ops/reconcile/`, `ops/resolve/`, `ops/remote.rs`, `ops/find.rs` and their `cli/cmd/` files, `cli/picker/` | removed | |

The paused branches (i31 version conflicts, i33 tiers, i42 decision
fixes, i44 forget draft) are deleted unmerged and saved as git bundles
outside the repository; the rework plan names where. The parts of i42
that still apply (removing the unused error kind, the folder lookup
fix) are carried over from there.

## Documentation after agreement

Once the concept is agreed, it becomes the new `docs/specs/spec-v1.md`
and this document goes. ADRs about parts that no longer exist are
deleted, the others are rewritten to the new model; numbers stay, gaps
are fine. The todos for removed parts are closed in the same step.

The user documentation, once written, must say:

- that the target is a mirror, not a backup, and what that means for
  deletions;
- that symlinks on the target point to whatever their paths name
  there;
- how to check a version by hand (`zstd -dc <listing> | shasum -a 256
  -c -`);
- that rsync compares only what it transfers;
- what `fsync = false` costs, and which `rsync_args` an SMB or exFAT
  target needs, and that ssh is the recommended way to reach a NAS;
- that a folder renamed only in case is copied in a second pass on a
  case-insensitive target;
- what `push --init` does and when to use it.

## Open questions

1. `fork` writes `.antiq-unearthed` too. Does the marker keep that name,
   or get one that fits both commands?
2. The exact layout of `list` (and of `grep` and `status`), decided from
   rendered examples.
