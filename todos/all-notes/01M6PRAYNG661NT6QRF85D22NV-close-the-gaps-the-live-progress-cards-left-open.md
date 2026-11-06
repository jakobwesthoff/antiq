---
title: "Close the gaps the live progress cards left open"
kind: improvement
component: cli
origin: implementation
tags: [ux, testing]
---
# Close the gaps the live progress cards left open

The live progress cards of `archive`, `push` and `unearth` (ADR 0064,
spec section "Progress cards") landed with a few known gaps. Each was noted in
the implementation plan or the diff review of 2026-11-05 and left out
of that unit of work. They are independent of each other and can be
taken one at a time.

## Decisions

- These are features, not leftovers: all of them are implemented now,
  before the README and docs rework, so its captures show the final
  output (Jakob, chat, 2026-11-05).
- Packing counts bytes instead of files, with no extra label for the
  tail (lead, 2026-11-05). What still runs after the last byte is
  measured first; if that tail stays long, the lead decides again.

## A resize does not reflow the region

The driver reads the terminal size for every frame but does not handle
`SIGWINCH`. After a resize, fragments of old cards can stay in the
scrollback; a finished line is never erased. Handling `SIGWINCH` and
redrawing the region at the new width would remove the fragments.

## fork and rebuild show no progress

`bring_back_to` takes a `Tracker`; `unearth` passes a real one, `fork`
and the other callers pass `Tracker::silent()`. `rebuild` reports no
events either. Both can run long on a large piece.

## The push dry run over ssh is unmeasured

On a terminal that can redraw, `push` runs an rsync dry run of the same
mirror before the real one, to know the totals for the cards. Against a
local target that costs one extra scan. Over ssh it costs a second
remote file listing, which has not been measured. If it is slow, the
cards could start without totals instead.

## Tests that depend on compressed sizes

A scenario test in `tests/scenarios/push.rs` compared the size of a
stored archive with a fixed number (453 B). The archive is compressed
from a tree that carries the times of the test run, so its size changed
with the clock and the test failed now and then. It reads the size from
the store now. Other tests may compare compressed sizes the same way;
they should be found and changed to read the size, not assume it.
