---
title: "Rank contiguous matches above scattered fuzzy matches in find"
kind: improvement
component: lib
origin: incident
tags: [ux]
---
# Rank contiguous matches above scattered fuzzy matches in find

`antiq find <query>` (alias `list`) shows every piece whose path the
fuzzy query matches, in path order. A piece whose path holds the query
as one contiguous run of letters is listed among pieces that match only
because the letters appear scattered across the path, often far down
the list. Seen in real use on 2026-11-23: a five-letter query matched
about twenty pieces, and the one piece whose name contains the word was
listed near the end, after pieces that matched it only letter by letter
across unrelated words.

## Problem

`src/commands/list.rs`, `gather`, sorts the piece folders by path and
uses `Query::score` (`src/query.rs`) only as a filter: a score of
`Some(_)` keeps the piece, the score itself is dropped. Nothing ranks
the matches, although the doc comment on `Query` says search-word
references and `find` "rank alike". Read in the code, 2026-11-23.

## Goal

Pieces whose path contains the query as a contiguous substring come
first, pieces that match only as a scattered fuzzy match come after.
The versions of one piece stay together and newest first, as now.

## Proposal

Order the matching pieces in two tiers before the versions are
gathered: contiguous matches, then the rest. A contiguous match uses the
same smart case and normalisation as the fuzzy match, so `find` does not
start to treat case or accents differently between the tiers. nucleo's
pattern syntax has a substring atom (`'word`), which may give the
contiguous check without a second matching implementation.

## Open questions

- Order within a tier: keep path order, or sort by the nucleo score.
  Path order keeps the output predictable; the score would put a match
  at a word start above one inside a word.
- A query of several words: does a piece count as contiguous when every
  word appears contiguously, or only when all of them do in a row?
- `src/reference.rs`, `resolve_piece`, lists the candidates of an
  ambiguous piece reference in path order as well. Whether that list
  gets the same tiers.

## Done when

- A test in `src/commands/list.rs` stores pieces where one path holds
  the query contiguously and others match it only scattered, and the
  contiguous piece comes first.
- The versions of each piece remain grouped and newest first.
