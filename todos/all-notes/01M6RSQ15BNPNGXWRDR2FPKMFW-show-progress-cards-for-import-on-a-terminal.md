---
title: "Show progress cards for import on a terminal"
kind: feature
component: cli
origin: request
tags: [ux, migration]
---
# Show progress cards for import on a terminal

`antiq import` prints one line, `imported <piece>: version <stamp>`, also
on a terminal, while `archive`, `push`, `unearth`, `fork` and `rebuild`
show progress cards there ("Progress cards" in `docs/specs/spec-v1.md`,
ADR 0064). Bringing the 313 archives of an old archive folder (15 GB of
`tar.xz`) into a new collection one `import` at a time on 2026-11-06 left
the person watching a silent terminal for minutes per archive, and they
asked for the card that `archive` shows.

## Approach

`import` packs, verifies and stores with steps 2 to 4 of `archive`
(spec, "import"), so its card can be the one of `archive` with the
operations `packing`, `verifying` and `storing`, and the closing line of
the plain output as its result. The plain line stays for a dry run,
`--json` and a stdout that is not a terminal.

The spec's "Progress cards" list of commands and the "import" section
change with it.

## Where the code stands (2026-11-06)

- `src/commands/import.rs` stores through the same `StoreContext` as
  `archive`, but hands it `Tracker::silent()`, so the counters the cards
  read exist and only stay unobserved.
- `archive` has `archive_observed` beside `archive`, both on
  `run_archive(.., observer: Option<&dyn Observer>)`
  (`src/commands/archive/mod.rs`); `import` needs the same pair, with a
  tracker built from the observer in place of the silent one.
- `src/cli/cmd/import.rs` prints the line directly; `src/cli/cmd/archive.rs`
  wraps the call in `progress::watch` and prints the card result through
  `progress::archive_pieces` and `archive_summary`
  (`src/cli/cmd/progress.rs`), which `import` can mirror for one piece.
