# 74. Show the archive size on the finished line and in the summary of an archive

Date: 2026-11-12

## Status

Accepted

## Context

A run of `archive` or `import` on a terminal does not tell how large the
archive of a piece is. The finished line of ADR 0064 shows the size of
the content only.

## Decision

The finished line of a piece that `archive` or `import` archived shows the
content size, an arrow and the size of the archive, then in dim the share of
the content size that the archive kept: `412 MB → 96 MB  23 %`. The arrow is
`->` in the ASCII symbol set. The arrow reads as "became" without a column
header. The share shows at a glance how well a piece compressed: media stays
near 100 %.

The share is the archive size over the content size, as a whole percent. It
can pass 100 % and is then shown as it is. It reads `<1 %` when it rounds
to 0 for an archive that is not empty, and is left out when the content size
is 0. A piece that `archive` lists at the end without a progress event has
the same sizes on its line.

The line shows the first form that fits:

1. the arrow and the share, when the whole name of the longest piece of the
   run fits beside them;
2. the arrow without the share, when the whole name fits;
3. the arrow without the share and the names shortened in the middle, to as
   few as 16 columns;
4. the content size alone, from 50 columns;
5. no size.

The shortening of a name hides the part that tells two pieces apart, so the
share goes first. Dropping the share costs less.

The closing line of `archive` ends with the total size of the archives of
the pieces archived in the run, `1.3 GB stored`. `stored` marks the total as
the compressed size at little cost in width. The closing line of `import`
shows the content size, the archive size and the share of the version in
parentheses: `Imported scanner as version 2026-11-12T155819 (412 MB → 96 MB,
23 %)`. A closing line wider than the terminal is written without the total
and without the parentheses.

The live cards, the lines of `archive` on a pipe, the JSON output and the
finished lines of `unearth`, `fork`, `push` and `rebuild` stay as they are.

## Consequences

None further.
