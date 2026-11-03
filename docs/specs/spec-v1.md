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
- tar archives compressed with xz
- an index of each archive's contents
- searching the archives and bringing one back through a picker
- rebuilding missing index entries
- backing up to a NAS

On top of that, version 1 moves archives through tiers by age, backs live
folders up as packages, and lets several machines share one remote.
Whether it includes the remote-only tier (ADR 0007) is open.

Not in version 1:

- syncing one live folder between machines (ADR 0008, ADR 0017)
- reading the archives and listings that `archive.sh` produced
  (ADR 0009)
- changing a collection's selection strategy after `init` (ADR 0012)
- more than one remote per collection (ADR 0015)
- backing up content outside every piece (ADR 0041)
- a setting that keeps a folder until its archive is on the remote
  (ADR 0032)

## Collections and pieces

A managed folder is called a collection, and each unit in it a piece.
(ADR 0022)

A collection is marked by `.antiq/` at its top. Commands find it by
searching upwards from the current directory, or from the path given
with `-C`, and fail outside a collection. `.antiq/` holds the
collection's config, metadata, index and local archives. (ADR 0011)

`init` lists what the patterns select and asks before creating the
collection. It detects the layout `archive.sh` leaves (`archive/` with
`*.tar.xz` and `index/*.lst`) and proposes excluding it. It notes when a
pattern contains `**`.

### Selection

Pieces are the directories antiq treats as one thing each. A list of
glob patterns selects them, default `*`. When matches nest, the
shallowest match is the piece. (ADR 0012) An entry whose name starts
with a dot is a piece only when a pattern names it with a literal
leading dot in that path component. (ADR 0028)

Never pieces: `.antiq/`, symlinks, and paths on the collection's exclude
list. Selection uses lstat and never descends into a symlink. Skipped
symlinks and dot entries are counted in `status`.

A folder at a path that no tracking entry claims is a new piece with a
new ID, also when an archived piece has the same name; `status` shows
that case.

### Idle

A piece is idle when the newest modification time of any regular file
in it is older than the configured number of days. Directory times do
not count. The walk uses lstat and does not follow links. (ADR 0013)

Ignore rules in gitignore syntax, from the config and from
`.antiqignore` files at any level, exclude paths from this check only.
Changes to `.antiqignore` and `.antiq-pin` files do not count as
activity. (ADR 0013, ADR 0029)

A fixed junk list, by default `.DS_Store`, is left out of the idle
check, packages, listings and verification. It is separate from the
ignore rules.

For an unearthed piece, the idle clock starts at the later of its newest
file modification time and the time it was unearthed. (ADR 0037)

A piece without regular files is never idle and never archived; it is
listed as empty. (ADR 0038)

### Pins

A piece with a `.antiq-pin` file at its top is pinned: it is never
archived, still backed up, and shown as pinned. `pin` and `unpin`
create and remove the file. The pin is checked before the idle and
empty tests. (ADR 0024)

## Objects

Every piece is an object with a ULID. The ID survives archiving,
unearthing and archiving again; the newest archive replaces the
previous one. Forking an unearthed piece gives it a new ID. (ADR 0014)

The ID time is the earliest of the directory's birth time and the birth
and modification times of every entry inside it, ignoring times before
1990. Its source (`birth`, `oldest-mtime`, `import`) is stored with it.
The ID is minted once and never recomputed. (ADR 0039) Only `archive`
and `sync` assign IDs; `status` shows a piece without one as new.

A live folder's package and its later archive are the same object.
(ADR 0018)

### Metadata

Metadata per ID, shared through the remote: format version, ID, owning
collection, relative path, name, creation time and its source,
lifecycle (`live` or `archived`), a list of payloads (kind `archive` or
`live`, package format, size, checksum with its algorithm, version,
writing machine, creation time), an append-only history of actions with
time and machine, and the time the remote copy was last verified.

Metadata files are JSON with `format` as the first field, a fixed key
order, RFC 3339 UTC times and sizes in bytes. Unknown fields are kept on
rewrite. A file with a higher `format` is read-only and any write to it
fails with a clear error. (ADR 0053)

### Local state

