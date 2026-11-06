---
title: "Status reports a folder as missing when its lookup fails for another reason"
kind: bug
component: lib
origin: review
tags: [error-handling]
---
# Status reports a folder as missing when its lookup fails for another reason

Found in the opus diff review of I35 (finding V1, 2026-11-04). When a
tracked piece lies below a directory that selection picked as a piece
of its own, `status` looks the piece's folder up with an lstat. Any
failure of that lstat, including permission denied on the parent or an
I/O error, reads as "the folder is not there", so the piece derives as
`gone`.

## Problem

`src/commands/status.rs`, `presence` (around line 1005) falls through to
`exists_exact` (around line 1037), which turns every lstat error into
`Missing`. Shown by the reviewer in a scratch copy: a tracked piece
`dir/alpha` with its folder in place, `chmod 000 dir`; `status` shows
the piece as `gone`. Selection picks `dir` itself as a piece, so it
never has to read inside it, and `unreadable_directories` stays empty.

The spec defines `below_unreadable` only for "a piece that selection
could not read" and says nothing about a failed lstat in this lookup.

## Impact

`status` tells the user a folder is gone while it is still on disk.
`reconcile --prune` had the same mistake and drops only on `NotFound`
or `NotADirectory` since the V1 fix in I35; `status` still has it.

## Suggested fix

Treat only `NotFound` and `NotADirectory` as missing. Any other error
gives an unknown presence (like `below_unreadable`) with the error as a
note, and the spec "States" says how such a piece is shown.

## Relations

- Part of: [Rework antiq v1 to the simplified concept](01M6KFHS2FVQ9APAGQ947FP3CM-rework-antiq-v1-to-the-simplified-concept.md)
