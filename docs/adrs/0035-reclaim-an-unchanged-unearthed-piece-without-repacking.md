# 35. Reclaim an unchanged unearthed piece without repacking

Date: 2026-11-03

## Status

Accepted

## Context

An unearthed piece that was only looked at becomes idle again. Packing
it anew would produce a new version and a new upload for the same
content.

## Decision

When an unearthed piece is archived, antiq compares it with the listing
of its archive: path, type, size, file modification time in whole
seconds, mode, link target and content hash. Directory modification
times and junk files do not count, since showing a folder in Finder
changes them. A file the archive holds does count even when its name is
on the junk list. If nothing differs, the folder is removed and the existing
archive stays, with no new payload and no upload. The gate for removing
the folder is the one ADR 0033 sets for reclaiming.

## Consequences

None further.
