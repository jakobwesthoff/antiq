---
title: "Retry the lock in the hold_lock helper of the integration tests"
kind: bug
component: lib
impact: low
origin: implementation
tags: [testing]
---
# Retry the lock in the hold_lock helper of the integration tests

`tests/live_progress.rs`,
`a_run_that_cannot_take_the_lock_prints_no_cards_and_fails_as_before`,
failed twice in 21 runs of the test file on 2026-11-12, both times with
`the lock is free: HeldUnknown` from `hold_lock` in
`tests/support/cmd.rs`. Thirty runs on `main` and thirty on the branch
that saw it passed, so it is rare and shows under load (parallel builds
and a screenshot render ran at the same time).

## Problem

The test takes `hold_lock` on the same collection again for each of its
five commands. The lock is a file lock on an open file description. The
likely cause, read in the code and not proven by an experiment: another
test thread forks a child (a pty or `pre_exec` spawn) while this thread
holds the descriptor. The child keeps a copy until its `exec` closes it,
so after `_lock` drops the lock stays held for that moment, and
`lock.rs` reports `HeldUnknown` because the holder record is gone. The
library tests avoid the same race with `crate::lock::retry_while_held`;
`hold_lock` has no retry.

## Suggested fix

Retry `antiq::lock::acquire` in `hold_lock` while it reports the lock as
held, bounded the way `retry_while_held` is (that function is
`pub(crate)`, so the test support needs its own loop or a public test
helper). Confirm the cause first by running the test in a loop with a
thread that spawns children in parallel.
