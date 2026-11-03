# 51. Compress archives with liblzma and size its threads by its memory estimate

Date: 2026-11-03

## Status

Accepted

## Context

Archives are xz (ADR 0019) and can be several GB. Multithreaded xz at
preset 9 needs about 1,249 MiB per thread
(`docs/research/compression.md`).

## Decision

antiq compresses archives with the `liblzma` crate at preset 9 with a
SHA-256 check. The thread count is the largest that fits a memory limit,
25 % of RAM by default, according to liblzma's own estimate. Decoding
uses the same memory limit.

## Consequences

Archives smaller than one xz block (192 MiB at preset 9) use one
thread.
