# 45. Hold every bulk deletion above 20 percent behind one brake

Date: 2026-11-03

## Status

Accepted

## Context

`timecapsule-sync.sh` stops before a sync would delete more than
20 % of the mirror. antiq deletes in several places: dropping entries,
removing live packages and moving archives to the remote-only tier.

## Decision

Every step that deletes in bulk goes through one brake. When the
planned removals exceed a share of the affected set, 20 % by default and
configurable, the step stops and lists up to ten of them. On a terminal
it asks; without one it refuses unless `--force` is given. `--dry-run`
shows when the brake would hold.

No archive on the remote is deleted except when a newer, verified and
committed version of the same ID replaces it.

## Consequences

None further.
