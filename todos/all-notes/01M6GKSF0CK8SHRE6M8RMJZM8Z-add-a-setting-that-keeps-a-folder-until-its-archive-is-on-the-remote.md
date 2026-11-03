---
title: "Add a setting that keeps a folder until its archive is on the remote"
kind: feature
component: lib
horizon: someday
origin: discussion
tags: [config]
---
# Add a setting that keeps a folder until its archive is on the remote

In v1, archiving removes an idle folder as soon as its archive is stored
and verified locally. The upload to the remote happens at the next sync,
so archiving works while the remote is away. Until that sync, the local
archive is the only copy. An optional config switch could require the
remote copy before the folder is removed.

## Decisions

- 2026-11-03, user: no such switch in v1; file a todo for a future
  feature ("no switch for now, but create a corresponding todo for a
  future feature").
- 2026-11-02, review fix agreed before that: archiving needs only a
  verified local archive; the remote put is pending work for sync.

## Open

- Default when the switch exists (proposed earlier: off).
- What archiving does with an idle folder while the remote is away and
  the switch is on (one option: skip it and report it).

## Revisit when

Losing the only local archive before a sync becomes a real concern, or a
user asks for two copies before removal.
