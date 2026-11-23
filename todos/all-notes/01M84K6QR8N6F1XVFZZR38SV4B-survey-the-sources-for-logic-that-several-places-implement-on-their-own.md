---
title: "Survey the sources for logic that several places implement on their own"
kind: investigation
component: lib
horizon: next
origin: discussion
tags: [architecture]
---
# Survey the sources for logic that several places implement on their own

Find the places in `src/` where one job (filtering, ordering, matching,
deciding a state, formatting one kind of value) is done by more than one
piece of code, so that each job has one implementation and the callers
only differ in how they write the result. Planned for after release
1.8.0.

## Background

While ranking `find` by nucleo's score for release 1.8.0
(2026-11-23), "filter the pieces
by the query and put them in order" turned out to exist three times on
`aa01f83`: `gather` in `src/commands/list.rs`, `resolve_piece` in
`src/reference.rs`, and `piece_table` in `src/cli/unresolved.rs`, which
derived its own order from the rows. The third copy got the order wrong:
the "closest pieces" table of an unknown reference showed path order
although the spec promises nearest first. That work moves the job into
one `Query::rank` in `src/query.rs`.

Not every repetition is duplicated logic. The `sort_by(path)` calls in
`src/archive/extract.rs`, `src/archive/verify.rs`,
`src/versions/store.rs` and `src/cli/cmd/status.rs` sort different
types for different purposes and are one line each; a survey that only
counts such calls says nothing. Read on 2026-11-23.

## What to look at

- The same decision or rule written out in several modules, where one
  copy can drift from the others, as the table order did.
- Code in `src/cli/` that recomputes what the library already returned
  instead of rendering it.
- Several writers (terminal, pipe, JSON) that each derive data instead
  of rendering one shared result.

## Done when

Each finding is either fixed through its own todo (one per job, filed
with the code places and what drifted or could drift) or recorded as
acceptable with the reason. The findings are summarised for the user.
