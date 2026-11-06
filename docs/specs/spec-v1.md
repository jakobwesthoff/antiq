# Specification, version 1

Draft. This file describes version 1 as its design settles. The reasons
behind the decisions are recorded as ADRs in `docs/adrs/`; research
behind technical choices is in `docs/research/`.

## Purpose

antiq looks after directories where working folders pile up, such as
`~/playground`. It packs folders that have not changed for a set number
of days into compressed archives, keeps every folder searchable through
its metadata, brings archives back on request, backs live folders up to
a remote, and moves archives to the remote in tiers.

## Scope

Version 1 covers what the scripts `archive.sh` and `timecapsule-sync.sh`
do for `~/playground`:

- archiving folders by age
- compressed tar archives
- an index of each archive's contents
- searching the archives and bringing one back through a picker
- rebuilding missing index entries
- backing up to a NAS

On top of that, version 1 moves archives through tiers by age, including
the remote-only tier (ADR 0007), backs live folders up as packages, lets
several machines share one remote, and imports existing compressed tar
archives (ADR 0055).

Not in version 1:

- syncing one live folder between machines (ADR 0008, ADR 0017)
- reading the archives and listings that `archive.sh` produced
  (ADR 0009)
- changing a collection's selection strategy after `init` (ADR 0012)
- more than one remote per collection (ADR 0015)
- backing up content outside every piece (ADR 0041)
- a setting that keeps a folder until its archive is on the remote
  (ADR 0032)
- extended attributes (ADR 0050)
- keeping sparse files sparse: they are stored in full and come back
  fully allocated

## Collections and pieces

A managed folder is called a collection, and each unit in it a piece.
(ADR 0022)

A collection is marked by `.antiq/` at its top. A `.antiq` that is a
file or a symlink does not mark a collection. Commands find it by
searching upwards from the current directory, or from the path given
with `-C`, and fail outside a collection. `.antiq/` holds the
collection's config, metadata, index and local archives. (ADR 0011)

`init` mints the collection's ULID and stores it in the config together
with the machine name. It lists what the patterns select and asks before
creating the collection. It detects the layout `archive.sh` leaves
(`archive/`, a real directory, holding a `*.tar.xz` file, and
`archive/index/` holding a `*.lst` file) and, when a pattern selects
`archive`, proposes excluding it; the listing is made with the proposed
exclude applied. It notes when a pattern contains `**`. Without a
terminal, or with `--json`, it creates nothing and prints the plan
unless `--yes` is given, and exits 3. Declining the question, or not
answering it, ends the same way. `--yes` accepts the plan, the proposed
exclude included. `--dry-run` prints the plan, asks nothing, creates
nothing and exits 0. `--pattern` (repeatable) sets the patterns; every
other setting starts at its default and is changed in the config file.
A `--pattern` that is invalid (see "Selection") is a usage error: kind
`usage`, exit 2.
`init` creates `.antiq/` with the directories of the local layout and
the config in one step, so it exists complete or not at all. It refuses,
also under `--dry-run`, where `.antiq/` already exists and inside a
collection, naming the existing collection's root, where a `.antiq` is a
file or a symlink, and where a folder that would be a piece holds a
`.antiq/` directory of its own, naming each such folder. Every refusal
creates nothing and exits 1.

### Selection

Pieces are the directories antiq treats as one thing each. A list of
glob patterns selects them, default `*`. When matches nest, the
shallowest match is the piece. (ADR 0012) An entry whose name starts
with a dot is a piece only when a pattern names it with a literal
leading dot in that path component. (ADR 0028)

Never pieces: `.antiq/`, symlinks, and paths on the collection's exclude
list. Exclude entries are literal paths relative to the collection
root, not patterns, and nothing below an excluded path is a piece. The
exclude list is applied before pieces are matched and before anything
below an entry is listed. An exclude entry that exists also excludes the
directory it names under another spelling that the volume treats as the
same name, such as another case, since the directory is recognised by its
device and inode as well. Selection uses lstat and never descends into
a symlink. Skipped symlinks and dot entries are counted in `status`: a
skipped symlink is one that a pattern selects, a skipped dot entry a
directory that a pattern would select, or look below, if the dot rule did
not keep it out.

A pattern is matched one path component at a time. A component that
begins with a dot is matched only by a pattern component that begins with
a literal dot; no other pattern component, `*.txt` included, and no `**`
matches it. `**` stands for any number of directories without a leading
dot, none included, and for one or more at the end of a pattern, so
`a/**` selects what is below `a` and not `a`. A pattern is refused if it has an
empty component (a trailing `/` included), a `.` or `..` component, or a
`/` inside a bracket class, which never matches a separator.

A path that is valid UTF-8 is matched against the patterns as it is. A
path with bytes that are not valid UTF-8 is matched in its escaped form
(see "Metadata"), since the glob matcher takes only UTF-8. A pattern can
therefore not tell the two apart: `a%FF` matches the valid name `a%FF`
and the name `a` followed by the byte `0xFF`, and in the escaped form a
`%` of the name reads `%25`. Patterns and names compare as bytes, so a
pattern in composed Unicode form does not match a name in decomposed
form.

A directory that selection has to look into and cannot read is skipped
and reported. A path at or below it is neither new
nor gone in that run. When the collection root itself cannot be listed,
the run fails. Running out of file descriptors fails the run as it does
in a walk. An entry deleted while selection runs is skipped. Selection
looks no more than 512 levels deep; a deeper directory is not entered and
is reported like one it cannot read.

An exclude entry that exists, found below a selected piece, is not part of
that piece: the piece is never archived, reclaimed or packed for backup,
and `status` lists it with the excluded path. An exclude entry that does
not exist does not count.

A folder at a path that no tracking entry claims is a new piece with a
new ID, also when an archived piece has the same path; `status` and
`find` show both.

### Walks

Every walk of a piece (idle check, packing, verifying, change detection,
deleting) uses lstat, does not follow links, and stays on the device of
the piece's top directory. A piece that contains another device (a
mount point or a btrfs subvolume) is never archived, reclaimed or packed
for backup; `status` lists it with the path of the mount point. A piece
whose top directory is itself on another device than the collection
root, is itself a mount root, or lies below a directory between the
collection root and the top that is a mount root, counts as containing a
mount point, at its own path. A mount root above the top shares the device
of everything below it, so the walk reads the directories above the top as
well. The walk
compares the device of every entry with the top directory's, not only
that of directories: an entry on another device is recorded, and a
directory there is not entered. The same holds for the root of a mounted
filesystem that shares its parent's device, such as a bind mount: the
walk reads whether an entry is a mount root. On Linux that is the
`STATX_ATTR_MOUNT_ROOT` attribute of `statx`; a kernel older than 5.8
does not report it, and there only the device numbers decide. On macOS
it is the mount point name of the directory (`f_mntonname`) differing
from its parent's. A firmlink (macOS links directories such as `/Users`,
`/Library` and `/usr/local` into the data volume) compares as a mount
root there, so a piece containing one is refused, which is the safe
direction. A directory that cannot be searched is not compared on
macOS; the delete helper reads its flag again once it has given the
directory owner access and has opened it.
The walk does not enter an excluded directory either: a piece that
contains an existing exclude entry is treated like one that contains a
mount point, and `status` lists it with the excluded path. So is a piece
that contains a directory named `.antiq`, the marker of a collection of
its own: `status` lists it with the directory that holds the marker, and
the walk goes on into it. A walk enters
directories up to 512 levels below the piece's top, since each level holds
one directory open. A deeper directory is not entered and makes the piece
unreadable.
The delete helper never crosses a device boundary or enters a mount root,
refuses a path with a mount root above its top, refuses the root of its
own tree, and stops at the same depth: a
directory more than 512 levels below the top it removes is not entered
and fails the removal, naming that directory.

An entry the walk cannot read makes the piece unreadable: it is never
idle, never archived, its live package is not refreshed, and `status`
lists it with the first entry that failed. That is the smallest path
among the failed entries, in the byte order the entries are sorted by.
An entry that has vanished since its directory was listed counts as one
the walk cannot read. Running out of file
descriptors (`EMFILE`, `ENFILE`) is not an unreadable entry: it fails
the run. antiq raises its soft limit of open files at start, since
launchd starts jobs with a soft limit of 256.

Paths longer than the platform's path limit are walked, packed,
verified, extracted and deleted relative to an open directory, never
through the full path.

### Idle

A piece is idle when the newest modification time of any regular file
in it is older than the configured number of days. A day is 24 hours,
and a file exactly that many days old is not yet old enough. Directory
times do not count, nor do those of symlinks and special files.
Modification times more than one day in the future do not count either;
a time exactly one day ahead still counts. `status` lists the files with
a time too far ahead. (ADR 0013)

Ignore rules in gitignore syntax, from the config and from
`.antiqignore` files at any level, exclude paths from this check only.
Changes to `.antiqignore` and `.antiq-pin` files do not count as
activity. (ADR 0013, ADR 0029)

The rules follow gitignore semantics. The lines of `collection.ignore` apply inside
every piece, anchored at the piece's top: a leading `/` means the top.
A `.antiqignore` applies to its own directory and everything below it,
anchored at that directory. Where several files have a rule for a path,
the nearest `.antiqignore` wins, then those further out, then the config;
within one file the last matching rule wins. A directory that an ignore
rule excludes hides everything below it, and a `!` rule cannot bring back
a path inside it. An excluded directory's `.antiqignore` is not read. Only
a regular file named `.antiqignore` is read. A rule that is not valid, a
file that is not UTF-8 text and a file that cannot be read each make the
check fail with the file and line named, so a piece with broken rules is
never called idle.

