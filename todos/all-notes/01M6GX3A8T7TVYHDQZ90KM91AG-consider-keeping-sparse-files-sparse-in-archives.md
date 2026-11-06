---
title: "Consider keeping sparse files sparse in archives"
kind: feature
component: lib
horizon: someday
origin: review
---
# Consider keeping sparse files sparse in archives

v1 stores a sparse file (a VM or container disk image, for example) as
a regular file: packing reads its full apparent size, and unearthing
writes it back fully allocated, so a piece that fit on disk before may
not fit after unearthing. `archive` warns when a piece's apparent size
exceeds its allocated size by more than 1 GiB, and all free-space checks
use the apparent size (spec "Operations").

## Decisions

- 2026-11-03, user: file this as a someday todo.

## Options

None of these was tried or measured.

- **Holes on restore only.** Unearth writes runs of zero blocks as
  holes (seeks past them) instead of writing them. The archive format
  stays plain tar, the content hash and strict verify are unchanged, and
  the restored file is sparse again. Packing still reads the full
  apparent size, and a file that held real zero blocks also comes back
  with holes there.
- **Sparse entries in the tar.** Packing finds the holes with
  `SEEK_DATA`/`SEEK_HOLE` and writes GNU sparse entries (the old GNU
  `S` type, or the pax `GNU.sparse.*` format 1.0). Holes are then
  neither read nor stored. It needs own header code next to the
  hand-built GNU headers (ADR 0050), a new package format id, and
  changes to the listing and the strict verify. Whether the `tar` crate
  reads such entries was not checked.
- **Leave it as in v1.**

## Revisit when

A piece with large sparse files is archived and the full read or the
fully allocated restore becomes a problem.
