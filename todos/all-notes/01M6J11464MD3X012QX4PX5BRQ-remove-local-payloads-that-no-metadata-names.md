---
title: "Remove local payloads that no metadata names"
kind: bug
component: lib
impact: low
origin: implementation
---
# Remove local payloads that no metadata names

`archive` stores a new piece's payload before it records the piece's
metadata, and `import` copies the payload into place before it records
metadata as well. A crash between those two steps leaves a payload in
the local store that no metadata names. Nothing removes it, so it costs
disk space until someone deletes it by hand. The folder or the input
file is still there, so the data itself is not lost.

Found while planning I19 (`archive`), 2026-11-04.

## Suggested fix

A start-of-run repair, or a step of `reconcile`, removes a local payload
whose ID no local metadata names. Before removing it, the repair checks
that the remote's metadata does not name that ID either: until a pull
has run, a payload fetched for another machine's object could look
unnamed. It is still open whether the repair runs only after a
successful pull, or only for IDs minted on this machine.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
