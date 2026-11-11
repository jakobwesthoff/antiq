---
title: "Decide whether piece-matching patterns get named levels"
kind: investigation
component: lib
status: needs-discussion
horizon: someday
origin: discussion
tags: [config]
---
# Decide whether piece-matching patterns get named levels

Which directories of a collection count as pieces is configured with
glob patterns such as `*`, `*/*/*` or `**/*` (business-logic design round,
2026-11-02). An idea from that round: named levels such as
`[group]/[year]/*`, whose names would be stored in each version's
listing header, so a piece carries "group" and "year" as fields.

## Decisions

- 2026-11-02, user: plain globs first; named levels possibly later. The
  user doubts they are needed, since the piece's relative path is stored
  in the listing header anyway.

## Revisit when

`find` or `grep` needs to filter or group archives by parts of their
original path.
