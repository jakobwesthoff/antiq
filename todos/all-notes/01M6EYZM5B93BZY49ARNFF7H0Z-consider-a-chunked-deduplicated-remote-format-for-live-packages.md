---
title: "Consider a chunked, deduplicated remote format for live packages"
kind: investigation
component: lib
status: needs-discussion
horizon: someday
origin: discussion
tags: [architecture]
---
# Consider a chunked, deduplicated remote format for live packages

Live folders go to the remote as packages: tar with a fixed file order,
compressed with zstd in its rsync-friendly mode, transferred with rsync so
only changed blocks move (business-logic design round, 2026-11-02). That
delta only works where rsync works; a cloud remote receives every live
package in full, and every change re-packs the whole folder locally.

A chunked, deduplicated store (as restic or borg use) would give delta
transfer on every remote and avoid re-uploading unchanged data, at the
cost of a storage engine of its own.

## Decisions

- 2026-11-02, user: not in v1; rsync-friendly packages instead.

## Revisit when

Live packages become too large or too slow to transfer, or a remote
without rsync is used for live folders.
