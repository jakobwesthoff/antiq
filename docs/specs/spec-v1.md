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
("unreadable: <dir>") and `archive` names it in a warning of the same
text, also in a dry run. The exit code stays 0.
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
marker of a nested collection (the name is compared without regard to the
case of ASCII letters, since a volume may ignore case): `status` names the directory that holds the
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
or a directory inside the piece that the user does not own and cannot
write, is locked: it is never archived, and `status` names it ("locked:
<path>"). A directory the user owns but has made unwritable is not locked,
since removal gives itself owner access. The directory that holds the
top is locked when it carries such a flag or when the user cannot write
it, whoever owns it, since antiq changes nothing outside the piece.
`status` names it as "locked: <path>", the path relative to the collection
root, `.` for the root itself.

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
with or without its line end, and is read only as a regular file at the
piece's top. The time is written `YYYY-MM-DDTHH:MM:SSZ`, whole seconds in
UTC, and no other spelling is read. `unearth` and
`fork` write it (see there).

Markers (`.antiq-restored`, `.antiq-pin`) are written in `.antiq/tmp/` and
renamed over their place, then the piece's top is flushed; a leftover
stays in `tmp/`, which the repairs empty. A top without owner write gets
it for the rename and its recorded mode back right after. A top on another
device than `.antiq/tmp/`, and a top whose rename fails with `EXDEV`
(a bind mount shares the device number), is refused with the reason
"mount point". An entry at the marker's place that is not a file stays as
it is and the write is refused.

## Idle

The idle clock of a piece is the later of

- the newest modification time of the regular files that are not ignored,
  not timeless and not control files, and
- the time in `.antiq-restored`, if there is one.

Directories, symlinks and special files do not count. A piece is due when
its idle clock is older than `idle_days`; a day is 24 hours, and a piece
exactly that old is not yet due. A piece that is not yet due is soon when
it will be due in at most `soon_days` days; a piece that is due is not
soon. (ADR 0013, ADR 0037)

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
folder is not present, and on a folder that no pattern selects (kind
`unknown_reference`). A piece that is in the state asked for is left as
it is and reported as such. `pin` writes the file the way every marker is
written (see "Control files and markers"), and `unpin` gives a top without
owner write that permission for the removal and its mode back right
after. A pin and its removal are durable when `pin` and `unpin` return:
the file and the entry in the piece's directory are flushed. The pin is
checked before the idle and empty tests. (ADR 0024)

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
  not decode to its end (zstd checksum), decodes to more than 4 GiB (see
  "Listing format"), does not parse to its end (a
  header or entry line out of the format, a file or hard line without its
  checksum line, a path twice, `files` or `bytes` that differ from the
  entries) or lacks a parsable `piece`, `archived` or `archive-bytes`
  line; or whose checksum file is not
  exactly one checksum line for the archive's name. `status` reports it;
  `list` marks what it sees in the listing's head, the sidecars and the
  size of the archive; `rebuild` repairs a
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
  `format_too_new`, and `forget` takes it by its stamp. Damage beside it
  (a missing or damaged checksum file) is decided first: `list` and
  `status` show the version as damaged, and the three commands still
  refuse it as `format_too_new`. `push` neither
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
- **Size limit:** a reader takes at most 4 GiB (2^32 bytes) of decoded
  text from one listing, a head read as well as a full read. That is 2^23
  files at 512 bytes, against 306 bytes per file measured on a real tree.
  The limit counts the decoded bytes of the listing, not its entries, so
  the length of a single line does not matter. A listing past it is
  damaged, with the reason that it decodes to more than 4294967296 bytes.
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

A Packager trait packs, lists and unpacks; the strict verify is a function of its own (see "Verifying"). Archives are tar
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
time last; an unearth writes `.antiq-restored` into the top after that,
which changes the top's modification time. A symlink gets its stored mode
where the platform can change it (macOS) and keeps the platform's mode
where it cannot (Linux). The compressed stream is read to its end, so a
failing content checksum or data after the end of the tar stream fails the
extraction. (ADR 0050)

Archives are compressed with zstd at level 19 with long-distance matching
and a 128 MiB window, at the default job size of 64 MiB. (ADR 0051) Every
decoder antiq creates sets the maximum window to 128 MiB.

All compression in a run shares one pool of one worker per core. A piece
smaller than one job gets one worker, larger pieces take up to the free
workers, and several pieces are compressed in parallel. The number of
compressors in flight is also limited by memory against 25 % of the
machine's physical memory: an archive encoder counts 448 MiB plus 160 MiB
per worker. When nothing else is compressing, one worker is always
granted, with a warning, once for the run, when the memory limit is below
the cost of an encoder with one worker. A compressor that finds no free
worker or not
enough memory waits until another finishes, and waiting compressors are
served in the order they asked. When the operating system does not report
the physical memory, a real `archive` or `import` fails with `cannot size the
pool of workers: the operating system does not report the physical memory`,
the error kind `other`, after it has taken the lock and run the repairs.

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

A strict verify gates archiving. It reads the stream to its
end and checks its integrity. A cut or corrupt stream, a tar stream
without its end marker (two zero blocks after the last member), nonzero
data after the end of the archive, an empty archive, a first member that
is not a directory, a member outside the top-level directory, a member
path that is not a plain relative path, a path that occurs twice, a hard
link that does not name a file listed before it, a member that is neither
file, directory, symlink nor hard link and a modification time beyond 63
bits make the package damaged.

Then it compares every member with the tree on disk (type, size, content
hash, modification time in whole seconds with the same clamp, mode, link
target) and requires equal path sets under the same walk rules. The walk
is the one packing uses, so a tree with an unreadable entry or a mount
point is refused; a locked entry does not refuse. A file with several names must be stored as packing
stores it: the first path in byte order is the file and the others link to
it. Ignored paths are left out on both sides. Special files are not
archived and are not compared. Symlink modes are not compared, since Linux
cannot set them.

`unearth` and `fork` do not run this verify. Their extraction reads the
stream to its end, and the extracted tree is compared with the listing in
the same fields; the top-level directory has no line in the listing and is
not compared.

On a user's folder, directory modification times are not compared, the
top-level directory's included. The top-level directory's mode is compared
with the mode of the first member. On antiq's staging, directory
modification times are compared, since the final pass of the extraction
sets them from the package. A staging directory
whose mode keeps its owner out gets owner access for the read, and its
mode is put back afterwards. A user's folder is never changed.

While it reads, the verify records each entry's lstat (kind, size,
modification time in nanoseconds, ctime, inode), special files and the
top-level directory included, and the members of the package in archive
order. A file that is replaced, resized or touched while it is read is
reported as changed while read. (ADR 0033)

## Operations

Every command prints readable text on a terminal and has `--json`. Data
goes to stdout, messages to stderr (see "Console output" for the
messages that belong to a result).

**Dry runs.** Every command that changes something has `-n`/`--dry-run`:
`init`, `archive`, `unearth`, `fork`, `forget`, `rebuild`, `import`,
`pin`, `unpin` and `push`. A dry run takes no lock and changes nothing.
It computes the repairs read-only and lists them first, as actions of
kind `repair` (each naming the version or tree it would delete, rename
back or move to `recovered/`); its other actions assume the repairs were
done. A dry run that finds the lock held says so, names the holder and
lists no repairs, since the running command owns those directories; it
reads nothing else. A holder that has not yet written its pid does not
count as a holder here, so the repairs are listed, and neither does a pid
that names no running process. The time on the console is UTC, RFC 3339. It exits 0, and under
`--json` the plan is `{"dry_run": true, "lock_held": {"pid": ...,
"started": ...}, "actions": []}`; on the console it is the line `The
lock is held by pid <pid>, started <time>. No repairs are listed.` Each
command's section says what else its dry run prints. On the console a
plan is one `Would ...` line per action, or `Nothing to do.` when there
is none; a terminal shows the lines as described in "Console output". Under `--json` the plan is JSON (see "JSON output"). (ADR 0046)

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
| 3 | partial: a step failed for some pieces (packing, verifying, moving, a difference in step 6 of `archive`, a refused rename, a failed deletion, no room), or an `unearth` or `fork` that did not finish (see there) |
| 4 | target unreachable (only `push`, and 0 instead under `--if-reachable`) |

Pieces the walk refuses do not change the exit code; `status` and the
command's output name them. The exception is `archive <piece>`, which exits
1 when the piece it names is pinned, empty or refused.

**Naming a version.** `unearth` and `fork` take the version as the flag
`--version <version>`; without it they take the newest version. `forget`
and `rebuild` take it as a positional argument. Text output shows local time with
seconds, `--json` and pipe output UTC (RFC 3339 in JSON, the version stamp
in pipes). The input's spelling decides how it is matched, and the two are
never mixed:

- an input containing `T` (`2026-11-04T090211`, optional `-N`) is matched
  as a prefix of the version stamps, so `2026-11-04T09` names every
  version stamped in that hour and `2026-11-04T090211` is a prefix of the
  stamp `2026-11-04T090211-2` as well;
- any other input (`2026-11-04 10:02:11`, or a date alone such as
  `2026-11-04`) is matched against each version's time rendered in the
  machine's current zone, as a prefix of that text. Near midnight a local
  date can differ from the UTC date of the stamp.

An exact match wins over prefix matches. Several matches are an error of
the kind `other` that reads `` `<input>` matches several versions of
<piece>: <stamps> ``, the stamps newest first. An input that matches no
version is an error of the kind `other` that reads `` `<input>` matches no
version of <piece>; there are <stamps> ``, listing the stamps of all the
versions of the piece, incomplete ones included, newest first for every
command. When two versions of one piece render alike
in local time at different UTC offsets (the repeated hour when daylight
saving time ends), text output adds the UTC offset. A local input matches
the text with that offset where the output shows it
(`2026-10-25 02:30:00 +02:00`) and also without it, in which case it
matches both versions. An incomplete version matches only by stamp.
(ADR 0060)

