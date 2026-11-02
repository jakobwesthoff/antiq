# 5. Use typed errors in the library and anyhow in the binary

Date: 2026-11-02

## Status

Accepted

## Context

Errors arise in the library and are reported by the binary.

## Decision

The library defines its errors as types with `thiserror`. The binary
propagates errors with `anyhow` and adds context to each.

## Consequences

None further.
