---
title: "A search word with a precomposed accent misses a decomposed piece path"
kind: bug
component: lib
horizon: next
origin: review
tags: [ux]
---
# A search word with a precomposed accent misses a decomposed piece path

Found in the 2026-11-11 plan review of the `list` highlighting. A search
word typed with a precomposed accent (`café`, é = U+00E9) does not match
a piece path stored in decomposed form (`cafe` + U+0301, NFD). `list`
and every command that takes a search-word piece reference then miss the
piece; typing the word without the accent works.

## Problem

`src/query.rs`, `Query::parse` and `Query::score`, hand the query and
the path to nucleo 0.5.0 unchanged. nucleo treats the haystack as
graphemes and its smart normalisation only lets a plain letter match an
accented one, so `é` in the query has nothing to match in `e` + U+0301.
Established on 2026-11-11 with a probe against nucleo 0.5.0 and the
pattern settings of `Query` (`CaseMatching::Smart`,
`Normalization::Smart`):

| Query | Path | Match |
|---|---|---|
| `café` (U+00E9) | `café` (U+00E9) | yes |
| `café` (U+00E9) | `cafe` + U+0301 | no |
| `cafe` | `cafe` + U+0301 | yes |
| `cafe` | `café` (U+00E9) | yes |

## Impact

A folder whose name came in as NFD, as names from HFS+ volumes and some
applications do, cannot be found by typing its name with the accent.

## Suggested fix

Bring query and path to one normal form (NFC) before matching. The
highlight positions of `Query::matched` count characters of the path as
shown, so a normalised haystack needs its positions mapped back onto the
original characters.
