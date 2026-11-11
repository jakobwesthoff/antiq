---
title: "The closest piece suggestion ignores Unicode composition"
kind: bug
component: lib
impact: low
horizon: next
origin: review
tags: [ux]
---
# The closest piece suggestion ignores Unicode composition

Found in the 2026-11-11 plan review of the fix for accented search words.
When a search word matches no piece, antiq suggests the piece paths
closest to it by edit distance. A path stored decomposed (NFD, `cafe` +
U+0301) counts as one or two edits away from the same name typed
precomposed (`café`, U+00E9), so a short name can miss its suggestion.

## Problem

`src/reference.rs`, `closest_pieces`, lowercases reference and path and
measures `strsim::osa_distance` on the characters as stored, with an
allowance of a third of the reference's length and at least 2. Read in
the code on 2026-11-11, not reproduced.

## Suggested fix

Compose both sides (NFC, `unicode-normalization` is already a
dependency) before measuring the distance.

## Relations

- Relates to: [A search word with a precomposed accent misses a decomposed piece path](01M752K7H5KN59DCWPXPH25WHC-a-search-word-with-a-precomposed-accent-misses-a-decomposed-piece-path.md), same normal-form gap in the search-word matching
