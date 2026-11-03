---
title: "Detect bind mounts inside pieces"
kind: improvement
component: lib
horizon: someday
origin: implementation
---
# Detect bind mounts inside pieces

antiq never archives a piece that contains another device (spec
"Walks"): it compares each entry's `st_dev` with the piece's top
directory. A bind mount of a directory from the same filesystem keeps
the same `st_dev`, so the rule cannot see it, and such a piece would be
archived and its folder deleted, the bound directory's content with it.
Found while planning work item I05 of the v1 plan (2026-11-04).

## Approach

Recognising a mount root needs more than `st_dev`: on Linux
`statx` reports `STATX_ATTR_MOUNT_ROOT`; on macOS `statfs` gives the
mount point of a path (`f_mntonname`), which differs from the parent's
for a nullfs mount. Either would need a mount-root flag on the walk's
entries (`TreeEntry` in `src/tree.rs`).

## Revisit when

antiq is used on a machine with bind mounts below a collection, or
before the first release if the walk is touched anyway.
