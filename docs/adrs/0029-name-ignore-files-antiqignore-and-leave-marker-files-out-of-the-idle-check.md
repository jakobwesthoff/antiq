# 29. Name ignore files .antiqignore and leave marker files out of the idle check

Date: 2026-11-03

## Status

Accepted

## Context

ADR 0013 lets ignore files of antiq's own, at any directory level,
exclude paths from the idle check. ADR 0024 puts a pin file into a
piece. Both are files inside the piece.

## Decision

antiq's ignore files are named `.antiqignore`. Changes to `.antiqignore`
and `.antiq-pin` files do not count as activity in the idle check. Both
are archived with the piece.

## Consequences

None further.
