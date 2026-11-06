---
title: "Decide whether the index belongs in a SQLite database"
kind: investigation
component: lib
status: needs-discussion
origin: discussion
tags: [architecture]
---
# Decide whether the index belongs in a SQLite database

The spec keeps the index as plain files, but its file format was never discussed as its own question. This todo asks whether the index data antiq stores locally would be better kept in a SQLite database than in one JSON file per object.

## Current state

- Spec "Storage": the index is derived from metadata and listings, kept as plain files, rebuildable, and behind a module interface. Read-only commands that find it missing compute it in memory and do not write it. No ADR records why plain files were chosen.
- I15 (index and piece references) implements it this way: `.antiq/index/<ULID>.json` holds the member paths of the object's current listing together with that listing's file name. Which objects exist, their paths, names, sizes and states are computed from the metadata and the tracking entries on every call and are not stored. Search runs over these files with `nucleo` (U1: member paths of the listings are searched too).
- Other local state also lives as one file per object: tracking entries in `.antiq/tracking/<ulid>.json`, live manifests in `.antiq/tracking/<ulid>.manifest.json`. Metadata and listings under `.antiq/objects/` mirror the remote layout.
- No SQLite crate is a dependency today.

## Questions

- Does a database pay off for search over all member paths of all listings (number of objects and members, time of a `find` with and without a warm cache)?
- What does it change for crash safety, rebuild (`reconcile`), the read-only rule (read-only commands write nothing) and the lock?
- Should only the index move, or also tracking entries and manifests?
- Which crate (for example `rusqlite` with bundled SQLite) and what it costs in build time and binary size.

## Timing

I15 builds the JSON member cache now. The index sits behind a module interface, so a later switch stays inside `src/index.rs`; the decision is cheapest before more callers (I18 `find`, I36 picker) are built on the files directly.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