A junk list, by default `.DS_Store`, is left out of the idle check,
packages, listings and verification. Junk files are not packed into
archives or live packages. Only entries that are not directories are
junk: a directory with a junk name is walked like any other. The junk list is separate from the ignore
rules.

For an unearthed piece, the idle clock starts at the later of its newest
file modification time and the time it was unearthed. An unearthing time
more than one day in the future is taken as one day ahead. (ADR 0037)

A piece in which the idle check finds no file to take a time from (no
regular files after the ignore rules, marker files and the junk list) is
empty: never idle and never archived. A piece whose only countable files
lie in the future is not empty and not idle, and the unearthing time
does not change that. A piece that the walk could not read completely
has no verdict. (ADR 0038)

### Pins

A piece with a `.antiq-pin` file at its top is pinned: it is never
archived, still backed up, and shown as pinned. Any entry of that name
that is not a directory pins the piece, a dangling symlink included; a
directory of that name is not a pin, and `unpin` refuses to remove it.
`pin` and `unpin` create and remove the file and refuse on a piece whose
folder is not present. They take a new piece, which has no ID yet, as well
as an object. A piece that is in the state asked for is left as it is and
reported as such. A pin and its removal are durable when `pin` and
`unpin` return: the file and the entry in the piece's directory are
flushed. The pin is checked before the idle and empty tests. (ADR 0024)

## Objects

Every piece is an object with a ULID. The ID survives archiving,
unearthing and archiving again; the newest archive replaces the
previous one as the current archive. Forking gives an object a new ID.
(ADR 0014) ULIDs are written in upper case, in metadata, file names and
output alike; references accept either case.

The ID time is the earliest of the directory's birth time and the birth
and modification times of every entry inside it, ignoring times before
1990; when a birth time and a modification time are equal, the source is
`oldest-mtime`; when no such time remains, it is the time of minting,
with a warning. Its source (`birth`, `oldest-mtime`, `import`, `fork`,
`minted`) is stored with it. The ID is minted once and never
recomputed. (ADR 0039) `archive` and `sync` mint IDs for new pieces,
`fork` for the fork (source `fork`, the time of the fork), `import` for
each archive (source `import`). `status` shows a piece without an ID as
new.

A live folder's package and its later archive are the same object.
(ADR 0018)

Each object has an owning collection: the collection that created it,
or the one that last unearthed or forked it. A machine applies
selection, idle, tiers, the brake and reclaiming only to objects its
collection owns, and to local archives it holds. A folder copied
together with its `.antiq/` to another machine is not detected as a
copy.

### Versions

Two numbers are kept apart:

- the **metadata version** N of `meta.<N>`, which every committed change
  to an ID's metadata increases;
- the **payload version** of each payload, counted per ID and payload
  kind, starting at 1.

Conflicts and divergence compare metadata versions. Tier checks, the
current archive and `unearth --version` refer to archive payloads.

### Metadata

Metadata per ID, shared through the remote, is one JSON object. Keys
are written in this order:

| Key | Type | Content |
|-----|------|---------|
| `format` | integer | metadata format, `1` |
| `id` | string | the object's ULID |
| `collection` | string | ULID of the owning collection |
| `path` | string | relative path in the collection, escaped form |
| `name` | string | the folder's name, escaped form |
| `created` | string | ID time, RFC 3339 UTC |
| `created_source` | string | `birth`, `oldest-mtime`, `import`, `fork` or `minted` |
| `lifecycle` | string | `live` or `archived` |
| `payloads` | array | payload objects, below |
| `history` | array | append-only, entries `{time, machine, action}` |

A payload object has, in this order:

| Key | Type | Content |
|-----|------|---------|
| `kind` | string | `archive` or `live` |
| `version` | integer | payload version |
| `file` | string | payload file name |
| `package_format` | string | such as `tar+zstd/1` |
| `size` | integer | bytes |
| `checksum` | object | `{algorithm: "sha256", value: <hex>}` |
| `listing` | string | listing file name (archives) |
| `machine` | string | machine name of the writer |
| `created` | string | RFC 3339 UTC |
| `verified` | string or null | time the remote copy was last checked (ADR 0043) |
| `superseded` | boolean | the archive lost a conflict and is kept |
| `encoder` | object | zstd version and parameters (live packages, ADR 0052) |

`listing`, `verified`, `superseded` and `encoder` are left out where they
do not apply; `verified` is `null` where it applies and the remote copy
was never checked. Times are RFC 3339 UTC in whole seconds, sizes in
bytes.
The history entry that a version adds carries the time that version was
committed, not the time the change was recorded locally; the
remote-only tier is measured from it.
`file` and `listing` name files in the object's directory, so each is one
plain name: not empty, without `/` or NUL, not starting with a dot. In
format 1 they have the names of the remote layout under "Storage". A
document that breaks either rule is refused when it is read.
Unknown fields are kept on rewrite. A file with a higher `format` is
read-only and any write to it fails with a clear error. (ADR 0053)

The `action` of a history entry is one of these:

| Action | Written by |
|--------|------------|
| `archive` | `archive` |
| `reclaim` | reclaiming |
| `unearth` | `unearth` |
| `fork` | `fork`, on the old and on the new ID |
| `sync` | `sync` |
| `import` | `import` |
| `resolve` | `resolve` |

Paths and names are stored as the bytes the filesystem holds. In the
escaped form, valid UTF-8 stays as it is, `%` becomes `%25`, and every
byte that is not part of valid UTF-8 becomes `%XX`, with upper-case hex
digits. Every byte sequence has this one escaped form, and a reader
refuses any other spelling, such as `%41` for `A` or `%ff`. A `path` is
names joined by single `/`; a name is not empty, has no `/` or NUL and is
neither `.` nor `..`.

### Listing

Each archive payload has a listing in its own zstd-compressed file next
to the metadata: JSON Lines, a first line `{"format":1}`, then one line
per entry with path (escaped form), type, size, modification time in
whole seconds, mode, link target and SHA-256. Paths are relative to the
archive's top-level directory (see "Packaging"), which has no line of
its own. Each line holds what the archive stores for the member: the
size is 0 for directories, symlinks and hard links, and a modification
time before 1970 is listed as 0. The listing is the
manifest that reclaiming compares against, for archived and imported
pieces alike. Listings are compressed with zstd at level 3 with the
content checksum and read with the same 128 MiB window limit as every
other decoder. The content checksum is checked when the last byte is read,
so the entries of a listing are verified only once it has been read to its
end. A reader refuses a listing whose first line names a higher format.

An entry line is a JSON object with these keys, in this order:

| Key | Type | Content |
|-----|------|---------|
| `path` | string | path in the escaped form |
| `type` | string | `file`, `dir`, `symlink` or `hardlink`; special files are not archived and have no type |
| `size` | integer | bytes |
| `mtime` | integer | modification time in whole seconds since the epoch |
| `mode` | integer | permission bits |
| `link` | string | the target in the escaped form, for a `hardlink` the path of the file it links to; for `symlink` and `hardlink` only |
| `sha256` | string | lower-case hex; for `file` only |

`link` is required for a `symlink` and a `hardlink`, and `sha256` for a
`file`. A listing with a line that breaks these rules or the size and time
rules above, or with a line for the top-level directory, is refused when
it is read and when it is written.

### Local state

Each machine keeps a tracking entry per folder: path, ID, the metadata
version taken out, the payload version it was unearthed from, the time
it was unearthed, a state (`unearthing`, `live`, `removing`, `gone`),
the archive payload a `removing` folder is compared with, which
payloads are present locally (each by its file name), the metadata
versions this machine committed, pending decisions, the time the entry
was written, a stale backup's time, and whether the entry is missing on
the remote or diverged. While a recorded change is not committed yet,
the version taken out reads `pending`; the commit replaces it with the
committed number. "Remote-only" is derived.

The entry states mean:

- `unearthing`: `unearth` is bringing the folder.
- `live`: the folder is in place.
- `removing`: the archive is durable and the folder is on its way out.
- `gone`: the folder was removed, or was found missing. An archived or
  remote-only piece keeps its entry in this state, with its local
  payloads.

At most one entry is `unearthing`, `live` or `removing` for a path. A
pending decision about a diverged folder implies diverged; a folder can
be diverged without a decision pending. A pending decision carries the
answers of its row under "Decisions".

An entry file that cannot be read is reported and skipped, and the
other entries are used.

### States

| State | Folder | Local payloads | Remote payloads |
|-------|--------|----------------|-----------------|
| new | present, no ID | none | none |
| live | present | none | latest live package, if backing up is on |
| unearthed | present | archive, unless its remote copy is confirmed | archive, and a live package if backing up is on |
| gone | missing, entry live | none | latest live package, if any |
| archived | gone | archive | archive once synced |
| remote-only | gone | metadata only | archive |
| live on another machine | elsewhere | metadata only | that machine's live package |

A piece with an archive payload in its metadata shows as unearthed
when its folder is present, otherwise as live. The state is derived
from the entry, the metadata and whether the folder exists. Cases the
table has no row for:

- A `live` entry whose folder is present but whose metadata is already
  archived is unearthed: the archive is durable and the entry has not
  caught up.
- An `unearthing` entry shows as unearthed with its folder, and as the
  metadata says without it.
- The folder of a `removing` entry does not count: the piece is
  archived, or remote-only without a local archive.
- A `gone` entry whose metadata is live and owned by this collection
  gets its folder back when one appears at its path, with its ID. With
  archived metadata, a folder at that path is a new piece next to the
  archived one.
- Archived metadata without an entry shows as remote-only. An entry
  without metadata, metadata of another ID, and live metadata of this
  collection without an entry are errors of that piece, which `status`
  lists and the start-of-run repairs fix.

