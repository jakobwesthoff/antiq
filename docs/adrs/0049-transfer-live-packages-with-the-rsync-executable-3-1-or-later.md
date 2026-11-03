# 49. Transfer live packages with the rsync executable 3.1 or later

Date: 2026-11-03

## Status

Accepted

## Context

Refreshing a live package by delta needs a receiver on the NAS that
applies the delta. A stock NAS runs rsync; no Rust crate implements the
rsync sender protocol (`docs/research/rsync.md`).

## Decision

antiq runs rsync 3.1 or later. It refuses openrsync, the default
`/usr/bin/rsync` on macOS, whose stats output, flags and timeout
behaviour differ, and the rsync binary is configurable. It runs rsync
with `LC_ALL=C`, `--stats`, `--no-human-readable`, `--no-whole-file`
and `--timeout`, and never with `--inplace` or `--append`. The new
package is seeded on the NAS as a hard link to the current one and
transferred onto it.

## Consequences

Transfer statistics come from parsing rsync's text output. Exit code
24 is tolerated.
