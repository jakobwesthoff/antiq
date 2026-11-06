# 14. Identify every piece by its path

Date: 2026-11-02

## Status

Accepted

## Context

The same folder can be archived, brought back, changed and archived
again. Its versions need an identity.

## Decision

A piece is identified by its path relative to the collection. Archiving
the same path again adds a version of the same piece. A live folder
whose path the collection's volume treats as the same name (case,
Unicode normalization) is the same piece and is archived into the store
folder with the stored spelling.

A version brought back with `fork --name <new path>` and archived later
is a new piece.

## Consequences

None further.
