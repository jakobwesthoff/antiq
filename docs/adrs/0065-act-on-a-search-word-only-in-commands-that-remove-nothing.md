# 65. Act on a search word only in commands that remove nothing

Date: 2026-11-06

## Status

Accepted

## Context

A piece reference that is not the path of a candidate is a search word.
Matched fuzzily, it used to act when it fit exactly one candidate. A
misspelled word could then unearth a version, which removes it (ADR 0061),
or delete one with `forget`.

## Decision

`unearth` and `forget` act only on the exact path of a candidate. The
fuzzy search still runs, and a word that matches exactly one candidate
is the error `inexact_reference`, which lists that candidate and does
nothing. The person repeats the command with the path. The reason is that
the piece is then named explicitly while the search stays.

A word that matches several candidates stays `ambiguous_reference`, and a
word that matches none stays `unknown_reference`. `archive`, `pin`,
`unpin`, `fork` and `rebuild` remove nothing and keep acting on a word
that matches exactly one candidate.

## Consequences

None further.
