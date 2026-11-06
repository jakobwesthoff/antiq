# 64. Show the progress of a long run as cards drawn in place on a terminal

Date: 2026-11-05

## Status

Accepted

## Context

`archive`, `push` and `unearth` printed nothing until they ended. The
lines scripts read must stay as they are (ADR 0020).

## Decision

A real run of these commands on a rich stdout (ADR 0063) shows one card
for each piece that is working. A card has two lines: the piece, its size
and the current operation, then a bar with a percentage and a count. A
piece that has finished leaves one line with its result and its time, and
a refused, skipped or unfinished piece keeps its reasons under that line.
The finished lines scroll away like any output. Below them stay the cards
of the running pieces and one line `○ N waiting`.

One formatter writes the card and the finished line, so the lines on the
screen during the run and the lines that remain after it look alike.

The region of cards is redrawn in place with the carriage return, cursor
up and erase to the end of the screen, and nothing else. antiq switches no
terminal mode, uses no alternate screen and no scroll region, and hides
no cursor. A terminal whose `TERM` is unset or `dumb` gets each finished
line when its piece ends and no cards.

A pipe, `--json` and a dry run keep their output unchanged. Warnings and
notes still follow the result.

`push` on a terminal that moves the cursor runs the mirror once as a dry
run before the real run, to learn the files and bytes of each piece. The
extra scan is accepted. A terminal whose `TERM` is unset or `dumb` gets no
dry run, and `push` lists its pieces at the end.

## Consequences

`push` on a terminal that moves the cursor scans the source twice and
makes one more ssh connection to an ssh target.

The size of the terminal is read at each redraw. A resize during a run
can leave fragments of old cards on the screen. A finished line is never
erased.
