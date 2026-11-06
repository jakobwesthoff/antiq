# 30. Make a version exist if and only if its .tar.zst exists

Date: 2026-11-03

## Status

Accepted

## Context

A version is three files in a store folder: the `.tar.zst`, the listing
and the checksum file. They cannot be written or deleted in one step.

## Decision

A version exists if and only if its `.tar.zst` exists. `archive` moves
the listing, then the checksum file, then the `.tar.zst` into the store,
each flushed, so the `.tar.zst` comes in last. Deleting a version
removes the `.tar.zst` first, then the listing and the checksum file.
Sidecars without a `.tar.zst` are an incomplete version, not a version.

## Consequences

None further.
