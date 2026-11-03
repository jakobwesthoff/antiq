# 49. Transfer packages over SSH with the rsync executable 3.1 or later

Date: 2026-11-03

## Status

Accepted

## Context

Refreshing a live package by delta needs a receiver on the NAS that
applies the delta. A stock NAS runs rsync; no Rust crate implements the
rsync sender protocol (`docs/research/rsync.md`). Archives of several
GB need a transfer that resumes after an interruption.

## Decision

On the SSH remote, antiq moves live packages and archives with rsync
3.1 or later. It refuses openrsync, the default `/usr/bin/rsync` on
macOS, whose stats output, flags and timeout behaviour differ, and the
rsync binary is configurable. It runs rsync with `LC_ALL=C`, `--stats`,
`--no-human-readable`, `--no-whole-file` and `--timeout`, and never
with `--inplace` or `--append`. A new live package is seeded on the NAS
as a hard link to the current one and transferred onto it. An archive
upload keeps its partial file, so the next attempt resumes. Any
non-zero exit fails the transfer.

## Consequences

Transfer statistics come from parsing rsync's text output.
