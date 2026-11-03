# 45. Hold bulk deletions without a per-item check behind a 20 percent brake

Date: 2026-11-03

## Status

Accepted

## Context

`timecapsule-sync.sh` stops before a sync would delete more than
20 % of the mirror. antiq deletes in several places. Some deletions
check each item first: a live package is removed only after its
archive is committed, and a local archive moves to the remote-only tier
only after its remote copy is confirmed (ADR 0043). Both happen in bulk
in normal use, such as after the first archive run or when many imported
archives reach the tier age on the same day.

## Decision

Bulk deletions that do not check each item go through one brake:
dropping entries, detaching the remote while pieces exist only on it,
and dropping superseded versions. When the planned removals exceed a
share of the affected set, 20 % by default and configurable, the step
stops and lists up to ten of them. On a terminal it asks; without one it
refuses unless `--force` is given. `--dry-run` shows when the brake
would hold. Removing live packages and moving archives to the
remote-only tier do not go through the brake.

No archive on the remote is deleted except when a newer, verified and
committed version of the same ID replaces it.

## Consequences

None further.
