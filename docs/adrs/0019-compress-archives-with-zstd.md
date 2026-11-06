# 19. Compress archives with zstd

Date: 2026-11-02

## Status

Accepted

## Context

Archives are written once and kept for a long time. On ten existing
archives, zstd at level 19 with a 128 MiB window gave 98.6 % of the size
of `xz -9`, compressed 2.4 times and decompressed 9 times faster, and
needed 140 MB instead of up to 2.8 GB to decompress
(`docs/research/compression-bench.md`).

## Decision

Packaging sits behind a Packager trait, which also verifies what it
packed. Archives are tar compressed with zstd (ADR 0051).

## Consequences

The archives `archive.sh` made are xz. They are unpacked outside antiq
and imported as folders; antiq reads no xz.
