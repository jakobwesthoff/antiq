---
title: "Console errors repeat their causes and omit the candidates"
kind: bug
component: project
origin: implementation
tags: [architecture]
---
# Console errors repeat their causes and omit the candidates

Found by the E3 scenario tests on main 6bca1bf. `antiq unearth scan` against three store pieces matching `scan` prints on stderr:

    error: cannot find the piece: `scan` matches 3 pieces: cannot find the piece: `scan` matches 3 pieces: `scan` matches 3 pieces
      while attempting to unearth the version

The spec ("Piece references", and unearth) says an ambiguous word is an error that lists the candidates. `--json` lists them under `error.candidates`; the console names none and repeats the same sentence three times. The pin and archive binary tests skip the text side of this case.

## Problem

1. The console chain prints every cause even when an error's `Display` already includes its source's text, so the message repeats. The same was seen for `PinError` around a marker error (plan 8.7, Z note from C3), and archive's own `chain()` had to drop such repeats (C4 diff review AR4); rebuild has a `chain()` of its own that may do the same.
2. The console form of an ambiguous reference does not list the candidates.

## Fix

Tests first, through the binary in text mode: an ambiguous word for every command that takes a piece reference (archive, unearth, fork once merged, forget, pin, unpin, rebuild) prints each candidate path once on stderr and no sentence twice; the pin-over-marker error prints its message once. Then one rule for the console chain (no cause whose text the line before it already shows, or errors whose `Display` does not embed their source), applied in the shared renderer, and the per-command `chain()` helpers of archive and rebuild either use it or are shown not to need it. The JSON output stays as it is.

## Relations

- Part of: [Rework antiq v1 to the simplified concept](01M6KFHS2FVQ9APAGQ947FP3CM-rework-antiq-v1-to-the-simplified-concept.md)
