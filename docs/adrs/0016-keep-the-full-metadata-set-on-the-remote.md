# 16. Keep the full metadata set on the remote

Date: 2026-11-02

## Status

Accepted

## Context

In the remote-only tier only the local metadata knows that an archive
exists (ADR 0007). Losing the marker directory would lose track of those
archives, and another machine could not find them.

## Decision

The remote holds the full metadata set, like the local storage, with one
metadata file per ID next to the archives. Attaching a remote copies its
metadata locally; archives are fetched when they are needed.

## Consequences

The index can be rebuilt from the remote alone.
