# 64. Show the progress of a long run as cards drawn in place on a terminal

Date: 2026-11-05

## Status

Accepted

## Context

`archive`, `push`, `unearth`, `fork` and `rebuild` printed nothing until they ended. The
lines scripts read must stay as they are (ADR 0020).

## Decision

A real run of these commands on a rich stdout (ADR 0063) shows one card
for each piece that is working. A card has two lines: the piece, its size
and the current operation, then a bar with a percentage and a count. A
piece that has finished leaves one line with its result and its time, and
a refused, skipped or unfinished piece keeps its reasons under that line.
The finished lines scroll away like any output. Below them stay the cards
of the running pieces and one line `○ N waiting`.

A `push` is one run of rsync over the whole collection, so it has one card
instead of one for each piece. The card counts the entries rsync lists
against the total rsync reports, shows no percentage, and is removed when
rsync ends. The table of the pieces and the closing line follow it.

One formatter writes the card and the finished line, so the lines on the
screen during the run and the lines that remain after it look alike.

The bar of an operation follows the work in bytes where bytes are what
takes time, and counts the content bytes of the piece's files, the unit of
the size on the card: packing counts the content bytes among those the
compressor has consumed, and the verifying of an archive the content bytes
of the members as the archive hands them out plus those of the files of the
folder as they are read, with the count under the bar halved. The verifying of a
rebuild counts the content bytes of the members of its single read of the archive; without a
listing the size is unknown and the count shows the bytes checked so far. A count under the bar shows numbers only in
units that match the card (its size, files, entries). The compressor works
behind the writer, so the bytes handed to it reach the total long before it
has compressed them; the bytes it has consumed are what the bar follows.

The region of cards is redrawn in place with the carriage return, cursor
up and erase to the end of the screen, and nothing else. antiq switches no
terminal mode, uses no alternate screen and no scroll region, and hides
no cursor. A terminal whose `TERM` is unset or `dumb` gets each finished
line when its piece ends and no cards.

A pipe, `--json` and a dry run keep their output unchanged. Warnings and
notes still follow the result.

`push` on a terminal that moves the cursor follows its one run of the
mirror. The run lists every entry rsync looks at, with its length, and
reports progress. A terminal whose `TERM` is unset or `dumb` gets the
plain listing, and `push` lists its pieces at the end.

## Consequences

The size of the terminal is read at each redraw. A redraw after a change
of size waits until the size has stayed the same for the length of one
redraw interval, so that a window being dragged is not drawn for a width
that is already gone. Then
the region is erased and drawn again for the new size.

How far the cursor climbs to the first row of the region depends on what
the terminal does to old lines when its width changes. A terminal that
lays them out again gives a line wider than the window as many rows as it
needs, so the cursor climbs the sum of those rows. Any other terminal
leaves each line on one row, and the cursor climbs one row per line. The
environment names the terminals that lay lines out again: VTE
(`VTE_VERSION`), kitty (`KITTY_WINDOW_ID`), Alacritty (`ALACRITTY_LOG`),
tmux (`TMUX`, with a `TERM` of `tmux` or `screen`), and iTerm2,
Terminal.app, WezTerm and ghostty (`TERM_PROGRAM`). The `TERM` has to be
one that terminal exports, such as `xterm-kitty` for kitty. A terminal
that is not named is treated as one that does not lay lines out again,
because a fragment of an old card that stays on the screen costs less
than a finished line that is erased. The same holds when the variables
name two terminals at once, when the `TERM` does not fit the one named,
or when `XTERM_VERSION`, `STY`, `INSIDE_EMACS` or `VIM_TERMINAL` names
xterm, GNU screen, Emacs or Vim: a terminal passes its variables on to
the programs started from it, including another terminal.

A finished line is never erased as long as the width read at a redraw is
the width the terminal has when it parses the redraw. If the window is
wider by then, the climb can be too long and reach finished lines. Where a
terminal does not join the rows of a line again when the window gets wider
(kitty), a window that was narrowed and widened again leaves a fragment of
the region's top on the screen. Rows that scrolled into the scrollback
because the window got shorter stay there. When a run ends or a panic
halts it while the size has not held for a redraw interval, the region is
taken off with a climb of one row per line, which can leave the top of the
old region behind but reaches no finished line.
