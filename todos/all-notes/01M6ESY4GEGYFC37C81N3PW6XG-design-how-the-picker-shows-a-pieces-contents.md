---
title: "Design how the picker shows a piece's contents"
kind: investigation
component: cli
status: needs-discussion
origin: discussion
tags: [ux]
---
# Design how the picker shows a piece's contents

The `find` picker starts as a copy of ntropy's picker and offers
unearthing, a view of a piece's contents, printing the path, forking and
pinning (ADR 0057). How the contents are shown is open: ntropy's picker
has no preview, and a plain preview pane is not assumed.

## Decisions

- 2026-11-03, user: copy ntropy's picker; actions unearth, contents
  view, print, fork, pin/unpin.
- 2026-11-03, user, on the preview: "not sure we even want a "normal"
  preview pane. I could think of doing that more intelligently for our
  specific usecase somehow, maybe an overlay, or something else like
  tree like integration in the picker or something"

## Findings

From the picker research of 2026-11-02 (session logbook
`~/playground/2026_11_02_antiq-logbook/plans/research/picker.md`):

- ntropy's picker (`src/bin/ntropy/run/picker/` in
  `~/Development/github/jakobwesthoff/ntropy`) is about 3100 lines
  including tests; only `layout.rs` (ntropy's candidate type) and one
  `open_tty()` call are tied to ntropy; it draws on `/dev/tty`
  (ntropy ADR 0036).
- `nucleo-picker` 0.12.2 has no preview and fixed colours (ntropy
  ADR 0027); `skim` 5.9.0 has a preview but pulls 161 to 257 crates; a
  ratatui renderer was rejected in ntropy (ntropy ADR 0069).
- A plain preview pane was estimated at 200 to 350 lines (a guess).

## Open questions

- Overlay, a tree view integrated into the list, a preview pane, or
  something else; which information about a piece it shows (listing,
  state, versions, size).
- Whether to extract a shared picker crate with ntropy once the design
  has settled.
