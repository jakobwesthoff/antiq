# Specification, version 1

Draft. This file describes version 1 as its design settles. The reasons
behind the decisions are recorded as ADRs in `docs/adrs/`.

## Purpose

The tool looks after directories where working folders pile up, such as
`~/playground`. It packs folders that have not changed for a set number
of days into compressed archives, keeps an index of each archive's
contents so it can be searched, restores archives on request, and moves
archives to a backup store.

## Scope

Version 1 covers what the scripts `archive.sh` and `timecapsule-sync.sh`
do for `~/playground`:

- archiving folders by age
- tar archives compressed with xz
- an index of each archive's contents
- searching the archives and restoring one through a picker
- rebuilding missing index entries
- syncing to a NAS

On top of that, version 1 moves archives through tiers by age. Whether
it includes the remote-only tier (ADR 0007) is open.

Not in version 1:

- syncing one managed folder between machines (ADR 0008)
- reading the archives and listings that `archive.sh` produced
  (ADR 0009)

## Command line

_Not yet established._
