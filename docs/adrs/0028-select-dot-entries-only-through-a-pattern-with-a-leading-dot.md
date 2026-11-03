# 28. Select dot entries only through a pattern with a leading dot

Date: 2026-11-03

## Status

Accepted

## Context

Most glob matchers let `*` match names that start with a dot, so the
default pattern would select entries such as `.claude` or `.git` as
pieces.

## Decision

An entry whose name starts with a dot is a piece only when a pattern
names it with a literal leading dot in that path component, such as
`.claude`, `.*` or `projects/.*`. The default `*` selects no dot
entries. `.antiq/` is never a piece.

## Consequences

None further.
