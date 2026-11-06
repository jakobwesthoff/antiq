# Specification, version 1

Draft. This file describes version 1 as its design settles. The reasons
behind the decisions are recorded as ADRs in `docs/adrs/`; research
behind technical choices is in `docs/research/`.

## Purpose

antiq looks after directories where working folders pile up, such as
`~/playground`. It archives the folders of a collection that nobody has
touched for a while, finds them again by name or by the files they hold,
brings them back, and mirrors the whole collection to one backup target.

## Scope

All work happens on one machine.

The target is a plain copy of the collection: copying it back restores
the live folders and the versions. Not kept, neither in versions nor on
the target: extended attributes, ACLs, access and creation times. Hard
links are kept, in versions and in the mirror of live folders; copying
the target back keeps them only with `rsync -aH`. Sparse files are stored
in full in versions and come back fully allocated.

Everything antiq writes can be understood and used without antiq:
archive files are `tar.zst`, and a listing is a checksum list that
`shasum -c` reads once `zstd` has decompressed it. Inside an unpacked
folder: `zstd -dc <listing> | shasum -a 256 -c -`.

Version 1 covers what the scripts `archive.sh` and `timecapsule-sync.sh`
do for `~/playground`:

- archiving folders by age into compressed tar archives
- searching the archives by piece name and by the paths of the files they
  hold, and bringing one back
- mirroring the collection to one target

Not in version 1:

- multi-machine support, version conflicts, claims, versioned metadata
  documents and tiers; a second machine that needs a version copies its
  files from the target by hand (ADR 0008)
- reading the archives and listings that `archive.sh` produced; old
  archive files are brought in by unpacking them and importing the
  folders (ADR 0009)
- extended attributes (ADR 0050)

## Concepts

- **Collection:** a directory with a `.antiq/` folder.
- **Piece:** a folder of the collection that antiq manages, chosen by
  `patterns` and `exclude`. Its path relative to the collection is its
  identity: archiving the same path again adds a version of the same
  piece.
- **Live:** a piece that exists as a folder.
- **Version:** one archived state of a piece, made at one moment. A piece
  can have several versions side by side. User-facing text calls it a
  version throughout; "archive" is only the verb.
- **Version stamp:** the UTC time of a version, written
  `YYYY-MM-DDTHHMMSS`, with a counter `-2`, `-3`, ... when the same piece
  has several versions within one second (`-1` is not a valid spelling).
  A stamp names a UTC time from `0001-01-01T000000` to
  `9999-12-30T215959`; a file name whose stamp lies outside is not a
  version. Stamp and piece path together name a version exactly.
- **Archive file:** the `.tar.zst` of a version. The word appears only
  where the files on disk matter.
- **Listing:** the `.sha256.zst` of a version (see "Listing format").
- **Checksum file:** the `.tar.zst.sha256` of a version.
- **Sidecars:** the listing and the checksum file together.
- **Pinned:** a live piece with a `.antiq-pin` file at its top, never
  archived.
- **Control files:** `.antiqignore` and `.antiq-timeless` (rule files, at
  any level), `.antiq-pin` and `.antiq-restored` (markers, only at a
  piece's top; elsewhere they are ordinary files, packed and counted like
  any other).
- **Target:** the backup location, an absolute path (a mounted disk or
  share) or `[user@]host:path` reached over ssh by rsync. A port or other
  ssh options go into `~/.ssh/config`.

## Collections and pieces

A managed folder is called a collection, and each unit in it a piece.
(ADR 0022)

A collection is marked by `.antiq/` at its top. A `.antiq` that is a file
or a symlink does not mark a collection. Commands find it by searching
upwards from the current directory, or from the path given with `-C`, and
fail outside a collection. `.antiq/` holds the collection's config, lock,
staging directories and the versions of its pieces. (ADR 0011)

## Selection

Pieces are the directories antiq treats as one thing each. A list of glob
patterns selects them, default `*`; `**` is allowed. When matches nest,
the shallowest match is the piece. (ADR 0012) A folder whose name starts
with a dot is a piece only when a pattern names that dot literally.
(ADR 0028)

Never pieces: `.antiq/`, symlinks, and paths on the collection's exclude
list. Exclude entries are literal paths relative to the collection root,
not patterns, and nothing below an excluded path is a piece. The exclude
list is applied before pieces are matched and before anything below an
entry is listed. An exclude entry that exists also excludes the directory
it names under another spelling that the volume treats as the same name,
such as another case, since the directory is recognised by its device and
inode as well. Selection uses lstat and never descends into a symlink.

A pattern is matched one path component at a time. A component that
begins with a dot is matched only by a pattern component that begins with
a literal dot; no other pattern component, `*.txt` included, and no `**`
matches it. `**` stands for any number of directories without a leading
dot, none included, and for one or more at the end of a pattern, so
`a/**` selects what is below `a` and not `a`. A pattern is refused if it
has an empty component (a trailing `/` included), a `.` or `..`
component, or a `/` inside a bracket class, which never matches a
separator.

Paths and names are stored as the bytes the filesystem holds. In the
escaped form, valid UTF-8 stays as it is, `%` becomes `%25`, and every
byte that is not part of valid UTF-8 becomes `%XX`, with upper-case hex
digits. Every byte sequence has this one escaped form, and a reader
refuses any other spelling, such as `%41` for `A` or `%ff`. A path is
names joined by single `/`; a name is not empty, has no `/` or NUL and is
neither `.` nor `..`. Paths in `--json` output and in the config's
`exclude` list use the escaped form.

A path that is valid UTF-8 is matched against the patterns as it is. A
path with bytes that are not valid UTF-8 is matched in its escaped form,
since the glob matcher takes only UTF-8. A pattern can therefore not tell
the two apart: `a%FF` matches the valid name `a%FF` and the name `a`
followed by the byte `0xFF`, and in the escaped form a `%` of the name
reads `%25`. Patterns and names compare as bytes, so a pattern in
composed Unicode form does not match a name in decomposed form.

A directory that selection has to look into and cannot read is skipped
and reported: `status` lists it as a note below the table
("unreadable: <dir>") and `archive` names it in its output. The exit
code stays 0.
When the collection root itself cannot be listed, the run
fails. Running out of file descriptors fails the run as it does in a walk.
An entry deleted while selection runs is skipped. Selection looks no more
than 512 levels deep; a deeper directory is not entered and is reported
like one it cannot read.

An exclude entry that exists, found below a selected piece, is not part of
that piece: the piece is never archived, and `status` names the entry. An
exclude entry that does not exist does not count.

Symlinks are never pieces, and no walk follows a link. Links inside a
piece are archived as links, and rsync copies them as links. A link on the
target points to whatever its path names there.

### Spelling

The folder under `.antiq/pieces/` holds the spelling of a piece's path. A
live folder whose path the collection's volume treats as the same name
(case, Unicode normalization, as probed per ADR 0054) is the same piece
and is archived into that folder. `unearth` and `fork` extract to the
stored spelling, `list` shows it, and `status` warns when a live spelling
differs from the stored one. A new store folder below an existing parent
folder is created inside the parent's stored spelling.

## Walks

Every walk of a piece (idle check, packing, verifying, deleting) uses
lstat, does not follow links, and stays on the device of the piece's top
directory.

A piece that contains another device (a mount point or a btrfs subvolume)
or a mount root is never archived; `status` names the mount point. A
piece whose top directory is itself on another device than the collection
root, is itself a mount root, or lies below a directory between the
collection root and the top that is a mount root, counts as containing a
mount point, at its own path. A mount root above the top shares the device
of everything below it, so the walk reads the directories above the top as
well. The walk compares the device of every entry with the top
directory's, not only that of directories: an entry on another device is
recorded, and a directory there is not entered. The same holds for the
root of a mounted filesystem that shares its parent's device, such as a
bind mount: the walk reads whether an entry is a mount root. On Linux that
is the `STATX_ATTR_MOUNT_ROOT` attribute of `statx`; a kernel older than
5.8 does not report it, and there only the device numbers decide. On macOS
it is the mount point name of the directory (`f_mntonname`) differing from
its parent's. A firmlink (macOS links directories such as `/Users`,
`/Library` and `/usr/local` into the data volume) compares as a mount root
there, so a piece containing one is refused, which is the safe direction.
A directory that cannot be searched is not compared on macOS; the delete
helper reads its flag again once it has given the directory owner access
and has opened it.

