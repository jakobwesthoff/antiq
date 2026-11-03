# 46. Offer --dry-run on every state-changing command

Date: 2026-11-03

## Status

Accepted

## Context

Both scripts have `--dry-run`.

## Decision

Every command that changes state takes `--dry-run`, which prints what
it would do and changes nothing. Under `--json` the plan is JSON.

## Consequences

None further.
