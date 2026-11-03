# 53. Store metadata as versioned JSON that keeps unknown fields

Date: 2026-11-03

## Status

Accepted

## Context

Metadata files are shared through the remote by machines that may run
different antiq versions.

## Decision

Each metadata file is JSON whose first field is `format`. Keys are
written in a fixed order, times in RFC 3339 UTC in whole seconds, sizes
in bytes. Fields antiq does not know are kept when it rewrites a file.
A file with a higher `format` than antiq knows is read-only to it and
produces a clear error on any write.

## Consequences

None further.
