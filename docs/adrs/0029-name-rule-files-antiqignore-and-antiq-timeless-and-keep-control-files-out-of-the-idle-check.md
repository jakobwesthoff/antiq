# 29. Name rule files .antiqignore and .antiq-timeless and keep control files out of the idle check

Date: 2026-11-03

## Status

Accepted

## Context

ADR 0013 lets rule files of antiq's own, at any directory level, exclude
paths from the idle check. ADR 0024 puts a pin file into a piece. All
are files inside the piece.

## Decision

The ignore files are named `.antiqignore`, the timeless files
`.antiq-timeless`. What an ignore rule matches is not packed, not listed
and does not count for the idle check; `archive` deletes it with the
folder. What a timeless rule matches is packed but does not count for
the idle check.

`.antiqignore`, `.antiq-timeless`, `.antiq-pin` and `.antiq-restored`
are control files and never count as activity. The rule files apply at
any level; the two markers only at a piece's top. The rule files are
packed with the piece. `.antiq-restored` is not packed and is deleted
with the folder.

## Consequences

None further.
