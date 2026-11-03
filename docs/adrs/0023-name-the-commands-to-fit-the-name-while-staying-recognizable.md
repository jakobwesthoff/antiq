# 23. Name the commands to fit the name while staying recognizable

Date: 2026-11-03

## Status

Accepted

## Context

The commands need names. They lean towards the theme of the name antiq
and stay recognizable to someone who has not read the documentation.

## Decision

The commands are:

- `init` makes a directory a collection.
- `status` shows the pieces by state.
- `archive` archives idle pieces and removes their folders.
- `unearth` brings an archived piece back.
- `find` lists and searches all pieces and opens the picker (ADR 0021).
- `fork` gives an unearthed piece a new ID.
- `pin` and `unpin` keep a piece from being archived (ADR 0024).
- `sync` exchanges backups and archives with the remote.
- `resolve` answers pending decisions (ADR 0025).
- `remote` attaches and shows the remote (ADR 0026).
- `reconcile` brings the index in line with the stored metadata.

The flag that prints the item picked in the picker (ADR 0020) is
`--print`.

## Consequences

None further.