Each machine keeps tracking entries per folder: path, ID, the version
taken out per payload kind, the time it was unearthed, a state
(`unearthing`, `live`, `removing`), which payloads are present locally,
and the versions this machine committed. "Remote-only" is derived.

### States

| State | Folder | Local payloads | Remote payloads |
|-------|--------|----------------|-----------------|
| new | present, no ID | none | none |
| live | present | none | latest live package, if backing up is on |
| unearthed | present | archive, unless its remote copy is confirmed | archive, and a live package if backing up is on |
| archived | gone | archive | archive once synced |
| remote-only | gone | metadata only | archive |
| live on another machine | elsewhere | metadata only | that machine's live package |

A machine applies selection, idle, tiers, the brake and reclaiming only
to objects of its own collection. A folder copied together with its
`.antiq/` to another machine is not detected as a copy.

Another machine's live pieces are shown as live on that machine. They
can be unearthed only as a fork; `archive` and `sync` never touch them.
(ADR 0040)

## Storage

Local storage in `.antiq/` is fixed: config, objects, tracking, `tmp/`,
a staging directory, `trash/` and `lock`. Remotes sit behind one trait,
and every remote supports every operation. (ADR 0015)

One layout module serves local storage and every remote:

- `objects/<ulid>/meta.<N>`, where the highest committed N is current;
- payloads `objects/<ulid>/<version>-<sha256 prefix>-<name>.<ext>`,
  never overwritten;
- the listing in its own compressed file next to the metadata;
- a manifest per package for change detection.

The index is derived from metadata and listings, kept as plain files,
rebuildable, and behind a module interface. Listings are fetched
incrementally by metadata version.

The config is TOML. The policy (patterns, strategy, ignore rules,
thresholds, exclude lists) belongs to the collection. The machine and
remote section (host, user, path, network check, rsync path, machine
name) is local only, parsed strictly and never uploaded. The machine
name is the macOS ComputerName, or the hostname elsewhere, stored at
`init`.

### Committing a version

Payloads are uploaded under a temporary name that carries the machine
and the start time, then renamed to their final name. A writer claims
version N+1 by creating `meta.<N+1>.claim` exclusively, writes the
metadata under a temporary name and renames it to `meta.<N+1>`. A
version is committed once `meta.<N+1>` exists; readers read only
`meta.<N>` files. An existing claim is a conflict. A claim without
metadata older than the grace period is abandoned and skipped.
Abandoned claims, unreferenced payloads and old metadata files are
removed after the grace period, or earlier for this machine's own
leftovers when its lock shows no active run. `reconcile` reports any ID
whose committed version was written by another machine than the local
record says. (ADR 0030)

## Remotes

The remote holds the full metadata set, one metadata file per ID, next to
the archives. Attaching a remote copies its metadata; archives are
fetched on demand. (ADR 0016)

A remote root holds a marker file with the remote's ULID and the layout
version, and the config stores that ULID. Every remote operation checks
the marker first and refuses when it is missing or does not match.
`remote add --init` creates the marker; `remote add` attaches only to a
root that has one. A path remote whose root is not a mount point gets a
warning. (ADR 0026, ADR 0031)

Remote operations include reachability with three results (reachable,
away, broken), bulk metadata pull and push, payload put, get and delete,
and exclusive creation.

The SSH remote is built first. The path remote stays in version 1 and
copies plainly: a temporary name in the target directory, written,
flushed and renamed. Live packages on a path remote are uploaded in full
on every change. (ADR 0047)

### SSH remote

antiq runs the system's `ssh` with one master connection per run, a
short finite `ControlPersist` and a short `ControlPath`. Commands run as
channels of the master; rsync joins it through `-e`. Every call passes
`BatchMode=yes`, `ConnectTimeout`, `ServerAliveInterval`,
`ServerAliveCountMax`, and `HostKeyAlias` when configured. antiq quotes
remote arguments itself. A sync without changes uses one connection.
Exit status 255 and ssh's error text decide between away and broken.
Exclusive creation runs as a shell command with `set -C`, never through
rsync. (ADR 0048)

