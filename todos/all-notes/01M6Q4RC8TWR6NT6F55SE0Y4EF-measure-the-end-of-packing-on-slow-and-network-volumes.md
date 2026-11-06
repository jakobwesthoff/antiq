---
title: "Measure the end of packing on slow and network volumes"
kind: investigation
component: lib
status: needs-discussion
origin: review
tags: [ux]
---
# Measure the end of packing on slow and network volumes

Not measured yet. The packing card follows the bytes zstd has consumed,
mapped to the content of the piece. After the last byte, more work
still runs under `packing`: the flush of the archive file, the fsync of
the archive, writing and flushing the listing and the checksum file,
and the flush of the staging directory (`src/commands/archive/store.rs`).
The card shows that time at 99 percent with a bar one cell short of
full.

On a local SSD this tail was measured at about 24 ms for an 895 MB piece
(2026-11-05). On a slow disk, a USB stick or a network volume the
fsyncs can take seconds; that has not been measured.

## Next step

Measure the tail on a slow and a network volume. If it stays short, delete
this todo. If it is long, decide whether the card shows it as its own
step (for example `syncing`) or keeps the 99 percent.
