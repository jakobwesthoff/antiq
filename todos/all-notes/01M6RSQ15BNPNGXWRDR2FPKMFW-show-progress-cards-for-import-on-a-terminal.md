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
