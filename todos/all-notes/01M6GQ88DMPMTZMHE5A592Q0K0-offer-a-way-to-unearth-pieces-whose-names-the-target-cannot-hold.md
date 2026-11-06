---
title: "Offer a way to unearth pieces whose names the target cannot hold"
kind: feature
component: lib
horizon: someday
origin: discussion
tags: [ux]
---
# Offer a way to unearth pieces whose names the target cannot hold

In v1, unearthing refuses a piece whose member names the target cannot
hold: names that are not valid UTF-8 on macOS, names over the length
limit, and names that collide under the target's case or Unicode rules
(ADR 0054). Such a piece can only be unearthed on a machine whose rules
fit it. The silent case is the dangerous one: on case-insensitive APFS,
`README` and `Readme`, or `café` in NFC and NFD, extract as one file
with exit 0 (`docs/research/names.md`).

## Decisions

- 2026-11-03, user: refuse with a list in v1, and file a todo to handle
  this later and give users a solution, if it turns out to be a real
  problem or use case.
- 2026-11-03, user: Linux machines may archive into a shared remote in
  v1 ("possibly"). v1 now has one machine; a version reaches another
  machine by copying its files from the target.

## Facts

- No such name in 57,062 live names in `~/playground` and 123,220 names
  in the 313 archive listings (2026-11-03); all come from macOS.
- Options already evaluated in `docs/research/names.md`: extraction with
  escaped names plus a mapping file (changes what an unearth restores
  and needs a mapping-aware strict verify), git-style settings that fix
  one side, GNU tar's `--transform` as a manual rename.

## Revisit when

A user hits the refusal, or a version made on Linux is copied from the target to a macOS machine
and holds names that differ only in case or normalisation.
