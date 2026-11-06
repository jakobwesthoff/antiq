# 33. Verify an archive strictly against the folder before deleting anything

Date: 2026-11-03

## Status

Accepted

## Context

Archiving deletes the folder, and unearthing moves a restored tree into
place. A check of the compressed stream alone does not show that the
archive holds what the folder holds.

## Decision

Before archiving deletes a folder and before unearthing moves a tree
into place, a strict verify reads the archive and compares it with the
tree on disk: the stream's integrity, and for every member its type,
size, content hash, modification time, mode and link target. The set of
paths must be equal, collected with the same walk rules. Ignored paths
are left out of the comparison. Any difference stops the step.

Unearthing compares every entry of the staged tree with the listing.
`fork` extracts a damaged version with a warning and without that
comparison. `rebuild` compares the members of an archive with the
listing before it writes a missing checksum file.

## Consequences

None further.
