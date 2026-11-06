# 41. Mirror the whole collection, content outside the pieces included

Date: 2026-11-03

## Status

Accepted

## Context

Pieces are selected by patterns, so files at the top of a collection
belong to no piece. `timecapsule-sync.sh` mirrored the whole playground,
loose files included.

## Decision

`push` mirrors the whole collection. Content outside every piece is
copied like a live folder.

## Consequences

None further.
