# 50. Write tar in-process with hand-built headers and real mtimes

Date: 2026-11-03

## Status

Accepted

## Context

Packing has to give the same bytes for unchanged files, compute each
file's hash in the same read, and keep modes for the strict verify (ADR
0033). The `tar` crate's deterministic mode resets every mode to 0644 or
0755 (`docs/research/tar.md`).

## Decision

antiq writes tar with the `tar` crate and builds each header itself: GNU
format, mode masked to permission bits, uid and gid 0, empty owner
names, the file's real modification time, no extended attributes.
Entries are sorted by path bytes. Hard links are detected by device and
inode and stored as links; the listing writes them as `hard` lines with
a checksum line of their own. Special files are skipped and reported.

An archive holds one top-level directory, named after the folder when it
is packed, with the piece's entries below it. Listings and the strict
verify compare the paths below that directory, so a piece that is
renamed, forked under a new name or unearthed elsewhere still compares.

Unearthing reads the members with the crate and writes each one itself,
after antiq's own checks, through directory handles that never follow a
symlink. It stops at the first refused member and sets directory modes
and times in a final pass.

## Consequences

A file whose modification time changes gets a new header even when its
content is the same.
