---
title: "Forget a piece for good with a forget command"
kind: feature
component: cli
status: needs-discussion
origin: discussion
tags: [ux]
---
# Forget a piece for good with a forget command

antiq never deletes a live package by itself: a package that is no
longer refreshed (`backup_live = false`, or `live_exclude` covers the
piece) is marked `stale_backup_since` and kept, and `reconcile
--prune` spares a gone piece while its live package is on the remote.
The user asked how such a package, or a whole piece, can then be
deleted, and chose an explicit command `forget <piece>` (asqr round 2,
2026-11-04).

## Decided

- An explicit command `forget <piece>` deletes a piece's live package,
  its metadata and its tracking entry, on the remote and so for every
  machine, through the brake and with `--dry-run`.

## Next step

The lead drafts the spec section and an ADR and puts them to the user
before an item is planned. Open there: whether `forget` also deletes
archive payloads or refuses while one exists, how other machines that
still track the piece learn of it (a tombstone in the remote
metadata?), and what `status` shows before and after.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
