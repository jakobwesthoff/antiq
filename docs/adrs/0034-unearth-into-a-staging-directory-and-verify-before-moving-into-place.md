# 34. Unearth into a staging directory and verify before moving into place

Date: 2026-11-03

## Status

Accepted

## Context

An extraction that stops halfway leaves a partial tree. Extracted
directly into the collection, that tree would look like an idle piece,
and the next run could archive it over the only complete copy.

## Decision

Unearthing fetches the archive into `.antiq/`, checks its checksum,
extracts it into a staging directory inside `.antiq/`, and verifies the
staged tree strictly against the archive (ADR 0033). Only then is the
staged tree renamed to its place in the collection. The target is
checked again right before the rename; a file, a symlink or a piece in
its path makes unearthing refuse. Unearthing an ID that is already
unearthed does nothing and says where it is.

## Consequences

None further.
