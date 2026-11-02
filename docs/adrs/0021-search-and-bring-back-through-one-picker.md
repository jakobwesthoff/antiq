# 21. Search and bring back through one picker

Date: 2026-11-02

## Status

Accepted

## Context

`archive.sh` lists archives by name and restores one through an fzf
picker. Listing and searching are the same operation, with or without a
query.

## Decision

One command covers listing and searching. Interactively it opens a
picker over all objects, from which an object can be brought back or
acted on otherwise. With `--json` it prints the matches.

## Consequences

None further.
