---
title: "List the ignored paths that archive -n would delete unpacked"
kind: improvement
component: lib
origin: review
tags: [ux]
---
# List the ignored paths that archive -n would delete unpacked

Found in the adversarial review of the agent skill on 2026-11-11. Paths
that match `ignore` in `.antiq/config.toml` or an `.antiqignore` file in
a piece are not packed, and `archive` deletes them with the folder. The
dry run of `archive` does not name them, so neither a person nor an
agent can see before a run that, for example, a `node_modules/` or a
local database file will be gone without a copy.

## Goal

`archive -n` names, per piece it would archive, every entry it would
leave out, the way `import -n` already does.

## Proposal

- Text output: after each `Would archive <piece>` line, one `Would leave
  out <path>` line per ignored entry, a folder once with a trailing `/`,
  as `import -n` prints them (spec-v1.md, section "import", the `-n`
  bullet). A terminal shows them the way it shows the dry run of
  `import`.
- JSON: the action `{"kind": "archive", "piece": ...}` gains
  `left_out`, the list of paths the version would leave out, as the
  `import` action carries it (spec-v1.md, "JSON output").
- Today the action carries only `piece`:
  `src/commands/archive/report.rs`, `ArchiveAction::Archive`; the plan is
  built in `src/commands/archive/mod.rs` (around line 167), and the test
  near line 1992 pins `{"kind": "archive", "piece": "due"}`. The walk the
  dry run already does for the judgement knows the ignored entries.
- Update the spec (sections "archive" and "JSON output"), the README
  where it shows the dry run, the CHANGELOG, and the agent skill
  (`skills/antiq/SKILL.md`), which tells agents that `archive -n` does
  not list ignored paths.

## Open questions

- Whether `.antiq-restored` at a piece's top is listed as left out, as
  `import -n` lists it.
