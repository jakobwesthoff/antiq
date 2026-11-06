# 63. Show messages as blocks on a terminal and as plain lines on a pipe

Date: 2026-11-05

## Status

Accepted

## Context

Errors, warnings and notes were written as plain lines without
structure. Commands have to be usable from scripts (ADR 0020), so the
lines scripts read must stay as they are.

## Decision

Messages to a person are blocks of four kinds: error, warning, info and
hint. They are written once, in one module, and every command uses it.
A terminal shows a block with a badge on its own line, a coloured bar and,
for an error, the labelled sections "How to fix it" and "What antiq was
doing". A hint and the dry-run footer are one line with the text beside the
badge.

A stream is rich when it is a terminal, decided for stdout and stderr
separately. A rich stream gets blocks, symbols, info, hints and the
dry-run tag and footer. A stream that is not a terminal gets plain
lines: `error:` and `warning:` on stderr, and the `Would ...` lines of a
dry run on stdout. Info and hint blocks are not written to it.

An info or hint follows the stream of what it belongs to. One that
belongs to a command's result goes to stdout and is dropped when stdout
is not rich. One that belongs to a failure or a warning goes to stderr
and is dropped when stderr is not rich.

Colour uses the named ANSI colours only, so the terminal's theme decides
the shades. Environment variables that force colour on a stream that is
not a terminal do not make it rich.

## Consequences

A test that wants the rich form runs the binary on a pseudo-terminal.