Live packages are transferred with rsync 3.1 or later; openrsync is
refused, and the rsync binary is configurable. The local rsync version
is probed every run, the NAS's rsync and hash tool at `remote add`.
rsync runs with `LC_ALL=C`, `--stats`, `--no-human-readable`,
`--no-whole-file` and `--timeout`, never with `--inplace` or
`--append`; exit code 24 is tolerated. The new package is seeded on the
NAS as a hard link to the current one and transferred onto it; no
transfer of a live package deletes files. (ADR 0049)

### Several machines

Several machines may share one remote. Each ID's metadata has a
version; putting an ID back with a version that is no longer current is
a conflict, resolved by keeping mine (written as the next version beside
theirs), keeping theirs, or forking. The other machine's version is
removed only on an explicit choice. (ADR 0017, ADR 0030)

`sync` pulls metadata first. When the remote has a newer version of a
local archive, the older local archive is dropped once the remote copy
is confirmed, or fetched anew. A folder unearthed from an older version
is marked diverged, and `archive` and `sync` refuse it until it is
resolved.

## Packaging

A Packager trait packs, verifies, lists and unpacks. Package formats
have ids such as `tar+xz/1`. (ADR 0019)

Tar is written in-process with hand-built GNU headers: mode masked to
permission bits, uid and gid 0, empty owner names, the real modification
time, no extended attributes, entries sorted by path bytes, hard links
stored as links, special files skipped and reported. Each file's
SHA-256 is computed in the same read and written to the listing.
(ADR 0050)

Archives are tar compressed with xz at preset 9 with a SHA-256 check.
The thread count is the largest that fits a memory limit, 25 % of RAM
by default, by liblzma's estimate; decoding uses the same limit.
(ADR 0051)

Live packages are tar compressed with zstd at level 3 in rsyncable mode,
with a job size of 512 KiB, at least one worker and no long-distance
matching. The zstd version is pinned, and each package records the zstd
version and parameters. (ADR 0018, ADR 0052) An optional exclude list,
empty by default, applies to live packages only; archives always hold
the whole tree.

Symlinks are archived and restored as they are, whatever their target.
An archive with a member whose path leads outside the piece is refused,
and unearthing never writes through a symlink. (ADR 0027)

### Verifying

A strict verify gates every deletion and every move into place:
archiving, reclaiming and unearthing. It checks the stream's integrity,
then compares every member with the tree on disk (type, size, content
hash, modification time, mode, link target) and requires equal path
sets under the same walk rules. Junk files are left out. (ADR 0033)

A tolerant verify is used for live packages. Each file is checked
before and after it is read and re-read a limited number of times when
it changed; if it keeps changing, the piece is skipped for this run and
shown as "backup stale since <time>". Files that vanished are dropped.
The stream checksum is verified before upload.

### Change detection

Each package has a local manifest of path, type, size, mtime in
nanoseconds, inode, ctime and directories. A live piece is packed again
when its tree differs from the manifest. The first sync after unearthing
packs once.

## Operations

Commands that change state take an exclusive lock on `.antiq/lock` and
write their pid into it; read-only commands take none. A collection on
an SMB or NFS mount gets a warning. Every run first empties
`.antiq/trash/`.

### archive

For each idle, unpinned piece: pack into `.antiq/tmp/`, verify strictly
against the tree, store the version-named payload and flush it, commit
the metadata, then rename the folder into `.antiq/trash/` and delete
it. When the rename fails across devices, the tracking entry is set to
`removing` and the folder is deleted in place. Uploading is left to the
next `sync`. (ADR 0032)

An unearthed piece that has not changed against its stored manifest
(path, type, size, modification time, mode, link target, content hash)
is reclaimed: the folder is removed and the archive stays, with the
same version and no upload. (ADR 0035) A changed one replaces its
archive as the next version. A replacement that loses nothing happens
without asking. When files were removed or their content or metadata
changed, antiq shows what would be lost and asks: replace, fork or
skip; without a terminal it skips and lists the piece as needing a
decision. A collection can allow replacing without asking. (ADR 0036)
A diverged piece is refused.

### sync

