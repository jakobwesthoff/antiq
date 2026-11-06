# 55. Bring in existing folders with an import command

Date: 2026-11-03

## Status

Accepted

## Context

The 313 archives `archive.sh` made are `tar.xz`. antiq reads no xz (ADR
0019). Without a version in the store, `list` and `grep` do not see
them.

## Decision

`antiq import <folder>` packs a folder from outside the collection
directly as a version of `<path>` (default: the folder's name; `--as`
sets it), with the steps of `archive` up to the move into the store. The
source folder is left in place. The version time is the newest counted
modification time in the folder, or `--time`. The listing carries
`imported <time>`, the real time of the import. Old archive files are
unpacked with their original timestamps and each folder is imported. The
command is not tied to the layout of `archive.sh` (ADR 0009).

## Consequences

None further.
