---
title: "Back up content outside every unit as a root live package"
kind: feature
component: lib
horizon: someday
origin: review
---
# Back up content outside every unit as a root live package

Units are selected by glob patterns (default `*`). Loose files at the top
of a managed folder belong to no unit, so v1 neither archives nor backs
them up; the spec states this. `timecapsule-sync.sh`, which antiq
replaces, mirrored the whole playground, loose files included. A "root"
live package could cover everything outside every unit.

In `~/playground` on 2026-11-02 the only loose entries were `.DS_Store`,
`.gitignore` and the symlinks `archive.sh` and `timecapsule-sync.sh`
into the dotfiles repository.

## Decisions

- 2026-11-03, user: state the gap in the v1 spec, todo for later
  (adversarial review finding F16, question Q9).

## Revisit when

A managed folder holds loose files worth backing up.
