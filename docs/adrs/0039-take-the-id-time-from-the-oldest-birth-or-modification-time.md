# 39. Take the ID time from the oldest birth or modification time

Date: 2026-11-03

## Status

Accepted

## Context

ADR 0014 leaves open how a folder's creation time is found. On APFS a
directory's birth time is often later than its content: a folder can be
born weeks after the files in it. Copies made with `cp -Rp`, `rsync -a`
or `tar -x` get a birth time equal to the source's modification time.

## Decision

The ID time is the earliest of the directory's birth time and the
birth and modification times of every entry inside it. Times before
1990 are ignored. When a birth time and a modification time are equal,
the source is `oldest-mtime`. When no time from 1990 on remains, the
ID time is the time of minting, with a warning. The source of the time
(`birth`, `oldest-mtime`, `import`, `fork` or `minted`) is stored with
it. The ID is minted once and never recomputed.

## Consequences

None further.
