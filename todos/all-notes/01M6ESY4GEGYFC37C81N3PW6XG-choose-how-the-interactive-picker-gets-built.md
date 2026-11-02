---
title: "Choose how the interactive picker gets built"
kind: investigation
component: cli
status: needs-discussion
origin: discussion
tags: [ux, dependencies]
---
# Choose how the interactive picker gets built

The CLI gets one interactive picker over the archives (decided in the
first CLI design round, 2026-11-02): from it the user finds an archive and
acts on it. How the picker is built is open.

## Options

- Copy ntropy's picker: `src/bin/ntropy/run/picker/` in
  `~/Development/github/jakobwesthoff/ntropy` (`mod.rs`, `state.rs`,
  `layout.rs`, about 3100 lines including tests), its own rendering over
  `nucleo` and `crossterm`, one generic `pick` function, state unit-tested
  without a terminal.
- Extract that picker into a crate of its own that both ntropy and this
  tool use.
- Write a new picker, for example on `ratatui`.
- Use an existing picker library.

## Open questions

- Which actions the picker offers besides restoring, and whether that
  changes which option fits.
