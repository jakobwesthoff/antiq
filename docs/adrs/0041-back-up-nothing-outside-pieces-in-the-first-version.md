# 41. Back up nothing outside pieces in the first version

Date: 2026-11-03

## Status

Accepted

## Context

Pieces are selected by patterns, so files at the top of a collection
belong to no piece. `timecapsule-sync.sh` mirrored the whole playground,
loose files included.

## Decision

In the first version, content outside every piece is neither archived
nor backed up.

## Consequences

Loose files at the top of a collection need another backup.
