---
title: "Support several remotes per managed folder"
kind: feature
component: lib
horizon: someday
origin: discussion
tags: [config, architecture]
---
# Support several remotes per managed folder

A managed folder has exactly one remote in the first version (business
logic design round, 2026-11-02). Several remotes per folder, along the
lines of git remotes, may come later: for example a NAS and a cloud store
for the same folder.

## Decisions

- 2026-11-02, user: one remote in v1; several later, like git remotes.
- 2026-11-02, user: the config shape allows a list of remotes from the
  start, and v1 accepts exactly one entry, so adding more needs no config
  migration.

## Revisit when

A second backup target for the same managed folder is wanted.
