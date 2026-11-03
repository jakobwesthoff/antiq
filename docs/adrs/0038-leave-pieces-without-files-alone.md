# 38. Leave pieces without files alone

Date: 2026-11-03

## Status

Accepted

## Context

A piece in which the idle check finds no regular file has no newest
modification time, so ADR 0013 does not say when it is idle. That
includes a piece whose files are all ignored, marker files or junk.
`archive.sh` deletes empty trees without an archive.

## Decision

Such a piece is never idle and never archived. It is listed as empty
and left alone.

## Consequences

None further.
