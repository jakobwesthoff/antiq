---
title: "Apply the find, unearthed-loss and prune decisions of 2026-11-04"
kind: improvement
component: lib
origin: discussion
tags: [ux]
---
# Apply the find, unearthed-loss and prune decisions of 2026-11-04

Four small changes the user decided in asqr rounds 1 and 2 on
2026-11-04, done as one fix batch before the shell integration, since
both touch `find`.

## Changes

1. `find --print`: Enter prints the picked path and closes the picker.
   Staying open was a wrong decision; ADR 0057 is corrected in place
   (ADRs stay rewritable until the first release), with spec "Command
   line" and the picker code and tests.
2. Re-archiving an unearthed piece: a changed mode of a directory no
   longer counts as content loss (spec "archive", the loss rule; I26's
   comparison in `src/ops/archive/unearthed.rs` and `repair.rs`).
3. `reconcile --prune` spares a gone piece while its live package is on
   the remote; only what exists nowhere any more is dropped (spec
   "reconcile", "States"; `src/ops/reconcile/prune.rs`).
4. Remove the error kind `not_implemented`: nothing raises it since
   I26 and I35 (`src/error.rs`, spec error-kind table).

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
