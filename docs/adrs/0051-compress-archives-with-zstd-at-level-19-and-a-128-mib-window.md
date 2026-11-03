# 51. Compress archives with zstd at level 19 and a 128 MiB window

Date: 2026-11-03

## Status

Accepted

## Context

Archives are zstd (ADR 0019) and can be several GB. Plain level 19
was 61 % larger than `xz -9` on database snapshots and 51 % larger on
SQLite files; a 128 MiB window brought them to 1.7 % above and 31 %
below `xz -9` (`docs/research/compression-bench.md`).

## Decision

antiq compresses archives with the `zstd` crate (ADR 0052) at level 19
with long-distance matching and a window of 128 MiB (window log 27),
one worker per core, at zstd's default job size for these settings
(64 MiB). A window log of 27 is the largest that zstd decodes without
extra settings. When a run archives several pieces, it compresses them
in parallel, since an archive smaller than one job uses one core.

## Consequences

Highly compressible tabular text gets larger than with xz: 15.9 % on
the CSV and JSON sample.
