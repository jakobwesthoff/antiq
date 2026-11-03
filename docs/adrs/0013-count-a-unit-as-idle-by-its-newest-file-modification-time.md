# 13. Count a unit as idle by its newest file modification time

Date: 2026-11-02

## Status

Accepted

## Context

Archiving starts once a unit has been idle for a configured number of
days, so "idle" needs a definition.

## Decision

A unit is idle when the newest modification time of any regular file
in it is older than the threshold.

Ignore rules in gitignore syntax exclude paths from this check. They
come from the config and from ignore files of antiq's own at any
directory level. `.gitignore` files are not honoured.

## Consequences

None further.
