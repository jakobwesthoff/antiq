---
title: "Compare the whole target with the collection on request"
kind: feature
component: lib
horizon: someday
origin: discussion
---
# Compare the whole target with the collection on request

`push` in the simplified v1 concept (`docs/concept-v1-simplified.md`)
relies on rsync's own check: rsync computes a whole-file checksum of
each transferred file while the receiver writes it and compares it with
the sender's, and `--fsync` makes the receiver flush each finished
file. That covers the transfer, not what the target's disk loses later
(bit rot). Decided with the user in asqr on 2026-11-04: v1 uses rsync
with `--fsync` and nothing more; a full comparison waits for this todo.

## Proposal

`push --verify` (or a separate `verify` command) runs
`rsync -a -c -n --itemize-changes` against the target. With `-c` rsync
reads every file on both sides in full and compares checksums, so any
file that differs shows up as an itemized change; the run reports them
per piece and changes nothing.

## Open points

- Command form: a flag on `push` or its own command.
- Whether it also checks the local archives against their `.sha256`
  files (local bit rot), since a mirror would carry a damaged local
  archive to the target on the next change.
- The cost: it reads the whole collection and the whole target, over
  the network for an ssh target.

## Findings

- macOS ships openrsync as `/usr/bin/rsync` (protocol 29); its help
  lists no `--fsync`. Homebrew rsync 3.5.1 has `--fsync` and the
  whole-file check (checked 2026-11-04).

## Revisit when

`push` is implemented.
