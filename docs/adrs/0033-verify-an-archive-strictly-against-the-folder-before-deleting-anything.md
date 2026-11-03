# 33. Verify an archive strictly against the folder before deleting anything

Date: 2026-11-03

## Status

Accepted

## Context

Archiving deletes the folder, reclaiming deletes an unearthed folder,
and unearthing moves a restored tree into place. A check of the
compressed stream alone does not show that the archive holds what the
folder holds.

## Decision

Before any of these steps, a strict verify reads the archive and
compares it with the tree on disk: the stream's integrity, and for every
member its type, size, content hash, modification time, mode and link
target. The set of paths must be equal, collected with the same walk
rules. Junk files such as `.DS_Store` are left out of the comparison.
Any difference stops the step.

## Consequences

None further.
