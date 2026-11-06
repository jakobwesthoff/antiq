# 32. Archive with a verified local version and mirror it at the next push

Date: 2026-11-03

## Status

Accepted

## Context

`archive.sh` archives without the NAS. If archiving waited for the
target, nothing could be archived while the NAS is away.

## Decision

Archiving needs only the local store. `archive` packs a piece, verifies
the archive, moves the version into the store, renames the folder into
`trash/`, walks it again and compares it with the record of the verify,
and only then moves it to `doomed/` and deletes it. A difference gives
the tree back. `push` mirrors the version later. There is no setting
that keeps a folder until its version is on the target.

A piece with an entry that carries an immutable or append-only flag, or
with a directory, the top's parent included, that the user does not own
and cannot write, is not archived, since it could not be moved or
deleted. A directory the user owns but has made unwritable is not an
obstacle, since removal gives itself owner access.

## Consequences

Until the next push, the local version is the only copy.
