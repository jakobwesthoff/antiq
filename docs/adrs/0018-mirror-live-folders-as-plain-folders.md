# 18. Mirror live folders as plain folders

Date: 2026-11-02

## Status

Accepted

## Context

antiq archives the folders of a collection that nobody has touched for a
while and mirrors the whole collection to one backup target. The target
is a plain copy of the collection.

## Decision

`push` mirrors the whole collection, live folders included, to one
target as plain folders with rsync. The target is a plain copy of the
collection: copying it back restores the live folders and the versions.
Hard links are kept in the mirror. ssh is the recommended way to reach a
NAS.

## Consequences

Ignored paths such as `node_modules/` are mirrored with the live folder;
the ignore rules apply to versions only. Over SMB, `timecapsule-sync.sh`
was too slow with many small files; ssh is the recommended way to reach
a NAS.
