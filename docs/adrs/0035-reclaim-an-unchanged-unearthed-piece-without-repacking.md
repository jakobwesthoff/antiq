# 35. Reclaim an unchanged unearthed piece without repacking

Date: 2026-11-03

## Status

Accepted

## Context

An unearthed piece that was only looked at becomes idle again. Packing
it anew would produce a new version and a new upload for the same
content.

## Decision

When an unearthed piece is archived, antiq compares it with the
manifest stored with its archive: path, type, size, modification time,
mode, link target and content hash. If nothing differs, the folder is
removed and the existing archive stays, with the same version and no
upload.

## Consequences

None further.