The walk does not enter an excluded directory either: a piece that
contains an existing exclude entry is never archived, and `status` names
the entry. So is a piece that contains a directory named `.antiq`, the
marker of a nested collection: `status` names the directory that holds the
marker, and the walk goes on into it.

A walk enters directories up to 512 levels below the piece's top, since
each level holds one directory open. A deeper directory is not entered and
makes the piece unreadable. The delete helper never crosses a device
boundary or enters a mount root, refuses a path with a mount root above
its top, refuses the root of its own tree, and stops at the same depth: a
directory more than 512 levels below the top it removes is not entered and
fails the removal, naming that directory.

An entry the walk cannot read makes the piece unreadable: it is never
archived, and `status` names the first entry that failed. That is the
smallest path among the failed entries, in the byte order the entries are
sorted by. An entry that has vanished since its directory was listed
counts as one the walk cannot read. Running out of file descriptors
(`EMFILE`, `ENFILE`) is not an unreadable entry: it fails the run. antiq
raises its soft limit of open files at start, since launchd starts jobs
with a soft limit of 256.

A piece with an entry that carries an immutable or append-only flag
(macOS `st_flags` `UF_IMMUTABLE`, `SF_IMMUTABLE`, `UF_APPEND`,
`SF_APPEND`; Linux `statx` `STATX_ATTR_IMMUTABLE`, `STATX_ATTR_APPEND`),
or a directory, the top's parent included, that the user does not own and
cannot write, is locked: it is never archived, and `status` names it ("locked: <path>"). A directory the user
owns but has made unwritable is not locked, since removal gives itself
owner access.

The walk before packing also enters ignored paths for these checks (see
"Ignore and timeless rules").

Paths longer than the platform's path limit are walked, packed, verified,
extracted and deleted relative to an open directory, never through the
full path.

## Ignore and timeless rules

What the `ignore` lines of the config and the `.antiqignore` files match
is not packed, not listed and does not count for the idle check;
`archive` deletes it with the folder. The default is
`ignore = [".DS_Store"]`. A typical use is `node_modules/`, which can be
fetched again.

What the `timeless` lines of the config and the `.antiq-timeless` files
match is packed but does not count for the idle check, such as `.git/`,
which changes on every `git fetch`. The default is empty.

The syntax of both kinds is gitignore's. Config lines are anchored at each
piece's top, a file's lines at its directory. A pattern without a `/`
except at its end matches at any depth below; a `/` at the start or in the
middle anchors it, so `docs/*.log` in `a/.antiqignore` matches
`a/docs/x.log` but not `a/b/docs/x.log` (that needs `**/docs/*.log`). The
nearest file wins, then those further out, then the config; within one
file the last matching rule wins. A directory that a rule matches hides
everything below it, and a `!` rule cannot bring back a path inside it. An
ignored directory's rule files are not read. Only a regular file named
`.antiqignore` or `.antiq-timeless` is read as a rule file. A rule that is
not valid, a file that is not UTF-8 text and a file that cannot be read
each refuse the piece, with the file named and, where a rule is at
fault, its line ("rule file: <path>:<line>"), so a piece with broken rules
is never called due. A path that an ignore rule and a timeless rule both
match is ignored.

**Ignored subtrees are still checked.** The walk before packing enters
ignored paths too, without hashing them, for mount points, nested
collections, unreadable entries, exclude entries, locked entries and the
depth limit, since `archive` deletes them with the folder. Only packing,
the listing and the idle check leave them out.

## Control files and markers

Control files never count as activity and never make a piece non-empty.
At a piece's top a regular file is a marker (for `.antiq-pin` also a
symlink); any other entry with a marker's name makes the piece refused
("<name> is not a file"). Such an entry is never packed silently and
never deleted unpacked. The two rule files are packed. `.antiq-restored`
is not packed and is deleted with the folder. `.antiq-pin` never reaches
packing, since a pinned piece is not archived.

`.antiq-restored` holds one line, `restored <UTC time>[ version <stamp>]`,
and is read only as a regular file at the piece's top. `unearth` and
`fork` write it (see there).

Markers (`.antiq-restored`, `.antiq-pin`) are written in `.antiq/tmp/` and
renamed over their place, then the piece's top is flushed; a leftover
stays in `tmp/`, which the repairs empty. A top without owner write gets
it for the rename and its recorded mode back right after. A top on another
device than `.antiq/tmp/` is refused with the reason "mount point".

## Idle

The idle clock of a piece is the later of

- the newest modification time of the regular files that are not ignored,
  not timeless and not control files, and
- the time in `.antiq-restored`, if there is one.

Directories, symlinks and special files do not count. A piece is due when
its idle clock is older than `idle_days`; a day is 24 hours, and a piece
exactly that old is not yet due. (ADR 0013, ADR 0037)

A file time or a marker time in the future counts like any other, so the
piece is not due before that time has passed; `status` lists such files
and markers so the user can fix them.

A marker that cannot be read or parsed fails the idle check of that piece
with the file named, so the piece is never called due by mistake.

A piece without a countable file is empty: never archived. A piece that
the walk could not read completely has no verdict. (ADR 0038)

## Pins

A piece with a `.antiq-pin` file at its top is pinned: it is never
archived, still mirrored by `push`, and shown as pinned. A regular file or
a symlink of that name pins the piece, a dangling symlink included. A
directory or any other entry of that name is not a pin: it refuses the
piece (see "Control files and markers"), and `unpin` refuses to remove it.
`pin` and `unpin` create and remove the file and refuse on a piece whose
folder is not present. A piece that is in the state asked for is left as
it is and reported as such. A pin and its removal are durable when `pin`
and `unpin` return: the file and the entry in the piece's directory are
flushed. The pin is checked before the idle and empty tests. (ADR 0024)

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
  spelling. A folder there is a piece of the store when it holds at least
  one `.tar.zst`.
- File names are `<name>--<version stamp>` plus the suffix. `<name>` is
  the piece path with every character outside `[A-Za-z0-9._]` replaced by
  `-` and repeated `-` collapsed. It is for people only: nothing parses
  it, it need not be unique, and it is cut from the front at a `-`
  boundary (inside a single over-long component at a character boundary)
  so the longest file name stays within 200 bytes. The cut counts the
  longest stamp, one with the counter `-9999`, and the longest suffix,
  `.tar.zst.sha256`, so `<name>` has at most 161 bytes and is the same for
  every version of a piece. A file name is parsed only for its stamp and
  suffix: it is split at the last `--`, whatever precedes that is not
  read, and the suffix is `.tar.zst`, `.sha256.zst` or
  `.tar.zst.sha256`. (ADR 0060)
- A file in a store folder whose name does not parse is ignored by every
  command and by the repairs, keeps its folder from counting as empty,
  and `status` lists it as a note.
- Versions order by time, then by counter as a number; no counter counts
  as 1.
- The version time of `archive` is the UTC second at which the piece's
  packing starts. A new stamp for a piece is that second without a counter
  when no file in the piece's store folder and no directory
  `tmp/pack/<stamp>/<piece path>` uses it; otherwise the same second with
  counter 2, then 3, and so on, the first one unused, up to the counter
  9999.
- The checksum file holds one line, `<hash>  <archive file name>`, with
  the hash in lower-case hex, two spaces, the bare file name and a final
  newline, and nothing else, so `shasum -a 256 -c <checksum file>` run in the
  store folder checks the archive. The listing is described under
  "Listing format".
- Every directory antiq creates (store folders under `pieces/`, parents
  above an unearth destination, the `<stamp>/` chains in `trash/` and
  `doomed/`) is flushed together with its parent before any step relies on
  it, in particular before `archive` deletes a folder and before `unearth`
  deletes a version.
- **A version exists if and only if its `.tar.zst` exists.** The
  `.tar.zst` is moved in last and deleted first. (ADR 0030)