## Repairs at the start of a run

There is no `reconcile` command. Every locked run (`push` included) first
runs these repairs, in this order; a dry run lists them instead. A repair
that fails is a warning naming the path. Whenever a tree leaves `trash/`
or `doomed/`, the parents that became empty go too. (ADR 0059)

Before repair 1, the run creates every folder of `.antiq/` that "Store
layout" names and that is missing: `tmp/`, `trash/`, `doomed/`,
`recovered/` and `pieces/`. `push` leaves `tmp/`, `trash/` and `doomed/`
out of the target, so a copy from the target lacks them. Each created
folder is flushed together with its parent, like every directory antiq
creates. A name that something other than a directory holds is left as it
is. A folder that cannot be created is a warning naming its path. A dry
run lists the creations first, and while `tmp/` is missing it judges
the volume and the free space of the staging on `.antiq/`.

1. **Interrupted archive step 4:** for each
   `.antiq/tmp/pack/<stamp>/<piece path>/` whose sidecars already lie in
   `.antiq/pieces/<path>/` without their `.tar.zst`: delete those
   sidecars. The folder is still live, since step 5 waits for step 4.
2. **Temporary files:** delete what `.antiq/tmp/` holds, except the
   `.antiq/tmp/pack/<stamp>/` folders that repair 1 may have left
   unfinished: when repair 1 failed to delete the sidecars of a version, that
   version's folder stays; when repair 1 could not read something it needs
   (the store, the staging, a folder of either), every folder of
   `.antiq/tmp/pack/` stays. A dry run warns for each such folder as well.
   The staged archive stays until the next run repairs the sidecars.
   A run that ends removes what it staged in `.antiq/tmp/`, whether it
   succeeded or stopped on a handled failure, so this repair finds work only
   after a crash and after a move into the store that stopped half way and
   could not be undone.
3. **`doomed/`:** delete every tree there without a comparison, and every
   entry that is no directory. Directory chains that hold no
   non-directory entry are removed as they are.
4. **`trash/`:** rename every tree there back to its live path without a
   comparison, or, when that is not possible for any reason (path taken,
   a parent missing or not a directory, the rename refused), to
   `.antiq/recovered/<stamp>/<piece path>/`; the chain inside
   `recovered/` is created and flushed. Directory chains that hold no
   non-directory entry are removed as they are. The piece path is the
   shortest path below `<stamp>/` for which a version with that stamp
   exists, matched by the volume's name equality (ADR 0054); when no
   version has that stamp, the first component below `<stamp>/` goes back
   as it is, which usually lands in `recovered/`; so does every other
   child of `<stamp>/` that holds a non-directory entry. An entry that is
   no directory (a file or a symlink, never followed) goes to
   `recovered/` as it is, also one that lies directly in `trash/`. A tree
   that can go neither way stays in `trash/` and is a warning that names
   both reasons. `status` names every folder in `recovered/`.
5. **Died unearth:** for each live piece whose `.antiq-restored` names a
   version stamp, delete what is left of that version of that piece (the
   `.tar.zst` first, if it still exists, then its sidecars), and rewrite
   the marker to the time alone, after the deletion succeeded. A stamp
   that names nothing is simply dropped from the marker. A marker that
   cannot be read is a warning and the piece is skipped. A tree that
   repair 4 gives back counts as the live piece for this repair. Before it
   deletes a version it flushes the piece's folder and the folder above it,
   as the unearth may have died before it flushed the rename; a flush that
   fails leaves the version and the stamp for the next run. A version
   copied back by hand between a crash and the next run is deleted, and the
   repair names it.
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

The `config.toml` it writes, here without `--pattern` and with the
placeholder `01JBX…` for the ID:

```toml
# antiq collection configuration.

[collection]
# ID of this collection. The marker `push` writes to the target records it.
id = "01JBX…"

# Glob patterns that select the pieces, relative to this directory. When
# matches nest, the shallowest one is the piece. A folder whose name starts
# with a dot is a piece only if a pattern names the dot.
# patterns = ["*"]

# Paths that are never pieces: literal paths relative to this directory,
# not patterns. A `%` is written `%25` and a byte that is not UTF-8 `%XX`.
# exclude = []

# gitignore lines for paths that are not packed, not listed and not counted
# for the idle check. `archive` deletes them with the folder. A line without
# a `/` (except at its end) matches at any depth below the top of a piece; a
# `/` at the start or in the middle anchors it at that top.
# ignore = [".DS_Store"]

# gitignore lines for paths that are packed but not counted for the idle
# check, such as ".git/". The lines follow the same rules as `ignore`.
# timeless = []

# Days a piece must be idle before it is due.
# idle_days = 30

# Days before a piece is due from which `status` shows it as soon.
# soon_days = 3

# Where `push` mirrors the collection. Remove the `#` from the lines you
# need; `url` is the only one `push` requires.
#
# [target]
# An absolute path, or [user@]host:path for an ssh target, for example
# "backup@nas.local:/volume1/antiq/projects".
# url = "/Volumes/nas/antiq/projects"
# The local rsync. Without it, rsync from PATH.
# rsync = "/opt/homebrew/bin/rsync"
# The rsync on the target, passed as --rsync-path.
# rsync_path = "/usr/local/bin/rsync"
# Set to false when the rsync on the target lacks --fsync.
# fsync = false
# Extra rsync options, one string each. An option here overrides antiq's own,
# such as --no-perms after -a.
# rsync_args = ["--no-perms"]
```

The collection ID is always an active line. With `--pattern`, the
`patterns` line is active too and holds the given patterns. Every other
`[collection]` key is commented out at its default, so that a change of a
default reaches the collections that never set the key. The `[target]`
block is commented out as a whole.

It refuses, also under `-n`, creating nothing and exiting 1, where
`.antiq/` already exists, inside another collection (naming its root),
where `.antiq` is a file or a symlink, and where a folder that would be a
piece holds a `.antiq/` directory of its own, naming each such folder.
`-n` prints the config it would write. The line of the collection ID in
that config is the comment `# id is minted by the real run`, as a real run
mints the ID. In a pipe the config is alone on stdout and `Would create a
collection at <path>` is on stderr. On a terminal stdout has the two action
lines `would create the antiq collection at <path>` and `would write
.antiq/config.toml, the settings of the collection:`, the config below them
behind a `│` on each line, and the dry-run footer. A pattern that contains `**` gets a note on stderr, in `warnings`
under `--json`.

After creating the collection, `init` prints how many folders the next
`archive` would archive, counted as `status` shows the due ones, as the
last line on stdout: `The next archive would archive 2 folders.` (`1 folder`
for one). `-n` prints no count. If the new collection cannot be surveyed,
the collection stays, no count is printed and a warning says why.

### archive

`archive [<piece>]`. Without an argument, archives every live piece that
is due. With a piece, archives that one now, however recently it changed.
`<piece>` is a live piece's path as the patterns select it, or a search
word that matches exactly one live piece (see "Piece references");
anything else is refused. An empty `<piece>` is a usage error. Only the
idle check is skipped: a pinned piece is refused (reason `pinned`, at its
`.antiq-pin`) until it is unpinned, a piece without a file that counts is
refused (reason `empty`, at the piece), and the walk refusals still apply.
`archive <piece>` exits 1 when that piece is refused. Without an argument,
refused pieces never change the exit code, and only the pieces the walk
refuses are listed, due or not: a pinned piece and a piece without a file
that counts are not. `-n` lists the pieces it would archive and the ones it
refuses, with the reason. It judges the folders as they lie in the
collection now, so a tree that repair 4 would give back to its live path is
not judged.

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
   `.antiq/pieces/<path>/`, flushing each and the directory. When a flush
   fails the version stays in the store and the folder stays: the piece is
   skipped with reason `error`, and the next run archives the folder again
   as a further version.
5. Rename the folder to `.antiq/trash/<version stamp>/<piece path>/`,
   under its live spelling (one rename on the same device; a piece on
   another device counts as containing a mount point and never gets
   here). If the rename is refused anyway, step 4 is undone: the
   `.tar.zst`, then the checksum file, then the listing go back to the
   staging, flushed as in step 4 (repair 1 finishes this after a crash);
   the folder stays, the piece is reported with the error, and the run
   exits 3. The staging folder, empty after step 4, stays until the
   rename succeeded and is removed then.
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
   flush, and delete it from there. A rename that is refused gives the
   tree back as in step 6 and skips the piece with reason `error`. A deletion that fails leaves the tree
   in `doomed/`, where the repairs try again; `status` names the tree and
   its first entry in walk order, with a cause it can read (a locked flag,
   a directory it cannot write). The error itself appears in the output of
   the run that failed and in the warning of each later run's repair 3.
   Whenever a tree leaves `trash/` or `doomed/`, the parents that became
   empty are removed, up to and including `<stamp>/`.
8. One output line per folder on stdout, `archived <piece>`, `skipped
   <piece>: <message>` or `refused <piece>: <reasons>`, then the summary
   `Archived N, skipped N, refused N.` The repairs of the start of the run
   go to stderr as `note:` lines and the warnings as `warning:` lines. A dry run prints the
   repair lines and `Would archive <piece>` lines, then `Refused <piece>:
   <reasons>` for each refused piece, and `Nothing to do.` when it prints
   none. A skipped piece has the reason `differs` (the folder changed while
   it was packed, and stayed where it is, or after the verify, and the
   message says whether it went back to its path or into `recovered/`),
   `no_room` or `error` (any other failure; the message names the step and
   the place, and where the folder went when a step moved it).

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

