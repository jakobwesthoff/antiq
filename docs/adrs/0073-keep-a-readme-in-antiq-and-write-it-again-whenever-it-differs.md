# 73. Keep a README in .antiq and write it again whenever it differs

Date: 2026-11-12

## Status

Accepted

Amends [59. Repair interrupted runs at the start of every locked run](0059-repair-interrupted-runs-at-the-start-of-every-locked-run.md)

## Context

Someone who finds the `.antiq/` folder of a collection, in the
collection or in the copy that `push` makes on the target, cannot tell
what it is or how to get the folders back.

## Decision

`.antiq/README.md` explains the folder: what it is, what antiq is and
where to get it, the main commands, how to read a version without antiq,
and, in one sentence for AI agents, that `antiq skill` explains the tool.
It lives in `.antiq/` because the collection root is the user's folder.
Its text is compiled into the binary and is the same for every
collection of one antiq.

antiq owns the file. `init` writes it, and every locked run, `push`
included, writes it again when it is missing or differs from the text of
the antiq that runs. A change made by hand is replaced.

The write is a seventh repair at the start of a run, after the six of
ADR 0059. A dry run lists it as an action of kind `repair`. An entry at
that name that is no regular file stays as it is and is a warning, and
so is a write that fails. `push` mirrors the file like `config.toml`.

## Consequences

A collection that an older antiq made gets the README with its first
locked run, which reports it as a repair.

When two versions of antiq open one collection, each replaces the
README with its own text.
