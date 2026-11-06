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
highest committed version, by creating an empty `meta.<N+1>.claim`
exclusively. It writes the metadata under a temporary name and checks
that its claim still exists, that no claim above its own exists, that no
version above the highest one it saw has been committed, and that the files the metadata names are present
with their sizes. Then it renames the metadata to `meta.<N+1>` and looks
at the directory once more; a claim above its own at that point leaves
the version committed but contested until the claim is released or becomes
a higher version. A version is committed once `meta.<N+1>`
exists, and the highest committed version is current. Readers read only
`meta.<N>` files.

A claim that already exists is a conflict, resolved as ADR 0017
describes. So is a claim above the highest committed version that is
younger than the grace period: its writer goes on, and this writer tries
again at its next sync. Skipping such a claim would put this version
above one that is being committed and that this writer did not base
itself on, and that writer would lose without a conflict being reported
(ADR 0017). A claim without metadata that is older than the grace period
is abandoned, and the next version number skips it. So does a claim with
this machine's own metadata temp name beside it, which is an interrupted
attempt of this machine. An abandoned claim is removed only once a higher
version is committed, so a version number is never used twice. The highest
committed version is retained; an older metadata file is removed once a
higher version is committed and it is older than the grace period.
Payloads that no metadata version references are removed once they are
older than the grace period. Ages are measured on the remote's clock.

A change recorded locally waits as a pending file until a sync commits
it (ADR 0058); the protocol gives it its version number.

## Consequences

The protocol needs only exclusive file creation and rename, which the
local disk, an SMB mount and a shell over SSH all offer.
