---
title: "Count the two columns of the ASCII done mark in the layout of finished lines"
kind: bug
component: cli
impact: low
origin: implementation
tags: [ux]
---
# Count the two columns of the ASCII done mark in the layout of finished lines

With the ASCII symbol set, the mark of a finished line is `ok`, two
columns wide, but the layout of the finished lines counts the mark as
one column. Every ASCII finished line of `unearth`, `fork`, `push` and
`rebuild` is therefore one column wider than the layout planned, so at
the exact terminal width it can wrap.

## Problem

`src/cli/console/cards.rs`, `Layout::of`, reserves `4` columns for the
indent, the mark and its blank (`let fixed = 4 + 2 + result + ...`),
which holds for the one-column `✔` but not for the ASCII `done: "ok"` in
`src/cli/console/symbols.rs`. Found while planning the archive size on
the finished lines (2026-11-12) and confirmed by reading the code, not
reproduced on a terminal yet. The `archive` and `import` runs count the
real mark width since that work; the other commands still use the old
count.

## Suggested fix

Take the width of `symbols.done` (and of `error` and `warning`, which
are one column in both sets today) in `Layout::of` instead of the fixed
`4`, as the archive and import layout does, and add a test at the exact
width with the ASCII symbols for one of these commands.
