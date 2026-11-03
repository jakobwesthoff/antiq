# 19. Compress archives and live packages with zstd

Date: 2026-11-02

## Status

Accepted

## Context

Archives are written once and kept for a long time. Live packages are
rewritten whenever a folder changes. On ten existing archives, zstd at
level 19 with a 128 MiB window gave 98.6 % of the size of `xz -9`,
compressed 2.4 times and decompressed 9 times faster, and needed 140 MB
instead of up to 2.8 GB to decompress (`docs/research/compression-bench.md`).

## Decision

Packaging sits behind a Packager trait, which also verifies what it
packed. Archives and live packages are tar compressed with zstd, with
different settings (ADR 0051, ADR 0052).

## Consequences

The archives `archive.sh` made are xz. They are recompressed once,
outside antiq, when they are imported; antiq reads no xz.
