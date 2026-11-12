# 59. Repair interrupted runs at the start of every locked run

Date: 2026-11-04

## Status

Accepted

Amended by [73. Keep a README in .antiq and write it again whenever it differs](0073-keep-a-readme-in-antiq-and-write-it-again-whenever-it-differs.md)

## Context

A run can stop at any point and leave staged files, folders in `trash/`
or `doomed/`, an unearth that stopped before its version was deleted, or
empty store folders. The next run has to deal with them.

## Decision

There is no `reconcile` command and no state file: everything antiq
needs follows from the folders, the files of the versions and the
control files. Every locked run, `push` included, first runs six repairs
in a fixed order and lists them in a dry run. A repair that fails is a
warning naming the path and does not fail the run.

1. Delete the sidecars of an archive whose step 4 was interrupted.
2. Delete what `.antiq/tmp/` holds.
3. Delete every tree in `doomed/` without a comparison, because it was checked whole before any deletion began.
4. Rename every tree in `trash/` back to its live path without a comparison, because no deletion has touched it, or move it to `recovered/` when that is not possible.
5. Finish an unearth that died: delete what is left of the version named in `.antiq-restored` and rewrite the marker to the time alone.
6. Remove empty store folders.

## Consequences

None further.
