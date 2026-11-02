# 15. Keep local storage fixed and put remotes behind one trait

Date: 2026-11-02

## Status

Accepted

## Context

Archives live locally and on backup stores of different kinds. Separate
traits for stores, mirrors and locations would create many combinations
to wire up and test.

## Decision

Local storage in the marker directory is fixed, not a trait. Everything
remote sits behind one Remote trait, and each implementation provides
every operation, backing up live folders included.

The first version has a path remote (a local path, such as an
SMB-mounted NAS) and an SSH remote that uses rsync. A managed folder has
one remote in the first version; the config holds a list, so more can
follow.

## Consequences

None further.
