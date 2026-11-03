# 14. Identify every folder by a ULID that survives archiving cycles

Date: 2026-11-02

## Status

Accepted

## Context

The same folder can be archived, brought back, changed and archived
again. Its archives need an identity that does not depend on its name.

## Decision

Each unit is an object with a ULID. The ULID's time is the folder's
creation time, not the archiving time, because the archiving time
changes with every cycle of archiving and bringing back. How the
creation time is determined is decided separately.

The ID stays the same when a folder is brought back and archived again.
The new archive replaces the old one as the ID's current archive. The
index tracks a brought-back folder by its path.

Forking a brought-back folder gives it a new ID, whose time is the time
of the fork. Archiving it then creates a new object and leaves the old
archive untouched.

## Consequences

None further.
