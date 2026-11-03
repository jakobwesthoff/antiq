# 36. Ask before replacing an archive when content would be lost

Date: 2026-11-03

## Status

Accepted

## Context

Archiving an unearthed piece again replaces its archive (ADR 0014).
`archive.sh` asks first when the replacement would discard content kept
nowhere else.

## Decision

A replacement that loses nothing happens without asking. When files
were removed or their content or metadata changed, antiq shows what
would be lost and asks: replace, fork or skip. Without a terminal it
skips and lists the piece as needing a decision (ADR 0025). A
collection can allow replacing without asking.

## Consequences

None further.
