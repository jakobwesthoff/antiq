# 30. Commit a version by claiming its number and renaming the metadata into place

Date: 2026-11-03

## Status

Accepted

## Context

Several machines can write versions of the same ID to one remote
(ADR 0017). An upload of an archive takes minutes, so two machines can
overlap. On an SMB mount, a file created exclusively is visible to other
machines while it is still being written, and no rename refuses an
existing target (`docs/research/filesystem.md`).

## Decision

Payload files are named after a prefix of their SHA-256 and are never
overwritten. A payload is uploaded first, under a temporary name that
carries the machine, and then renamed to its final name.

Only then does the writer claim metadata version N+1, one above the
highest committed version or claim, by creating an empty
`meta.<N+1>.claim` exclusively. It writes the metadata under a temporary
name and renames it to `meta.<N+1>`. A version is committed once
`meta.<N+1>` exists, and the highest committed version is current.
Readers read only `meta.<N>` files.

A claim that already exists is a conflict, resolved as ADR 0017
describes. A claim without metadata that is older than a grace period is
abandoned, and the next version number skips it. An abandoned claim is
removed only once a higher version is committed, so a version number is
never used twice. Payloads that no retained metadata version references
and old metadata files are removed once they are older than the grace
period. Ages are measured on the remote's clock.

## Consequences

The protocol needs only exclusive file creation and rename, which the
local disk, an SMB mount and a shell over SSH all offer.
