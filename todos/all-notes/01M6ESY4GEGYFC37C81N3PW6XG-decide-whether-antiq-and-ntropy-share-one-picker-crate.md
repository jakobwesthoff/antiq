---
title: "Decide whether antiq and ntropy share one picker crate"
kind: investigation
component: cli
horizon: someday
origin: discussion
tags: [ux, architecture]
---
# Decide whether antiq and ntropy share one picker crate

antiq's `find` picker starts as a copy of ntropy's picker (ADR 0057).
Once antiq's picker has settled, including its contents tree, it may be
worth extracting one picker crate both projects use instead of keeping
two copies apart.

## Decisions

- 2026-11-03, user: copy ntropy's picker; actions unearth, contents
  view, print, fork, pin/unpin.
- 2026-11-03, user: the contents view is a tree inside the list (chosen
  from rendered pictures over an overlay and a side pane); it is in
  ADR 0057 and spec "Command line", and work item I40 of the v1 plan
  builds it.

## Findings

From the picker research of 2026-11-02 (session logbook
`~/playground/2026_11_02_antiq-logbook/plans/research/picker.md`):

- ntropy's picker (`src/bin/ntropy/run/picker/` in
  `~/Development/github/jakobwesthoff/ntropy`) is about 3100 lines
  including tests; only `layout.rs` (ntropy's candidate type) and one
  `open_tty()` call are tied to ntropy; it draws on `/dev/tty`
  (ntropy ADR 0036).

## Revisit when

antiq's picker with its contents tree (I36, I40) has landed.