Entries are written in an order the repairs rely on. A new ID's
metadata comes before its entry. `archive` makes payload, listing,
metadata and the entry (`pending`) durable, then sets `removing` with
the payload, renames the folder and sets `gone`. `unearth` writes
`unearthing`, renames, records the metadata change, and only then
writes `live`. `fork` records the old ID's archived metadata, sets
its entry `gone` with its local payloads, then records the new ID's
metadata and entry.

A renamed folder is a new piece, and the entry of its old path becomes
gone. A gone piece keeps its live package; `unearth` restores it from
there when no archive exists, and `reconcile --prune` drops it.

Another collection's live pieces are shown as live on that machine. They
can be unearthed only as a fork; `archive` and `sync` never touch them.
(ADR 0040)

### Metadata changes

Changes are recorded locally and committed as a new metadata version at
the next `sync`. A recorded change is the full next metadata document,
written as `objects/<ulid>/meta.pending`; `objects/<ulid>/meta.<N>`
holds only versions known to be committed. Local readers prefer
`meta.pending`. `sync` commits it with the protocol under "Committing a
version", which gives its number, and renames it locally to that
`meta.<N>`. Without a remote it stays pending. (ADR 0058)

The rename leaves the bytes as they are, so the local and the committed
copy are identical. It is refused when `N` is not above the highest
committed version in the object's directory, unless `meta.<N>` already
holds the same bytes, which a promotion cut short by a crash leaves
behind; then the leftover `meta.pending` is removed. A `meta.pending`
that cannot be read is an error, not a reason to use an older version,
as it holds the only copy of the change, and no change is recorded on top
of it. No change is recorded on top of a committed version that cannot be
read or has a higher `format`.

| Command | Changes |
|---------|---------|
| `archive` | lifecycle `archived`, a new archive payload, history |
| reclaiming | lifecycle `archived`, live payload removed, history |
| `unearth` | lifecycle `live`, owning collection, path and name (the target), history |
| `fork` | old ID: lifecycle `archived`, live payload removed; new ID created |
| `sync` | live payload replaced, `verified` times, history |
| `import` | new ID with its archive payload |
| `resolve` | as the decision table under Command line says |

`pin` and `unpin` change no metadata.

## Storage

Local storage in `.antiq/` is fixed. Remotes sit behind one trait, and
every remote supports every operation. (ADR 0015)

Local layout:

```
.antiq/config.toml
.antiq/lock
.antiq/remote-state.json              when the remote was last reachable, when the last sync finished
.antiq/objects/<ulid>/...             same layout as on the remote
.antiq/objects/<ulid>/meta.pending    a recorded change not committed yet
.antiq/tracking/<ulid>.json           tracking entries
.antiq/tracking/<ulid>.manifest.json  live manifests
.antiq/index/                         derived, rebuildable, private to this machine
.antiq/tmp/
.antiq/staging/
.antiq/trash/<ulid>                   a folder on its way out
.antiq/recovered/
```

Remote layout:

```
<root>/antiq-remote.json             {format, id, layout}
<root>/objects/<ulid>/meta.<N>
<root>/objects/<ulid>/meta.<N>.claim
<root>/objects/<ulid>/<sha256 16 hex>-<name part>.tar.zst
<root>/objects/<ulid>/<sha256 16 hex>.listing.zst
<root>/objects/<ulid>/.antiq-partial/
```

Payload files are never overwritten. A put onto an existing payload name
succeeds only when the full SHA-256 matches; for a listing, which the
metadata records no checksum for, the SHA-256 of its bytes is compared.
The name part is the piece's name with every byte outside `[A-Za-z0-9._-]`
written as `%XX`, cut to 64 bytes without splitting an escape; the real
name is only in the metadata. The listing of a payload is named after it:
the first 16 hex digits of the payload's SHA-256, followed by
`.listing.zst`. Upload temp names are `.<final name>.<machine part>.part`,
where the machine part is the machine name escaped the same way and cut
to 32 bytes; the same machine reuses the name on its next attempt. One
writer runs per collection, and the temp name sits in the object's ULID
directory, so it is unique per object.

Payloads and listings are written in `.antiq/tmp/`, flushed, and renamed
into the object's directory, which is then flushed. Every other file antiq
writes in `.antiq/` (metadata, manifests, tracking, index, config) goes
through one atomic write: a temp file in the same directory, `sync_all`
with a fallback to plain `fsync` on `ENOTSUP`, rename, `fsync` of the
directory. Whenever antiq may create an object's directory (recording a
change, storing a pulled version, putting a file), it flushes `objects/`
and `.antiq/` as well, so the directory itself is durable.

The index is derived from metadata and listings, kept as plain files,
rebuildable, and behind a module interface. Listings are fetched
incrementally: `sync` fetches the listings it does not have when it
pulls metadata, and `unearth` fetches one together with its archive.
The index holds, in `.antiq/index/<ULID>.json`, the member paths of the
current payload's listing together with that listing's file name, for
every object whose listing is stored here. Which objects exist, their
paths, names, sizes and states come from the metadata and the tracking
entries on every call. A file counts for an object only while it names
the object's current listing; a listing's file name follows from its
payload's checksum, so such a file is up to date. Commands that change
state bring the files in line with the store, and `reconcile` rebuilds
them from nothing; an index rebuilt this way equals one that was kept
up to date. Read-only commands that find a file missing, outdated or
unreadable read the listing instead and do not write the file.

### Config

The config is `.antiq/config.toml`. The `[collection]` table holds the
policy. The `[machine]` table and the `[[remote]]` list are local to the
machine, parsed strictly and never uploaded. The config holds a list of
remotes; version 1 accepts exactly one.

| Key | Default | Content |
|-----|---------|---------|
| `collection.id` | minted at `init` | collection ULID |
| `collection.patterns` | `["*"]` | selection patterns |
| `collection.exclude` | `[]` | literal paths that are never pieces, in the escaped form of the metadata |
| `collection.ignore` | `[]` | gitignore lines for the idle check, anchored at each piece's top |
| `collection.junk` | `[".DS_Store"]` | junk list |
| `collection.idle_days` | 14 | archive tier |
| `collection.remote_only_days` | 180 | remote-only tier, from the archive's commit |
| `collection.backup_live` | `true` | back up live pieces |
| `collection.live_exclude` | `[]` | paths relative to the collection root, in the escaped form of the metadata, left out of live packages only; an empty entry is an error |
| `collection.replace_without_asking` | `false` | ADR 0036 |
| `collection.brake_percent` | 20 | ADR 0045, 0 to 100 |
| `collection.grace_hours` | 24 | commit protocol |
| `collection.away_warn_days` | 7 | see `--if-reachable` |
| `collection.memory_percent` | 25 | compression memory limit, 1 to 100 |
| `collection.remote_jobs` | 4 | remote operations at a time, at least 1 |
| `collection.live_retries` | 3 | restarts of a live piece in the tolerant verify, at least 1 |
| `machine.name` | ComputerName on macOS, hostname elsewhere | stored at `init` |
| `machine.rsync` | resolved at `remote add` | absolute path |
| `machine.remote_rsync` | none | passed as `--rsync-path` |
| `machine.connect_timeout` | 30 | seconds, at least 1 |
| `machine.server_alive_interval` | 15 | seconds, at least 1 |
| `machine.server_alive_count` | 4 | at least 1 |
| `machine.control_persist` | 60 | seconds, at least 1 |
| `machine.rsync_timeout` | 300 | seconds, at least 1 |
| `remote.url` | | as given to `remote add` |
| `remote.id` | | the remote's ULID |
| `remote.host_key_alias` | none | |

On macOS the hostname stands in when ComputerName cannot be read or is
empty. An empty `machine.name` is an error.

A value outside the range given in the table is an error that names the
key. An unknown key in `[machine]` or in a `[[remote]]` entry is an
error that names it. Unknown keys under `[collection]` are ignored, and
each one is reported in a warning.

### Committing a version

1. The payload is uploaded under its temp name, flushed, and renamed to
   its final name.
2. The writer claims metadata version N+1, one above the highest
   committed version or claim, by creating `meta.<N+1>.claim`
   exclusively. An existing claim is a conflict.
3. It writes the metadata under a temp name, checks that its claim still
   exists, that no `meta.<M>` with M >= N+1 exists, and that the payload
   is still present with its size, and renames the metadata to
   `meta.<N+1>`. On the SSH remote this rename refuses an existing
   target (`ln` to the final name, then remove the temp name).
4. It lists the directory again; a higher committed version is a
   conflict.

A version is committed once `meta.<N+1>` exists; readers read only
`meta.<N>` files. (ADR 0030)

Cleanup:

- A claim without metadata older than the grace period is abandoned and
  skipped. It is removed only once a higher version is committed.
- The highest committed `meta.<N>` is retained. An older one is removed
  once a higher version is committed and it is older than the grace
  period. A listing goes with the payload it describes.
- A final-named payload is removed only when no retained metadata
  version references it, it is older than the grace period, and no
  claim is open for its ID. A referenced payload, superseded ones
  included, is never removed by cleanup.
- Cleanup runs in an object directory only when every `meta.<N>` there
  parses with a known `format`; otherwise the directory is left alone
  and reported.
- A collection removes its own temp names and partial files before the
  grace period only in object directories whose committed metadata
  names it as owner, while its lock shows no other run. Its temp names
  include `<temp name>.` followed by six characters of `[A-Za-z0-9]`,
  the file rsync leaves when it is killed hard; such files of other
  machines are removed only after the grace period.
- Ages are measured on the remote's clock: over SSH against `date +%s`
  printed by the same command, on a path remote against the modification
  time of a probe file written there. A dry run writes no probe file;
  it reports cleanup that depends on an age as "age unknown in a dry
  run".