Removing a piece changes the link count, and so the change time, of every
file it shares with another piece by hard link. Before the pieces start,
when the run has more than one, `archive` walks each of them once more and
records the files that have more than one name. Pieces that have such a
file in common run one after the other, in path order, on one thread, so
the removal of one is over before the next is walked. A file joins pieces
only when at least one of them packs it, since step 6 does not compare
ignored entries. Running out of file descriptors in this walk ends the run
as it does in the walk of a piece, with every piece skipped as not started.
A hard link to a file outside the run is no change as long as no name of it
is added or removed between the verify and step 6; one that is, whether
made after this walk or by someone else, gives the folder back or skips it
with reason `differs` like any other change.

A crash between steps 4 and 5 leaves the live folder next to a complete
version of it; the next run may archive the unchanged folder again, which
gives a second, identical version that `forget` removes. The same holds
for a crash between steps 5 and 6: the repairs give the folder back, since
`trash/` holds only folders not yet checked again.

`archive` checks the names of the members against the rules of macOS and
against those of Linux and warns about each name that one of them would
refuse. (ADR 0054) The rules of macOS count a name in UTF-16 code units and
a symlink target in bytes, up to 255 units and 1023 bytes; those of Linux
count both in bytes, up to 255 and 4095.

The warnings of a piece start with its path and `: `, except the one about
the memory limit (see "Packaging"), which comes first. After it come, in
this order:

- what the packing did not store as it found it: a fifo, a socket, a
  character device, a block device or a file of unknown type is not
  archived; a modification time before 1970 is stored as 1970; a file that
  was shorter when it was read than when the walk recorded it is padded with
  zeros;
- `its files add up to <N> bytes and take <M> bytes on disk; sparse files
  are stored in full`, when the sizes of the piece's regular files (a file
  with several links counted once) exceed the bytes they take on disk by more
  than 1 GiB;
- the name warnings above.

### unearth

`unearth <piece> [--version <version>]` brings a version of a piece back
as a folder and **removes that version** once the folder is complete and
verified. The folder is an ordinary live folder again. Archiving it later
creates a new version. `-n` names the version, the destination path and the
checks it would make. (ADR 0061)

- `<piece>` is a path, or a search word that matches exactly one piece
  (see "Piece references"); anything else is an error that lists the
  candidates. Without `--version` the newest version is taken. A folder of
  the store that holds only versions without an archive is no piece to
  unearth: the error is `unknown_reference` and says that the piece holds
  only incomplete versions, which `forget` removes. A version named by
  `--version` that has no archive is refused with the same hint.
- A damaged version is refused, naming `fork` (which extracts it with a
  warning and keeps it), `rebuild` and copying the missing file back from
  the target. A version with two files in one slot is refused naming
  the files and `forget`, as `fork` refuses it too.
- A version whose top holds a `.antiq-restored` that is not a regular file,
  or a `.antiq-pin` that is neither a regular file nor a symlink, is
  refused; a hard link counts as a regular file. This holds for `fork` too.
- The destination is the piece's stored path. It is refused, and nothing
  changes, when it is not one the patterns select, is excluded or lies
  below an excluded folder, lies in `.antiq/`, inside another piece or
  below a file or symlink, or when anything is at it, a dangling symlink
  included. The collection root is no destination, and a path is in
  `.antiq/` when its first component is `.antiq` in any case of its ASCII
  letters. Nothing is followed through a symlink. A folder above the
  path that the volume finds under another spelling than the path's
  (another case, or another Unicode normalisation) refuses too. Folders
  above the destination that do not exist are created; a failure before the
  rename removes what was created; a crash between creating them and the
  rename leaves them empty. A destination whose nearest existing folder is
  on another volume than `.antiq/tmp/` is refused as a mount point, before
  anything is extracted.
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
- A failure after the rename (the flush of the folder, the deletion of the
  version or the rewrite of the marker) leaves the folder in place. The run reports `unearthed` as
  true with a warning that names what failed, exits 3, and the repairs of
  the next run finish it. No room for the version (the unpacked size of the
  listing against the free space of `.antiq/tmp/`) skips it before anything
  is written: exit 3, `unearthed` false. A volume that runs full during the
  extraction skips the version the same way: the staging is removed, and
  `needed` is the unpacked size of the listing.
- `-n` prints a `Would unearth` line for the unearth and one indented
  `check` line per check. On a terminal the checks stand without the word
  `check` under the line `Checks the real run makes first:`, and an info
  that says what `unearth` does follows them. It makes the checks of the version, of the
  destination and of the free space. A tree that repair 4 would give back to
  the destination makes it taken, and a destination on another volume than
  `.antiq/tmp/` is refused as a mount point. The checks of the member names
  and of the extracted tree are named and not run, as they need the
  extraction, which writes into `.antiq/tmp/`. Without room for the version
  `-n` prints a `Would skip` line instead and exits 3, also where the
  destination is taken, as the real run measures the room before it looks
  at the destination. A dry run chooses
  among the versions as they are, and a refusal is softened into a warning
  only when a planned repair that deletes sidecars, a version or an empty
  folder names the piece's path as given.
- The top of the unearthed folder has the modification time of the last
  write of `.antiq-restored`, not the archived one.
- On the console a run prints `Unearthed <piece> (version <version>)` on
  stdout. The repairs that were done (`note: repaired: <repair>`) and the
  warnings (`warning: <warning>`) go to stderr. A skipped version prints
  `Skipped <piece>: <size> needed, <size> free` on stderr and no stdout
  line, the sizes in decimal units. In `-n` the `Would skip` line reads
  `Would skip <piece> (version <version>): <size> needed, <size> free`, and
  the `check` lines read `the version <version> is sound`, `the destination
  <path> is free`, `there is room for <size>`, `the member names fit the
  volume (not run by -n)` and `the extracted tree equals the listing (not
  run by -n)`; `Nothing to do.` stands for an empty plan.
- On a terminal the version is named by its local time, with the UTC offset
  where another version of the piece renders alike; in a pipe it is named by
  its stamp.

Before extracting, `unearth` checks every member name against the volume it extracts to:
names it cannot hold, name lengths, and names that collide under its case
and Unicode rules, which come from a probe of the volume of `.antiq/tmp/`. It
refuses the piece with a list of the offending names and the reason. A
name is too long when it exceeds the maximum the volume reports (`fstatvfs`,
255 on APFS and ext4), counted in UTF-16 code units, on the name as stored,
on macOS and in bytes elsewhere; a volume that stores names
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
without the comparison against the listing; a version without an archive,
one with a listing of a newer format and one with two files in a slot are
refused. `fork` writes
`.antiq-restored` with the time alone (no version stamp, so the repairs
never treat it as an unearth). Archived later, the copy becomes a new
version (same path) or a new piece (new path); without edits that is an
identical version, once due or at once with `archive <piece>`. `-n` names
the version and the destination path, and the checks as `unearth -n` does,
with an info that says what `fork` does. (ADR 0061)

No room for the version (the unpacked size of the listing against the free
space of `.antiq/tmp/`) skips it before anything is written, and so does a
volume that runs full during the extraction: exit 3, `forked` false,
`skipped.reason` `no_space`, and `needed` null where there is no usable
listing to read the size from. A version without one is not measured
beforehand.

On the console a run prints `Forked <piece> (version <version>) to <path>`
on stdout. The repairs that were done (`note: repaired: <repair>`) and the
warnings (`warning: <warning>`) go to stderr. A skipped version prints
`Skipped <piece>: <size> needed, <size> free` on stderr and no stdout line,
or `Skipped <piece>: out of space during the extraction, <size> free` where
`needed` is null. In `-n` a line reads `Would fork <piece> (version
<version>) to <path>` with one indented `check` line per check, or `Would
skip <piece> (version <version>): <size> needed, <size> free`. The `check`
lines read as those of `unearth`, with `the version <version> is damaged and
is extracted with a warning` for a damaged version.

### forget

`forget <piece> (<version> | --all-versions --force)` deletes exactly one
version: the `.tar.zst` first, then the listing and the checksum file. It
also takes the stamp of an incomplete version and removes its sidecars,
saying that the version was incomplete. The store folders that the deletion
leaves empty are removed with it. Deleting every version of a piece
takes `--all-versions --force`, which also removes the piece's incomplete
versions and says so for each. `--all-versions` without `--force` is a
usage error (exit 2) that names the piece and the command that confirms;
nothing is deleted, and under `--json` its message is `the following
required arguments were not provided: --force`. A crash inside `forget` can leave an
incomplete version; running the same `forget` again removes it. The next
`push` mirrors the deletion to the target.

A version that cannot be deleted is reported, `forget` goes on with the
next version and ends with exit 3. What the failed deletion had removed
stays removed, the `.tar.zst` first, so the same `forget` finds the rest as
an incomplete version. A store folder whose sidecars belong to a version
that is still being moved into the store (its archive is staged in
`tmp/pack/`) is not a piece to forget. An empty piece or an empty version
is a usage error.

On the console `forget` prints one line on stdout for each deleted
version, `Deleted version <time> of <piece>` or `Deleted incomplete version
<stamp> of <piece>`. A complete version is named by its local time on a
terminal and by its stamp otherwise; an incomplete version is always named
by its stamp. The repairs that were done, the warnings and the failed
deletions go to stderr. `-n` lists the repairs as `Would ...` lines, then
for each version `Would delete version <time> of <piece>: <file>, ...`,
naming the files in the order they are deleted. On a terminal each version
is an action line `would delete version <time> of <piece>, <n> files:`
followed by one line for each file, indented by two columns, with its part
(`archive`, `listing` or `checksum`) in front of its name. When the plan
deletes a version, an info says that `forget` deletes a version for good
and that `unearth` and `fork` cannot bring it back, between the rows and
the footer. With `--all-versions` the
versions are deleted oldest first, incomplete ones among them. The
`Deleted` and the `Would delete` lines, and `forgotten` and `actions` in
JSON, list them newest first.

