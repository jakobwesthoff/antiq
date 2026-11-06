# 24. Pin a piece with an .antiq-pin file inside it

Date: 2026-11-03

## Status

Accepted

## Context

Some folders must never be archived, whatever their age. `archive.sh`
skips folders that contain a `.no-archive` file; four folders in the
playground have one.

## Decision

`antiq pin <piece>` creates the file `.antiq-pin` at the top of the
piece, and `antiq unpin <piece>` removes it. A pinned piece is never
archived. It is still mirrored by `push` and is shown as pinned.

A regular file or a symlink of that name pins the piece. Any other entry
of that name refuses the piece. The file is written in `.antiq/tmp/` and
renamed into place.

## Consequences

The pin moves and is renamed together with its folder.