When the highest `meta.<N>` does not parse, `status` and `reconcile`
report it, readers use the highest version that parses, and nothing is
written on top of it until it is resolved. An object with no version that
parses and no recorded change has no readable metadata, which is an
error. `reconcile` reports any ID whose committed version was written by
another machine than the local record says.

## Remotes

The remote holds the full metadata set, one current metadata version
per ID, next to the archives. Attaching a remote copies its metadata;
archives are fetched on demand. (ADR 0016)

A remote root holds the marker `antiq-remote.json` with the remote's
ULID and the layout version, and the config stores that ULID. Every
remote operation checks the marker first and refuses when it is missing
or does not match. (ADR 0031)

`remote add <url>` attaches an existing remote and refuses when its
root has no marker; `remote add --init <url>` creates a new remote root.
(ADR 0026) The URL is `ssh://[user@]host[:port]/<path>` for the SSH
remote or an absolute local path for the path remote. The root path
may contain only `[A-Za-z0-9._/-]`. `remote add` refuses while a remote
is attached, and refuses with the reason when a probe fails: no rsync
3.1 or later, openrsync, no SHA-256 tool on the NAS, a failing
exclusive create, the remote unreachable. It writes nothing then.
`remote show` prints kind, URL, remote ULID, reachability and the probed
versions. `remote remove` only detaches: it deletes nothing on the
remote and keeps the local metadata, lists the remote-only pieces that
become unreachable, and goes through the brake. A path remote whose root
is not a mount point gets a warning.

Without a remote, `sync` fails with exit 1 and the error kind
`no_remote`, `remote show` reports that none is attached and exits 0,
`reconcile` does its local part, and `--if-reachable` changes nothing.

Remote operations include reachability with three results (reachable,
away, broken), bulk metadata pull and push, payload put, get and delete,
and exclusive creation. A run does at most `remote_jobs` remote
operations at a time; compression may run ahead of uploads.

When the remote has been away for longer than `away_warn_days` while
remote work is pending, `status` warns and `--if-reachable` no longer
turns away into exit 0. The time the remote was last reachable is kept
in `.antiq/remote-state.json`, a JSON object. Its key `last_sync` holds the
time the last `sync` finished (RFC 3339, whole seconds); a reader takes
that key and ignores every other.

### Path remote

The SSH remote is built first. The path remote stays in version 1 and
copies plainly: a temporary name in the target directory, written,
flushed and renamed. Flushing is `sync_all` with a fallback to plain
`fsync` on `ENOTSUP`, for the file and for its directory after the
rename. Live packages on a path remote are uploaded in full on every
change. (ADR 0047)

Reachability: a missing root, or a root that is not a mount point and
has no marker, is away. A marker with another ULID, a mount point
without a marker, or an I/O error on the marker is broken. A matching
marker is reachable.

### SSH remote

antiq runs the system's `ssh` with a master connection per run, a short
finite `ControlPersist` and a short `ControlPath` in a per-user
directory, named with the run's pid and a counter that keeps two
sessions of one process apart (`<pid>-<n>.sock`). The directory is
`/tmp/antiq-<uid>/` with mode `0700`, checked before use: it must be a
directory, not a link, owned by the user. The `ControlPath` is at most
86 bytes, since `ssh -M` binds the path plus a suffix of 17 characters
within the 104 bytes macOS allows for a socket path; a longer one is
refused with antiq's own error. At start, the sockets and master logs
of runs that no longer exist are removed, and so is a socket at the new
run's own path, which an earlier run with the same pid may have left
behind. Only sockets and master logs are removed, and only in this
directory.

The master writes its messages to a log next to the socket
(`<pid>-<n>.log`, `ssh -E`), since a detached master has no standard
error. The failures of starting a master and the keepalive timeout of a
master that lost its server are read from that log.

Before each remote phase antiq runs `-O check`; when it fails, antiq
sends `-O exit`, removes the socket (only a socket), starts a new master
and checks it again. If anything other than a socket is at the control
path, no master is started, because `ssh -M` there leaves a plain
connection running that is no master. If starting the master fails with
exit status 255, the failure is classified as away or broken like a
command's. If the master starts and the second check still fails, or
something is in the way, commands run as direct connections, with a
warning. At the end of the run it sends `-O exit`. Every call passes
`BatchMode=yes`, `ConnectTimeout`, `ServerAliveInterval`,
`ServerAliveCountMax`, and `HostKeyAlias` when configured. (ADR 0048)

Every remote command runs as `/bin/sh -s` with the script and its
quoted values on standard input, so the login shell parses nothing but
that. The script is one brace group that starts by setting `LC_ALL=C`,
so a connection that ends early runs nothing. Exclusive creation runs
in that shell with `set -C`, never through rsync; `remote add` checks
that a second create fails.

ssh runs with `LC_ALL=C`, because its error text is matched, and with
`LogLevel=INFO`, `ClearAllForwardings=yes` and `PermitLocalCommand=no`
set on the command line, so that settings in the user's `~/.ssh/config`
neither hide that text nor start forwardings or local commands. Commands
also get `-T` and `RemoteCommand=none`, so that a configured terminal or
remote command does not change the script's input and output.

