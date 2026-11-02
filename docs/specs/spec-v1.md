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
- changing a managed folder's selection strategy after `init`
  (ADR 0012)
- more than one remote per managed folder (ADR 0015)

## Managed folders and units

A managed folder is marked by `.antiq/` at its top. Commands find it by
searching upwards from the current directory, or from the path given
with `-C`, and fail outside a managed folder. `.antiq/` holds the
folder's config, metadata, index and local archives. (ADR 0011)

Units are the directories antiq treats as one thing each. A list of glob
patterns selects them, default `*`. When matches nest, the shallowest
match is the unit. (ADR 0012)

A unit is idle when the newest modification time of any file in it is
older than the configured number of days. Ignore rules in gitignore
syntax, from the config and from antiq's own ignore files, exclude paths
from this check. (ADR 0013)

## Objects

Every unit is an object with a ULID whose time is the folder's creation
time. The ID survives archiving, bringing back and archiving again; the
newest archive replaces the previous one. Forking a brought-back folder
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

## Command line

Every command takes `--json`, which also makes it non-interactive.
Otherwise a command is interactive when a controlling terminal is
available. A flag prints the item picked in the picker. (ADR 0020)

One command lists and searches all objects; interactively it opens a
picker from which an object can be brought back. (ADR 0021)

Command names: _Not yet established._ They follow the name antiq.
