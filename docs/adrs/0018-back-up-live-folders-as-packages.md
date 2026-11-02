# 18. Back up live folders as packages

Date: 2026-11-02

## Status

Accepted

## Context

`timecapsule-sync.sh` mirrors live folders as file trees. Over SMB that
was too slow with many small files, and a tree mirror does not fit every
kind of remote. Live folders grow quickly, so a backup has to transfer
only what changed.

## Decision

Live folders go to the remote as packages. A package and the later
archive of the same folder are one object under one ID. A live folder is
packaged again only when it changed since its last package, and only the
latest package is kept. Backing up live folders is a configurable
option.

Live packages are tar with a fixed file order, compressed in zstd's
rsync-friendly mode, and transferred with rsync. When rsync runs on the
remote's side, a small change then transfers a small part of the file.

A chunked, deduplicated store would give delta transfer on every remote
but is a storage engine of its own.

## Consequences

None further.
