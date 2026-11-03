# 58. Keep an uncommitted metadata change in a pending file until sync commits it

Date: 2026-11-03

## Status

Accepted

## Context

Commands such as `archive` and `unearth` record their metadata changes
locally, and the next sync commits them to the remote. The
local object directory has the remote's layout, where `meta.<N>` is a
committed version. The number of the next version is known only when
the commit claims it (ADR 0030), and another machine may commit in
between.

## Decision

A recorded change is the full next metadata document, written as
`meta.pending` in the object's local directory. `meta.<N>` files there
hold only versions known to be committed, pulled or committed by this
machine. Local readers prefer `meta.pending`.

A sync commits the pending document with the protocol of ADR 0030,
which gives its version number, and then renames it locally to that
`meta.<N>`. Until then the tracking entry's version taken out reads
`pending`. Without a remote the change stays pending.

## Consequences

None further.
