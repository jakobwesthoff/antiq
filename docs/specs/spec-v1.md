# Specification, version 1

Draft. This file describes version 1 as its design settles. The reasons
behind the decisions are recorded as ADRs in `docs/adrs/`.

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
- changing a collection's selection strategy after `init`
  (ADR 0012)
- more than one remote per collection (ADR 0015)

## Collections and pieces

A managed folder is called a collection, and each unit in it a piece.
(ADR 0022)

A collection is marked by `.antiq/` at its top. Commands find it by
searching upwards from the current directory, or from the path given
with `-C`, and fail outside a collection. `.antiq/` holds the
collection's config, metadata, index and local archives. (ADR 0011)

Pieces are the directories antiq treats as one thing each. A list of
glob patterns selects them, default `*`. When matches nest, the
shallowest match is the piece. (ADR 0012) An entry whose name starts
with a dot is a piece only when a pattern names it with a literal
leading dot in that path component; `.antiq/` is never a piece.
(ADR 0028)

A piece is idle when the newest modification time of any file in it is
older than the configured number of days. Ignore rules in gitignore
syntax, from the config and from `.antiqignore` files at any level,
exclude paths from this check. (ADR 0013, ADR 0029)

A piece with a `.antiq-pin` file at its top is pinned: it is never
archived, still backed up, and shown as pinned. Changes to
`.antiqignore` and `.antiq-pin` files do not count as activity.
(ADR 0024, ADR 0029)

## Objects

Every piece is an object with a ULID whose time is the folder's creation
time. The ID survives archiving, unearthing and archiving again; the
newest archive replaces the previous one. Forking an unearthed piece
gives it a new ID. (ADR 0014)

A live folder's package and its later archive are the same object.
(ADR 0018)

| State | Folder | Locally | On the remote |
|-------|--------|---------|---------------|
| live | present | metadata | latest live package, if backing up live folders is on |
| archived | gone | metadata, archive | metadata, archive |
| remote-only | gone | metadata | metadata, archive |

## Storage and remotes

Local storage in `.antiq/` is fixed. Remotes sit behind one trait, and
every remote supports every operation. Version 1 has a path remote and
an SSH remote that uses rsync. (ADR 0015)

The remote holds the full metadata set, one metadata file per ID, next to
the archives. Attaching a remote copies its metadata; archives are
fetched on demand. (ADR 0016)

Several machines may share one remote. Each ID's metadata has a version;
putting an ID back with a version that is no longer current is a
conflict, which the user resolves by keeping their own, keeping the
remote's, or forking. (ADR 0017)

## Packaging

A Packager trait packs, verifies, lists and unpacks. Archives are tar
with xz; live packages are tar with a fixed file order and zstd in its
rsync-friendly mode, transferred with rsync. (ADR 0018, ADR 0019)

Symlinks are archived and restored as they are, whatever their
target. An archive with a member whose path leads outside the piece is
refused, and unearthing never writes through a symlink. (ADR 0027)

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
available. `--print` prints the item picked in the picker. (ADR 0020,
ADR 0023)

`find` lists and searches all objects; interactively it opens a picker
from which an object can be unearthed. (ADR 0021)

A run without a terminal leaves decisions pending and lists them.
`resolve` goes through them interactively, or answers one for a piece
with `--mine`, `--theirs`, `--fork`, `--replace` or `--skip`. `archive`
and `sync` ask in place when a terminal is available. (ADR 0025)

`remote add <url>` attaches an existing remote and refuses when its root
has no marker; `remote add --init <url>` creates a new remote root.
(ADR 0026)
