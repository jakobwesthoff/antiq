# 67. Embed the emblem renderings and choose one by cell shape and glyph set

Date: 2026-11-06

## Status

Accepted

## Context

`antiq --version` shows Archie's emblem on a terminal. A picture drawn
for one cell shape looks squeezed or stretched in another, and fonts differ
in the width of their cells from 0.39 to 0.54 of their height. Sextants
come out as boxes of a missing glyph where neither the terminal nor its
font draws them. No escape sequence tells which glyphs a terminal can show.

## Decision

The emblem exists as one file for each cell shape and glyph set,
`assets/emblem/terminal/archie-r<ratio>.<sextant|block>.ansi`, where the
ratio is the cell width over the cell height in hundredths. `just
emblem-ansi` generates them and they are in git.

`build.rs` lists the directory and embeds every file in the executable. A
file whose name does not follow the pattern fails the build. A new
rendering is a new file in the directory and needs no change in the code.

The cell shape is the window's size in pixels over its size in cells, from
`TIOCGWINSZ`. The rendering with the nearest ratio is taken, the smaller on
a tie. A window that reports no pixels gets 0.52, chafa's default shape.

Sextants are used on a terminal known to draw them, block elements on
every other. The terminal is known from the environment variables that
name it and from their versions. Inside tmux, those variables describe the
terminal tmux was started from, so one `tmux display-message` asks tmux for
its client's answer to XTVERSION and for the size of the client's cells in
pixels. The pixel size is used when the window reports none. tmux not
answering within half a second, or at all, counts as unknown: block
elements and 0.52. GNU screen and Zellij get block elements. The terminal
itself is never asked with an escape sequence.

No emblem is shown when stdout is not a terminal, when colour is off, when
`COLORTERM` does not name 24-bit colour, under `--json`, or when the window
is too small. The plain version line is printed then.

## Consequences

The `tmux` call of the version screen writes no run log, as the command has
no collection to write one to.
