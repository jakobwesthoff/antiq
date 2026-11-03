# 40. Show other machines' live pieces and unearth them only as a fork

Date: 2026-11-03

## Status

Accepted

## Context

On a shared remote, one machine sees the live pieces another machine
backs up. If it could unearth one under the same ID, both machines would
refresh the same live package, which is the syncing ADR 0008 leaves
out.

## Decision

Another machine's live pieces are shown as live on that machine. They
can be unearthed only as a fork with a new ID. `archive` and `sync`
never touch them.

## Consequences

None further.
