---
title: "Decide whether unit-matching patterns get named levels"
kind: investigation
component: lib
status: needs-discussion
horizon: someday
origin: discussion
tags: [config]
---
# Decide whether unit-matching patterns get named levels

Which directories of a managed folder count as units is configured with
glob patterns such as `*`, `*/*/*` or `**/*` (business-logic design round,
2026-11-02). An idea from that round: named levels such as
`[group]/[year]/*`, whose names would be stored in each archive's
metadata, so a unit carries "group" and "year" as fields.

## Decisions

- 2026-11-02, user: plain globs first; named levels possibly later. The
  user doubts they are needed, since the unit's relative path is stored
  in the metadata anyway.

## Revisit when

Search or the picker needs to filter or group archives by parts of their
original path.
