---
title: "Archive pieces that share hard-linked files on one worker"
kind: improvement
component: lib
impact: low
origin: review
---
# Archive pieces that share hard-linked files on one worker

`archive` runs pieces in parallel. When two pieces hold names of the same
inode (`cp -al`, pnpm or git stores that hard-link), deleting the first
piece's tree changes the shared inode's link count and so its ctime. The
second piece, between its verify and its re-walk, then sees a change and
comes back as unearthed, or is skipped as `differs` when the delete hits
during its verify. Nothing is lost, but the result depends on timing.
Found in the opus diff review of I19 (finding R5), 2026-11-04; the spec
documents the behaviour.

## Suggested fix

Group candidates whose trees share a (device, inode) with a link count
above one and run each group on one worker in path order. The step 0 walk
already records link count, device and inode, so the grouping needs
either a pre-walk or a scheduling decision deferred until the walk.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