### rebuild

`rebuild <piece> <version>` repairs a damaged version by writing only what
is missing, after checking the archive file against what survives. It runs
only on the user's request, never as a repair. (ADR 0062)

- A listing is regenerated by the rules `archive` uses, from the tar
  members: mode and whole-second time from the header, size and hash from
  reading each member, `hard` for tar link members with the first path as
  target, no line for the top-level directory; `files`, `bytes` and
  `archive-bytes` come from the same read. The `imported` line of a
  version made by `import` is not restored.
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
- Every refusal of a damaged version names `forget` and copying back from
  the target as the ways out. A version that is not damaged is refused. So
  is an incomplete version, which has no archive; the refusal names
  `forget`. A folder of the store that holds only versions without an
  archive is no piece to rebuild: the error is `unknown_reference` and says
  that the piece holds only incomplete versions, which `forget` removes. A
  listing of a newer format is refused with the error kind
  `format_too_new`, also when the checksum file is missing or corrupt. An
  empty piece or version is a usage error. It takes the lock and has `-n`,
  which says what it would check and write.

On the console `rebuild` prints `Rebuilt version <version> of <piece>: wrote
<file>, ...` on stdout, the version named by its local time on a terminal
and by its stamp otherwise. The statement of the case (the archive is the
one checksummed when it was stored; every member of the archive matches the
listing; the archive reads without error, which does not show that it is
unchanged), the repairs that were done and the warnings go to stderr as
notes. `-n` lists the repairs as `Would ...` lines, then one of `Would check
<archive> against <checksum file> and write <listing>`, `Would check every
member of <archive> against <listing> and write <checksum file>` and `Would
read <archive> to its end and write <listing>, <checksum file>`. A dry run
applies the refusals that follow from the sidecars and the size of the
archive. It neither hashes nor reads the archive, so it can succeed where
the real run refuses a mismatch. On a terminal the version is the action
`Would rebuild version <time> of <piece>`, followed by one indented row
for each step: `check`, `against`, `write` (one row for each file) or
`read`. An info says what `rebuild` does, between the rows and the
footer.

### import

