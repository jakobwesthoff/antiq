# 76. Order the pieces of find by their fuzzy score

Date: 2026-11-23

## Status

Accepted

## Context

`find` listed the pieces that match a query by path. A piece whose path
fits the query closely stood among pieces that match only with letters
spread over the path, and could be listed far down.

## Decision

`find` lists the matching pieces by the score of the fuzzy match, best
first. The versions of a piece stay together, newest first. Pieces with
an equal score come by path. A query that scores every match alike, such
as none or only negated words, lists by path.

A query of several words is scored as the fuzzy matcher scores it: each
word on its own against the path, the scores added. antiq adds no rule
on top.

The candidates of an `ambiguous_reference` come by the same rule, best
score of the word against the escaped path first and by path among equal
scores. The terminal table, the `candidate:` lines and `error.candidates`
all use that order.

The output carries no mark or separator for the ranking.

The ranking is the fuzzy matcher's own, without grouping rules of antiq,
for all of antiq's filtered output.

## Consequences

A query that starts inside a word or at a camelCase boundary gets a
smaller bonus than letters at word starts. Such a piece can rank below a
scattered match: for `scan`, `tools/oldScan` scores 89 and
`archive/rescan-logs` scores 76, below `clients/scala-notes` at 103.
