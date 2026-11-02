# 17. Let several machines share one remote

Date: 2026-11-02

## Status

Accepted

## Context

The playground exists on more than one machine (ADR 0008). A folder
archived on one machine should be retrievable on another.

## Decision

Several machines may use the same remote. Different machines write
different IDs, so they do not collide.

Each ID's metadata carries a version. Putting an ID back checks that the
remote still has the version that was taken out. A mismatch is a
conflict that the user resolves: keep mine, keep theirs, or fork.

No file on the remote is shared by machines for writing; a combined
index is built locally. Syncing one live folder between machines stays
out of the first version (ADR 0008).

## Consequences

None further.
