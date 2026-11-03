# 55. Register existing archives with an import command

Date: 2026-11-03

## Status

Accepted

## Context

The 313 archives `archive.sh` made are recompressed with zstd outside
antiq (ADR 0019). Without an ID, metadata and a listing, `find` does not
see them.

## Decision

`antiq import` takes one or more compressed tar archives and registers
them as archived pieces of the collection. Each gets a new ID whose time
is the oldest modification time among its members (ADR 0039, source
`import`), its name from the archive's single top-level folder, a
listing generated from the archive, and the archive as its first
version. An archive that does not have exactly one top-level folder is
refused. The command is not tied to the layout of `archive.sh` (ADR
0009).

## Consequences

None further.