- A version moves into its store folder as listing, checksum file,
  `.tar.zst`. Each file is flushed where it lies, renamed without
  replacing anything, and its folder flushed. A failure before the
  `.tar.zst` has moved puts the files that moved back; when that fails
  too, they and the staging stay where they are for repair 1, whatever the
  cause was, a full disk included. A failure to flush the folder after the
  `.tar.zst` has moved leaves the version in the store; the piece then
  goes no further and its folder stays live.
- When the last version of a piece goes, its folder under `pieces/` and
  every parent that became empty go with it.
- There is no state file: everything antiq needs follows from the
  folders, the files of the versions, and the control files.
- Stores in the old layout (`.antiq/objects/`) are not converted.

## Damage

antiq's own crashes never leave the states below behind for good (see
"Repairs at the start of a run"). They come from outside: a file deleted
by hand, an incomplete copy back from the target, a disk error. None of
them is deleted or rewritten automatically. (ADR 0062)

- **Incomplete version:** a listing or `.tar.zst.sha256` without its
  `.tar.zst`. A store folder that holds sidecars only is not a piece of
  the store, but its incomplete versions are found and reported. `list`
  marks an incomplete version, `grep` skips it, `status` reports it, and
  `forget` removes it, also in such a folder, by piece path and stamp.
- **Damaged version:** a `.tar.zst` whose listing or checksum file is
  missing; or whose size differs from the `archive-bytes` its listing
  records (archive files never change once stored); or whose listing does
  not decode to its end (zstd checksum), does not parse to its end (a
  header or entry line out of the format, a file or hard line without its
  checksum line, a path twice, `files` or `bytes` that differ from the
  entries) or lacks a parsable `piece`, `archived` or `archive-bytes`
  line; or whose checksum file is not
  exactly one checksum line for the archive's name. `status` reports it;
  `list` marks what it sees in the listing's head; `rebuild` repairs a
  missing sidecar; `fork` can still extract it, with a warning and
  without the comparison against the listing; `unearth` refuses it.
- **Doubled slot:** two or more files in one slot of a version, that is,
  with the same stamp and suffix and a different `<name>`. antiq cannot
  tell which of them belongs to the version, so the version is damaged and
  no file of it is picked. `status` and `list` report it naming every such
  file; `unearth`, `fork` and `push` refuse it; `rebuild` refuses it,
  naming the files to remove by hand; `forget` removes every file of the
  version's slots, the `.tar.zst` files first.
- **Newer format:** a listing whose first line names a higher listing
  format than antiq knows is neither incomplete nor damaged. `list` and
  `status` mark it "newer format", `grep` skips it with a warning,
  `unearth`, `fork` and `rebuild` refuse it with the error kind
  `format_too_new`, and `forget` takes it by its stamp. `push` neither
  refuses nor counts it.
- `push` refuses while either kind exists, naming the exact `forget` or
  `rebuild` command. It decodes every listing in full for that, in
  parallel like `grep`, and so does `status`. A scheduled push stops there
  until the user acts.
- A change of an archive file that keeps its size is not detected
  locally.

## Listing format

A listing is a SHA-256 checksum list, zstd-compressed (level 3, with the
content checksum), with antiq's metadata in comment lines. `shasum -a 256
-c` and GNU `sha256sum -c` both skip lines that start with `#`, also with
`--strict`. A comment after the path on the same line does not work: both
tools read it as part of the file name.

```text
# antiq listing 1
# piece         projects/scanner
# archived      2026-11-04T09:02:11Z
# archive       projects-scanner--2026-11-04T090211.tar.zst
# archive-bytes 233
# files         1
# bytes         4211
#
# dir   0755  2026-10-30T17:40:00Z                 -  src/
# link  0755  2026-10-30T17:41:00Z                 -  src/latest	-> main.rs
# file  0644  2026-10-30T17:44:02Z              4211  src/main-copy.rs
86a08606186d72ffc5c4c34cb07b4ae234333cce251383c907bdeed0f17ce9d0  src/main-copy.rs
# hard  0644  2026-10-30T17:44:02Z                 -  src/main.rs	=> src/main-copy.rs
86a08606186d72ffc5c4c34cb07b4ae234333cce251383c907bdeed0f17ce9d0  src/main.rs
```

