# 25. Answer pending decisions with a resolve command

Date: 2026-11-03

## Status

Accepted

## Context

A run without a terminal cannot ask. Decisions such as a conflict with
another machine's version or a replacement that would lose content stay
pending, and the run lists them as needing a decision.

## Decision

`antiq resolve` goes through the pending decisions interactively.
`antiq resolve <piece>` with `--mine`, `--theirs`, `--fork`, `--replace`
or `--skip` answers one decision from a script. `archive` and `sync`
still ask in place when a terminal is available.

## Consequences

None further.
