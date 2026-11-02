# 4. Keep the index and metadata local for remote-only archives

Date: 2026-11-02

## Status

Accepted

## Context

Folders move through tiers as they age. In one tier a folder is packed
into an archive that stays on the local disk. In a later tier the archive
lives only on a backup store.

## Decision

When an archive moves to the remote-only tier, the local archive file is
deleted. Its index and metadata stay on the local machine. Searching
covers remote-only archives the same way as local ones, and restoring a
remote-only archive fetches it from the store when the store is
reachable.

Whether the first version ships the remote-only tier is not decided.

## Consequences

A remote-only archive takes local disk space only for its index and
metadata.
