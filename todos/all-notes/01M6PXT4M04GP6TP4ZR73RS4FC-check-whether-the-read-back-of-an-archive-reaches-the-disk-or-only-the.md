---
title: "Check whether the read-back of an archive reaches the disk or only the page cache"
kind: investigation
component: lib
status: needs-discussion
origin: discussion
tags: [error-handling]
---
# Check whether the read-back of an archive reaches the disk or only the page cache

Not confirmed. While discussing the verify step on 2026-11-05 the lead
noticed that archiving reads the archive back right after writing and
flushing it, with no option that bypasses the operating system's page
cache: no `F_NOCACHE`, `O_DIRECT` or `posix_fadvise` appears in `src/`
(checked with `rg` on 2026-11-05). The bytes read back then most likely
come from memory, so the read-back hash proves that the written bytes are
right in the cache, and only partly that they are right on the disk. How
much the flush before the read-back already guarantees, and whether
macOS and Linux behave the same here, is not established.

## Question

Is a read-back that bypasses the cache (`F_NOCACHE` on macOS,
`O_DIRECT` or `posix_fadvise(DONTNEED)` after the flush on Linux) worth
its cost for the guarantee ADR 0033 and the spec's verify step describe,
or is the flushed write plus the read-back from the cache enough?

## Next step

Find out what the read-back proves today on both platforms, then decide
with the user; if the cache read stays, say plainly in the spec what the
read-back proves.
