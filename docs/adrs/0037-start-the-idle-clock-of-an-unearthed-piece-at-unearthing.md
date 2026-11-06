# 37. Start the idle clock of an unearthed piece at unearthing

Date: 2026-11-03

## Status

Accepted

## Context

Unearthing restores the files' modification times, so an unearthed piece
counts as idle at once. With scheduled runs it would be archived again
within minutes.

## Decision

`unearth` and `fork` write `.antiq-restored` at the top of the piece
with the time of the restore. The idle clock is the later of the newest
file modification time and the time in that file (ADR 0013). While an
unearth is under way the file also holds the version stamp, so the
repairs of the next run can finish an unearth that stopped between the
rename and the deletion of the version (ADR 0059).

## Consequences

None further.
