# 11. Mark managed folders with an .antiq directory

Date: 2026-11-02

## Status

Accepted

## Context

Commands need to know which managed folder they act on, without a global
list of folders.

## Decision

A managed folder is marked by a directory `.antiq/` at its top, which
`init` creates. Every command searches upwards from the current
directory for it, as git does for `.git/`; `-C <path>` starts the search
at another path. Outside a managed folder, commands fail.

The marker directory holds the folder's config, the lock, the staging
directories `tmp/`, `trash/`, `doomed/` and `recovered/`, and `pieces/`
with the versions of its pieces.

## Consequences

None further.
