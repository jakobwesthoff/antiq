# 38. Leave pieces without files alone

Date: 2026-11-03

## Status

Accepted

## Context

A piece without a countable file has no newest modification time, so ADR
0013 does not say when it is due. That includes a piece whose files are
all ignored, timeless or control files. `archive.sh` deletes empty trees
without an archive.

## Decision

Such a piece is never archived and is left alone.

## Consequences

None further.
