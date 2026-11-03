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
archived. It is still backed up and is shown as pinned.

## Consequences

The pin moves and is renamed together with its folder.
