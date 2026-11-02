---
title: "Allow switching the selection strategy of a managed folder that has archives"
kind: feature
component: lib
status: needs-discussion
horizon: someday
origin: discussion
tags: [config]
---
# Allow switching the selection strategy of a managed folder that has archives

A managed folder decides which directories are units through a selection
strategy (for example "shallowest match wins") named in its config. In
the first version the strategy is chosen when the folder is initialised
and cannot change afterwards (business-logic design round, 2026-11-02).
Switching it later can change what a unit is: a path that was archived
as one unit may become two, or a unit may now sit inside another.

## Open questions

- What a switch has to show before it happens (units that would split,
  merge or nest).
- What happens to existing archives whose path no longer is a unit.

## Revisit when

Someone needs a different strategy on a folder that already has archives.
