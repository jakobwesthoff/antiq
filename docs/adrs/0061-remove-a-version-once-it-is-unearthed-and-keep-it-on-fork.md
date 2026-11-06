# 61. Remove a version once it is unearthed and keep it on fork

Date: 2026-11-04

## Status

Accepted

## Context

An unearthed folder is an ordinary live folder. The version it came from
can stay in the store or go.

## Decision

`unearth` removes the version once the folder is complete and verified.
Archiving the folder later creates a new version, changed or not; there
is no reclaim. `fork` extracts a working copy and keeps the version. It
writes `.antiq-restored` with the time alone, so the repairs never treat
it as an unearth.

## Consequences

A crash between the rename of the folder and the deletion of the version
leaves both. The repairs finish the unearth (ADR 0059).