`sync` pulls metadata, refreshes the live packages of changed pieces,
uploads pending archives, deletes a live package only after its
archive's metadata is committed on the remote, and applies the tiers.
(ADR 0018)

The archive tier starts from a piece's activity; the remote-only tier
is measured from the archive's commit time. (ADR 0042) A local archive
moves to the remote-only tier only after the remote's identity, the
same version and the remote copy's checksum are confirmed (computed on
the NAS over SSH, read back on a path remote); it is then deleted
through the trash and the time of the check is recorded. (ADR 0007,
ADR 0043)

### unearth

`unearth` takes the newest archive of an archived ID. It fetches the
archive into `.antiq/tmp/`, checks its checksum, extracts it into the
staging directory, verifies the staged tree strictly, writes a tracking
entry `unearthing`, checks the target again (missing parents are
created; a file, symlink or piece in the path refuses), renames the tree
into place and sets the entry to `live` with the time of unearthing.
Unearthing an ID that is already unearthed does nothing and says where
it is. The remote copy is kept while the piece is live; the local
archive is dropped only when the remote copy is confirmed and is always
kept without a remote. (ADR 0034)

### fork

`fork` works only on unearthed pieces. It gives the piece a new ID with
the time of the fork, and optionally a new name; the old ID's live
package is deleted. Unearthing the old ID afterwards needs a target
path.

### reconcile

`reconcile` rebuilds the index from local and remote metadata, completes
or drops `unearthing` entries, finishes `removing` entries, cleans up
after a crash during archiving, removes leftovers as the commit protocol
describes, and reports IDs whose committed version another machine
wrote. Entries whose archive is missing on the remote are marked and
shown in `status`; `reconcile --prune` drops them. (ADR 0044)

### status

`status` is read-only. It shows pieces by state: pinned, new, empty,
stale backups, diverged, needing a decision, pending remote work,
skipped symlinks and dot entries, and "remote-only in N days".

### Safety

Every step that deletes in bulk (dropping entries, removing live
packages, moving archives to the remote-only tier) goes through one
brake: when planned removals exceed 20 % of the affected set
(configurable), the step stops and lists up to ten. On a terminal it
asks; without one it refuses unless `--force` is given. No archive on
the remote is deleted except by a newer, verified and committed version
of the same ID. (ADR 0045)

## Command line

| Command | Does |
|---------|------|
| `init` | makes a directory a collection |
| `status` | shows the pieces by state |
| `archive` | archives idle pieces and removes their folders |
| `unearth` | brings an archived piece back |
| `find` | lists and searches all pieces; opens the picker |
| `fork` | gives an unearthed piece a new ID |
| `pin`, `unpin` | create or remove a piece's `.antiq-pin` |
| `sync` | exchanges backups and archives with the remote |
| `resolve` | answers pending decisions |
| `remote add`, `remote show`, `remote remove` | attach, show and detach the remote |
| `reconcile` | brings the index in line with the stored metadata |

(ADR 0023)

Every command takes `--json`, which also makes it non-interactive.
Otherwise a command is interactive when a controlling terminal is
available, also when its output is redirected. `--print` prints the
item picked in the picker. Every JSON output carries a `format` field.
(ADR 0020, ADR 0023)

`find` lists and searches all objects; interactively it opens a picker
from which an object can be unearthed. (ADR 0021)

Without a terminal, every decision point takes the non-destructive
default: no action on that object, which is listed as needing a
decision with its options; the command continues and exits non-zero.
`resolve` goes through pending decisions interactively, or answers one
for a piece with `--mine`, `--theirs`, `--fork`, `--replace` or
`--skip`. `archive` and `sync` ask in place when a terminal is
available. (ADR 0025)

`remote add <url>` attaches an existing remote and refuses when its root
has no marker; `remote add --init <url>` creates a new remote root.
(ADR 0026)

Every state-changing command takes `--dry-run`, which prints the plan
and changes nothing; under `--json` the plan is JSON. (ADR 0046)

`--if-reachable` makes a run exit 0 when the remote is away, for
scheduled runs.

Exit codes distinguish success, partial success and error. Colour is
used only on a terminal. Data goes to stdout, messages to stderr.
