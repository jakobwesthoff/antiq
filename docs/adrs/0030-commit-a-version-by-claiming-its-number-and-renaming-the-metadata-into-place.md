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

Payload files are named after their version and a prefix of their
SHA-256 and are never overwritten. A payload is uploaded under a
temporary name that carries the machine and the start time, then renamed
to its final name.

A writer claims version N+1 by creating an empty `meta.<N+1>.claim`
exclusively. It writes the metadata under a temporary name and renames
it to `meta.<N+1>`. A version is committed once `meta.<N+1>` exists, and
the highest committed version is current. Readers read only `meta.<N>`
files.

A claim that already exists is a conflict, resolved as ADR 0017
describes. A claim without metadata that is older than a grace period is
abandoned, and the next version number skips it. Abandoned claims,
payloads no version references and old metadata files are removed after
the grace period, or earlier for the writer's own leftovers once its
local lock shows that no run is active.

## Consequences

The protocol needs only exclusive file creation and rename, which the
local disk, an SMB mount and a shell over SSH all offer.
