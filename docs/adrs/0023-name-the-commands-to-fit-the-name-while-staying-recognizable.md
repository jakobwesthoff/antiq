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
- `status` shows the pieces that have a state.
- `archive` archives due pieces and removes their folders.
- `unearth` brings a version back and removes it.
- `fork` extracts a working copy of a version and keeps it.
- `forget` deletes a version.
- `rebuild` repairs a damaged version.
- `import` packs a folder from outside the collection as a version.
- `list` lists the versions.
- `grep` searches the paths of all archived files.
- `pin` and `unpin` keep a piece from being archived (ADR 0024).
- `push` mirrors the collection to the target.

`push` is not called `backup` because the target is a mirror, not a
backup with history (ADR 0049).

## Consequences

None further.
