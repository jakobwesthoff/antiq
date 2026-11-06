# 56. Keep the interface to console output

Date: 2026-11-03

## Status

Accepted

## Context

antiq could present its state in a full-screen terminal interface or as
console output. The output has to work in terminals of 80 columns as
well as in wide ones.

## Decision

antiq writes console output only. Messages in it are blocks on a
terminal and plain lines on a pipe (ADR 0063).

`status` lists only the pieces that have a state (`due`, `soon`,
`pinned`, `refused`), since a list of every piece each time says little.
Plain live folders are counted in the header. The output adapts to the
terminal's width: a wide layout where there is room, since it reads more
easily, and a narrow one at 80 columns, in which paths that do not fit
are shortened in the middle.

## Consequences

None further.
