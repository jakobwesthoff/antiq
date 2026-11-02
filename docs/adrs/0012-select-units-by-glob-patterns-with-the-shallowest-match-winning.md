# 12. Select units by glob patterns with the shallowest match winning

Date: 2026-11-02

## Status

Accepted

## Context

The playground archives each top-level folder as one unit. Other layouts
group folders deeper. A unit must never contain another unit.

## Decision

Units are selected by a list of glob patterns relative to the managed
folder. The default is `*`; `**/*` is allowed.

When matches nest, the shallowest wins: a match is a unit only if no
ancestor is a unit. Under this rule `**/*` selects the same units as
`*`. Deepest-wins would select every leaf directory and split a project
into many small archives.

The selection strategy is a replaceable part. A managed folder's strategy
is chosen at `init` and cannot change in the first version.

## Consequences

None further.
