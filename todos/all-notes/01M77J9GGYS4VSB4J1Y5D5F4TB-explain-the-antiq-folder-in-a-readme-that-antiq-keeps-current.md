---
title: "Explain the .antiq folder in a README that antiq keeps current"
kind: feature
component: lib
origin: request
---
# Explain the .antiq folder in a README that antiq keeps current

Someone who finds a collection's `.antiq/` folder, in the collection or in
the copy that `push` makes on the target, cannot tell what it is or how to
get the folders back. An AI agent that comes across it does not know that
`antiq skill` explains the tool. ntropy solves the same for its vaults with
a README seeded by `init` (ntropy `src/vault/seed.rs`, `VAULT_README`).

## Decisions

- 2026-11-12, user: the README lives at `.antiq/README.md`, not in the
  collection root (that folder is the user's).
- 2026-11-12, user: `init` writes it, and every run that takes the lock
  (archive, unearth, fork, forget, rebuild, import, pin, unpin, push)
  writes it again when it is missing or differs from the text of the
  antiq that runs, so existing collections get it and it stays current.
  antiq owns the file; changes by hand are overwritten.
- 2026-11-12, user: content: what the folder is and that nothing in it is
  changed by hand; what antiq is and where to get it (website
  <https://antiq.westhoffswelt.de>, `cargo install antiq`,
  `brew install jakobwesthoff/tap/antiq`); the main commands (`status`,
  `find`, `grep`, `unearth`); how to read a version without antiq
  (`zstd`, `tar`, `shasum`); one sentence for AI agents: run `antiq skill`
  and follow it.
- 2026-11-12, user: implemented through the conductor and released as 1.5.0.

## Open questions

- Whether a dry run lists the README write as an action, and how a
  failure to write it is reported (a warning, as for other repairs, seems
  to fit).
- How the store layout, `status`, `push` and the repairs treat the new
  file (it must not look like an unparsable file in the store).
