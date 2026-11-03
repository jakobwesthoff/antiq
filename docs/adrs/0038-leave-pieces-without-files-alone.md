# 38. Leave pieces without files alone

Date: 2026-11-03

## Status

Accepted

## Context

A piece with no regular files has no newest modification time, so
ADR 0013 does not say when it is idle. `archive.sh` deletes empty trees
without an archive.

## Decision

A piece without regular files is never idle and never archived. It is
listed as empty and left alone.

## Consequences

None further.
