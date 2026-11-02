# 7. Keep the index and metadata local for remote-only archives

Date: 2026-11-02

## Status

Accepted

## Context

Archives move through tiers as they age. In a later tier an archive lives
only on a backup store, and it has to be settled what stays on the local
machine.

## Decision

The local archive file is deleted. Its index and metadata stay local.
Searching covers remote-only archives like local ones. Restoring one
fetches it from the store when the store is reachable.

## Consequences

None further.
