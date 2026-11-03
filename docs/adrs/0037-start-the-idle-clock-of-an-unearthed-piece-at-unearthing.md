# 37. Start the idle clock of an unearthed piece at unearthing

Date: 2026-11-03

## Status

Accepted

## Context

Unearthing restores the files' modification times, so an unearthed
piece counts as idle at once. With scheduled runs it would be archived
again within minutes.

## Decision

For an unearthed piece the idle clock starts at the later of its
newest file modification time and the time it was unearthed. Other
pieces keep the rule of ADR 0013.

## Consequences

None further.
