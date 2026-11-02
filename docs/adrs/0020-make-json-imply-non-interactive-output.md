# 20. Make --json imply non-interactive output

Date: 2026-11-02

## Status

Accepted

## Context

Commands have to be usable from scripts, and some of them open an
interactive picker.

## Decision

Every command takes `--json`. With `--json`, a command never opens the
picker or asks a question.

Without it, a command is interactive when a controlling terminal is
available, also when its standard output is redirected. A flag prints
the item picked in the picker, so the result can be used in a shell. The
first version has no separate flag for non-interactive mode.

## Consequences

None further.
