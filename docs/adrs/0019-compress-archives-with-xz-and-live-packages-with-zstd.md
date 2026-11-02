# 19. Compress archives with xz and live packages with zstd

Date: 2026-11-02

## Status

Accepted

## Context

Archives are written once and kept for a long time. Live packages are
rewritten whenever a folder changes.

## Decision

Packaging sits behind a Packager trait, which also verifies what it
packed. Archives are tar compressed with xz. Live packages are tar
compressed with zstd.

## Consequences

None further.