- **Header:** `piece` (the stored spelling), `archived` (the version time,
  the one in the file name), `archive` (the archive file's name),
  `archive-bytes` (its size, written when packing finishes), `files` and
  `bytes` (count and total size of the regular files, so `list` and
  `status` read only the head of a listing). The writer derives `files`
  and `bytes` from the entries, and a full read refuses a listing whose
  counts differ from its entries. Each header line is `# `, the key padded
  with spaces to 14 characters, then the value; `piece` is spelled like a
  path in a comment line (see "Escaping in comment lines"). The header
  lines come in this order, after the first line `# antiq listing 1` (a
  listing whose first line names a higher number is of a newer format,
  see "Damage"), and end with a line holding only `#`. A version made by
  `import` adds `imported`, the real time of the import, and a listing
  written by `rebuild` adds `rebuilt`, both after `bytes` in this order,
  with a time written like `archived`.
- **Entry lines:** after `# `, four fixed-width fields: the type, left
  aligned in 4 characters; the mode as 4 octal digits; the UTC time with
  `T` in 20 characters; the exact size in bytes, right aligned in 16
  characters, or `-` for a type without a size. A listing cannot be
  written for a file of 10^16 bytes or more. Two spaces separate the fields and two spaces
  separate the size from the path, which runs to the end of the line and
  starts at byte offset 54. A link line separates path and target with one
  tab: `<path>\t-> <target>`. The time field holds the years 0001 to 9999;
  a listing cannot be written for a file with a time outside them.
- **Types:** `dir` (the path ends in `/`), `file`, `link`, and `hard` for
  a hard link, written `<path>\t=> <target>` with size `-`; the archive
  stores it as a tar link member, and it does not count in `files` or
  `bytes`, which count stored data once. Special files are not archived
  and have no line.
- **Checksum lines:** every `file` and `hard` line is followed by a
  standard checksum line for its path, so `shasum -c` checks hard links
  like any file.
- **Escaping in checksum lines:** a path containing a newline or a
  backslash gets a leading `\` on the line, with newline written `\n` and
  backslash `\\`; nothing else is escaped, a carriage return or tab stays
  raw. Both macOS `shasum` and GNU `sha256sum` read that. GNU's own `\r`
  escape is not used. The exception is a path that ends in a carriage
  return: `shasum` reads its line, while GNU `sha256sum` strips the
  carriage return from the line and looks for the path without it.
- **Escaping in comment lines:** never a leading backslash. Inside paths
  and link targets, backslash, newline, tab and carriage return are
  written `\\`, `\n`, `\t` and `\r`.
- A test listing with such names, a path ending in a carriage return
  apart, passes `shasum -a 256 --strict -c` and
  `sha256sum --strict -c` with exit 0.
- An archive unpacks to one top-level directory named after the folder
  when it was packed; the listing has no line for it, and its paths are
  relative to it. The hand check therefore runs inside that directory, for
  example after `tar --zstd -xf <archive file>`.

## Packaging

A Packager trait packs, verifies, lists and unpacks. Archives are tar
compressed with zstd, with zstd's content checksum. (ADR 0019)

Tar is written in-process with hand-built GNU headers: mode masked to
permission bits, uid and gid 0, empty owner names, the modification time
in whole seconds (the floor of the file's time, times before 1970 clamped
to 0 and reported), no extended attributes, entries sorted by path bytes,
hard links stored as links, special files skipped and reported. Members
follow the byte order of their paths below the top-level directory,
without the trailing slash of directories. A hard link names the first
path in that order that holds the same file as its target. A symlink's
target is stored as the bytes the filesystem holds. Each file is read
through a reader limited to the size in its header; a short read is padded
with zeros and marks the file as changed. Each file's SHA-256 is computed
in the same read and written to the listing. Sparse files are stored in
full; `archive` warns when a piece's apparent size exceeds its allocated
size by more than 1 GiB. (ADR 0050)

An archive holds one top-level directory, named after the folder when it
was packed, with the piece's entries below it. That directory is the first
member and has the mode and modification time of the piece's top
directory. Listings and the strict verify compare the paths below it, so
neither that name nor the folder's current name counts: a piece renamed by
`fork --name` or imported with `--as` compares like any other. Unearthing
extracts the directory's contents into the target. Each file, symlink and
directory is created by antiq itself through open directory handles, never
through a symlink, and a name that is already taken fails the extraction.
Directories keep owner access while their members are written; once all
members exist, the stored modification time and then the stored mode of
every directory are set, the deepest first, and the target's modification
time last. A symlink gets its stored mode where the platform can change it
(macOS) and keeps the platform's mode where it cannot (Linux). The
compressed stream is read to its end, so a failing content checksum or
data after the end of the tar stream fails the extraction. (ADR 0050)

Archives are compressed with zstd at level 19 with long-distance matching
and a 128 MiB window, at the default job size of 64 MiB. (ADR 0051) Every
decoder antiq creates sets the maximum window to 128 MiB.

All compression in a run shares one pool of one worker per core. A piece
smaller than one job gets one worker, larger pieces take up to the free
workers, and several pieces are compressed in parallel. The number of
compressors in flight is also limited by memory against 25 % of the
machine's physical memory: an archive encoder counts 448 MiB plus 160 MiB
per worker. When nothing else is compressing, one worker is always
granted, with a warning when the memory limit is below the cost of an
encoder with one worker. A compressor that finds no free worker or not
enough memory waits until another finishes, and waiting compressors are
served in the order they asked.

Symlinks are archived and restored as they are, whatever their target. An
archive with a member whose path leads outside the piece is refused, and
unearthing never writes through a symlink. (ADR 0027)

Before it writes anything, extraction reads the whole package and refuses
it for the first member that is absolute or has a `..` component; lies
outside the one top-level directory; is a kind other than file, directory,
symlink or hard link; appears twice; sits in a directory that an earlier
member did not create or in something that is not a directory (so never
below a symlink); lies more than 512 directory levels below the top-level
directory; is a symlink without a target or with a NUL in it; is a hard
link to anything but a regular file that an earlier member created in the
archive; or has a mode or modification time that does not parse or is
beyond 63 bits of seconds. Paths are judged by the names the archive
carries, including those of GNU long-name and pax records, and by their
exact bytes, so `a/x` is not placed in a directory `A`. Long-name,
long-link and pax records above 64 KiB, sparse members, and a pax `size`
that differs from the member's header size are refused as well, and so is
a package without members. The package is decompressed three times: once
for the records, once for the members and once to extract. (ADR 0050)

### Verifying

A strict verify gates archiving and unearthing. It reads the stream to its
end and checks its integrity. A cut or corrupt stream, a tar stream
without its end marker (two zero blocks after the last member), nonzero
data after the end of the archive, an empty archive, a first member that
is not a directory, a member outside the top-level directory, a member
path that is not a plain relative path, a path that occurs twice, a hard
link that does not name a file listed before it, a member that is neither
file, directory, symlink nor hard link and a modification time beyond 64
bits make the package damaged.

Then it compares every member with the tree on disk (type, size, content
hash, modification time in whole seconds with the same clamp, mode, link
target) and requires equal path sets under the same walk rules. The walk
is the one packing uses, so a tree with an unreadable entry or a mount
point is refused. A file with several names must be stored as packing
stores it: the first path in byte order is the file and the others link to
it. Ignored paths are left out on both sides. Special files are not
archived and are not compared. Symlink modes are not compared, since Linux
cannot set them.

On a user's folder, directory modification times are not compared, the
top-level directory's included. The top-level directory's mode is compared
with the mode of the first member. On antiq's staging, directory
modification times are compared, since the final pass of the extraction
sets them from the package, and the top-level directory's modification
time is compared but not its mode: the staging root keeps the mode its
maker gave it until the piece is moved into place. A staging directory
whose mode keeps its owner out gets owner access for the read, and its
mode is put back afterwards. A user's folder is never changed.

While it reads, the verify records each entry's lstat (kind, size,
modification time in nanoseconds, ctime, inode), special files and the
top-level directory included, and the members of the package in archive
order. A file that is replaced, resized or touched while it is read is
reported as changed while read. (ADR 0033)

## Operations

Every command prints readable text on a terminal and has `--json`. Data
goes to stdout, messages to stderr.

**Dry runs.** Every command that changes something has `-n`/`--dry-run`:
`init`, `archive`, `unearth`, `fork`, `forget`, `rebuild`, `import`,
`pin`, `unpin` and `push`. A dry run takes no lock and changes nothing.
It computes the repairs read-only and lists them first, as actions of
kind `repair` (each naming the version or tree it would delete, rename
back or move to `recovered/`); its other actions assume the repairs were
done. A dry run that finds the lock held says so, names the holder and
lists no repairs, since the running command owns those directories. It
exits 0, and under `--json` the plan carries `pid` and `started`. Each
command's section says what else its dry run prints. On the console a
plan is one `Would ...` line per action, or `Nothing to do.` when there
is none. Under `--json` the plan is JSON (see "JSON output"). (ADR 0046)

**Lock.** Every command that changes the collection (`archive`,
`unearth`, `fork`, `forget`, `rebuild`, `import`, `pin`, `unpin`) and
`push`, for its whole run, takes the exclusive lock `.antiq/lock` and
writes its pid, machine name and start time into it. A command that finds
the lock held fails at once with exit 1 and names the pid and start time,
when the lock file has them: a run that has taken the lock but not yet
written them is named by neither. `list`, `grep` and `status` take no
lock and say when it is held, read from the pid in the lock file without
trying the lock; the note is absent in the moment between a run taking
the lock and writing the file. A collection on an SMB or NFS mount gets a
warning.

**Repairs.** Every locked run, `push` included, first runs the repairs
(see "Repairs at the start of a run"). A repair that fails is a warning
naming the path and does not fail the run.

**Free space.** `archive` and `import` need room in `.antiq/` for a
piece's apparent size, counted over the entries that are packed; parallel
pieces reserve their sum, and `archive` works on as many pieces at a time
as the machine has cores. A piece's reservation lasts until its version
is stored or removed. `unearth` and `fork` need room for the listing's
`bytes`; `fork` of a version without a usable listing skips the check
with a warning. A piece without room is skipped before staging, and
`ENOSPC` during a step removes that step's staging and skips the piece
(exit 3). The exception is the move into the store (`archive` step 4)
after a file has moved and the undo failed: the files and the staging
stay for repair 1.

Moving a piece's top directory to another parent (into `trash/`, into
place) adds owner write to it first and restores the recorded mode right
after.

**Exit codes.**

| Exit | Meaning |
|------|---------|
| 0 | success |
| 1 | error, a held lock included |
| 2 | usage error |
| 3 | partial: a step failed for some pieces (packing, verifying, moving, a difference in step 6 of `archive`, a refused rename, a failed deletion, no room) |
| 4 | target unreachable (only `push`, and 0 instead under `--if-reachable`) |

Pieces the walk refuses do not change the exit code; `status` and the
command's output name them.

**Naming a version.** `unearth` and `fork` take the version as the flag
`--version <version>`; without it they take the newest version. `forget`
and `rebuild` take it as a positional argument. Text output shows local time with
seconds, `--json` and pipe output UTC (RFC 3339 in JSON, the version stamp
in pipes). The input's spelling decides how it is matched, and the two are
never mixed:

- an input containing `T` (`2026-11-04T090211`, optional `-N`) is matched
  against the version stamps;
- any other input (`2026-11-04 10:02:11`, or a date alone such as
  `2026-11-04`) is matched against each version's time rendered in the
  machine's current zone, as a prefix of that text. Near midnight a local
  date can differ from the UTC date of the stamp.

An exact match wins over prefix matches; several matches are an error
naming each version's stamp. When two versions of one piece render alike
in local time (the repeated hour when daylight saving time ends), text
output adds the UTC offset. An incomplete version matches only by stamp.
(ADR 0060)

## Repairs at the start of a run

There is no `reconcile` command. Every locked run (`push` included) first
runs these repairs, in this order; a dry run lists them instead. A repair
that fails is a warning naming the path. Whenever a tree leaves `trash/`
or `doomed/`, the parents that became empty go too. (ADR 0059)

1. **Interrupted archive step 4:** for each
   `.antiq/tmp/pack/<stamp>/<piece path>/` whose sidecars already lie in
   `.antiq/pieces/<path>/` without their `.tar.zst`: delete those
   sidecars. The folder is still live, since step 5 waits for step 4.
2. **Temporary files:** delete what `.antiq/tmp/` holds.
3. **`doomed/`:** delete every tree there without a comparison. Directory
   chains that hold no non-directory entry are removed as they are.
4. **`trash/`:** rename every tree there back to its live path without a
   comparison, or, when that is not possible for any reason (path taken,
   a parent missing or not a directory, the rename refused), to
   `.antiq/recovered/<stamp>/<piece path>/`; the chain inside
   `recovered/` is created and flushed. Directory chains that hold no
   non-directory entry are removed as they are. The piece path is the
   shortest path below `<stamp>/` for which a version with that stamp
   exists, matched by the volume's name equality (ADR 0054); when no
   version has that stamp, the first component below `<stamp>/` goes back
   as it is, which usually lands in `recovered/`. `status` names every
   folder in `recovered/`.
5. **Died unearth:** for each live piece whose `.antiq-restored` names a
   version stamp, delete what is left of that version of that piece (the
   `.tar.zst` first, if it still exists, then its sidecars), and rewrite
   the marker to the time alone. A stamp that names nothing is simply
   dropped from the marker. A version copied back by hand between a crash
   and the next run is deleted, and the repair names it.
6. **Empty store folders:** remove folders under `pieces/` that hold
   nothing.

Sidecars without a `.tar.zst` and without a staged archive file in
`tmp/pack/` are an incomplete version and stay (see "Damage"). The
repairs name every version and tree they delete or move.

## Commands

### init

Makes a directory a collection: writes `.antiq/config.toml` with a new
collection ID and the patterns given, without asking. `--pattern`
(repeatable) sets the patterns; every other setting starts at its default
and is changed in the config file. An invalid `--pattern` is a usage
error: kind `usage`, exit 2. `init` creates `.antiq/` with `config.toml`,
`tmp/`, `trash/`, `doomed/`, `recovered/` and `pieces/` in one step, so it
exists complete or not at all. It does not look for the layout of
`archive.sh`; its old archive files are brought in by hand with `import`.

It refuses, also under `-n`, creating nothing and exiting 1, where
`.antiq/` already exists, inside another collection (naming its root),
where `.antiq` is a file or a symlink, and where a folder that would be a
piece holds a `.antiq/` directory of its own, naming each such folder.
`-n` prints the config it would write.

After creating the collection, `init` prints how many folders the next
`archive` would archive.

### archive

`archive [<piece>]`. Without an argument, archives every live piece that
is due. With a piece, archives that one now, however recently it changed.
`<piece>` is a live piece's path as the patterns select it, or a search
word that matches exactly one live piece (see "Piece references");
anything else is refused. Only the idle check is skipped: a pinned piece
is refused until it is unpinned, and the walk refusals and the empty rule
still apply. `-n` lists the pieces it would archive and the ones it
refuses, with the reason.

1. Find the pieces and judge each with the rules of "Selection", "Walks",
   "Ignore and timeless rules" and "Idle".
2. Pack the piece into `.antiq/tmp/pack/<version stamp>/<piece path>/`,
   under the final file names: the `.tar.zst`, the listing and the
   `.tar.zst.sha256`.
3. Verify: read the whole archive file back and compare it with the
   listing and the folder (see "Verifying"). The hash of the archive file
   read back must equal the hash computed while writing it, and the
   listing of the packing must equal the members the strict verify reads.
   A folder that changes meanwhile is skipped.
4. Move the listing, then the `.tar.zst.sha256`, then the `.tar.zst` into
   `.antiq/pieces/<path>/`, flushing each and the directory.
5. Rename the folder to `.antiq/trash/<version stamp>/<piece path>/`,
   under its live spelling (one rename on the same device; a piece on
   another device counts as containing a mount point and never gets
   here). If the rename is refused anyway, step 4 is undone: the
   `.tar.zst`, then the checksum file, then the listing go back to the
   staging, flushed as in step 4 (repair 1 finishes this after a crash);
   the folder stays, the piece is reported with the error, and the run
   exits 3.
6. Walk the trashed tree again and compare it with the record of the
   verify: per entry type, size, modification time, inode and change time,
   directories included; the top only by type, inode and modification
   time, since the rename changed its change time. A path found on one
   side only is a difference. This walk enters ignored subtrees for the
   same refusals as the walk before packing (any refusal counts as a
   difference); ignored entries and `.antiq-restored` at the top are not
   compared. On any difference rename the tree back; when that is not
   possible for any reason (the path is taken, a parent is missing or not
   a directory, the rename is refused), move it to
   `.antiq/recovered/<version stamp>/<piece path>/` instead (the chain
   inside `recovered/` is created and flushed) and list it in `status`;
   the version stays.
7. Rename the tree to `.antiq/doomed/<version stamp>/<piece path>/`,
   flush, and delete it from there. A deletion that fails leaves the tree
   in `doomed/`, where the repairs try again; `status` names the tree and
   its first entry in walk order, with a cause it can read (a locked flag,
   a directory it cannot write). The error itself appears in the output of
   the run that failed and in the warning of each later run's repair 3.
   Whenever a tree leaves `trash/` or `doomed/`, the parents that became
   empty are removed, up to and including `<stamp>/`.
8. One output line per folder, a summary at the end.

Running out of file descriptors in the walk of a piece ends the run:
pieces already running finish, the piece and every piece not yet started
are skipped with reason `error`, one warning names the cause, and the run
exits 3.

A write after the walk of step 6 is lost. After the rename into `trash/`,
only a process that held a file of the folder open before the rename, or
has its working directory inside it, can still write. The deletion lists
each directory before it deletes the entries. A file created after the
deletion listed its directory survives; the deletion then fails and the
next run finds it.

Pieces that share hard-linked files change each other's change times when
one of them is removed. When such pieces are archived in the same run, the
outcome of each depends on timing: it can be given back instead of
removed, or skipped with reason `differs`.

A crash between steps 4 and 5 leaves the live folder next to a complete
version of it; the next run may archive the unchanged folder again, which
gives a second, identical version that `forget` removes. The same holds
for a crash between steps 5 and 6: the repairs give the folder back, since
`trash/` holds only folders not yet checked again.

`archive` checks the names of the members against the rules of macOS and
against those of Linux and warns about each name that one of them would
refuse. (ADR 0054)

### unearth

`unearth <piece> [--version <version>]` brings a version of a piece back
as a folder and **removes that version** once the folder is complete and
verified. The folder is an ordinary live folder again. Archiving it later
creates a new version. `-n` names the version, the destination path and the
checks it would make. (ADR 0061)

- `<piece>` is a path, or a search word that matches exactly one piece
  (see "Piece references"); anything else is an error that lists the
  candidates. Without `--version` the newest version is taken.
- A damaged version is refused, naming `fork` (which extracts it with a
  warning and keeps it), `rebuild` and copying the missing file back from
  the target.
- A version whose top holds a non-file with a marker's name is refused.
- The destination is the piece's stored path. It is refused, and nothing
  changes, when it is not one the patterns select, is excluded or lies
  below an excluded folder, lies in `.antiq/`, inside another piece or
  below a file or symlink, or when anything is at it, a dangling symlink
  included. Nothing is followed through a symlink. A folder above the
  path that the volume finds under another spelling than the path's
  (another case, or another Unicode normalisation) refuses too. Folders
  above the destination that do not exist are created; a failure before the
  rename removes what was created.
- **Order:** extract into `.antiq/tmp/extract/<n>/`; verify every entry against the
  listing; write `.antiq-restored` at the staged top; flush every
  extracted file and directory; check the destination path; rename into place;
  flush the parent; delete the version (`.tar.zst` first, then the
  listing and the checksum file, flushing between); rewrite
  `.antiq-restored` to the time alone. No version is deleted before the
  folder replacing it is durable. The extracted tree is flushed as
  follows. On macOS every extracted file and directory gets a plain
  `fsync`, then one `F_FULLFSYNC` on a file of the volume of `.antiq/tmp/`
  runs before the rename. On Linux one `syncfs` of that volume runs, on a
  descriptor opened before the extraction starts. On a kernel before 5.8,
  and where `F_FULLFSYNC` or `syncfs` is refused, every file and directory
  gets `sync_all`.
- `.antiq-restored` holds `restored <UTC time> version <stamp>` between
  the rename and the deletion of the version; if a crash falls in
  between, the repairs finish the unearth (see "Repairs at the start of a
  run"). The idle clock starts at its time.
- There is no reclaim: an unearthed folder that is archived again,
  changed or not, becomes a new version.

Before extracting, `unearth` checks every member name against the volume it extracts to:
names it cannot hold, name lengths, and names that collide under its case
and Unicode rules, which come from a probe of the volume of `.antiq/tmp/`. It
refuses the piece with a list of the offending names and the reason. A
name is too long when it exceeds 255 bytes on Linux or 255 UTF-16 code
units, counted on the name as stored, on macOS; a volume that stores names
decomposed (HFS+) counts them in that form. macOS also refuses a name that
is not valid UTF-8. A symlink whose target is longer than 1023 bytes on
macOS or 4095 bytes on Linux is refused for the same reason. The probe
also tries to create a hard link and a symlink on that volume; a
hard link or symlink member of the package is refused, with the others,
when the volume has none (exFAT has no hard links). Two members collide
when their paths are equal after Unicode normalisation (where the volume
ignores it) and full case folding (where the volume ignores case); the
refusal lists both names. A member more than 512 directory levels below
the top directory is refused, since the walk, the strict verify and the
removal of staging go no deeper. The names of the destination path are held to
the same rules as directories, and on a volume that stores names
decomposed a folder made above the destination, or the destination, is refused when
its name would be stored in another form than it is written. Paths are not
checked for length: a path longer than the platform's path limit is
extracted directory by directory. (ADR 0054)

### fork

`fork <piece> [--version <version>] [--name <new path>]` extracts a
working copy of a version, which stays unchanged. Without `--name` the
copy goes to the original path. The same destination rules and staging as for
`unearth` apply; when the original path is taken, the refusal names
`--name` as the way out. A damaged version is extracted with a warning and
without the comparison against the listing. `fork` writes
`.antiq-restored` with the time alone (no version stamp, so the repairs
never treat it as an unearth). Archived later, the copy becomes a new
version (same path) or a new piece (new path); without edits that is an
identical version, once due or at once with `archive <piece>`. `-n` names
the version and the destination path. (ADR 0061)

### forget

`forget <piece> (<version> | --all-versions --force)` deletes exactly one
version: the `.tar.zst` first, then the listing and the checksum file. It
also takes the stamp of an incomplete version and removes its sidecars,
saying that the version was incomplete. Deleting every version of a piece
takes `--all-versions --force`, which also removes the piece's incomplete
versions and says so for each. A crash inside `forget` can leave an
incomplete version; running the same `forget` again removes it. The next
`push` mirrors the deletion to the target. `-n` names the files it would
delete.

### rebuild

`rebuild <piece> <version>` repairs a damaged version by writing only what
is missing, after checking the archive file against what survives. It runs
only on the user's request, never as a repair. (ADR 0062)

- A listing is regenerated by the rules `archive` uses, from the tar
  members: mode and whole-second time from the header, size and hash from
  reading each member, `hard` for tar link members with the first path as
  target, no line for the top-level directory; `files`, `bytes` and
  `archive-bytes` come from the same read.
- Listing missing, checksum file present: hash the archive file and
  compare it with the checksum file. On a mismatch refuse (exit 1, naming
  both hashes); otherwise write the listing, with a `rebuilt` line,
  and say that the archive is the one checksummed when it was stored.
- `.tar.zst.sha256` missing, listing present: verify every member against
  the listing, as `unearth` does. On any difference refuse; otherwise
  write the checksum file.
- Both missing: read the archive in full, write the listing (with
  `# rebuilt`) and the checksum file, and say that this shows only that
  the archive reads without error, not that it is unchanged.
- An archive file whose size differs from its listing's `archive-bytes`
  cannot be repaired by `rebuild`, since the hash against the surviving
  checksum file can only fail; `rebuild` refuses and says so.
- A sidecar that is present but corrupt (see "Damage") is not
  overwritten: `rebuild` refuses and names deleting that file (after which
  it is missing and can be rebuilt) or copying it back from the target.
- Every refusal names `forget` and copying back from the target as the
  ways out. A version that is not damaged is refused. It takes the lock
  and has `-n`, which says what it would check and write.

### import

`import <folder> [--as <path>] [--time <time>]` packs a folder from
outside the collection directly as a version of `<path>` (default: the
folder's name), with steps 1 to 4 of `archive`; the source folder is left
in place.

- The source must not lie inside the collection.
- `<path>` must be one the patterns select, not excluded, not in
  `.antiq/` and not inside another piece. A pin on a live folder at
  `<path>` does not block the import.
- The idle check does not apply. The source's top is treated as a piece's
  top: `.antiq-restored` there is left out, `.antiq-pin` there refuses the
  import, and a source without a countable file is refused as empty, also
  with `--time`.
- The ignore and timeless rules of the collection and the folder's own
  rule files apply, and so do the walk refusals.
- The version time is the newest counted modification time in the folder,
  or `--time`. `--time` follows the rules for naming a version: with `T`
  a UTC stamp, otherwise a local date-time with seconds
  (`2026-11-04 10:02:11`) in the current zone; a date alone is refused. A
  version time in the future is refused, naming the file.
- The listing carries `imported <time>`, the real time of the import.
- This is also the way to bring in old archive files: unpack them with
  their original timestamps somewhere temporary, then import each folder.
  (ADR 0009, ADR 0055)
- `-n` names the version it would create and what it would leave out.

### list

`list [query]` prints one row per version, newest first within a piece;
the query matches piece paths fuzzily. Each row reads only the head of its
listing. SIZE is the archive file's size (`archive-bytes`) in decimal
units (1 GB = 10^9 bytes); `--json` also carries the unpacked size and the
file count. Damaged and incomplete versions are marked. The piece is in
every row, so each row stands on its own after `sort` or `grep`. With no
version, or no piece matching the query, `list` exits 0 and says so on
stderr. A held lock is noted on stderr, and under `--json` in a `note`
field. (ADR 0021)

```text
$ antiq list scan
PIECE              VERSION               SIZE     FILES
projects/scanner   2026-11-04 10:02:11   1.1 GB   48 211
projects/scanner   2026-09-03 12:15:00   1.0 GB   47 980
tools/scan-old     2025-02-11 19:40:07    40 MB      812
```

### grep

`grep <pattern>` searches the paths of every archived file, as
`<piece>/<path>`, in all listings of existing versions (incomplete
versions are skipped), with a regular expression (`regex::bytes` over each
decoded listing, the files in parallel on all cores but two). A listing
that does not decode is skipped with a warning naming the version; the
exit code stays 0.

On a terminal the hits are grouped by piece, each in-piece path once with
the versions that hold it, the match highlighted:

```text
projects/scanner                       2 versions, newest 2026-11-04
  src/main.rs                          both versions
  tools/bench/main.rs                  2026-09-03
tools/scan-old                         1 version, 2025-02-11
  cli/src/main.rs                      2025-02-11

3 files in 2 pieces · antiq unearth <piece> [--version <version>]
```

The pattern is matched against the unescaped path bytes; text output shows
the path with the comment-line escapes, the pipe form prints the raw path.
A match wholly inside the piece path gives one row, "piece name matches",
with the version and file counts and no file list. In a pipe every hit and
version is one tab-separated line `piece  stamp  path`, with the full
version stamp (`-` as the path for a name match). With no hit `grep` exits
0 and says so on stderr; a pattern that is not a valid regular expression
is a usage error, exit 2. A held lock is noted as for `list`.

### status

`status` is local only: it takes no lock and writes nothing below
`.antiq/`. It prints one row per piece that has a state, sorted by path,
the state coloured in the first column: `due` (archived on the next run),
`soon` (due within `soon_days`), `pinned`, and `refused` for the pieces
`archive` refuses, with their reason (mount point, nested collection,
unreadable entry, exclude entry, locked entry, unreadable marker, a
control file that is not a file, broken rule file). Plain live folders are
only counted in the header. Below the table: files and markers with times
in the future, damaged and incomplete versions, folders in `recovered/`,
trees left in `trash/` and leftovers in `doomed/`, live folders whose
spelling differs from the stored one, directories that selection could not
read ("unreadable: <dir>"), and files in store folders whose
names do not parse.

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

### pin and unpin

`pin <piece>` and `unpin <piece>` create or remove `.antiq-pin` at the top
of a live piece (see "Pins"). `-n` says which.

## Push

`push [--init] [--force] [--if-reachable]` mirrors the collection to the
target. `--init` and `--force` are each refused together with
`--if-reachable`. The steps run in this order.

1. **Repairs** run first, under the lock (see "Operations").
2. **Check the store.** While a damaged or incomplete version exists,
   `push` refuses with exit 1 and prints the exact `forget` or `rebuild`
   command for each. It also refuses, naming both, when two piece paths
   or two store folders of the collection differ only in case or Unicode
   normalization. (The same clash between file names inside one live
   folder is not checked.) And it refuses while `.antiq/trash/` still
   holds a tree after the repairs.
3. **Check rsync.** The local rsync's `--version` must name rsync 3.2.4 or
   newer, the release that added `--fsync`. openrsync, which macOS ships
   as `/usr/bin/rsync`, and older versions are refused with a message
   naming the binary and the config key `[target] rsync`. A target whose
   own rsync is openrsync (any macOS machine) works with `fsync = false`.
4. **Pre-check the target.** The target path is canonicalized first. A
   path target that is the collection root or lies below it (compared by
   the volume's name equality, ADR 0054) is exit 1, also with `--init`,
   and so is a path target that is a mount root ("use a folder on the
   volume, not its root"). Otherwise the checks use the file system for a
   path target and `rsync --list-only <target>/` for an ssh target:

   | Situation | Result |
   |---|---|
   | ssh cannot connect | unreachable |
   | path target answers ENOTCONN, EHOSTDOWN, EHOSTUNREACH, ENETUNREACH or ETIMEDOUT | unreachable |
   | path target directory does not exist | unreachable, also with `--init` |
   | ssh target directory does not exist | exit 1, unless `--init` |
   | directory present, no `.antiq-target`, empty | unreachable with a warning naming `push --init`, unless `--init` |
   | directory present, no `.antiq-target`, not empty | exit 1; `--init` is refused |
   | `.antiq-target` of another collection, or not readable | exit 1 |
   | `.antiq-target` of this collection naming another host or another collection path | exit 1, naming both; `--force` pushes anyway and leaves the marker as it is |

   When the listing shows `.antiq-target` on an ssh target, it is fetched
   with one rsync run of `<target>/.antiq-target` into `.antiq/tmp/`, with
   the same options as the marker run below; a fetch that fails, or
   content that does not parse, is "not readable". `--force` changes
   nothing in the other rows: no marker still needs `--init`, another
   collection's marker is still exit 1, and with a matching marker it is a
   plain push, noted in the output.

   rsync runs with `LC_ALL=C`. The listing carries the mirror's `-e`,
   `--rsync-path`, `--timeout` and `rsync_args`, but not `--fsync`. Over
   ssh, exit 255 counts as "cannot connect" only when ssh's line starts with `ssh: connect to host` or
   `ssh: Could not resolve hostname`, or contains `timed out`; any other
   255 (a changed host key, a refused key) is exit 1 with the line. Exit
   23 counts as "does not exist" only when the failing `change_dir` or
   `link_stat` line ends in `(2)`, or openrsync's `(l)stat:` line ends in
   `No such file or directory`; any other 23 is exit 1.

   Unreachable means exit 0 under `--if-reachable`, otherwise exit 4, and
   the reason is printed in text and `--json` every time. A missing
   directory of a path target is never created by antiq: the user creates
   it on the mounted volume.
5. **Marker.** `.antiq-target` sits at the target root, outside the
   collection. It is one line of JSON with a fixed key order,
   `{"format":1,"id":"<collection ID>","host":"<machine name>","path":"<collection path>"}`.
   The machine name is the one antiq detects (the ComputerName on macOS,
   the hostname elsewhere), never a network-dependent host name; the path
   is the canonical collection root, compared byte for byte. Unknown keys
   are ignored, a higher `format` is refused as written by a newer antiq.
   The marker is created only by `push --init` and never rewritten by
   antiq; after a renamed machine or a moved collection the user edits its
   `host` or `path` by hand, or pushes with `--force` each time. `--init`
   is the user's statement that this empty target is the right one;
   `--force` that this copy may overwrite the target once, after the user
   made sure the target holds nothing newer. A forced push leaves the
   marker as it was, so the machine it names pushes on as before. The
   marker is sent by its own rsync run, as a temp directory in
   `.antiq/tmp/` holding only `.antiq-target`, sent to `<target>/` with
   `rsync -r --chmod=F644` (no `-a`, `-p`, `-t` or `--delete`, so the
   target root keeps its mode and time) plus the same `-e`,
   `--rsync-path`, `--timeout`, fsync setting and `rsync_args` as the mirror; the
   marker arrives with mode 0644 masked by the receiving side's umask. For
   an ssh target that creates the last missing directory level, and a
   deeper missing path is exit 1 with rsync's mkdir line. `push -n` never
   writes it. (ADR 0031)
6. **Mirror.** One rsync run, antiq's options first, then the options of
   `[target] rsync_args` verbatim, then the two paths:

   ```text
   rsync -a -H -x --delete --delete-delay --itemize-changes
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
   antiq's ssh command, BatchMode included. The excludes are anchored at
   the collection root, so the same names inside a live piece are still
   copied; the exclude of the marker also protects the target's marker
   from `--delete`. The macOS volume-root directories and `.DS_Store` at
   the root are left out. `--timeout` counts seconds without data, and
   every rsync run of `push` carries it, the listing and the marker runs
   too; rsync's exit 30 (timeout) is exit 1 with its line. `-x` keeps rsync on the collection's
   device: a mount point to another device inside a live piece arrives as
   an empty directory with the mounted directory's attributes; a bind
   mount of the same device is mirrored like a folder. `-H` keeps hard
   links. With `--delete-delay`, deletions happen only at the end, so an
   interrupted run removes nothing that was on the target before.
7. **Second pass.** When the run's output has a line starting with
   `*deleting`, `push` runs the same command with `-n` and, if that prints
   any itemized line, the mirror once more.
8. **Result.** rsync exit 0 is success; 24 (files vanished during the run)
   is success with a note naming the paths of the `file has vanished`
   lines. Everything else, including any failure after the pre-check, is
   exit 1 with rsync's message, also under `--if-reachable`. With exit 23
   and the line `IO error encountered -- skipping file deletion`, push
   says that no deletion reached the target and names the directories
   rsync could not open. Exit 12 with ``unrecognized option `--fsync'``
   (openrsync) or `--fsync: unknown option` (older rsync) is reported as:
   the target's rsync (openrsync, or older than 3.2.4) lacks `--fsync`;
   set `[target] rsync_path` to a 3.2.4+ rsync there, or `fsync = false`.
9. `-n` runs the checks and summarizes rsync's `--itemize-changes` per
   piece.

With `--fsync` the target flushes each file's content before it is renamed
into place; a power loss can then at most lose the newest names, which the
next push sends again. Without it, files rsync reported as written can be
empty or missing. Beyond that antiq relies on rsync's own check: rsync
computes a whole-file checksum of each file it transfers while writing it
and compares it with the sender's. Files rsync does not transfer, because
size and modification time match, are not compared, so damage on the
target stays unseen. antiq reads nothing back. Nothing records when the
last push ran.

`push` copies live folders whole, ignored paths such as `node_modules/`
included; the ignore rules apply to versions only. It mirrors the whole
collection, content outside every piece included. (ADR 0041)

`push` has no brake: whatever is deleted locally, by antiq or by hand, is
deleted on the target by the next push. The target is a mirror, not a
backup with history. (ADR 0049)

`--if-reachable` makes `push` exit 0 when the target is unreachable as
defined above, for scheduled runs. Scheduling runs the two commands one
after the other, such as `antiq archive; antiq push --if-reachable`, so
the push runs after a partial archive too (the scheduler then sees only
the push's exit status); there is no command that does both. A held lock
is exit 1, also under `--if-reachable`.

## Configuration

The configuration is per collection, in `.antiq/config.toml`:

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

| Key | Default | Content |
|-----|---------|---------|
| `collection.id` | minted at `init` | collection ULID |
| `collection.patterns` | `["*"]` | selection patterns |
| `collection.exclude` | `[]` | literal paths that are never pieces, in the escaped form |
| `collection.ignore` | `[".DS_Store"]` | gitignore lines for ignored paths, anchored at each piece's top |
| `collection.timeless` | `[]` | gitignore lines for timeless paths, anchored at each piece's top |
| `collection.idle_days` | 30 | days a piece must be idle before it is due |
| `collection.soon_days` | 3 | days before due that `status` shows `soon` |
| `target.url` | none | an absolute path or `[user@]host:path` |
| `target.rsync` | rsync from `PATH` | path of the local rsync |
| `target.rsync_path` | none | the target's rsync, passed as `--rsync-path` |
| `target.fsync` | `true` | pass `--fsync` |
| `target.rsync_args` | `[]` | extra rsync options |

A path target may be any absolute path; the path of an ssh target is
passed to rsync verbatim (rsync 3.2.4 and later protect arguments by
default). A `url` starting with `/` is a path. A `url` does not start with
`-`, and the host of an ssh target does not hold a `/`. The user, the host
and the path of an ssh target are not empty when given. A path after the
host that starts with `:` or `//` is refused, since rsync reads
`host::module` and `rsync://host/module` as daemon syntax. An IPv6 host is
written in brackets, `[::1]:path`. Unknown keys under `[collection]` or
`[target]` are ignored, and each one is reported in a warning.

The keys of the earlier design are refused with all their names in one
error: `junk`, `remote_only_days`, `backup_live`, `live_exclude`,
`replace_without_asking`, `brake_percent`, `grace_hours`,
`away_warn_days`, `memory_percent`, `remote_jobs`, `live_retries`, the
`[machine]` table and the `[[remote]]` list.

### Setting up a target

Put the `[target]` keys into the config, `url` at least, as in the example
above. Then run `push --init` on an empty directory of the target; it
writes the marker (see "Push"). `push` without `url` fails with the error
kind `no_target`, exit 1, naming the key and `push --init`. A `url` that
is neither an absolute path nor `[user@]host:path` is a configuration
error naming the key, exit 1.

## Command line

| Command | Does |
|---------|------|
| `init [--pattern <glob>]... [-n]` | makes a directory a collection |
| `status` | shows the pieces that have a state |
| `archive [<piece>] [-n]` | archives due pieces, or one piece now |
| `unearth <piece> [--version <version>] [-n]` | brings a version back and removes it |
| `fork <piece> [--version <version>] [--name <new path>] [-n]` | extracts a working copy of a version |
| `forget <piece> (<version> \| --all-versions --force) [-n]` | deletes a version, or all of them |
| `rebuild <piece> <version> [-n]` | repairs a damaged version |
| `import <folder> [--as <path>] [--time <time>] [-n]` | packs a folder as a version |
| `list [<query>]` | lists the versions |
| `grep <pattern>` | searches the paths of all archived files |
| `pin <piece> [-n]`, `unpin <piece> [-n]` | create or remove a piece's `.antiq-pin` |
| `push [--init] [--force] [--if-reachable] [-n]` | mirrors the collection to the target |

(ADR 0023)

Every command takes `-C <path>` and `--json`. A flag a command does not
take is a usage error. No command asks a question. (ADR 0020)

### Piece references

A piece reference is a path or a search word. A path is resolved against
the current directory, normalised, and taken relative to the collection
root; a path outside the collection is refused with the error kind
`outside_collection`, exit 1. `.` and `..` in a path are resolved from its
text, and a `..` that would go above the root of the filesystem is
refused; `--as` and `--name` are normalised the same way. Only the
collection root itself is recognised by device and inode, so another
spelling of it, such as `/var` and `/private/var` on macOS, still counts
as the root; symlinks below the root are never followed.

An input that names a candidate piece as a path is that piece. On a volume
that treats other spellings of a name as the same (see "Spelling"), a path
in another spelling names the candidate whose folder it leads to. Any other
input is a search word, matched with the same fuzzy pattern `list` uses
(smart case, smart normalization) against the candidates' paths in their
escaped form, and must match exactly one candidate.
No match is the error kind `unknown_reference`; several matches are
`ambiguous_reference`, listing the candidates. Both exit 1. The
candidates are the live pieces for `archive`, `pin` and `unpin`; the
pieces of the store for `unearth`, `fork` and `rebuild`; the pieces of the
store and the store folders that hold sidecars only for `forget`.

### Console output

The output adapts to the terminal's width: on a terminal of 100 columns or
more tables use their wide layout, with full paths; on a narrower one they
use their narrow layout, in which a path that does not fit is shortened in
the middle, keeping its start and its end. Without a terminal the width is
80. Colour is used only on a terminal, and not when the environment
variable `NO_COLOR` is set to a non-empty value. (ADR 0056)

Data goes to stdout, messages to stderr. Under `--json` an error is data:
its JSON goes to stdout, like any other JSON output, and a usage error too
prints the JSON envelope (kind `usage`) and exits 2. When the reader of
stdout or stderr is gone, antiq stops writing to that stream without a
panic, and the run ends with the exit code it would have had.

### JSON output

Every JSON output is one object `{"format": 1, "command": "<name>", ...}`
on one line of stdout. `<name>` is the command's name. A command line that
fails to parse before naming a command has `"command": null`. An error is
`{"format": 1, "command": ..., "error": {"kind": ..., "message": ...}}`,
where `message` is the text of the error without the context lines a
console shows. Some kinds add keys to the error object. A `--dry-run` plan
carries `"dry_run": true` and an `actions` list, in that order; repairs
come first in it, as actions of kind `repair`. Paths are in the escaped
form.

| Kind | Exit | Raised when | Added keys |
|------|------|-------------|------------|
| `usage` | 2 | the command line does not parse, a `--pattern` of `init` is invalid, the `--name` of `fork` is not a path, a pattern of `grep` is not a valid regular expression, or `--init` or `--force` is given with `--if-reachable` | |
| `not_a_collection` | 1 | a command runs outside a collection | |
| `ambiguous_reference` | 1 | a piece reference matches several candidates | `candidates`: list of `{path}` |
| `unknown_reference` | 1 | a piece reference matches no candidate | |
| `outside_collection` | 1 | a path given as a piece, `--as` or `--name` lies outside the collection | |
| `lock_held` | 1 | another run holds the lock | `pid`, `started` (RFC 3339, whole seconds); neither while the holder has not written them yet |
| `target_unreachable` | 4 | the target cannot be reached | |
| `no_target` | 1 | `push` runs without `[target] url` | |
| `not_implemented` | 1 | a command is not built yet; removed once every command is | |
| `format_too_new` | 1 | `unearth`, `fork` or `rebuild` meets a listing of a newer format (see "Damage"), or `push` meets a marker with a higher `format` | |
| `other` | 1 | any other failure | |

`--help` and the top-level `--version` print text and exit 0, also under `--json`.

`list` adds the unpacked size and the file count of each version, and a
`note` field while the lock is held. The keys that the other commands add
are not yet established.
