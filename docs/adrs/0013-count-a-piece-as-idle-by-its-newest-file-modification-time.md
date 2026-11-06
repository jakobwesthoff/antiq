# 13. Count a piece as idle by its newest file modification time

Date: 2026-11-02

## Status

Accepted

## Context

Archiving starts once a piece has been idle for a configured number of
days, so "idle" needs a definition.

## Decision

The idle clock of a piece is the later of the newest modification time
of its regular files that are not ignored, not timeless and not control
files, and the time in `.antiq-restored` (ADR 0037). A piece is due when
its idle clock is older than the threshold. A file time or a marker time
in the future counts like any other.

Ignore rules in gitignore syntax exclude paths from this check. They
come from the config and from `.antiqignore` files at any directory
level. Timeless rules, from the config and from `.antiq-timeless` files,
exclude paths from this check as well but leave them in the version (ADR
0029). `.gitignore` files are not honoured.

## Consequences

None further.
