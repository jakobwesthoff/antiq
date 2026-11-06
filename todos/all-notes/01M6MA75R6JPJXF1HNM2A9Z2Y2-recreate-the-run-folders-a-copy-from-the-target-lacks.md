---
title: "Recreate the run folders a copy from the target lacks"
kind: bug
component: project
origin: implementation
tags: [architecture]
---
# Recreate the run folders a copy from the target lacks

Found by the E4 scenario tests on main 717245d. `push` mirrors the collection without `.antiq/tmp/`, `.antiq/trash/`, `.antiq/doomed/` and the lock (spec push step 6). A collection copied back from the target with `rsync -aH` therefore lacks them, and the spec (Scope) promises that copying it back restores the live folders and the versions. In such a copy `unearth`, `pin`, `rebuild` and `archive <piece>` fail: `cannot stat <copy>/.antiq/tmp: No such file or directory`, and `archive` also `cannot open <copy>/.antiq/trash`. `list`, `grep`, `status`, `archive -n`, `forget`, `unpin` and `push` work. `fork` and `import` were not probed.

## Fix

A locked run creates the run folders that are missing (`.antiq/tmp/`, `trash/`, `doomed/`, and any other the mirror leaves out) before the repairs, flushed like the rest of the layout; a dry run plans their creation without writing. Tests first: one per command that writes (archive, unearth, fork, import, pin, unpin, rebuild, forget, push) in a collection without those folders. The spec says what a run does when they are missing.

## Relations

- Part of: [Rework antiq v1 to the simplified concept](01M6KFHS2FVQ9APAGQ947FP3CM-rework-antiq-v1-to-the-simplified-concept.md)
