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
  collection: copying it back restores the live folders and the
  versions. Not kept, neither in versions nor on the target: extended
  attributes, ACLs, access and creation times. Hard links are kept
  inside versions; in the mirror of live folders they arrive as
  separate files.
- Everything antiq writes can be understood and used without antiq:
  archive files are `tar.zst`, and a listing is a checksum list that
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
  `YYYY-MM-DDTHHMMSS`, with a counter `-2`, `-3`, ... when the same
  piece has several versions within one second (`-1` is not a valid
  spelling). Stamp and piece path together name a version exactly.
- **Archive file:** the `.tar.zst` of a version. The word appears only
  where the files on disk matter.
- **Listing:** the `.sha256.zst` of a version (see "Listing format").
- **Checksum file:** the `.tar.zst.sha256` of a version.
- **Sidecars:** the listing and the checksum file together.
- **Pinned:** a live piece with a `.antiq-pin` file at its top, never
  archived.
- **Control files:** `.antiqignore` and `.antiq-timeless` (rule files,
  at any level), `.antiq-pin` and `.antiq-restored` (markers, only at
  a piece's top; elsewhere they are ordinary files, packed and counted
  like any other).
- **Target:** the backup location, an absolute path (a mounted disk or
  share) or `[user@]host:path` reached over ssh by rsync. A port or
  other ssh options go into `~/.ssh/config`.

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
- **Locked entries:** a piece with an entry that carries an immutable or
  append-only flag (macOS `st_flags` UF/SF_IMMUTABLE, UF/SF_APPEND;
  Linux `statx` STATX_ATTR_IMMUTABLE/APPEND), or whose parent directory
  is not writable, is never archived, since it could not be moved or
  deleted; `status` names it ("locked: <path>").
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
- **Control files** (at a piece's top for the markers) never count as
  activity and never make a piece non-empty. At a piece's top a regular
  file is a marker (for `.antiq-pin` also a symlink, as built); any
  other entry with a marker's name makes the piece refused ("<name> is
  not a file"); it is never packed silently and never deleted unpacked. The two rule files are
  packed; `.antiq-restored` is not packed and is deleted with the
  folder; `.antiq-pin` never reaches packing, since a pinned piece is
  not archived.
- **Idle clock:** the later of (a) the newest modification time of the
  regular files that are not ignored, not timeless and not control
  files, and (b) the time in `.antiq-restored`, if there is one.
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
    tmp/                                staging; pack/<stamp>/<piece path>/ while archiving
    trash/                              folders taken out, not yet checked again
    doomed/                             folders checked again, being deleted
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
- The checksum file holds one line, `<hash>  <archive file name>`, with
  the bare file name, so `shasum -a 256 -c <checksum file>` run in the
  store folder checks the archive. The listing is described below.
- Every directory antiq creates (store folders under `pieces/`, parents
  above an unearth target, the `<stamp>/` chains in `trash/` and
  `doomed/`) is flushed together with its parent before any step relies
  on it, in particular before `archive` deletes a folder and before
  `unearth` deletes a version.
- **A version exists if and only if its `.tar.zst` exists.** The
  `.tar.zst` is moved in last and deleted first.
- **Damage.** antiq's own crashes never leave the states below behind
  for good (see "Repairs at the start of a run"); they come from
  outside: a file deleted by hand, an incomplete copy back from the
  target, a disk error. None of them is deleted or rewritten
  automatically; the user looks at them first.
  - *Incomplete version:* a listing or `.tar.zst.sha256` without its
    `.tar.zst`. `list` marks it, `grep` skips it, `status` reports it,
    and `forget` removes it.
  - *Damaged version:* a `.tar.zst` whose listing or checksum file is
    missing; or whose size differs from the `archive-bytes` its listing
    records (archive files never change once stored); or whose listing
    does not decode to its end (zstd checksum) or lacks a parsable
    `piece`, `archived` or `archive-bytes` line; or whose checksum file
    is not exactly one checksum line for the archive's name. `status`
    reports it; `list` marks what it sees in the listing's head;
    `rebuild` repairs a missing sidecar; `fork` can
    still extract it, with a warning and without the comparison against
    the listing; `unearth` refuses it.
  - `push` refuses while either kind exists, naming the exact `forget`
    or `rebuild` command (it decodes every listing in full for that, in
    parallel like `grep`, and so does `status`), since the mirror would otherwise delete or
    overwrite the target's good copy. A scheduled push stops there
    until the user acts.
  - A change of an archive file that keeps its size is not detected
    locally; the full comparison is todo 01M6K6WN14S91E2X5TRQYZRHJ5.
- When the last version of a piece goes, its folder under `pieces/` and
  every parent that became empty go with it.
- There is no state file: everything antiq needs follows from the
  folders, the files of the versions, and the control files.

## Listing format

A listing is a SHA-256 checksum list, zstd-compressed, with antiq's
metadata in comment lines. `shasum -a 256 -c` and GNU `sha256sum -c`
both skip lines that start with `#` (verified 2026-11-04 with
`/usr/bin/shasum` 6.02, Homebrew shasum 6.04 and GNU coreutils 9.12,
also with `--strict`). A comment after the path on the same line
does not work: both tools read it as part of the file name.

```text
# antiq listing 1
# piece         projects/scanner
# archived      2026-11-04T09:02:11Z
# archive       projects-scanner--2026-11-04T090211.tar.zst
# archive-bytes 1093744120
# files         48211
# bytes         1288490188
#
# dir   0755  2026-10-30T17:40:00Z                 -  src/
# file  0644  2026-10-30T17:44:02Z              4211  src/main.rs
3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b  src/main.rs
# hard  0644  2026-10-30T17:44:02Z                 -  src/main-copy.rs	=> src/main.rs
3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b  src/main-copy.rs
# link  0755  2026-10-30T17:41:00Z                 -  src/latest	-> main.rs
```

- **Header:** `piece` (the stored spelling), `archived` (the version
  time, the one in the file name), `archive` (the archive file's name),
  `archive-bytes` (its size, written when packing finishes),
  `files` and `bytes` (count and total size of the regular files, so
  `list` and `status` read only the head of a listing). A version made
  by `import` adds `imported <time>`, the real time of the import; a
  listing written by `rebuild` adds `rebuilt <time>`.
- **Entry lines:** four fixed-width fields (type, octal mode, UTC time
  with `T`, the exact size in bytes or `-`), one space, then the path
  to the end of the line, starting at a fixed byte offset. A link line
  separates path and target with one tab: `<path>\t-> <target>`.
- **Types:** `dir` (the path ends in `/`), `file`, `link`, and `hard`
  for a hard link, written `<path>\t=> <target>` with size `-`; the
  archive stores it as a tar link member, and it does not count in
  `files` or `bytes`, which count stored data once. Special files are
  not archived and have no line (spec "Packaging", unchanged).
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
- An archive unpacks to one top-level directory named after the folder
  when it was packed; the listing has no line for it, and its paths are
  relative to it. The hand check therefore runs inside that directory,
  for example after `tar --zstd -xf <archive file>`.

## Rules for every command

- Every command prints readable text on a terminal and has `--json`.
  Data goes to stdout, messages to stderr. The JSON envelope of the
  current code stays, with the plan's `actions` for dry runs; the
  brake's keys go.
- **Dry runs:** every command that changes something has
  `-n`/`--dry-run`: `init`, `archive`, `unearth`, `fork`, `forget`,
  `rebuild`, `import`, `pin`, `unpin` and `push`. A dry run takes no
  lock and changes nothing. It computes the repairs read-only and lists
  them first, as actions of kind `repair` (each naming the version or
  tree it would delete, rename back or move to `recovered/`); its other
  actions assume the repairs were done. A dry run that finds the lock
  held says so, names the holder and lists no repairs, since the
  running command owns those directories. Each command's section says
  what else its dry run prints.
- **Lock:** every command that changes the collection (`archive`,
  `unearth`, `fork`, `forget`, `rebuild`, `import`, `pin`, `unpin`) and
  `push`, for its whole run, takes the exclusive lock `.antiq/lock`. A
  held lock fails the command at once, naming the holder. `list`,
  `grep` and `status` take no lock and say when it is held. Without the
  lock on `push`, rsync could scan a piece halfway through an `unearth`
  and leave the target with neither the folder nor the version.
- **Repairs:** every locked run, `push` included, first runs the
  repairs (see "Repairs at the start of a run"). A repair that fails is
  a warning naming the path and does not fail the run.
- **Markers** (`.antiq-restored`, `.antiq-pin`) are written in
  `.antiq/tmp/` and renamed over their place, then the piece's top is
  flushed; a leftover stays in tmp, which the repairs empty. A top
  without owner write gets it for the rename and its recorded mode back
  right after.
- **Free space:** the rules of spec "Operations" and spec "unearth"
  apply unchanged: `archive` and `import` need room in `.antiq/` for a
  piece's apparent size (parallel pieces reserve their sum), `unearth`
  and `fork` room for the listing's `bytes`; a piece without room is
  skipped before staging, and ENOSPC during a step removes that step's
  staging and skips the piece (exit 3).
- **Exit codes:** 0 success; 1 error, a held lock included; 2 usage;
  3 partial (a step failed for some pieces: packing, verifying, moving,
  a difference in step 6, a refused rename, a failed deletion, no
  room); 4 target unreachable (only `push`, and 0 instead under
  `--if-reachable`). Pieces the walk refuses do not change the exit
  code; `status` and the command's output name them.
- **Naming a version:** an optional version is the flag
  `--version <version>` (`unearth`, `fork`; without it the newest
  version), a required one is positional (`forget`, `rebuild`). Text
  output shows local time with seconds, `--json` and pipe output UTC
  (RFC 3339 in JSON, the version stamp in pipes). The input's spelling
  decides how it is matched, and the two are never mixed:
  - an input containing `T` (`2026-11-04T090211`, optional `-N`) is
    matched against the version stamps;
  - any other input (`2026-11-04 10:02:11`, or a date alone such as
    `2026-11-04`) is matched against each version's time rendered in
    the machine's current zone, as a prefix of that text. Near
    midnight a local date can differ from the UTC date of the stamp.

  An exact match wins over prefix matches; several matches are an
  error naming each version's stamp. When two versions of one piece
  render alike in local time (the repeated hour when daylight saving
  time ends), text output adds the UTC offset.

## Commands

### `init`

Makes a directory a collection: writes `.antiq/config.toml` with a new
collection ID and the patterns given, without asking. It does not look
for the layout of `archive.sh`; its old archive files are brought in by
hand with `import`. It refuses, creating nothing and exiting 1, where
`.antiq/` already exists, inside another collection (naming its root),
where `.antiq` is a file or a symlink, and where a folder that would be
a piece holds a `.antiq/` of its own (spec "Collections and pieces",
unchanged). `-n` prints the config it would write.

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
the walk refusals and the empty rule still apply. `-n` lists the pieces
it would archive and the ones it refuses, with the reason.

**Proposal for the flow:**

1. Find the pieces and judge each with the rules of "Selection, walk
   and idle check".
2. Pack the piece into `.antiq/tmp/pack/<version stamp>/<piece path>/`,
   under the final file names: the `.tar.zst`, the listing and the
   `.tar.zst.sha256`.
3. Verify: read the whole archive file back and compare it with the
   listing and the folder. A folder that changes meanwhile is skipped.
4. Move the listing, then the `.tar.zst.sha256`, then the `.tar.zst`
   into `.antiq/pieces/<path>/`, flushing each and the directory.
5. Rename the folder to `.antiq/trash/<version stamp>/<piece path>/`,
   under its live spelling (one rename on the same device; a piece on
   another device counts as containing a mount point and never gets
   here). If the rename is refused anyway, step 4 is undone: the
   `.tar.zst`, then the checksum file, then the listing go back to the
   staging, flushed as in step 4 (repair 1 finishes this after a
   crash); the folder stays, the piece is reported with the error, and
   the run exits 3.
6. Walk the trashed tree again and compare it with the record of the
   verify: per entry type, size, modification time, inode and change
   time, directories included; the top only by type, inode and
   modification time, since the rename changed its change time (spec
   "archive", unchanged). This walk enters ignored subtrees for the same
   refusals as the walk before packing (any refusal counts as a
   difference); ignored entries and `.antiq-restored` at the top are not
   compared, since they go with the folder in any case. On any difference rename it back; when that is not possible for any
   reason (the path is taken, a parent is missing or not a directory,
   the rename is refused), move it to
   `.antiq/recovered/<version stamp>/<piece path>/` instead (missing
   parents are not created) and list it in `status`; the version
   stays. A write
   after this walk is lost, as in spec "archive". Pieces that share
   hard-linked files can be kept instead of removed when archived in
   the same run, as spec "archive" states.
7. Rename the tree to `.antiq/doomed/<version stamp>/<piece path>/`,
   flush, and delete it from there. A deletion that fails leaves it in
   `doomed/`, where the repairs try again and `status` reports it with
   the first entry that could not be deleted and the error. Whenever a
   tree leaves `trash/` or `doomed/`, the parents that became empty are
   removed, up to and including `<stamp>/`.
8. One output line per folder, a summary at the end.

A crash between steps 4 and 5 leaves the live folder next to a complete
version of it; the next run may archive the unchanged folder again,
which gives a second, identical version that `forget` removes. The same
holds for a crash between steps 5 and 6: the repairs give the folder
back, since `trash/` holds only folders not yet checked again.

### `unearth <piece> [--version <version>]`

Brings a version of a piece back as a folder and **removes that
version** once the folder is complete and verified. The folder is an
ordinary live folder again. Archiving it later creates a new version.
`-n` names the version, the target path and the checks it would make.

- `<piece>` is a path, or a search word that matches exactly one
  piece; anything else is an error that lists the candidates. Without
  `--version` the newest version is taken.
- A damaged version is refused, naming `fork` (which extracts it with a
  warning and keeps it), `rebuild` and copying the missing file back
  from the target.
- The target path is refused, and nothing changes, when it is not one
  the patterns select, is excluded, lies in `.antiq/`, inside another
  piece or below a file or symlink, or when anything is at it, a
  dangling symlink included (spec "unearth", unchanged). Member names
  are checked against the target volume as in spec "unearth" (ADR
  0054). Missing parents are created; a failure before the rename
  removes what was created.
- A version whose top holds a non-file with a marker's name is
  refused, as is a damaged version.
- **Order:** extract into `.antiq/tmp/`; verify every entry against the
  listing; write `.antiq-restored` at the staged top; flush every
  extracted file and directory; check the target path; rename into
  place; flush the parent; delete the version
  (`.tar.zst` first, then the listing and the checksum file, flushing
  between); rewrite `.antiq-restored` to the time alone. No version is
  deleted before the folder replacing it is durable; how the tree is
  flushed (per-file `sync_all` or `fsync` plus one full flush) is
  measured in the rework plan.
- **`.antiq-restored`** holds one line,
  `restored <UTC time>[ version <stamp>]`, and is read only as a
  regular file at the piece's top. The idle clock starts at its time.
  The version stamp is there only between the rename and the deletion
  of the version: if a crash falls in between, the repairs finish the
  unearth (see there). A marker that cannot be read or parsed fails the
  idle check of that piece with the file named, so the piece is never
  called due by mistake.
- There is no reclaim: an unearthed folder that is archived again,
  changed or not, becomes a new version.

### `fork <piece> [--version <version>] [--name <new path>]`

Extracts a working copy of a version, which stays unchanged. Without
`--name` the copy goes to the original path. The same target rules and
staging as for `unearth` apply; when the original path is taken, the
refusal names `--name` as the way out. A damaged version is extracted
with a warning and without the comparison against the listing. `fork`
writes `.antiq-restored` with the time alone (no version stamp, so the
repairs never treat it as an unearth). Archived later, the copy becomes
a new version (same path) or a new piece (new path); without edits that
is an identical version, once due or at once with `archive <piece>`.
`-n` names the version and the target path.

### `forget <piece> (<version> | --all-versions --force)`

Deletes exactly one version: the `.tar.zst` first, then the listing and
the checksum file. It also takes the stamp of an incomplete version and
removes its sidecars, saying that the version was incomplete. Deleting
every version of a piece takes `--all-versions --force`, which also
removes the piece's incomplete versions and says so for each. A crash inside
`forget` can leave an incomplete version; running the same `forget`
again removes it. The next `push` mirrors the deletion to the target.
`-n` names the files it would delete.

### `rebuild <piece> <version>`

Repairs a damaged version by writing only what is missing, after
checking the archive file against what survives. It runs only on the
user's request, never as a repair, since the user is to look at such a
version first (and may prefer to copy the missing file back from the
target).

- A listing is regenerated by the rules `archive` uses, from the tar
  members: mode and whole-second time from the header, size and hash
  from reading each member, `hard` for tar link members with the first
  path as target, no line for the top-level directory; `files`,
  `bytes` and `archive-bytes` come from the same read.
- Listing missing, checksum file present: hash the archive file and
  compare it with the checksum file. On a mismatch refuse (exit 1,
  naming both hashes); otherwise write the listing, with
  `# rebuilt <UTC time>`, and say that the archive is the one
  checksummed when it was stored.
- `.tar.zst.sha256` missing, listing present: verify every member
  against the listing, as `unearth` does. On any difference refuse;
  otherwise write the checksum file.
- Both missing: read the archive in full, write the listing (with
  `# rebuilt`) and the checksum file, and say that this shows only that
  the archive reads without error, not that it is unchanged.
- An archive file whose size differs from its listing's `archive-bytes`
  cannot be repaired by `rebuild`, since the hash against the surviving
  checksum file can only fail; `rebuild` refuses and says so.
- A sidecar that is present but corrupt (see "Damage") is not
  overwritten: `rebuild` refuses and names deleting that file (after
  which it is missing and can be rebuilt) or copying it back from the
  target.
- Every refusal names `forget` and copying back from the target as the
  ways out. A version that is not damaged is refused. It takes the lock
  and has `-n`, which says what it would check and write.

### `import <folder> [--as <path>] [--time <time>]`

Packs a folder from outside the collection directly as a version of
`<path>` (default: the folder's name), with steps 1 to 4 of `archive`;
the source folder is left in place.

- The source must not lie inside the collection.
- `<path>` must be one the patterns select, not excluded, not in
  `.antiq/` and not inside another piece. A pin on a live folder at
  `<path>` does not block the import.
- The idle check does not apply. The source's top is treated as a
  piece's top: `.antiq-restored` there is left out, `.antiq-pin` there
  refuses the import, and a source without a countable file is refused
  as empty, also with `--time`.
- The ignore and timeless rules of the collection and the folder's own
  rule files apply, and so do the walk refusals.
- The version time is the newest counted modification time in the
  folder, or `--time`. A version time in the future is refused, naming
  the file.
- This is also the way to bring in old archive files: unpack them with
  their original timestamps somewhere temporary, then import each
  folder.
- `-n` names the version it would create and what it would leave out.

### `list [query]`

One row per version, newest first within a piece; the query matches
piece paths fuzzily. Each row reads only the head of its listing.
SIZE is the archive file's size (`archive-bytes`) in decimal units
(1 GB = 10^9 bytes); `--json` also carries the unpacked size and the
file count. Damaged and incomplete versions are
marked.

```text
$ antiq list scan
PIECE              VERSION               SIZE     FILES
projects/scanner   2026-11-04 10:02:11   1.1 GB   48 211
projects/scanner   2026-09-03 12:15:00   1.0 GB   47 980
tools/scan-old     2025-02-11 19:40:07    40 MB      812
```

The layout above is the one decided from rendered examples: the piece
in every row, so each row stands on its own after `sort` or `grep`.

### `grep <pattern>`

Searches the paths of every archived file, as `<piece>/<path>`, in all
listings of existing versions (incomplete versions are skipped), with a
regular expression (`regex::bytes` over each decoded listing, the files
in parallel on all cores but two). Measured in
`docs/research/listing-grep.md`: about 0.2-0.26 s for 300 versions of
50 000 entries with compressed listings, warm cache, 14 threads.

On a terminal the hits are grouped by piece, each in-piece path once
with the versions that hold it, the match highlighted:

```text
projects/scanner                       2 versions, newest 2026-11-04
  src/main.rs                          both versions
  tools/bench/main.rs                  2026-09-03
tools/scan-old                         1 version, 2025-02-11
  cli/src/main.rs                      2025-02-11

3 files in 2 pieces · antiq unearth <piece> [--version <version>]
```

The pattern is matched against the unescaped path bytes; text output
shows the path with the comment-line escapes, the pipe form prints the
raw path. A test covers a name with a tab and a backslash.

A match wholly inside the piece path gives one row, "piece name
matches", with the version and file counts and no file list. In a pipe
every hit and version is one tab-separated line `piece  stamp  path`,
with the full version stamp (`-` as the path for a name match).

### `status`

Local only. One row per piece that has a state, sorted by path, the
state coloured in the first column: `due` (archived on the next run),
`soon` (due within `soon_days`, default 3), `pinned`, and `refused` for
the pieces `archive` refuses, with their reason (mount point, nested
collection, unreadable entry, exclude entry, unreadable marker). Plain
live folders are only counted in the header. Below the table: files and
markers with times in the future, damaged and incomplete versions,
folders in `recovered/`, trees left in `trash/` and leftovers in
`doomed/`, and live folders whose spelling differs from the stored one.

```text
13 folders · 214 versions (38.2 GB) · archive after 30 days idle

STATE    PIECE                                 IDLE  WHAT
due      experiments/rust-gpu                   33d  archives next run
due      old/client-site                       141d  archives next run
refused  media/camera-import                    52d  mount point: media/camera-import/card
pinned   projects/antiq                        120d  kept by .antiq-pin
soon     tools/scan-old                         28d  archives in 2 days

! 1 file with a time in the future: notes/plan.md (2027-01-12)
! 1 damaged version: tools/scan-old 2024-06-30 08:12:45 (no listing)
```

### `pin <piece>` / `unpin <piece>`

Create or remove `.antiq-pin` at the top of a live piece. `-n` says
which.

### `push`

Mirrors the collection to the target. **Proposal for the flow:**

1. **Repairs** run first, under the lock (see "Rules for every
   command").
2. **Check the store.** While a damaged or incomplete version exists,
   `push` refuses with exit 1 and prints the exact `forget` or
   `rebuild` command for each. It also refuses, naming both, when two
   piece paths or two store folders of the collection differ only in
   case or Unicode normalization, since a case-insensitive target would
   merge them. (The same clash between file names inside one live
   folder is not checked; the user documentation covers it.) And it
   refuses while `.antiq/trash/` still holds a tree after the repairs,
   since the mirror would delete the target's copy of that folder.
3. **Check rsync.** The local rsync's `--version` must name rsync 3.2.4
   or newer, the release that added `--fsync` (rsync NEWS.md, section
   3.2.4). openrsync, which macOS ships as `/usr/bin/rsync`, and older
   versions are refused with a message naming the binary and the
   config key `[target] rsync`. A target whose own rsync is openrsync
   (any macOS machine) works with `fsync = false`.
4. **Pre-check the target.** The target path is canonicalized first. A
   path target that is the collection root or lies below it (compared
   by the volume's name equality, ADR 0054) is exit 1, also with
   `--init`, and so is a path target that is a mount root ("use a
   folder on the volume, not its root"). Otherwise the checks
   use the file system for a path target and `rsync --list-only
   <target>/` for an ssh target:

   | Situation | Result |
   |---|---|
   | ssh cannot connect | unreachable |
   | path target answers ENOTCONN, EHOSTDOWN, EHOSTUNREACH, ENETUNREACH or ETIMEDOUT | unreachable |
   | path target directory does not exist | unreachable, also with `--init` |
   | ssh target directory does not exist | exit 1, unless `--init` |
   | directory present, no `.antiq-target`, empty | unreachable with a warning naming `push --init`, unless `--init` |
   | directory present, no `.antiq-target`, not empty | exit 1; `--init` is refused |
   | `.antiq-target` of another collection, or not readable | exit 1 |
   | `.antiq-target` of this collection naming another host or another collection path | exit 1, naming both; `--take-over` pushes anyway and rewrites the marker |

   When the listing shows `.antiq-target` on an ssh target, it is
   fetched with one rsync run of `<target>/.antiq-target` into
   `.antiq/tmp/`, with the same options as the marker run below; a
   fetch that fails, or content that does not parse, is "not
   readable". `--take-over` changes nothing in the other rows: no
   marker still needs `--init`, another collection's marker is still
   exit 1, and with a matching marker it is a plain push, noted in the
   output.

   rsync runs with `LC_ALL=C`. Over ssh, exit 255 counts as "cannot
   connect" only when ssh's line starts with `ssh: connect to host` or
   `ssh: Could not resolve hostname`, or contains `timed out`; any
   other 255 (a changed host key, a refused key) is exit 1 with the
   line. Exit 23 counts as "does not exist" only when the failing
   `change_dir` or `link_stat` line ends in `(2)`, or openrsync's
   `(l)stat:` line ends in `No such file or directory`; any other 23 is
   exit 1.

   Unreachable means exit 0 under `--if-reachable`, otherwise exit 4,
   and the reason is printed in text and `--json` every time. A missing
   directory differs by kind of target because an unmounted share on
   macOS takes its `/Volumes/<name>` directory with it, while an ssh
   host that answers but lacks the directory is misconfigured. A path
   target's directory is never created by antiq: antiq cannot tell an
   unmounted mount point from a real directory, so the user creates it
   on the mounted volume.
5. **Marker.** `.antiq-target` sits at the target root, outside the
   collection. It is one line of JSON with a fixed key order,
   `{"format":1,"id":"<collection ID>","host":"<machine name>","path":"<collection path>"}`.
   The machine name is the one `machine.rs` detects (the ComputerName
   on macOS, the hostname elsewhere), never a network-dependent host
   name; the path is the canonical collection root, compared byte for
   byte. A renamed machine or a moved collection therefore needs one
   `push --take-over`;
   unknown keys are ignored, a higher `format` is refused as written by
   a newer antiq. It is created only by `push --init` and rewritten
   only by `push --take-over`; both are refused together with
   `--if-reachable`. `--init` is the user's statement that this empty
   target is the right one, `--take-over` that the other host or path
   may overwrite the target after the user made sure. The marker is
   sent by its own rsync run, as a temp directory in `.antiq/tmp/`
   holding only `.antiq-target`, sent to `<target>/` with
   `rsync -r --chmod=F644` (no `-a`, `-p`, `-t` or `--delete`, so the
   target root keeps its mode and time) plus the same `-e`,
   `--rsync-path`, fsync setting and `rsync_args` as the mirror; for an
   ssh target that creates the last missing directory level, and a deeper missing
   path is exit 1 with rsync's mkdir line. `push -n` never writes it.
6. **Mirror.** One rsync run, antiq's options first, then the options
   of `[target] rsync_args` verbatim, then the two paths:

   ```text
   rsync -a -x --delete --delete-delay --itemize-changes
         --exclude=/.antiq-target --exclude=/.antiq/tmp/
         --exclude=/.antiq/trash/ --exclude=/.antiq/doomed/
         --exclude=/.antiq/lock --exclude=/.DS_Store
         --exclude=/.Spotlight-V100/ --exclude=/.fseventsd/
         --exclude=/.DocumentRevisions-V100/ --exclude=/.TemporaryItems/
         --exclude=/.Trashes/
         --timeout=300 [--fsync] [--rsync-path=<rsync_path>]
         -e "ssh -o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
         <rsync_args> <collection>/ <target>/
   ```

   `--fsync` unless `[target] fsync = false`. An option in `rsync_args`
   overrides antiq's (`--no-perms` after `-a`); an `-e` there replaces
   antiq's ssh command, BatchMode included. The excludes are anchored
   at the collection root, so the same names inside a live piece are
   still copied; the exclude of the marker also protects the target's
   marker from `--delete`. The macOS volume-root directories are left
   out because rsync cannot read them and any unreadable directory stops
   every deletion; `.DS_Store` at the root, because Finder visits would
   otherwise count as changes. The ssh options and `--timeout` (seconds
   without data, today's default) keep a sleeping NAS from holding the
   lock for hours; rsync's exit 30 (timeout) is exit 1 with its line. `-x` keeps rsync on the collection's device: a mount point to
   another device inside a live piece arrives as an empty directory
   with the mounted directory's attributes; a bind mount of the same
   device is mirrored like a folder.
   With `--delete-delay`, deletions happen only at the end, so an
   interrupted run removes nothing that was on the target before.
7. **Second pass.** When the run's output has a line starting with
   `*deleting`, `push` runs the same command with `-n` and, if that
   prints any itemized line, the mirror once more. On a case-insensitive target a
   folder renamed only in case (`Foo` to `foo`) is otherwise deleted at
   the end of the first run and copied again only by the next push.
8. **Result.** rsync exit 0 is success; 24 (files vanished during the
   run) is success with a note naming the paths of the
   `file has vanished` lines. Everything else, including any failure
   after the pre-check, is exit 1 with rsync's message, also under
   `--if-reachable`. With exit 23 and the line `IO error encountered --
   skipping file deletion`, push says that no deletion reached the
   target and names the directories rsync could not open. Exit 12 with
   `unrecognized option `--fsync'` (openrsync) or `--fsync: unknown
   option` (older rsync) is reported as: the target's rsync
   (openrsync, or older than 3.2.4) lacks `--fsync`; set `[target]
   rsync_path` to a 3.2.4+ rsync there, or `fsync = false`.
9. `-n` runs the checks and summarizes rsync's `--itemize-changes` per
   piece.

With `--fsync` the target flushes each file's content before it is
renamed into place; a power loss can then at most lose the newest
names, which the next push sends again. Without it, files rsync
reported as written can be empty or missing. Beyond that antiq relies
on rsync's own check: rsync computes a whole-file checksum of each file
it transfers while writing it and compares it with the sender's. Files
rsync does not transfer, because size and modification time match, are
not compared, so damage on the target stays unseen until a full
comparison (todo 01M6K6WN14S91E2X5TRQYZRHJ5). antiq reads nothing back.
Nothing records when the last push ran.

`push` copies live folders whole, ignored paths such as
`node_modules/` included; the ignore rules apply to versions only.

`push` has no brake: whatever is deleted locally, by antiq or by hand,
is deleted on the target by the next push. The target is a mirror, not
a backup with history, and the user documentation says so. That is why
the command is called `push` and not `backup`.

`--if-reachable` makes `push` exit 0 when the target is unreachable as
defined above, for scheduled runs. Scheduling runs the two commands one
after the other, such as `antiq archive; antiq push --if-reachable`, so
the push runs after a partial archive too (the scheduler then sees
only the push's exit status); there is no command that does both. A
held lock is exit 1, also under `--if-reachable`.

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
url = "nas.local:/volume1/antiq/projects"   # an absolute path or [user@]host:path
rsync = "/opt/homebrew/bin/rsync"   # optional, else from PATH
rsync_path = "/usr/local/bin/rsync" # optional, the target's rsync (--rsync-path)
fsync = true                        # false when the target's rsync lacks --fsync
rsync_args = []                     # extra rsync options, e.g. for SMB shares
```

A path target may be any absolute path; the path of an ssh target is
passed to rsync verbatim (rsync 3.2.4 and later protect arguments by
default).

The keys of the current config that go: `junk`, `remote_only_days`,
`backup_live`, `live_exclude`, `replace_without_asking`,
`brake_percent`, `grace_hours`, `away_warn_days`, `memory_percent`,
`remote_jobs`, `live_retries`, the `[machine]` table and the
`[[remote]]` list. A config that still has one of them is refused with
the keys named; old stores start fresh anyway.

## Repairs at the start of a run

There is no `reconcile` command. Every locked run (`push` included)
first runs these repairs, in this order; a dry run lists them instead.
A repair that fails is a warning naming the path. Whenever a tree
leaves `trash/` or `doomed/`, the parents that became empty go too.

1. **Interrupted archive step 4:** for each
   `.antiq/tmp/pack/<stamp>/<piece path>/` whose sidecars already lie
   in `.antiq/pieces/<path>/` without their `.tar.zst`: delete those
   sidecars. The folder is still live, since step 5 waits for step 4.
2. **Temporary files:** delete what `.antiq/tmp/` holds.
3. **`doomed/`:** delete every tree there without a comparison; it was
   checked whole before any deletion began. Directory chains that hold
   no non-directory entry are removed as they are.
4. **`trash/`:** rename every tree there back to its live path without
   a comparison, since no deletion has touched it, or, when that is not
   possible for any reason (path taken, a parent missing or not a
   directory, the rename refused), to
   `.antiq/recovered/<stamp>/<piece path>/`. Directory chains that hold
   no non-directory entry are removed as they are (an archived piece
   always held a countable file).
   The piece path is the shortest path below `<stamp>/` for which a
   version with that stamp exists, matched by the volume's name
   equality (ADR 0054); when no version has that stamp (deleted by
   hand), the first component below `<stamp>/` goes back as it is,
   which usually lands in `recovered/`. `status` names every folder in
   `recovered/`.
5. **Died unearth:** for each live piece whose `.antiq-restored` names
   a version stamp, delete what is left of that version of that piece
   (the `.tar.zst` first, if it still exists, then its sidecars), and
   rewrite the marker to the time alone. A stamp that names nothing is
   simply dropped from the marker. Since every locked run repairs
   before it creates versions, a stale stamp never meets a new version
   of the same run; a version copied back by hand between a crash and
   the next run is deleted, and the repair names it.
6. **Empty store folders:** remove folders under `pieces/` that hold
   nothing.

Sidecars without a `.tar.zst` and without a staged archive file in
`tmp/pack/` are an incomplete version and stay (see "Damage"). The
repairs name every version and tree they delete or move.

## Existing stores

Stores in the old layout (`.antiq/objects/`) are not converted; they
exist only in test collections, since v1 is unreleased.

## What remains from the built code

The modules below exist on main today (`src/`). How each fares,
**as a proposal for the rework plan**:

| Module | Fate | Note |
|---|---|---|
| `select.rs`, `walk.rs`, `idle.rs`, `tree.rs` | kept, adapted | ignore and timeless rules, marker time, control files |
| `pack/` | kept, adapted | without `pack/live.rs`, `pack/manifest.rs` and the live-package constants in `pack/zstd.rs`; listing gains `hard` lines; the pool's memory budget becomes a constant, 25 % of physical memory (today's default of `memory_percent`) |
| `sysmem.rs` | kept | physical memory for the pool budget |
| `listing.rs` | rewritten | new format; its file-name helpers move out of `meta.rs` |
| `fsx/` | kept, adapted | removal and moves also handle the immutable and append-only flags as refusals; `space.rs` stays for the free-space rules |
| `lock.rs`, `run.rs` | kept | `run.rs` loses the parts tied to tracking and metadata |
| `pin.rs`, `relpath.rs`, `id.rs`, `clock.rs`, `error.rs`, `report.rs`, `plan.rs` | kept, adapted | `plan.rs` loses the brake; `error.rs` its remote and conflict kinds |
| `reference.rs` | rewritten | resolves piece paths and search words against live pieces and `pieces/` (today it goes through `index.rs` and object IDs) |
| `interact.rs`, the question part of `cli/tty.rs` | removed | no command asks anything |
| `collection.rs` | adapted | new local layout |
| `config.rs` | rewritten | keys above |
| `machine.rs` | kept, adapted | machine name for the target marker; keeps only `detect` (no configured name) |
| `repair.rs` | rewritten | the repairs above only |
| `remote/rsync.rs` | kept, adapted | resolves and runs rsync; `--ignore-times`, `--no-whole-file` and the stats options go; the command is the one in `push`; floor 3.2.4; `machine.rsync` becomes `[target] rsync`; the ssh options of `remote/ssh/` move here |
| `remote/marker.rs` | kept, adapted | renamed to `.antiq-target`, `layout` dropped, `host` and `path` added, rewrite allowed (`--take-over`) |
| `remote/url.rs` | rewritten | an absolute path (any characters) or `[user@]host:path` |
| `cli/` (without `cli/picker/`), `main.rs`, `lib.rs` | adapted | new command set |
| `testing/` | kept, adapted | fixtures for the new store |
| `ops/archive/`, `ops/unearth/`, `ops/fork.rs`, `ops/import.rs`, `ops/status.rs`, `ops/init.rs`, `ops/pin.rs` | rewritten | new store and semantics; `ops/archive/unearthed.rs` (reclaim) and `ops/unearth/live.rs`, `ops/unearth/remote.rs` go |
| `list`, `grep`, `forget`, `rebuild`, `push` | new | |
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
- what is not kept (extended attributes, ACLs, access and creation
  times) and that hard links of live folders arrive as separate files;
- that symlinks on the target point to whatever their paths name
  there;
- how to check a version by hand (`zstd -dc <listing> | shasum -a 256
  -c -`);
- that rsync compares only what it transfers;
- what `fsync = false` costs, that a macOS target needs it, which
  `rsync_args` an SMB or exFAT target needs, and that ssh is the
  recommended way to reach a NAS;
- that a folder renamed only in case is copied in a second pass on a
  case-insensitive target, and that the target must be as strict about
  names as the collection's volume (file names inside one live folder
  that differ only in case are not checked);
- that a directory which held files and later became a mount point of
  another device loses those files on the target;
- that an unreadable directory anywhere in the collection stops every
  deletion on the target until it is fixed;
- that a damaged or incomplete version stops `push`, scheduled runs
  included, until the user runs `rebuild` or `forget` or copies the
  file back;
- what `push --init` and `push --take-over` do and when to use them,
  and that a renamed machine or a moved collection needs one
  `push --take-over`;
- that the scheduling line `antiq archive; antiq push --if-reachable`
  reports only the push's exit status;
- that a path target must be a folder on a volume, not the volume's
  root.
