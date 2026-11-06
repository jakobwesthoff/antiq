# 8. Leave multi-machine support out of the first version

Date: 2026-11-02

## Status

Accepted

## Context

The playground exists on more than one machine. `timecapsule-sync.sh`
mirrors each machine into a directory of its own on the NAS.

## Decision

The first version has no multi-machine support: no syncing of one
collection between machines, no version conflicts, no claims, no
versioned metadata documents and no tiers. A second machine that needs a
version copies its files from the target by hand. The layout does not
block adding more later.

## Consequences

None further.
