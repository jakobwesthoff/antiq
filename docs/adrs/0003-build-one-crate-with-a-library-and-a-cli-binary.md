# 3. Build one crate with a library and a CLI binary

Date: 2026-11-02

## Status

Accepted

## Context

The command-line interface and the logic behind it need a place in the
project layout.

## Decision

The project is one crate with a library and a binary. The library
(`src/lib.rs`) holds the logic. The binary (`src/main.rs`) parses
arguments and reports errors.

## Consequences

None further.
