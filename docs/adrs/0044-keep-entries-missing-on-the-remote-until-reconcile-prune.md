# 44. Keep entries missing on the remote until reconcile --prune

Date: 2026-11-03

## Status

Accepted

## Context

`reconcile` was to drop entries whose archive is gone. A wrong or
half-mounted remote makes every remote-only archive look gone, and the
metadata is the only searchable record of them (ADR 0007).

## Decision

`reconcile` marks such entries as missing on the remote and shows
them in `status`. They are dropped only with `reconcile --prune`.

## Consequences

None further.
