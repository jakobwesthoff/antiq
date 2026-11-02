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

## Findings

Research on 2026-11-02 (report in the session logbook,
`~/playground/2026_11_02_antiq-logbook/plans/research/picker.md`):

- ntropy's picker is about 3100 lines including tests. Only `layout.rs`
  (ntropy's candidate type) and one `open_tty()` call are tied to ntropy;
  it draws on `/dev/tty` (ntropy ADR 0036). It has no preview pane.
- `nucleo-picker` 0.12.2 has no preview pane and hardcoded colours, the
  reason ntropy dropped it (ntropy ADR 0027).
- `skim` 5.9.0 has a preview but is built around the `sk` command line,
  pulls 161 to 257 crates and draws on stderr.
- A ratatui renderer was tried in ntropy and rejected (ntropy ADR 0069).
- The research recommends copying ntropy's picker, adding a preview pane
  (estimated 200 to 350 lines, a guess), and extracting a shared crate
  once the preview has settled.

## Open questions

- Which actions the picker offers besides restoring, and whether that
  changes which option fits.
