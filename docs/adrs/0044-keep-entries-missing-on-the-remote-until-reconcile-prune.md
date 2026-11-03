# 44. Keep entries missing on the remote until reconcile --prune

Date: 2026-11-03

## Status

Accepted

## Context

A wrong or half-mounted remote makes every remote-only archive look
gone, and the metadata is the only searchable record of them
(ADR 0007). Dropping such entries automatically would lose that record.

## Decision

`reconcile` marks an entry as missing on the remote when it has no
local archive and its archive, which the remote held before, is gone.
`status` shows such entries. They are dropped only with
`reconcile --prune`. An entry whose archive exists locally and has not
been uploaded yet is pending upload and is never pruned.

## Consequences

None further.
