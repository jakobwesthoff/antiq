# 20. Print data to stdout, messages to stderr and offer --json on every command

Date: 2026-11-02

## Status

Accepted

## Context

Commands have to be usable from scripts.

## Decision

Every command takes `--json`. Data goes to stdout, messages to stderr.
Info and hints that belong to a command's result, and the lines of a
dry run, are data of the run and go to stdout (ADR 0063).
Under `--json` an error is data too: its JSON goes to stdout. No command
asks a question.

## Consequences

None further.
