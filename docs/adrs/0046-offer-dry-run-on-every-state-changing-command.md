# 46. Offer --dry-run on every state-changing command

Date: 2026-11-03

## Status

Accepted

## Context

Both scripts have `--dry-run`.

## Decision

`init`, `archive`, `unearth`, `fork`, `forget`, `rebuild`, `import`,
`pin`, `unpin` and `push` take `--dry-run`, which prints what the
command would do and changes nothing. A dry run takes no lock. It lists
the repairs of the start of a run first, as actions of kind `repair`.
Under `--json` the plan is JSON.

## Consequences

None further.
