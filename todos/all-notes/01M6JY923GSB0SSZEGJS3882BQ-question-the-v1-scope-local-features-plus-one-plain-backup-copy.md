---
title: "Question the v1 scope: local features plus one plain backup copy"
kind: investigation
component: project
status: needs-discussion
impact: high
origin: discussion
tags: [architecture]
---
# Question the v1 scope: local features plus one plain backup copy

On 2026-11-04, during the implementation run, the user questioned
whether v1 is far too complicated for the problem: tier-based sync,
handling several machines and their conflicts, and the state tracking
that comes with them may be overkill and add a great deal of
complexity. All implementation work was paused for this discussion.

## What the user wants (as stated)

- All the local features.
- Syncing with one external backup target: one more duplicate of
  everything, so that anything can be got back.
- Data stored so that it can be reached file by file without antiq,
  with a layout a person can understand: which file lies where. That
  would make a restore from the backup simple when something goes
  wrong, since it is clear which files to copy where.
- The expectation that this removes much of the state tracking,
  locking and synchronisation problems ("what if it is here but not
  there, or a specific process failed").
- Readiness to remove or throw away parts already built.

## State of the work at the time (2026-11-04, main `727553c`)

- Built and merged:
  - all local commands (init, status, archive, unearth, find with
    the picker and contents view, pin, fork, import, reconcile, the
    start-of-run repairs, resolve and re-archiving);
  - path and ssh remotes;
  - the versioned remote metadata with the commit protocol, claims and
    cleanup (I27, I30);
  - sync with uploads and live backups (I30, I32);
  - unearth through the remote (I34);
  - the brake, the tombstone and prune (I35).
- Built, not merged: I33, the tiers that drop a local archive once its
  remote copy is confirmed (branch `i33`, under review).
- Not built:
  - I31 version conflicts and divergence (paused right after its first
    red test);
  - I37 resolving diverged folders and superseded archives;
  - I44 `forget` (a design draft was started on branch `i44`).
- Unaffected local work: I41 progress rows, I42 decision fixes (in
  progress), I43 shell integration, I45 fast picker search, I38 trait
  ADR, I39 acceptance.

## Questions to settle

- Is v1 single-machine, with the remote only as a backup target? What
  happens if a second machine syncs to the same target?
- Do tiers go (local archives always kept), or stay optional?
- The layout on the remote and locally: content-addressed packages
  with versioned `meta.N` documents today, against a plain,
  human-readable layout. What must a restore without antiq look like?
- Which built parts are removed, which simplified, which kept.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