`import <folder> [--as <path>] [--time <time>]` packs a folder from
outside the collection directly as a version of `<path>` (default: the
folder's name), with steps 1 to 4 of `archive`; the source folder is left
in place.

- The source must not lie inside the collection, and must be a folder, not
  a symlink to one. A relative `<folder>` and a relative `--as` start at
  the directory `-C` names, else at the current directory. `--as` is
  resolved as a piece reference path is (see "Piece references"). An
  `--as` that is not a path is a usage error; the name of the folder, which
  stands in when `--as` is missing, that is not a valid piece name is the
  error kind `other`.
- `<path>` must be one the patterns select, not excluded, not in
  `.antiq/` and not inside another piece. A pin on a live folder at
  `<path>` does not block the import.
- The idle check does not apply. The source's top is treated as a piece's
  top: `.antiq-restored` there is left out, `.antiq-pin` there refuses the
  import, and a source without a countable file is refused as empty, also
  with `--time`.
- The ignore and timeless rules of the collection and the folder's own
  rule files apply, and so do the walk refusals. The exceptions are those
  that only matter when a folder is removed or moved, which import never
  does: a locked entry and a locked parent directory are no refusal, and
  neither is the folder being the root of a mount. A mount below the folder
  still refuses. The exclude list of the collection names paths of the
  collection and does not apply inside the folder. A refusal is an error
  of kind `other` (exit 1) that names each reason, with paths relative to
  the folder.
- The version time is the newest counted modification time in the folder,
  or `--time`. `--time` is either a UTC version stamp without a counter
  (`2026-11-04T090211`) or a local date-time with seconds
  (`2026-11-04 10:02:11`) in the current zone. Anything else is refused as a
  usage error (exit 2): a date alone, a prefix of a stamp, a stamp with a
  counter, and a local time that daylight saving time repeats or skips. A
  version time in the future is refused (exit 1): with the newest file's
  time the error names that file, with `--time` it names the time. A future
  file under a `--time` in the past is no obstacle.
- The listing carries `imported <time>`, the real time of the import.
- Packing, the verify and the move into the store are those of `archive`
  steps 2 to 4, with the room measured as in "Free space". When they fail
  the import is skipped, not refused: `skipped <piece>: <message>` with the
  reason `differs`, `no_room` or `error` as for `archive`, and exit 3.
  A version that is stored while its store folder may not be durable is
  reported as skipped with a message that says it was stored. On success
  the output is one line, `imported <piece>: version <stamp>`.
- This is also the way to bring in old archive files: unpack them with
  their original timestamps somewhere temporary, then import each folder.
  (ADR 0009, ADR 0055)
- `-n` lists the repairs, then `Would import <folder> as <piece>, version
  <stamp>` (`<folder>` is the canonical path of the source, the stamp has
  the counter the run would get), then `Would
  leave out <path>` for each ignored entry (a folder once, with a trailing
  `/`) and for `.antiq-restored`. When the import would not fit it prints
  `Would skip <piece>: <message>` instead of the import line and exits 3.
  Refusals are errors in a dry run too, with the same exit code.

### list

`list [query]` prints one row per version: pieces by path, the versions of
a piece newest first. The query matches piece paths fuzzily, as a search
word does, and keeps the pieces that match. Each row reads only the head of
its listing. SIZE is the archive file's size (`archive-bytes`) in decimal
units (B, kB, MB, GB, TB; 1 kB = 1000 bytes; one decimal below 10, none
from 10 on); `--json` also carries the unpacked size and the file count.
The piece is in every row, so each row stands on its own after `sort` or
`grep`. (ADR 0021)

On a terminal the rows are a table. VERSION is the local time with seconds,
with the UTC offset added where two versions of a piece render alike (see
"Naming a version"); FILES is grouped by thousands with a space.

```text
$ antiq list scan
PIECE              VERSION               SIZE     FILES
projects/scanner   2026-11-04 10:02:11   1.1 GB   48 211
projects/scanner   2026-09-03 12:15:00   1.0 GB   47 980
tools/scan-old     2025-02-11 19:40:07    40 MB      812
```

The header row is dim and has no rule under it. Without a terminal there is
no header and a row is one line of tab-separated
fields, `piece  stamp  archive-bytes  files  state`, with the version stamp
and the numbers unformatted. A control character in a piece path is written
as `%` and the two hexadecimal digits of each of its bytes, on a terminal and
in a pipe.

A version that is not sound is marked in a last column on a terminal, in
the `state` field in a pipe and in the `state` key of `--json`:

| State | Mark on a terminal | Colour | Meaning |
|-------|--------------------|--------|---------|
| `ok` | none | | |
| `damaged` | `damaged` | red | a damaged version (see "Damage") |
| `incomplete` | `incomplete` | yellow | an incomplete version; its VERSION is the stamp, as it is named by its stamp only |
| `newer-format` | `newer format` | magenta | a listing of a newer format than antiq reads |
| `unreadable` | `unreadable` | red | a file of the version cannot be read; SIZE and FILES are `-` |

SIZE and FILES are `-` on a terminal and in a pipe, and null in `--json`,
where the head of the listing lacks the number; SIZE is the size the listing
records, also when the archive on disk has another. A file that cannot
be read does not fail the run: its version is listed as `unreadable`, and
a warning names the file and the reason, as a `warning:` line on stderr in text
output and in the `warnings` of `--json`. Each fault of a damaged version
is a note on stderr in text output, `note: <piece> <stamp> is damaged:
<fault>`, and under `--json` an entry in the `damage` list of its row. A folder that holds sidecars only is
searched too, and its incomplete versions are listed. A version whose archive
is staged in `tmp/pack/` is being moved into the store by a run and has no
row.

On a narrow terminal (see "Console output") a piece path that does not fit is
shortened in the middle to the width the other columns leave, but not below
8 columns.

With no version in the store `list` exits 0 and says `note: no versions in
the collection` on stderr; with versions but no piece matching the query it
exits 0 and says ``note: no piece matches `<query>` ``, also under `--json`.
Damaged versions do not change the exit code. `list` takes no lock and
writes nothing. A held lock is noted on stderr, `note: another run holds the
lock (pid ..., machine ..., started ...)`, and under `--json` in the `note`
field instead of on stderr. Notes on stderr come in the order: settings of
the collection, the lock, nothing to list, the faults of damaged versions.

### grep

`grep <pattern>` searches the paths of the archived files in the listings of
all versions that have an archive, with a case-sensitive regular expression
(`regex::bytes`, the listings in parallel on all cores but two). A version
without an archive is incomplete and not searched. A path is searched as
`<piece>/<path>`, in its unescaped bytes. Files, symbolic links and hard
links are searched, directory entries are not.

Only the listing is read, so a version that is damaged in another way, such
as one with no checksum file or an archive whose size differs from its
listing, is searched as long as its listing reads.

A version that cannot be searched is skipped with a warning naming the piece
and the version, and the exit code stays 0. That is a version with no
listing file or two, a listing that cannot be opened or read, one that does
not decode, one that decodes to more than the size limit (see "Listing
format"), one of a newer format, and one with a line that is not an entry;
a listing that fails part of the way through reports none of its hits. A
doubled archive or checksum file does not stop the search.

On a terminal the hits are grouped by piece, each in-piece path once with
the versions that hold it, the match highlighted:

```text
projects/scanner        2 versions, newest 2026-11-04
  src/main.rs           both versions
  tools/bench/main.rs   2026-09-03
tools/scan-old          1 version, 2025-02-11
  cli/src/main.rs       2025-02-11

3 files in 2 pieces · antiq unearth <piece> [--version <version>]
```

A piece row counts the searched versions. The versions of a path read `both
versions` when it is in both of two, `all N versions` when it is in every one
of more than two, the date of the version when the piece has a single
searched version, and otherwise the dates of the versions that hold it,
newest first. A date is a local date. Where two versions of a piece share a
local date, searched or not, every version of that piece is named by its
local time with seconds instead, with the UTC offset added as `list` does.
The last line counts the files with hits and the pieces they are in, and the
pieces whose name matched.

The left column is as wide as the longest piece or indented path, and three
spaces separate it from the right column. On a terminal of 100 columns or
more a path is never cut. On a narrower one the left column gets the width
that the longest right text leaves, but at least 20 columns, and a longer
path is cut in the middle and not highlighted.

The pattern is matched against the unescaped path bytes; text output shows
the path with the comment-line escapes, any other control character as `%XX`
of its bytes and a byte that is not UTF-8 as `%XX`, and the pipe form prints
the raw path. A piece whose name matches gives one row, `piece name matches,
2 versions, newest 2026-11-04, 48 211 files`, with the version count and the
file count and no file list. The name matches when the
leftmost match of the pattern in `<piece>/`, the piece path and a slash, ends
before the slash. `scanner` matches the piece `projects/scanner`; `scanner$`
and `scanner.*` do not, and `scanner.*` lists the files below the piece. Only
the heads of the listings of such a piece are read, and the file count is the
one of its newest searched version. In a pipe every hit and
version is one tab-separated line `piece  stamp  path`, with the full
version stamp (`-` as the path for a name match), ordered by piece, then
path, then version with the newest first. A skipped version is a `warning:`
line on stderr. With no hit `grep` exits 0 and says `no archived path
matches <pattern>` on stderr; a pattern that is not a valid regular
expression is a usage error, exit 2, also outside a collection. A held lock
is noted as for `list`: a `note:` line on stderr, after the `warning:` lines
on the settings of the collection.

### status

`status` is local only: it takes no lock and writes nothing below
`.antiq/`. It exits 0 whatever state it reports: pieces that `archive`
refuses, damaged versions and every note below are output, not failures. A held lock
is a note on stderr that names the holder (pid, machine, start time).

The first line counts the live folders (every piece, plain ones included)
and the versions (those that have a `.tar.zst`), gives the sum of the
`archive-bytes` their listings record in decimal units, and repeats
`idle_days`. A blank line and the table follow: one row per piece that has
a state, sorted by path, the state coloured in the first column and the
header row and the WHAT column dim: `due`
(archived on the next run), `soon` (due within `soon_days`), `pinned`, and
`refused` for the pieces `archive` refuses. Plain live folders are only
counted in the header; when no piece has a state, the blank line and the
table are left out. A pin wins over a refusal, as it does over the idle
and empty tests (see "Pins").

The columns are STATE, PIECE, IDLE and WHAT. IDLE is the whole days since
the idle clock, `0d` for a clock in the future, and `-` when the clock
cannot be told: the piece has no file that counts, the walk could not read
all of it, or its marker cannot be read. WHAT is `archives next run`,
`archives in N days` (the started days until the piece is due, at least
one), `kept by .antiq-pin`, or, for `refused`, every reason of the piece
joined by `; `. A reason is one of these, with paths relative to the
collection root:

- `mount point: <path>`
- `nested collection: <dir>`, the directory that holds the nested marker;
  the WHAT column of `status` says `nested antiq collection: <dir>`, and
  `--json` and the `refused` lines of `archive` keep `nested collection`
- `unreadable entry: <path>`, the first entry that failed
- `exclude entry: <path>`
- `locked: <path>`, also for the directory that holds the piece's top
- `rule file: <path>:<line>`, without `:<line>` when the file cannot be
  read or is no text
- `<name> is not a file`, a marker name at the top
- `unreadable marker: <path>`, a marker that cannot be read or parsed

On a terminal of 100 columns or more the path column shows full paths;
on a narrower one a path that does not fit is cut in the middle and a
long WHAT wraps below its column.

```text
13 folders · 214 versions (38 GB) · archive after 30 days idle

STATE    PIECE                 IDLE  WHAT
due      experiments/rust-gpu   33d  archives next run
due      old/client-site       141d  archives next run
refused  media/camera-import    52d  mount point: media/camera-import/card
pinned   projects/antiq        120d  kept by .antiq-pin
soon     tools/scan-old         28d  archives in 2 days

! 1 file with a time in the future: notes/plan.md (2027-01-12)
! 1 damaged version: tools/scan-old 2024-06-30 08:12:45 (no listing)
```

Below the table, after a blank line, each kind of note has a line that
starts with `! ` and counts its items. One item stays on that line;
several are indented below it. The kinds, in this order:

- files and markers with a time in the future, with the local date
  (`<path> (2027-01-12)`);
- damaged versions, with the reasons joined by `; `: `no listing`,
  `no checksum file`, or the description of the damage (see "Damage");
- incomplete versions;
- versions in a newer format;
- folders in `recovered/`;
- trees left in `trash/`;
- leftovers in `doomed/`: the tree, its first entry in walk order (the byte
  order of the whole path) and what keeps it from being deleted, as far as
  `status` can read it: `locked` for an immutable or append-only flag,
  `its directory cannot be written`;
- live folders whose spelling differs from the stored one
  (`<live> (stored as <stored>)`);
- directories that selection could not read, one line each
  (`unreadable: <dir>`);
- files in store folders whose names do not parse.

On a terminal each of these notes is a warning block on stdout instead:
a bold title with the same count and items, a sentence that says what the
finding means, and a `→` line that says how to fix it where there is a
fix. When a piece is due, a hint `Run antiq archive -n to see what the next
run would archive.` follows. A pipe gets the lines above and no hint.

Damaged and newer-format versions are shown by their local time with
seconds, with the UTC offset added where two of a piece render alike. An
incomplete version is shown by its stamp, as it can be named by its stamp
only. A tree in `trash/` or
`recovered/` is named by the piece whose version has the tree's stamp,
matched by the volume's name equality, and by the first component below the
stamp when no version has it. Paths of these trees and of files in the
store are shown relative to `.antiq/`.

Warnings about the config, an unreadable lock file, a directory that cannot
be read, a version that cannot be inspected or a spelling that cannot be
looked up are warnings on stderr, each starting `warning: `; a held lock is a `note: ` line. When the store cannot
be read, `status` still prints the table of the live pieces, adds the note
`cannot read the store: <path>: <reason>`, leaves out the versions of the
header and the notes on versions, on spellings and on files in the store, and
exits 0. The notes on `recovered/`, `trash/` and `doomed/` stay, each tree
named by the first component below its stamp, as no version can be matched
to it.

### pin and unpin

`pin <piece>` and `unpin <piece>` create or remove `.antiq-pin` at the top
of a live piece (see "Pins"). `-n` says which.

The piece is a live piece, given as a path or a search word. A folder that
is not present, or that no pattern selects, is the error kind
`unknown_reference`. An empty `<piece>` is a usage error. A run prints
`Pinned <piece>`, `Unpinned <piece>`, `<piece> is already pinned` or
`<piece> is not pinned` and exits 0; `pin` on a piece that is pinned
already writes nothing and still flushes the piece's directory.

On a terminal a run adds an info block that says what a pin is: under
`Pinned <piece>` and `Unpinned <piece>` what the pin does, and in place of
the sentence for a piece in the state asked for ("Nothing to pin: <piece>
is already pinned." and "Nothing to unpin: <piece> is not pinned.", with
the line that `antiq status` shows which pieces are pinned). A dry run
prints the same kind of info, between its action line and its footer. A
pipe gets the sentences only.

A dry run prints the repairs as `Would ...` lines, then `Would pin <piece>`
or `Would unpin <piece>`. When the piece is in the state asked for it
prints the sentence of a real run instead of `Nothing to do.`. It assumes
the repairs were done, so a folder that repair 4 gives back to its live
path is found when a pattern selects it. It refuses what a real run
refuses: a pin file that is a directory, and, for `pin` on a piece that
has no pin, a top on another device (reason "mount point"). A rename that
fails with `EXDEV` in the real run cannot be foreseen.

## Push

`push [--init] [--force] [--if-reachable]` mirrors the collection to the
target. `--init` and `--force` are each refused together with
`--if-reachable`. The steps run in this order.

0. **Before the lock.** A missing target url, an invalid url, a collection
   root that is not UTF-8 and `rsync_args` that make rsync quiet (`-q`,
   `--quiet`) are refused first, also while another run holds the lock.
1. **Repairs** run first, under the lock (see "Operations").
2. **Check the store.** While a damaged or incomplete version exists,
   `push` refuses with exit 1 and prints the exact `forget` or `rebuild`
   command for each; a version whose archive is still staged in
   `.antiq/tmp/pack/` after a failed repair counts, in a real run, and a
   listing of a newer format does not. It also refuses, naming both, when two piece paths
   or two store folders of the collection differ only in case or Unicode
   normalization. (The same clash between file names inside one live
   folder is not checked.) And it refuses while `.antiq/trash/` still
   holds a tree after the repairs.
3. **Check rsync.** The local rsync's `--version` must name rsync 3.2.4 or
   newer, the release that added `--fsync`; a pre-release of 3.2.4 is
   refused. Without `[target] rsync`, the local rsync is the first `rsync`
   that passes this check in the absolute entries of `PATH`, then
   `/opt/homebrew/bin` and `/usr/local/bin`; one that fails it is passed
   over, and the refusals are named when none passes. A configured path
   must be absolute and is never replaced by another rsync. openrsync, which macOS ships
   as `/usr/bin/rsync`, and older versions are refused with a message
   naming the binary and the config key `[target] rsync`. A target whose
   own rsync is openrsync (any macOS machine) works with `fsync = false`.
4. **Pre-check the target.** The target path is canonicalized first. A
   path target that is the collection root or lies below it (compared by
   the volume's name equality, ADR 0054) is exit 1, also with `--init`,
   and so is a path target that holds the collection root, or is a mount
   root ("use a folder on the volume, not its root"). A directory that
   does not exist is judged at its nearest existing folder: below the
   collection root it is exit 1, anywhere else it counts as missing.
   Otherwise the checks use the file system for a
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
   | `.antiq-target` of this collection, with `--init` | exit 1 |

   `.antiq-target` is not readable when it is not a regular file (a link
   counts as not regular, also in an ssh listing), cannot be opened, or does
   not parse. An error
   other than the five above, while reading a path target, is exit 1 with
   the system's message.

   When the listing shows `.antiq-target` on an ssh target, it is fetched
   with one rsync run of `<target>/.antiq-target` into a directory of its
   own in `.antiq/tmp/` (a dry run uses the system's temp directory, since
   it changes nothing in the collection), with the same options as the
   marker run below plus `--max-size=64K`; a marker that the listing shows
   larger than 64 KiB is not fetched, and a fetch that fails, or content
   that does not parse, is "not readable". `--force` changes
   nothing in the other rows: no marker still needs `--init`, another
   collection's marker is still exit 1, and with a matching marker it is a
   plain push, noted in the output.

   rsync runs with `LC_ALL=C` and without any `RSYNC_*` variable or
   `CVSIGNORE` of the environment. The listing carries the mirror's `-e`,
   `--rsync-path`, `--timeout` and `rsync_args`, but not `--fsync`. Over
   ssh, exit 255 counts as "cannot connect" only when ssh's line starts
   with `ssh: connect to host` or `ssh: Could not resolve hostname`, or
   contains `timed out`; any other 255 (a changed host key, a refused key)
   is exit 1 with the line. rsync reports exit 12 with the line
   `connection unexpectedly closed` instead of 255 when it notices the
   closed connection before it has reaped ssh; that counts as 255. Exit
   23 counts as "does not exist" only when the failing `change_dir` or
   `link_stat` line ends in `(2)`, or openrsync's `(l)stat:` line ends in
   `No such file or directory`; any other 23 is exit 1. A listing counts
   only when it holds the entry `.` of kind `d`, which is the directory
   itself; a listing without it is exit 1, never an empty directory.

   Unreachable means exit 0 under `--if-reachable`, otherwise exit 4, and
   the reason is printed in text and `--json` every time. A dry run of an
   unreachable target under `--if-reachable` still lists its repairs in
   text, but never prints `Nothing to do.` on stdout; without
   `--if-reachable` the error is all it prints. A missing
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
   marker as it was, so the machine it names pushes on as before. It adds
   a note that names the host and path of the marker: an entry of `notes`
   in `--json`, a `note:` line on stderr in text.
   `--init` is refused where a marker exists, whoever's it is. The
   marker is sent by its own rsync run, as a temp directory in
   `.antiq/tmp/` holding only `.antiq-target`, sent to `<target>/` with
   `rsync -r --chmod=F644` (no `-a`, `-p`, `-t` or `--delete`, so rsync
   sets neither mode nor time of the target root from the temp directory)
   plus the same `-e`,
   `--rsync-path`, `--timeout`, fsync setting and `rsync_args` as the mirror; the
   marker arrives with mode 0644 masked by the receiving side's umask. For
   an ssh target that creates the last missing directory level, and a
   deeper missing path is exit 1 with rsync's mkdir line. A failing marker
   run is exit 1 with rsync's text; where the target's rsync does not know
   `--fsync` it carries the same advice as the mirror (step 8). Just before it, a path target's
   directory is checked to be the one that was inspected.
   `push -n` never writes it. (ADR 0031)
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
   `*deleting` and the run ended with exit 0, 23 or 24, `push` runs the
   same command with `-n` and, if that prints any itemized line (of a
   directory too), the mirror once more. A run that ended any other way
   deleted nothing and gets no second pass. The result is the worst of
   the runs, and the first failure's text is the one printed; the
   summary adds up the changes of both mirror runs.
8. **Result.** rsync exit 0 is success; 24 (files vanished during the run)
   is success with a note on stderr naming the paths of the `file has
   vanished` lines. Everything else, including any failure after the pre-check, is
   exit 1 with rsync's message (rsync ended by a signal included), also
   under `--if-reachable`. With exit 23
   and the line `IO error encountered -- skipping file deletion`, push
   says that no deletion reached the target and names the directories
   rsync could not open. Exit 12 with ``unrecognized option `--fsync'``
   (openrsync) or `--fsync: unknown option` (older rsync) is reported as:
   the target's rsync (openrsync, or older than 3.2.4) lacks `--fsync`;
   set `[target] rsync_path` to a 3.2.4+ rsync there, or `fsync = false`.
9. `-n` runs steps 1 to 4 and the mirror with `-n`. It lists the repairs
   without running them, and judges step 2 as those repairs would leave
   the store. It sends no marker and runs no second pass, since a dry run
   cannot see the effect of the first deletions. rsync's
   `--itemize-changes` output is summarized per piece: the files, links
   and other non-directory entries `created`, `updated` and `deleted`;
   changes to directories are not counted. A path belongs to the live
   piece or store folder that is its longest prefix, `.antiq/pieces/<folder>/`
   counts for `<folder>`, and every other path is `other`. A real run
   prints the same summary. In text it is `Changed on <target>:` (`Would
   change on <target>:` in a dry run), one `  <piece>: N created, N
   updated, N deleted` line per piece that changed and `  other files:
   ...` where other paths changed; where nothing changed it is `Pushed to
   <target>. Nothing changed.` (`The target <target> is up to date.` in a
   dry run). A written marker adds `Claimed the push target <target> for this
   antiq collection.` before it. On a terminal a push that wrote the marker
   prints `✔ Claimed the push target <target>: N created, N updated, N
   deleted`, a sentence that says the marker now records this antiq
   collection and host, and a table with the columns PIECE, CREATED, UPDATED
   and DELETED for the pieces that changed and the other files; a zero is
   dim.

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

The configuration is per collection, in `.antiq/config.toml`. The file
`init` writes (see "init") has the same keys as this example, all but the
ID and the given patterns commented out:

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
| `target.rsync` | the first suitable `rsync` on `PATH`, then in `/opt/homebrew/bin` and `/usr/local/bin` | absolute path of the local rsync |
| `target.rsync_path` | none | the target's rsync, passed as `--rsync-path` |
| `target.fsync` | `true` | pass `--fsync` |
| `target.rsync_args` | `[]` | extra rsync options |

A path target may be any absolute path; the path of an ssh target is
passed to rsync verbatim (rsync 3.2.4 and later protect arguments by
default). A `url` starting with `/` is a path. A `url` does not start with
`-`, and the host of an ssh target does not hold a `/`. The user, the host
and the path of an ssh target are not empty when given. A path after the
host that starts with `:` or `//` is refused, since rsync reads
`host::module` and `rsync://host/module` as daemon syntax. The path of an
ssh target holds none of `*`, `?`, `[` and `]`, which rsync leaves to be
expanded on the target, so that the pre-check and `--delete` could reach
different directories. An IPv6 host is
written in brackets, `[::1]:path`. Unknown keys under `[collection]` or
`[target]` are ignored, and each one is reported in a warning.

`rsync_args` holds neither `-q`, `--quiet` nor a cluster of short options
with a `q` in it (`-aq`; the letter after an option that takes a value,
such as `-e`, does not count). A quiet rsync prints no listing, which the
pre-check of an ssh target needs. `push` refuses it with exit 1 before it
runs rsync.

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

A piece reference is a path or a search word. A relative path is resolved
against the directory `-C` names, or against the current directory without
`-C`; a relative `-C` is itself taken against the current directory. The
path is normalised and taken relative to the collection root; a path
outside the collection is refused with the error kind
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

On a pipe the error line is followed by one line `    candidate: <path>` per
candidate, in the order and the escaped form of `error.candidates`. On a
terminal the report is one group on stderr: the error block, a bold caption
and a table, the info "Nothing was <done>." and, for a word that matches no
piece, a hint. `<done>` is `archived`, `pinned`, `unpinned`, `unearthed`,
`forked`, `deleted` or `rebuilt`, after the command. The error block says
that the matches are listed below, and its "What antiq was doing" names
the step "find the piece to <command>" (for a version, "find the version
to <command>").

- Several pieces match a word. The caption is "The N pieces that match
  `<word>`". For `unearth`, `fork`, `rebuild` and `forget` the table is the
  table of `antiq list` with all versions of the candidate pieces, in which
  the letters the word matched are bold red. For `archive`, `pin` and
  `unpin` it has the columns PIECE and VERSIONS, the number of stored
  versions, which is 0 for a live piece without any. `forget` uses that
  table too when a candidate is a folder that holds sidecars only.
- No piece matches a word. The block says so; its "How to fix it" tells to
  check the spelling and then to look for the piece with `antiq list`
  (`unearth`, `fork`, `rebuild`, `forget`) or to name it by its path
  (`archive`, `pin`, `unpin`). Pieces whose path or last
  name is within two edits of the word, or within a third of its length
  when that is more, are the closest pieces; a swapped pair of letters
  counts as one edit. Up to three of them, nearest first and by path among
  equals, are shown under the caption "The closest pieces" in the table
  of the first case, followed by the hint "Did you mean <piece>? Run
  <command>.", where the command is the one that ran with the nearest
  piece in place of the word. Without a closest piece there is no table and
  no hint.
- Several versions match a `--version` or version argument. The caption is
  "The versions of <piece>, the N matches marked". The table is the table
  of `antiq list` with all versions of the piece. A `▸` stands in front of
  each matching row, a row that does not match is dim in every column, and
  for an input without a `T` the part of the time that the input spelled
  is bold red. The "How to fix it" gives the command with the stamp of
  the newest matching version.

When the listing cannot be read, the table is left out and the matches are
listed in the "Why it happened" section instead. The JSON error does not
change: `error.candidates` lists the candidates, and the closest pieces
and the matching versions are not part of it.

### Console output

A stream is rich when it is a terminal. Stdout and stderr are decided
separately, so one can be rich while the other is a pipe. Environment
variables that force colour do not make a pipe rich. (ADR 0063)

The output adapts to the terminal's width: on a terminal of 100 columns or
more tables use their wide layout, with full paths; on a narrower one they
use their narrow layout, in which a path that does not fit is shortened in
the middle, keeping its start and its end. Without a terminal the width is
80. Prose in blocks is wrapped at the terminal's width but at most 100
columns, and not wrapped below 40. (ADR 0056)

Colour is written only on a rich stream, in the 16 named ANSI colours, so
the terminal's theme decides the shades. It is off when `NO_COLOR` is set
to a non-empty value, when `CLICOLOR` is `0`, and when `TERM` is unset or
`dumb`; a terminal in that case keeps the symbols and the words of the
badges. Symbols are Unicode when the first non-empty one of `LC_ALL`,
`LC_CTYPE` and `LANG` names UTF-8, and ASCII stand-ins otherwise.

A message to a person is a block of one of four kinds. Each has a badge
with a symbol and a word:

| Kind | Badge | Colour | Form |
|------|-------|--------|------|
| error | `✖ ERROR` | red | badge on its own line, then a bar with the bold title and the sections "Why it happened", "How to fix it" and "What antiq was doing", the last always at the bottom; a section without content is left out |
| warning | `⚠ WARNING` | yellow | badge and bold title on one line, then a bar with the text and a `→` line that says how to fix it |
| info | `ℹ INFO` | cyan | the form of a warning; one line when there is only a title |
| hint | `→ HINT` | cyan | badge and text on one line, wrapped text hangs under the text, no bar |

Every warning is its own block. Blocks are separated by an empty line, and
no bar runs past the last line of text.

Tables have a dim header row and no rule under it. This holds for the
tables of `list`, `status`, `push` and the tables under an error.

A dry run writes each action line behind a magenta tag `○ would <verb>`,
with the text behind the longest verb of the run lined up, and ends with an
empty line and "Dry run: nothing was <verb>. Run again without `-n` to
<what it would do>." A dry run with no action has no footer.

Only a rich stream gets blocks, symbols, info, hints, the dry-run tag and
the dry-run footer. A stream that is not a terminal gets plain lines: on
stderr `error:` and `warning:` lines, on stdout the `Would <verb> ...`
lines of a dry run. Info and hint blocks are not written to it.

An info or hint follows the stream of what it belongs to. One that belongs
to a command's result and the dry-run footer go to stdout and are dropped
when stdout is not rich. One that belongs to a failure or a warning goes to
stderr and is dropped when stderr is not rich.

The warnings of a run go to stderr after its result, one block each. An
unknown config key, a collection on an SMB or NFS mount, a filesystem or a
lock file that cannot be read have a title of their own in the words of the
console; an unknown key adds that antiq ignores it and a `→` line with the
fix. Every other warning has its message as the title. A pipe gets one
`warning: <message>` line for each. Notes that are no warning, such as a
repair that ran, a reason in `push` or the run that holds the lock, are
info blocks on a rich stderr and `note: <text>` lines on a pipe.

An error on a rich stderr is an error block. Its title is the error and
its causes joined with `: `, unless the error has an explanation of its own.
These errors have one, with a title in the words of the console and the
sections "Why it happened" and "How to fix it" where they have content:
outside an antiq collection, `push` without a target, `push` refused for a
damaged or incomplete store, a target marker that names another host or
path, a held run lock, a listing of a newer format, and a path outside the
antiq collection. The `message` of the JSON error is the text it was
before. The steps the run was in are listed innermost first under "What
antiq was doing". A reference that matches several pieces or versions, or
no piece, adds a table and an info after the block (see "Piece references").
A usage error of the command line parser is a block too. An
unknown argument, an unknown command, a missing argument and two arguments
that exclude each other get a title of their own and the parser's usage line
and a pointer to `--help` under "How to fix it"; every other usage error
has the parser's first paragraph as its title and the parser's tip and
usage line under "How to fix it".

On a pipe the error is plain lines. An error line gives the title,
prefixed `error:`; it is followed by a `  why:` and a `  fix:` line where
the error has those sections, one `    candidate: <path>` line per
candidate of an ambiguous reference and one
`  while attempting to <action>` line per context, the innermost first. The
reasons why one piece is refused are joined with `, ` by `archive` and with
`; ` by `status`. A cause whose text the cause above it already
contains is not repeated, on the console and in the JSON `message`. Under
`--json` an error is data: its JSON goes to stdout, like any other JSON
output, and a usage error too prints the JSON envelope (kind `usage`) and
exits 2. When the reader of stdout or stderr is gone, antiq stops writing to
that stream without a panic, and the run ends with the exit code it would have had.

### JSON output

Every JSON output is one object `{"format": 1, "command": "<name>", ...}`
on one line of stdout. `<name>` is the command's name. A command line that
fails to parse before naming a command has `"command": null`. An error is
`{"format": 1, "command": ..., "error": {"kind": ..., "message": ...}}`,
where `message` is the text of the error without the context lines a
console shows. Some kinds add keys to the error object. A `--dry-run` plan
carries `"dry_run": true` and an `actions` list after it (`lock_held`
stands between the two while another run holds the lock); repairs come
first in it, as actions of kind `repair`. A real run has neither
`dry_run` nor `actions`. Every action has a `kind`, and the repairs a real
run lists under `repairs` are `{"kind": "repair", ...}` as in a plan. Paths are in the escaped
form.

A repair action is `{"kind": "repair", "repair": "<name>", ...}` and the
fields depend on `<name>`. `piece` of `delete_sidecars` and `folder` of
`remove_empty_folder` are relative to `pieces/`, `piece` of
`delete_restored_version` to the collection root, and `entry` is a name in
`tmp/`. `tree` and `path` are relative to the stamp folder
`<area>/<stamp>/`, or to the area itself where `stamp` is absent. `stamp`
is a version stamp or the name of a stamp folder.

| `repair` | Repair | Fields |
|----------|--------|--------|
| `create_folder` | before 1 | `folder` (name of the missing folder below `.antiq/`) |
| `delete_sidecars` | 1 | `piece` (store folder), `version` (stamp) |
| `delete_temporary` | 2 | `entry` (name in `tmp/`) |
| `delete_doomed` | 3 | `stamp` (absent for an entry directly in `doomed/`), `tree` |
| `give_back` | 4 | `stamp` (absent for an entry directly in `trash/`), `tree`, `destination` (`live` or `recovered`) |
| `delete_restored_version` | 5 | `piece` (live path), `version` (stamp), `delete` (false when the stamp names no version and is only dropped from the marker) |
| `remove_empty_folder` | 6 | `folder` (below `pieces/`) |
| `remove_empty_chain` | 3, 4 | `area` (`trash` or `doomed`), `stamp`, `path` (below the stamp folder; empty for the stamp folder itself) |

In a dry run `destination` is a prediction: whether the live path is free
and its parent is a directory when the plan is made. The run decides.

| Kind | Exit | Raised when | Added keys |
|------|------|-------------|------------|
| `usage` | 2 | the command line does not parse, a `--pattern` of `init` is invalid, the `--name` of `fork` or the `--as` of `import` is not a path, the piece of `archive`, `pin`, `unpin`, `unearth`, `fork`, `forget` or `rebuild`, or the version of `unearth`, `fork`, `forget` or `rebuild`, is empty, the `--name` of `fork` is empty, the `--time` of `import` is not a time, a pattern of `grep` is not a valid regular expression, or `--init` or `--force` is given with `--if-reachable` | |
| `not_a_collection` | 1 | a command runs outside a collection | |
| `ambiguous_reference` | 1 | a piece reference matches several candidates | `candidates`: list of `{path}` |
| `unknown_reference` | 1 | a piece reference matches no candidate, or `unearth`, `fork` or `rebuild` is given a piece that holds only incomplete versions | |
| `outside_collection` | 1 | a path given as a piece, `--as` or `--name` lies outside the collection | |
| `lock_held` | 1 | another run holds the lock | `pid`, `started` (RFC 3339, whole seconds); neither while the holder has not written them yet |
| `target_unreachable` | 4 | the target cannot be reached; the message is the reason | |
| `no_target` | 1 | `push` runs without `[target] url` | |
| `format_too_new` | 1 | `unearth`, `fork` or `rebuild` meets a listing of a newer format (see "Damage"), or `push` meets a marker with a higher `format` | |
| `other` | 1 | any other failure | |

`--help` and the top-level `--version` print text and exit 0, also under `--json`.

`archive` adds `archived`, `skipped`, `refused`, `repairs` and `warnings`.
An element of `archived` has `piece`, `stamp` (the version stamp), `version`
(its time in UTC, RFC 3339), `archive_bytes`, `files` and `bytes`. An
element of `skipped` has `piece`, `reason` (`differs`, `no_room` or `error`)
and `message`. An element of `refused` has `piece` and `reasons`, each with
`kind`, `path`, `text` and, for a rule file at fault, `line`. The kinds are
`mount_point`, `nested_collection`, `unreadable_entry`, `exclude_entry`,
`locked`, `rule_file`, `not_a_file`, `unreadable_marker`, `pinned` and
`empty`. `repairs` lists the repairs of the start of the run, each as
`{"kind": "repair", "repair": "<name>", ...}`, and `warnings` is a list of
`{message}` objects. A dry run adds `refused` and `warnings` after
`actions`, which hold the repairs, each as in `repairs`, and, per piece,
`{"kind": "archive", "piece": ...}`.

`push` has `result`, `target` (the `[target] url` as written) and
`warnings`; `dry_run` comes first in a dry run only. `result` is `"pushed"` in a real run and in a dry run, `"unreachable"` for an unreachable target. In a real run `initialized` (the
marker was written), `forced`, `pieces` (each `{piece, created, updated,
deleted}`, only pieces that changed, by path), `other` (the same counts for
paths outside every piece), `vanished` (the paths of the `file has vanished`
lines, as rsync prints them: with the collection root in front and not
escaped), `second_pass` and `notes` come between `target` and `warnings`,
followed by `repairs` (the repairs of the start of the run, each as
`{"kind": "repair", "repair": ...}`) before `warnings`. A dry run has `actions` after `dry_run`: the repairs, then `{"kind":
"write_marker"}` where `--init` would write it, then `{"kind": "mirror",
"pieces": ..., "other": ...}`, and has none of `initialized`, `forced`,
`pieces`, `other`, `vanished`, `second_pass`, `notes` and `repairs`. A dry
run that finds the lock held has `dry_run`, `lock_held` (`pid`, `started`)
and an empty `actions`. A target that cannot be reached under
`--if-reachable` exits 0 with `result` `"unreachable"` and `reason` in place
of the counts, with `repairs` and `warnings` after them, and the console
prints `note: <reason>` on stderr and nothing on stdout. Without `--if-reachable` it is the error kind
`target_unreachable`. In text the repairs that were done go to stderr as
`note: repaired: <repair>` lines, after the `note: <reason>` of an
unreachable target, and the warnings follow as `warning: <message>` lines. The
warnings hold the notes on the settings of the collection (an unknown key
under `[target]` among them) besides those of the repairs.

`list` adds `versions`, `warnings` (the notes on the settings of the
collection and the messages about files that could not be read, as
`{message}` objects) and, while a run holds the lock, `note`
(a text). Each version is `piece`, `stamp` (the version stamp, which has the
counter), `version` (the time of the stamp in UTC, RFC 3339), `state` (see
"list"), `archive_bytes`, `bytes` (the unpacked size) and `files` (numbers,
null where the head of the listing lacks them) and `damage` (a list of texts).

`grep` adds `pattern`, `pieces`, `skipped` and `warnings` (the notes on the
settings of the collection and on the lock file, as `{message}` objects),
and a `note` field while the lock is held. Each element of `pieces` has
`piece`, `name_match`, `versions` and `files`. `versions` lists the searched
versions of the piece, newest first, each with `stamp` (the version stamp)
and `version` (its time as RFC 3339 in UTC); for a piece whose name matches
each also has `files`, the file count of that version. `files` lists the
matching paths in byte order, each with `path` and the `versions` that hold
it as stamps, newest first; it is empty for a piece whose name matches.
`skipped` lists the versions that could not be searched, each with `piece`,
`stamp` and `reason`, and is empty when there are none. Under `--json` a
skipped version and the lock note appear only in the object, while the note
that nothing matched is on stderr as in text output.

`pin` and `unpin` add `piece`, `pinned` (whether the piece is pinned after
the run), `changed` (whether the pin was in the other state before it),
`repairs` and `warnings`, as for `forget`. A dry run has `piece`, `pinned`
and `changed` after `dry_run`, then `actions` and `warnings`: the repair
actions first, then `{"kind": "pin", "piece": ...}` or `{"kind": "unpin",
"piece": ...}` when `changed` is true.

`forget` adds `forgotten`, `failed`, `repairs` and `warnings` (as for
`list`, plus the repairs that failed). Each forgotten version is `piece`, `stamp` (the version stamp),
`version` (its time as RFC 3339 in UTC), `incomplete` and `files`, the bare
names of its files in the order they were deleted. Each failed version is
`piece`, `stamp` and `reason`. `forgotten` and `failed` list their versions
newest first. `repairs` lists the repairs that were done,
each as `{"kind": "repair", "repair": ...}`. A dry run has `actions` and
`warnings`: the repair actions first, then one action
`{"kind": "forget_version", ...}` for each version, newest first, with the
keys of a forgotten version.

`rebuild` adds `piece`, `stamp` (the version stamp), `version` (its time as
RFC 3339 in UTC), `basis` (what the archive was checked against:
`checksum_file`, `listing` or `readable_only`), `written` (the bare names
of the files written, in order), `repairs` and `warnings`, as for `forget`.
A dry run has `actions` and `warnings`: the repair actions first, then
`{"kind": "rebuild_version", ...}` with `piece`, `stamp`, `version`,
`basis`, `archive`, `against` (the surviving file the archive is checked
against; null for `readable_only`) and `write` (the names of the files it
would write).

`import` adds `imported` (null, or `piece`, `stamp` (the version stamp),
`version` (its time in UTC, RFC 3339), `archive_bytes`, `files` and `bytes`),
`skipped` (null, or `piece`, `reason` and `message` as for `archive`),
`repairs` and `warnings`. A dry run has `actions` and `warnings`: the repair
actions first, then `{"kind": "import", "source", "piece", "stamp",
"version", "left_out"}` with `left_out` the list of paths the version leaves
out, and, when the import would not fit, `skipped` as above, which the
action carries as a key.

`init` adds `created`, `id` (absent in a dry run), `path` (the canonical
collection root), `patterns`, `due` (the number of folders the next `archive`
would archive; absent in a dry run and when the survey failed) and
`warnings`. A dry run also has `config`, the text of the config that would be
written, and `actions`, both before `warnings`; a real run has no `actions`.
The action is `{"kind": "create_collection", "path": ...}`.

`status` has these keys, all of them present with an empty list when there
is nothing to list: `folders`, `versions` and `archive_bytes` (the last two
`null` when the store cannot be read), `idle_days`, `soon_days`, `pieces`,
`future_times`, `damaged_versions`, `incomplete_versions`,
`newer_format_versions`, `recovered`, `trash`, `doomed`, `spelling`,
`unreadable_directories`, `unparsable_files`, `warnings` and `store_error`
(`null` or the text of the note). `pieces` lists the pieces that have a
state, sorted by path, each as `{"piece", "state", "idle_for_days",
"reasons"}` with the state `due`, `soon`, `pinned` or `refused`;
`idle_for_days` is a whole number or `null`; a `soon` piece adds
`"due_in_days"`; a reason is `{"kind", "path", "text"}` plus `"line"` for a
rule file, with the kind `mount_point`, `nested_collection`,
`unreadable_entry`, `exclude_entry`, `locked`, `rule_file`, `not_a_file` or
`unreadable_marker`. `future_times` holds `{"path", "time"}` (RFC 3339 in
UTC). The version lists hold `{"piece", "version"}` with the version stamp,
and `damaged_versions` adds `"reasons"`, a list of texts. `doomed` holds
`{"path", "first_entry", "cause"}` with the cause `locked`,
`directory_not_writable` or `null`; `spelling` holds `{"live", "stored"}`.
`recovered`, `trash`, `unparsable_files` and the paths of `doomed` are
relative to `.antiq/`. `warnings` is a list of `{message}` objects. While
the lock is held, a `note` field names the holder.

`unearth` has `unearthed` (false when the version did not fit), `piece`,
`stamp`, `version` (the time of the stamp in UTC, RFC 3339), `path`,
`bytes` (the unpacked size), `repairs` (the repairs done at the start of
the run, each as `{"kind": "repair", "repair": ...}`), `warnings`, and, only when no room was found,
`skipped` as `{"reason": "no_space", "needed", "available"}` in bytes.
A dry run has `dry_run`, `actions` and `warnings`; after the repairs it
lists one action `{"kind": "unearth", "piece", "stamp", "version",
"path", "bytes", "checks"}` with `checks` in the order `version`,
`destination`, `free_space`, `member_names`, `extracted_tree`. Without room
for the version the action has `skipped` as above and the exit status is 3.

`fork` has `forked` (false when the version did not fit), `piece`, `stamp`,
`version` (the time of the stamp in UTC, RFC 3339), `path` (the destination),
`bytes` (the unpacked size; null without a usable listing), `repairs`
(each as `{"kind": "repair", "repair": ...}`), `warnings`, and, only when no room was found, `skipped` as `{"reason":
"no_space", "needed", "available"}` in bytes, with `needed` null when the
volume ran full during the extraction of a version without a usable listing.
A dry run has `dry_run`, `actions` and `warnings`; after the repairs it
lists one action `{"kind": "fork", "piece", "stamp", "version", "path",
"bytes", "checks"}` with `checks` in the order `version`, `destination`,
`free_space`, `member_names`, `extracted_tree`, each left out where the
section "fork" says so, and `damage` (a list of texts) for a damaged
version. Without room for the version the action has `skipped` as above and
the exit status is 3.
