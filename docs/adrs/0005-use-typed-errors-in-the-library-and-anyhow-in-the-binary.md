# 5. Use typed errors in the library and anyhow in the binary

Date: 2026-11-02

## Status

Accepted

## Context

Errors arise in the library and are reported by the binary.

## Decision

The library defines its errors as types with `thiserror` and derives
`miette::Diagnostic` on each of them. The message of an error is its
`thiserror` text, the fix is the `help` of the diagnostic, and a stable
`code` names the errors that scripts or readers treat differently. The
binary propagates errors with `anyhow`, adds context to each, and renders
the diagnostics; it uses miette without the `fancy` feature.

## Consequences

None further.
