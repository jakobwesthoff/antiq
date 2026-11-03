# 52. Compress live packages with the zstd crate at a pinned zstd version

Date: 2026-11-03

## Status

Accepted

## Context

Live packages are zstd in its rsync-friendly mode (ADR 0018). The
`zstd` crate reproduces `zstd -3 --rsyncable -B524288` byte for byte,
and its output changes when the bundled zstd version changes
(`docs/research/compression.md`).

## Decision

antiq compresses live packages with the `zstd` crate at level 3, with
rsyncable mode, a job size of 512 KiB, at least one worker and no
long-distance matching. The bundled zstd version is pinned exactly, and
a test hashes the output for a fixed input. Each package records the
zstd version and the parameters that made it.

## Consequences

Moving the pinned zstd version uploads every live package once in
full.
