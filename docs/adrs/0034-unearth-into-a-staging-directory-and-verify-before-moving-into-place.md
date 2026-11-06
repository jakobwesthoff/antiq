# 34. Unearth into a staging directory and verify before moving into place

Date: 2026-11-03

## Status

Accepted

## Context

An extraction that stops halfway leaves a partial tree. Extracted
directly into the collection, that tree would look like an idle piece,
and the next run could archive it over the only complete copy.

## Decision

Unearthing extracts the version into a staging directory inside
`.antiq/` and verifies every entry of the staged tree (ADR 0033). It
writes `.antiq-restored`, flushes every extracted file and directory,
and checks the destination again right before the rename; a file, a symlink
or a piece in its path makes unearthing refuse. Only then is the staged
tree renamed to its place in the collection. The parent is flushed, and
the version is deleted last, so no version is deleted before the folder
replacing it is durable.

## Consequences

None further.
