# 20. Print data to stdout, messages to stderr and offer --json on every command

Date: 2026-11-02

## Status

Accepted

## Context

Commands have to be usable from scripts.

## Decision

Every command takes `--json`. Data goes to stdout, messages to stderr.
Under `--json` an error is data too: its JSON goes to stdout. No command
asks a question.

## Consequences

None further.