Exit status 255 and ssh's error text decide between away and broken. A
script never exits with status 255. Only lines in ssh's own forms count.
Lines about the control socket, the master connection or a proxy's
`UNKNOWN port 65535` do not, and failures of the master connection are
told apart from the remote's state. These are away: `ssh: connect to
host <host> port <port>: <reason>` with any reason, which ssh prints
when the connection itself fails, `ssh: Could not resolve hostname`, a
keepalive timeout (`Timeout, server <host> not responding.`) and a
banner exchange timeout. These are broken: a refused login, a refused
or changed host key, and a connection the server closes or resets. A
broken line wins over an away line. A failure with no line in these
forms is broken. "Connection refused" means away only in the connect
form; a stale control socket gives the same words in another form. When
the master connection drops while a command runs, ssh reports nothing
to the command, and antiq reads the master's log for the keepalive
text. A session the server refuses through its `MaxSessions` limit gives
one warning per run.

After writing a run's temp files and before their renames, antiq runs
`sync` on the NAS, and once more after the renames.

### Transfers

On the SSH remote, live packages and archives travel with rsync 3.1 or
later; openrsync is refused. The local rsync is resolved at
`remote add` (the configured path, else the first of `PATH`,
`/opt/homebrew/bin`, `/usr/local/bin` that passes the probe), stored as
an absolute path, and probed on every run. The NAS's rsync and hash tool
are probed at `remote add`, through the same ssh command antiq uses
later. (ADR 0049)

rsync runs with `LC_ALL=C`, every `RSYNC_*` variable and `CVSIGNORE`
removed from its environment, `--stats`, `--no-human-readable`,
`--no-whole-file` and `--timeout`, never with `--inplace` or
`--append`. A new live package is seeded on the NAS as a hard link to
the current one and transferred onto it; no transfer of a live package
deletes files. An archive is uploaded onto its temp name with
`--partial-dir=.antiq-partial`, so an interrupted upload resumes, and
fetched for unearthing into `.antiq/tmp/`. Any non-zero exit fails the
transfer: the temp name stays for the next attempt or is removed, and
the piece stays pending. After a transfer, the size on the NAS is
checked against the local file in the same command as the rename.

### Several machines

Several machines may share one remote. Putting an ID back with a
metadata version that is no longer current is a conflict:

| Choice | Remote | This machine |
|--------|--------|--------------|
| mine | new version: my archive current, theirs superseded | keeps tracking its version |
| theirs | nothing written | my local archive goes through trash; tracking follows theirs |
| fork | my archive becomes version 1 of a new ID; theirs stays current | tracks the new ID |

A superseded archive stays referenced until `resolve <piece>
--drop-superseded` removes it, through the brake, in a new version.
`status` lists pieces with superseded archives; `unearth --version`
brings one back. The machine whose version lost learns it at its next
sync. (ADR 0017, ADR 0030)

A pending change that touches only metadata, such as `verified` times
or history entries, is no conflict when the remote has a newer version:
it is applied on top of that version and committed as the next one. A
`verified` time is dropped when that version no longer references its
payload, the local copy of the payload is kept, and history entries
merge. (ADR 0017)

`sync` pulls metadata first. A local archive is dropped only when that
payload, by its checksum, is referenced by the remote's metadata
(current or superseded) and its remote check is recorded (ADR 0043). A
local archive that no remote metadata references is a conflict and is
never dropped. When the remote's current archive is newer than the local
one, the newer one is fetched while the piece is within the local tier;
otherwise only the metadata is updated.

An unearthed folder is diverged when the remote's current metadata
version is neither the one taken out nor one this machine committed
since. `archive` and `sync` refuse a diverged folder until it is
resolved.

## Packaging

A Packager trait packs, verifies, lists and unpacks. Package formats
have ids such as `tar+zstd/1`. Archives and live packages are both tar
compressed with zstd, with different settings, and with zstd's content
checksum, which `tar+zstd/1` implies. (ADR 0019)

Tar is written in-process with hand-built GNU headers: mode masked to
permission bits, uid and gid 0, empty owner names, the modification time
in whole seconds (the floor of the file's time, times before 1970
clamped to 0 and reported), no extended attributes, entries sorted by
path bytes, hard links stored as links, special files skipped and
reported. Members follow the byte order of their paths below the
top-level directory, without the trailing slash of directories. A hard
link names the first path in that order that holds the same file as its
target. A symlink's target is stored as the bytes the filesystem holds.
Each file is read through a reader limited to the size in its
header; a short read is padded with zeros and marks the file as changed.
Each file's SHA-256 is computed in the same read and written to the
listing. Sparse files are stored in full; `archive` warns when a piece's
apparent size exceeds its allocated size by more than 1 GiB. (ADR 0050)

An archive holds one top-level directory, named after the folder when it
was packed, with the piece's entries below it. That directory is the
first member and has the mode and modification time of the piece's top
directory. Listings, the strict
verify and reclaiming compare the paths below it, so neither that name
nor the folder's current name counts: a piece renamed by `fork --name`,
unearthed with `--to` or imported with `--path` compares like any
other. Unearthing extracts the directory's contents into the target.
Each file, symlink and directory is created by antiq itself through open
directory handles, never through a symlink, and a name that is already
taken fails the extraction. Directories keep owner access while their
members are written; once all members exist, the stored modification
time and then the stored mode of every directory are set, the deepest
first, and the target's modification time last. A symlink gets its stored
mode where the platform can change it (macOS) and keeps the platform's
mode where it cannot (Linux). The compressed stream is read to its end,
so a failing content checksum or data after the end of the tar stream
fails the extraction. (ADR 0050)

Archives are compressed with zstd at level 19 with long-distance
matching and a 128 MiB window, at the default job size of 64 MiB.
(ADR 0051) Live packages are compressed at level 3 in rsyncable mode,
with a job size of 512 KiB and no long-distance matching. The zstd
version is pinned, and each package records the zstd version and
parameters. (ADR 0018, ADR 0052) The `encoder` record holds `zstd` (the
version), `level`, `rsyncable`, `job_size`, `long_distance_matching` and
`checksum`. Every decoder antiq creates sets the maximum window to
128 MiB.

All compression in a run shares one pool of one worker per core. A
piece smaller than one job gets one worker, larger pieces take up to the
free workers, and several pieces are compressed in parallel. The number
of compressors in flight is also limited by memory against
`memory_percent` of RAM: an archive encoder counts 448 MiB plus 160 MiB
per worker, since zstd allocates a fixed round buffer for each encoder
with long-distance matching. A live encoder counts 4 MiB plus 2 MiB per
worker. When nothing else is compressing, one
worker is always granted, with a warning when the memory limit is below
the cost of an encoder with one worker. A compressor that finds no free
worker or not enough memory waits until another finishes, and waiting
compressors are served in the order they asked.

An optional exclude list, `collection.live_exclude`, empty by default,
applies to live packages only; archives hold the whole tree except junk
files. Each entry is a path relative to the collection root. The entry
and everything below it is left out of the live package and of its
manifest. A piece whose top directory is an entry, or lies below one, has
no live package. The packer reports the entries that left something out
of a package.

Symlinks are archived and restored as they are, whatever their target.
An archive with a member whose path leads outside the piece is refused,
and unearthing never writes through a symlink. (ADR 0027)

Before it writes anything, extraction reads the whole package and refuses
it for the first member that is absolute or has a `..` component; lies
outside the one top-level directory; is a kind other than file,
directory, symlink or hard link; appears twice; sits in a directory that
an earlier member did not create or in something that is not a
directory (so never below a symlink); lies more than 512 directory
levels below the top-level directory; is a symlink without a target or
with a NUL in it; is a hard link to anything but a regular file that an
earlier member created in the archive; or has a mode or modification
time that does not parse or is beyond 63 bits of seconds. Paths are
judged by the names the archive carries, including those of GNU long-name
and pax records, and by their exact bytes, so `a/x` is not placed in a
directory `A`. Long-name, long-link and pax records above 64 KiB, sparse
members, and a pax `size` that differs from the member's header size are
refused as well, and so is a package without members. The package is
decompressed three times: once for the
records, once for the members and once to extract. (ADR 0050)

### Verifying

A strict verify gates archiving and unearthing. It reads the stream to
its end and checks its integrity. A cut or corrupt stream, nonzero data
after the end of the archive, an empty archive, a first member that is
not a directory, a member outside the top-level directory, a member path
that is not a plain relative path, a path that occurs twice, a hard link
that does not name a file listed before it, a member that is neither
file, directory, symlink nor hard link and a modification time beyond 64
bits make the package damaged.

Then it compares every member with the tree on disk (type, size,
content hash, modification time in whole seconds with the same clamp,
mode, link target) and requires equal path sets under the same walk
rules. The walk is the one packing uses, so a tree with an unreadable
entry or a mount point is refused. A file with several names must be
stored as packing stores it: the first path in byte order is the file
and the others link to it. Junk files are left out on both sides.
Special files are not archived and are not compared. Symlink modes are
not compared, since Linux cannot set them.

On a user's folder, directory modification times are not compared, the
top-level directory's included, because creating a junk file such as
`.DS_Store` changes its directory's time without changing what is
archived. The top-level directory's mode is compared with the mode of
the first member. On antiq's staging, directory modification times are
compared, since the final pass of the extraction sets them from the
package, and the top-level directory's modification time is compared but
not its mode: the staging root keeps the mode its maker gave it until the
piece is moved into place. A staging directory whose mode keeps its
owner out gets owner access for the read, and its mode is put back
afterwards. A user's folder is never changed.

While it reads, the verify records each entry's lstat (kind, size,
modification time in nanoseconds, ctime, inode), special files and the
top-level directory included, and the members of the package in archive
order. A file that is replaced, resized or touched while it is read is
reported as changed while read. (ADR 0033)

Reclaiming compares the folder with the archive's listing: path, type,
size, modification time, mode, link target and content hash. Directory
modification times and junk files do not count, junk files are left out
of the listing side too, and the symlink modes are not compared (Linux
cannot set a symlink's mode). The top-level directory has no line in the
listing and is not compared. A file that is replaced, resized or touched
while it is read is reported as changed while read. A folder whose walk
finds a problem (an unreadable entry, a mount point) is refused. The
comparison applies no exclude entries, so the caller walks the folder
with the collection's rules first and refuses a piece that contains an
excluded path. The comparison also hands back the lstat record of the
folder that the deletion steps compare against. It requires a local
archive that passes the stream check or a remote copy whose check is
recorded; otherwise the piece is not reclaimed and is listed as pending
remote work. (ADR 0033, ADR 0035)

A tolerant verify is used for live packages. The walk's record of each
file is the state before its read. A file whose lstat (type, size,
modification time, ctime, inode) differs from that record after its read,
that read short, or that vanished after its read, discards the package
being written and restarts the piece with a new walk. So does a file that
was replaced between the walk and its open, by a symlink, a directory or
another file, or by a change above it. `live_retries` counts the
restarts, so a piece is tried `live_retries` + 1 times; then the piece is
skipped for this run and shown as "backup stale since <time>". A file that
is gone when the packer comes to open it is dropped from the package and
from the manifest. An entry that a restart's walk lost between listing
its directory and reading it counts as one more change; in the walk made
before the first attempt it makes the piece unreadable (see "Walks"). The
stream checksum is verified before upload: the finished package is read
back through the decoder, which checks the frame, and its SHA-256 must be
that of the bytes written.

### Change detection

Each live package has a local manifest, a JSON file with a `format` of
1 and one entry per file, directory and symlink the package holds, the
piece's top directory included: path (escaped form), type, size,
modification time and ctime in nanoseconds, and inode. The entries are
sorted by path bytes. Fifos, sockets and devices are not packed and are
not in the manifest. A live piece is packed again when its tree differs
from the manifest: a path was added or removed, or the type, size,
modification time, ctime or inode of a path changed. A manifest that
cannot be read counts as a tree that differs. When the
new package has the same SHA-256 as the current one, only the manifest
is replaced: no new version, no transfer, no history entry. The first
sync after unearthing packs once.

## Operations

Commands that change state take an exclusive lock on `.antiq/lock` and
write their pid, machine and start time into it. A command that finds
the lock held fails at once with exit 1 and names the pid and start
time, when the lock file has them: a run that has taken the lock but not
yet written them is named by neither. Read-only commands and dry runs
take no lock; `status`, `find` and dry runs print a note while a run
holds it, read from the pid in the lock file without trying the lock.
The note is absent in the moment between a run taking the lock and
writing the file. A collection on an SMB or NFS mount gets a warning.

Every state-changing run, right after taking the lock, in this order:

- finishes `removing` entries, for a folder still in place and for a
  tree in `.antiq/trash/<ulid>` alike, by comparing the tree with the
  listing of the archive payload the entry names (read from the current
  metadata) before deleting it, since the lstat record of the verify does
  not survive a crash. A tree in the trash decides over a folder at the
  path. A tree that holds nothing the archive lacks is deleted and the
  entry set to `gone`: only paths are missing from it and every remaining
  path equals the listing, which is what an interrupted delete leaves (a
  directory whose owner access was added to empty it counts as equal).
  A tree that differs in any other way, or that cannot be compared (no
  payload named, no metadata, no listing, a listing or a walk that cannot
  be read), is kept as unearthed at the committed version, which is not
  recorded when the payload cannot be found: a trashed one is renamed
  back to its path, and moved to `.antiq/recovered/<ulid>-<time>/` (the
  time as `YYYYMMDDTHHMMSSZ`) when it cannot be renamed back (the path is
  taken, another `unearthing`, `live` or `removing` entry holds it, or the
  rename is refused), and its entry becomes `gone`. A folder in place that
  another such entry holds is left to that entry, and the entry becomes
  `gone`. The move is flushed, with the directory it came from, before the
  entry is written. A `recovered/` tree of the entry that is newer than
  the entry means the move already happened: the entry becomes `gone` and
  a folder at the path is a new piece. A tree that cannot be moved or
  deleted stays in the trash, with a warning, and so does a tree whose
  presence in the trash cannot be established;
- empties `.antiq/trash/`, `.antiq/tmp/` and `.antiq/staging/`, except
  the trash trees of IDs whose tracking entry cannot be read and of the
  entries above that stayed; an entry that cannot be removed after one
  retry with owner write added stays, with a warning, and does not fail
  the run;
- removes the temp files an interrupted atomic write left in `.antiq/`
  and below it, outside `.antiq/recovered/`: regular files named
  `.<name>.<pid>.<n>.tmp`;
- turns an `unearthing` entry whose folder is in place into `live`, with
  the time the entry was written (recorded in the entry) as its
  unearthing time, when the object's metadata records that unearth: its
  history holds the `unearth` entry of this machine at that time. The
  folder may differ from the archive then, since a difference is an edit
  of the unearthed piece. Without that record the folder has to equal
  the listing of an archive payload of the version the entry records
  (any such payload the entry holds, or any of the version when it holds
  none), and the repair first records the metadata change: lifecycle
  `live`, owner this collection, the entry's path and name, history
  `unearth` at the entry's time. A change that cannot be written leaves
  the entry `unearthing` for the next run, with a warning. The version
  taken out reads `pending` once the change is recorded.
  Any other `unearthing` entry is dropped: it goes back to `gone` when it
  holds local payloads or committed versions, and is removed otherwise. A
  folder at its path that it does not recognise, or that another entry
  holds, becomes a new piece, reported once, unless the metadata of the
  entry's ID is live and owned by this collection: the folder then stays
  that piece's. An entry made while unearthing from a live package
  records no payload version and has no listing to match, so it is always
  dropped;
- treats a present folder of an ID this machine committed as archived as
  unearthed at that version, unearthed now: a `live` entry whose folder is
  in place and whose metadata is archived and owned by this collection,
  when this machine's newest archive payload that is not superseded is
  newer than the entry's unearthing, or the entry has none, and is not
  the version the entry records. The payload is added to the entry's
  local payloads when its file is present, and the version taken out
  follows the metadata (`pending` while a recorded change is not
  committed);
- writes the missing entry of an object that this collection owns and
  whose metadata says live: `live` when a folder is at the metadata's
  path and no other entry holds it, `gone` without local payloads when
  there is no folder;
- removes a live manifest whose ID has no tracking entry file, and keeps
  one whose entry file exists but cannot be read, which the run reports
  with a warning.

Read-only commands and `--dry-run` never touch these directories.
`.antiq/recovered/` is never emptied.

Before packing a piece, `archive` checks free space in `.antiq/`
against the piece's apparent size, and parallel pieces reserve their
sum. `unearth` checks for the archive's size, when it has to be fetched,
plus the listing's total size. A piece without enough space is skipped,
and `ENOSPC` during a step removes that step's temp output and skips the
piece; the run exits 3.

Moving a piece's top directory to another parent (into trash, into
place) adds owner write to it first and restores the recorded mode
right after.

### archive

For each idle, unpinned piece:

1. Pack into `.antiq/tmp/` and verify strictly against the tree.
2. Store the payload and listing and record the metadata change; payload,
   listing, metadata and tracking are durable before the next step.
3. Set the tracking entry to `removing`, naming the archive payload the
   folder is compared with, and rename the folder to
   `.antiq/trash/<ulid>`.
4. Walk the trashed tree and compare it with the lstat record from the
   verify. On any difference, rename it back and treat it as unearthed
   at the committed version; when its path is taken by then, move it to
   `.antiq/recovered/<ulid>-<time>/` and list it in `status`.
5. Delete it and set the entry to `gone`, keeping its local payloads.

When a rename fails, the folder is walked and compared in place and then
deleted there, with the entry `removing`. A process that still holds a
file open can write after the walk; that write is lost. Uploading is
left to the next `sync`. (ADR 0032)

An unearthed piece that has not changed against its listing is
reclaimed: the folder is removed the same way and the archive stays,
with no new payload and no upload. (ADR 0035) The listing compared is
the one of the payload version the piece was unearthed from, recorded
in its tracking entry. A changed one replaces its
archive with a new payload. A replacement that loses nothing happens
without asking. Content is lost when paths were removed or a file's
content, mode, link target or modification time changed. Then antiq
shows what would be lost and asks: replace, fork or skip; without a
terminal it skips and lists the piece as needing a decision. A skip
answered on a terminal leaves nothing pending, and the run exits 3. A
collection can allow replacing without asking. (ADR 0036) A diverged
piece is refused. Archiving warns about names that would not survive on
macOS or Linux and archives the piece anyway. (ADR 0054)

### sync

`sync` pulls metadata, refreshes the live packages of changed pieces,
uploads pending archives, commits recorded metadata changes, deletes a
live package only after its archive's metadata is committed on the
remote, and applies the tiers. (ADR 0018)

`sync` gives new pieces their ID. With `backup_live = false` it still
does, and commits their metadata without a live payload. After a fork,
the old ID's live package is deleted once the fork's change is
committed. A dry run reads the remote and writes nothing, locally or on
the remote.

The archive tier starts from a piece's activity; the remote-only tier
is measured from the archive's commit. (ADR 0042) A local archive moves
to the remote-only tier only after the remote's identity, the payload
and the remote copy's checksum are confirmed (computed on the NAS over
SSH, read back past the client's file cache on a path remote), and,
over SSH, after a NAS `sync` that followed its upload. It is then
deleted through the trash and the time of the check is recorded.
(ADR 0007, ADR 0043)

### unearth

`unearth <piece> [--to <path>] [--version <n>]` takes the current
archive of an archived ID, or the archive with payload version `n`. When
the remote is reachable it first pulls the ID's metadata and says when
the remote has a newer version; with the remote away it proceeds from
local data and warns. An archive that is not on this machine is fetched
into `.antiq/tmp/` first. `unearth` checks the archive's size and
checksum, extracts it into the staging directory, verifies the staged
tree strictly, sets the staged top directory's mode, writes a tracking
entry `unearthing`, checks the target again (missing parents are created;
a file, symlink or piece in the path refuses), renames the tree into
place, records the metadata change, and sets the entry to `live` with the
time of unearthing. Unearthing an ID that is already unearthed does
nothing and says where it is. (ADR 0034)

A volume without room for the listing's total size skips the piece, and
so does a volume that fills up during the extraction; the staged tree is
removed and the run exits 3. A step that fails before the rename leaves
the collection as it was, the entry as it was, and removes the folders it
made above the target.

The target is the stored relative path, or `--to`, a path inside the
collection that the patterns select. `unearth` refuses the collection
root, `.antiq/` and everything below it, and a path at or below an
excluded folder. It refuses a path that has a piece above it, meaning a
folder that the patterns select or that a tracked piece holds, and a path
that has a file or a symlink above it; nothing is followed through a
symlink. A folder above the path that the volume finds under another
spelling than the path's (another case, or another Unicode normalisation)
refuses too. A taken target refuses: anything at the path, a dangling
symlink included, or a tracked piece that holds it. Folders above the
target that do not exist are created.

A piece that is gone but whose metadata holds an archive is unearthed from
that archive, as an archived one is. `--version` names an archive payload
version, a superseded one included. Of two payloads that share a version,
the one this machine holds comes first.

Before extracting, `unearth` checks every member name against the
target: names it cannot hold, name lengths, and names that collide
under its case and Unicode rules, which come from a probe of the
staging volume. It refuses the piece with a list of the offending
names and the reason. A name is too long when it exceeds 255 bytes on
Linux or 255 UTF-16 code units, counted on the name as stored, on
macOS; a volume that stores names decomposed (HFS+) counts them in that
form. macOS also refuses a name that is not valid UTF-8. A symlink whose target is longer than 1023 bytes on macOS or
4095 bytes on Linux is refused for the same reason, and `archive` warns
about it for macOS. The probe also
tries to create a hard link and a symlink on the staging volume; a hard
link or symlink member of the package is refused, with the others, when
the volume has none (exFAT has no hard links). Two members
collide when their paths are equal after Unicode normalisation (where
the volume ignores it) and full case folding (where the volume ignores
case); the refusal lists both names. A member more than 512 directory
levels below the top directory is refused, since the walk, the strict
verify and the removal of staging go no deeper. The names of the target
path are held to the same rules as directories, and on a volume that
stores names decomposed a folder made above the target, or the target,
is refused when its name would be stored in another form than it is
written. Paths are not checked for length:
a path longer than the platform's path limit is extracted directory by
directory. (ADR 0054)

`archive` runs the same check with the macOS rules and with the Linux
rules and warns about each name that one of them would refuse. (ADR 0054)

The remote copy is kept while the piece is live. The local archive is
dropped only under the rule in "Several machines" and is always kept
without a remote. (ADR 0043) An archive fetched from the remote, with its
listing, stays as the local archive while the piece is within the local
tier, and the recorded check of the remote copy stays with it.

An ID without an archive (a gone piece, or another collection's live
piece unearthed as a fork) is unearthed from its current live package:
its stream checksum is checked and the staged tree is verified against
the package's members. A fork of another collection's piece creates a
new ID owned here. `unearth` warns when the package was marked stale or
made with the live exclude list. The result has no archive payload until
it is archived. A live piece without a live package, as with
`backup_live = false`, cannot be unearthed this way, and `unearth` says
so. A fork of another collection's piece writes nothing to that
collection's ID.

### fork

`fork <piece> [--name <name>]` works only on unearthed pieces. It gives
the piece a new ID with the time of the fork and keeps the tracking
entry's unearthed time; a fork made while unearthing another
collection's piece uses that unearthing time. `--name` renames the folder
within its parent; a taken name refuses. The old ID's live package is
deleted. Unearthing the old ID afterwards needs `--to`.

### import

`import [--path <dir>] <file>...` registers compressed tar archives as
archived pieces. For each file it:

- refuses anything but tar compressed with zstd (by magic bytes), naming
  the codec;
- refuses a frame whose window exceeds 128 MiB, naming
  `zstd -19 --long=27` to recompress it, and any frame without the
  content checksum;
- reads the whole stream, computes size and SHA-256 and builds the
  listing as `archive` does;
- refuses an archive without exactly one top-level folder, and members
  whose path leads outside the piece; that folder is the archive's
  top-level directory of "Packaging", and the listing holds the paths
  below it;
- warns about names as `archive` does (ADR 0054);
- skips and reports a file whose SHA-256 is already a payload of the
  collection;
- copies the file (a clone where the filesystem supports one, never a
  hard link) into `.antiq/tmp/`, flushes it, renames it to its payload
  name and records a new ID: the oldest member modification time as its
  time (source `import`), the top-level folder's name, and the archive as
  payload version 1.

The relative path is `--path`, by default the top-level folder's name at
the collection root; a path the patterns do not select refuses. A path
that a folder or another object already has gives a warning, and both
objects are listed by `find`. The input file is never moved or deleted.
The archive goes up at the next `sync`. (ADR 0055)

### reconcile

`reconcile` rebuilds the index from local and remote metadata, does the
start-of-run repairs above, removes leftovers as the commit protocol
describes, and reports IDs whose committed version another machine
wrote. An entry without a local archive whose archive, verified or
pulled from the remote before, is gone from the remote is marked missing
and shown in `status`; `reconcile --prune` drops such entries and gone
pieces. An entry with a local archive not yet uploaded is pending upload
and is never pruned. (ADR 0044) Without a remote, `reconcile` does its
local part only.

The local part empties the index and builds it again from the listings
stored on this machine. An object whose listing cannot be read stays out
of the index, is named in the report and ends the run with exit code 3. A
dry run lists the rebuild of the index and neither runs nor lists the
repairs.

### status

`status` is read-only: it takes no lock and writes nothing below
`.antiq/`. On the console it lists only the pieces that need the user or
change at the next run, one table row each with the state, the piece's
path, the days since its last activity and what is pending: an error of the
piece, needing a decision, diverged, a superseded archive, a stale backup,
missing on the remote, a recovered tree, unreadable, containing a mount
point, a collection of its own or an excluded path, future modification
times, gone, archiving at the next run, waiting for the upload, or new. A piece with several of
these has one row, in the state that comes first in that order, and its
text names them all. A damaged tracking file or object has a row of its
own. Rows of one state are listed by path, those of archiving at the next
run by age. A line above the table gives the remote's state, the time of
the last sync (or that there was none) and the counts of live and archived
pieces. Without a remote the line says so and no row waits for the upload.
A line below the table counts the pinned and empty pieces and the skipped
symlinks and dot entries, which are counted, not listed; a hint line below
that names `antiq resolve` when a decision is pending and `antiq find` for
every piece. Without a row `status` says that nothing is to do and names
the next change, a piece that archives or becomes remote-only, with the
days until then. Notes, such as a run holding the lock, go to stderr. The
JSON output carries every piece and the notes. `status` exits 0 whatever
it shows, unless the command itself fails.

On a terminal the table adapts to the width, see "Command line". Its wide
layout adds the command that deals with a row where there is room.

### Safety

Bulk deletions without a per-item check go through one brake: dropping
entries (`reconcile --prune`), `remote remove` while pieces exist only
on the remote, and `resolve --drop-superseded`. When planned removals
exceed `brake_percent` of the affected set, the step stops and lists up
to ten. On a terminal it asks; without one it refuses unless `--force`
is given. `--dry-run` reports where the brake would hold. Removing live
packages and moving archives to the remote-only tier do not go through
the brake. `archive` does not either: each folder is deleted only after
its own strict verify and commit. No archive on the remote is deleted
except by a newer, verified and committed version of the same ID or an
explicit drop of a superseded one. (ADR 0045)

The planned removals and the affected set of each step:

| Step | Planned removals | Affected set |
|------|------------------|--------------|
| `reconcile --prune` | entries to drop | all entries this machine tracks or knows from metadata |
| `remote remove` | remote-only pieces that become unreachable | all objects of the collection |
| `resolve --drop-superseded` | superseded payloads to drop | the ID's archive payloads |

With one current payload in the set, a drop of superseded payloads is at
least half of it, so every `resolve --drop-superseded` holds the brake
at the default: it asks on a terminal and needs `--force` without one.

## Command line

| Command | Does |
|---------|------|
| `init [--pattern <glob>]... [--yes]` | makes a directory a collection |
| `status` | shows the pieces by state |
| `archive` | archives idle pieces and removes their folders |
| `unearth <piece> [--to <path>] [--version <n>]` | brings an archived piece back |
| `find [<query>] [--print]` | lists and searches all pieces; opens the picker |
| `fork <piece> [--name <name>]` | gives an unearthed piece a new ID |
| `pin <piece>`, `unpin <piece>` | create or remove a piece's `.antiq-pin` |
| `sync` | exchanges backups and archives with the remote |
| `resolve [<piece> <answer>]` | answers pending decisions |
| `remote add [--init] [--host-key-alias <alias>] <url>`, `remote show`, `remote remove` | attach, show and detach the remote |
| `reconcile [--prune]` | brings the index in line with the stored metadata |
| `import [--path <dir>] <file>...` | registers existing compressed tar archives as archived pieces |

(ADR 0023, ADR 0055)

Every command takes `-C <path>` and `--json`. Every state-changing
command takes `--dry-run`. `--force` applies to the commands with a
brake step: `reconcile`, `remote remove`, `resolve`. `--if-reachable`
applies to the commands that contact the remote: `sync`, `unearth`,
`reconcile`. A flag a command does not take is a usage error.

A piece reference is a path or a ULID. A path is resolved against the
current directory, normalised, and taken relative to the collection
root; a path outside the collection is refused; archived pieces resolve
the same way although their folder is gone. `.` and `..` in a path are
resolved from its text, and a `..` that would go above the root of the
filesystem is refused; `--to` and `--path` are normalised the same way,
also relative to the current directory. Only the collection root itself
is recognised by device and inode, so another spelling of it, such as
`/var` and `/private/var` on macOS, still counts as the root; symlinks
below the root are never followed. A path outside the collection is
refused with the error kind `outside_collection`, exit 1. A ULID may be
shortened to a prefix of at least 6 characters that is unique among the objects this
machine knows at that moment. A reference that matches several objects
refuses and lists them with their IDs; one that matches none is an
error. Both exit 1. JSON output always carries full ULIDs; scripts use
them.

A reference is read as a path first: a path at which an object is
recorded, by its metadata or by its tracking entry, names that object,
also when the text could be a ULID prefix. At a path, the object whose
tracking entry says its folder is there, arriving or leaving names the
piece; the objects archived there as well, such as the old ID after a
`fork`, count only when no object holds the folder, and several of them
make the reference ambiguous. Only a text of 6 to 26 characters from the
ULID alphabet, in either case, that no object is recorded at as a path,
is a ULID prefix; a shorter text is an unknown reference. A folder that
no object is recorded at is an unknown reference. The objects this
machine knows for the prefix are those with metadata, those with an
object directory whose metadata cannot be read, and those with a
tracking entry or a damaged tracking file. A candidate's `path` is the
one in its metadata, else the one in its tracking entry, else empty.

antiq writes console output; the picker of `find` is its only
full-screen part. (ADR 0056) The output adapts to the terminal's width:
on a wide terminal tables and progress rows use their wide layout, with
full paths and extra columns such as sizes and transfer rates, since it
reads more easily; on a terminal of 80 columns they use their narrow
layout, in which a path that does not fit is shortened in the middle,
keeping its start and its end. A terminal of 100 columns or more is wide.
Without a terminal the width is 80. Colour is used only on a terminal, and
not when the environment variable `NO_COLOR` is set to a non-empty value.

Long runs show one row per piece: its path, its size, a progress bar,
the operation running on it (packing, verifying, storing, removing,
uploading, fetching) and one detail, such as the files checked or the
bytes moved. A row takes one line when the path fits and two lines
otherwise, the full path above the bar. `sync` shows its phases (pull,
live backups, upload, commit, tiers) as a checklist: a finished phase
is one summary line, the running phase lists its pieces as rows, later
phases are dimmed. Without a terminal only the finished lines are
printed.

`--json` also makes a command non-interactive. Otherwise a command is
interactive when a controlling terminal is available, also when its
output is redirected. (ADR 0020, ADR 0023)

`find` lists and searches all objects; interactively it opens a picker
from which an object can be unearthed. (ADR 0021) The query matches the
pieces' names and paths and the member paths in their listings, with
the picker's fuzzy matching, in and outside the picker. `find` lists
objects only; a new piece without an ID shows in `status`. The picker
starts as a copy of ntropy's picker. It offers unearthing (Enter), a
view of a piece's contents, printing the path (`--print`), forking, and
pinning or unpinning. A key expands the selected piece in place into a
tree of its contents, read from its listing; when the query matches
member paths,
the matching members show as the expanded piece's children. After
pinning, unpinning or printing the picker stays open; unearthing and
forking close it. (ADR 0057)
`--print` prints the absolute path of the picked piece's folder, for an
archived piece the path it would be unearthed to. Without a terminal it
prints the path of every match, one per line. The path is written as the
filesystem holds it, not in the escaped form of "Metadata", so that a
shell can use it; the table and the JSON use the escaped form. With
`--json`, `find` prints the matches as JSON and `--print` adds nothing.

Without `--print` and outside the picker, `find` lists the matches as a
table of the state, the path, the size of the current payload and the
shortest prefix of the ID, at least 6 characters, that no other known
object shares; on a wide terminal it adds the creation date and writes the
whole ID. Matches through the path come first, then matches through the
name alone, then matches through a member path alone. Without a query,
the newest object comes first. When nothing matches, stdout stays empty,
stderr says so and the exit code is 0. `find` takes no lock and writes
nothing.

### Decisions

Without a terminal, every decision point takes the non-destructive
default: no action on that object, which is listed as needing a
decision with its options; the command continues and exits 3. `archive`
and `sync` ask in place when a terminal is available. (ADR 0025)

`resolve` goes through the pending decisions interactively. `resolve
<piece> <answer>` executes one decision at once, under the lock, with
the loss computed at that moment; no answer is stored for a later run.
Without a piece and without a terminal, or with `--json`, it lists the
pending decisions and exits 3 when there are any. `resolve <piece>`
without an answer goes through that piece's pending decisions
interactively; without a terminal it lists them and exits 3.

| Decision | Answers |
|----------|---------|
| version conflict | `--mine`, `--theirs`, `--fork` (see "Several machines") |
| replacing with loss | `--replace`, `--fork`, `--skip` |
| diverged folder | `--theirs`: shows the folder's differences to the listing of the version taken out and asks (without a terminal only with `--force`), then the folder goes through trash and the current version is unearthed in its place; `--mine`: archives the folder as the next version after the loss check against the remote's current listing, the other becomes superseded; `--fork`: new ID, divergence cleared |
| superseded archive | `--drop-superseded` |

An answer that does not fit the pending decision is a usage error.

### Dry runs, scheduling, exit codes

Every state-changing command takes `--dry-run`, which prints the plan
and changes nothing; under `--json` the plan is JSON. (ADR 0046) A dry
run takes no lock and may read the remote; it writes nothing, locally or
on the remote. A dry run neither runs nor lists the start-of-run repairs.
On the console a plan is one `Would ...` line per action,
or `Nothing to do.` when there is none, followed by the place where the
brake would hold, if it would: `The brake would hold at <planned> of
<set size>:` and one line per item, up to ten, with path and ID.

`--if-reachable` makes a run exit 0 when the remote is away, for
scheduled runs, except as `away_warn_days` says. Without a remote it
changes nothing.

| Exit | Meaning |
|------|---------|
| 0 | success |
| 1 | error, including remote broken, lock held, a held brake without a terminal, an unknown or ambiguous piece |
| 2 | usage error |
| 3 | partial: some objects skipped (needing a decision, stale backup, no space, a failed transfer) |
| 4 | remote away |

Colour is used only on a terminal. Data goes to stdout, messages to
stderr. Under `--json` an error is data: its JSON goes to stdout, like
any other JSON output, and a usage error, too, prints the JSON envelope
(kind `usage`) and exits 2. When the reader of stdout or stderr is gone,
antiq stops writing to that stream without a panic, and the run ends
with the exit code it would have had.

### JSON output

Every JSON output is one object `{"format": 1, "command": "<name>",
...}` on one line of stdout. `<name>` is the words of the command, joined
by one space: `status`, `remote add`. A command line that fails to parse
before naming a command has `"command": null`. An error is `{"format": 1,
"command": ..., "error": {"kind": ..., "message": ...}}`, where `message`
is the text of the error without the context lines a console shows. Some
kinds add keys to the error object. A `--dry-run` plan carries
`"dry_run": true` and an `actions` list, in that order, and a `brake`
object (`planned`, `set_size`, `items`, as in the `brake` error) where the
brake would hold.

| Kind | Exit | Raised when | Added keys |
|------|------|-------------|------------|
| `usage` | 2 | the command line does not parse, an answer does not fit the pending decision, or a `--pattern` of `init` is invalid | |
| `not_a_collection` | 1 | a command runs outside a collection | |
| `ambiguous_reference` | 1 | a piece reference matches several objects | `candidates`: list of `{id, path}` |
| `unknown_reference` | 1 | a piece reference matches no object | |
| `outside_collection` | 1 | a path given as a piece, `--to` or `--path` lies outside the collection | |
| `brake` | 1 | the brake holds and there is no terminal to ask | `planned`, `set_size`, `items`: up to ten `{id, path}` |
| `lock_held` | 1 | another run holds the lock | `pid`, `started` (RFC 3339, whole seconds); neither while the holder has not written them yet |
| `no_remote` | 1 | a command needs a remote and none is attached | |
| `remote_away` | 4 | the remote cannot be reached | |
| `remote_broken` | 1 | the remote is broken, as "Remotes" defines | |
| `format_too_new` | 1 | a write meets metadata of a higher `format` | |
| `other` | 1 | any other failure | |

`--help` and `--version` print text and exit 0, also under `--json`.

`status` adds `collection` (`id`, `path`), `remote` (`state`: `none`,
`reachable`, `away`, `broken`; `url`, when a remote is attached) and
`pieces`, a list of objects with `id` (null for new pieces), `path`,
`name`, `state` and `flags`, a list of the conditions `status` shows, such
as `pinned`, `stale_backup`, `diverged`, `needs_decision`, `superseded`,
`pending_upload`, `missing_on_remote`, `unreadable`, `mount_point`,
`future_mtime`, and `remote_only_in_days` where it applies: for an archived
piece whose archive is on the remote, the days until the archive becomes
remote-only, at least 0, counted from the commit of the current archive
(the earliest history entry of an `archive`, `import`, `resolve` or `fork`
action from the creation of the archive on); it is left out while the
archive still has to go up or no remote is attached. `archives_in_days` is
the whole days until an active piece that is not diverged is idle, at least 1. `state` is one
of the states under "States" (`new`, `live`, `unearthed`, `gone`,
`archived`, `remote-only`, `live-on-another-machine`) or `error`, the
state of a piece whose entry, metadata or object cannot be used; the
piece then carries the flag `error`. A piece's `detail` object holds what
a state or flag needs to say and leaves out the keys that do not apply:
`error` (the message), `removal_unfinished` (a `removing` entry whose
folder is still in place), `below_unreadable` (the directory above a
piece that selection could not read, which is neither walked nor judged),
`machine` (the machine that backs up a live piece of another
collection), `stale_since`, `decisions` (the pending decisions as `{kind,
options}`), `unreadable_first`, `mount_points`, `nested_collections`,
`excluded_paths` and `future_files` (paths relative to the collection root, the top directory of
a piece being the piece's own path) and `idle_clock` (the time the idle
clock counts from). `age_days` is the whole days from the idle clock to
now; it is left out for a piece without a verdict. The flags `empty`,
`excluded`, `nested_collection` (a directory in the piece holds a `.antiq`
of its own) and `idle` also exist: `idle` marks a new, live or unearthed
piece that is not diverged and that the next `archive` archives. A piece
the walk could not read completely has no verdict. A `junk` entry that is
no file name and a `collection.ignore` line that is no valid rule each give
a warning; with such a line no piece has a verdict. A piece is `pending_upload` when a recorded change is not committed, or
when it holds its current archive locally and the metadata records no check
of the remote copy. `status` adds `last_sync` (from `.antiq/remote-state.json`,
left out when none is recorded) and also `recovered`, the names below
`.antiq/recovered/`; `damaged`, a list of `{id, path, error}` for tracking
files and objects that cannot be used, with the path relative to the
collection root; `unreadable_directories`, the directories selection could
not read; `skipped` (`symlinks`, `dot_entries`); `counts` (`live`,
`archived`, `remote_only`, `pinned`, `empty`) and `warnings`. `live` counts
the pieces in state `live`, `unearthed` or `new`, `archived` those in state
`archived` or `remote-only`; pieces in state `gone`,
`live-on-another-machine` or `error` are in no count. Pieces are sorted by
path, a new piece before an object at the same path.

`find` adds `query` (null without one), `matches`, a list of objects with
`id`, `path`, `abs_path`, `name`, `state`, `created` and `size`, and
`warnings`. `size` is the size in bytes of the object's current payload:
its newest archive that is not superseded, else its newest live package;
null without a payload. `state` is one of the states under "States" or
`error`, the state of an object whose entry, metadata or location cannot
be used. An object whose metadata cannot be read is not listed and a
warning names it; an object whose listing cannot be read is left out of
the matches through members and a warning names it.

`init` adds `created`, `id` (only when created), `path` (the
collection's root), `machine`, `patterns`, `exclude` (what `--yes` writes:
the proposed exclude or nothing), `pieces` (what the patterns select, with
the exclude applied), `actions` (one `create_collection` action with the
path) and `warnings` (one `message` per note). `dry_run` is present, and
true, under `--dry-run`. Without a terminal, or with `--json`, it creates
nothing and prints the plan, the same keys without `id`, unless `--yes` is
given, and exits 3. Under `--json` nothing goes to stderr.

`archive` has no payload defined yet.

`unearth` has no payload defined yet.

`fork` has no payload defined yet.

`pin` and `unpin` add `id` (null for a new piece), `path`, `pinned` (whether
the piece is pinned afterwards), `changed` (whether the command changed the
pin file) and `warnings`.

`sync` has no payload defined yet.

`resolve` has no payload defined yet. Without a piece and without a
terminal, or with `--json`, it lists the pending decisions and exits 3
when there are any.

`remote add`, `remote show` and `remote remove` have no payload defined
yet.

`reconcile` adds `index` (`written` and `removed`, the numbers of files
written to and removed from the index, and `skipped`, a list of `{id,
reason}` for the objects whose listing could not be read) and `warnings`.
Its dry-run action is `rebuild_index` with `member_lists`, the number of
listings stored on this machine.

`import` has no payload defined yet.
