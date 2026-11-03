---
title: "Define the v1 scope and architecture of antiq"
kind: investigation
component: project
status: needs-discussion
origin: idea
tags: [architecture]
---
# Define the v1 scope and architecture of antiq

antiq (working name timecapsule until 2026-11-02) replaces the two
playground scripts `~/playground/archive.sh` and
`~/playground/timecapsule-sync.sh` with a Rust tool that solves the use
case more generically. The idea came up on 2026-11-02 while finishing
the scripts' archive index and test backfill, in the dotfiles todo vault;
the todo moved here when the repository was created the same day. It holds
the braindump and the answers given so far, until the v1 spec and the
ADRs carry them.

## Context

What the scripts do today:

- `archive.sh` archives each playground directory whose files are all older
  than 14 days into `archive/<name>.tar.xz`, validates the archive before
  deleting the original, and resolves name collisions by comparing content.
- Every archive has a member listing in `archive/index/<name>.tar.xz.lst`;
  `reindex` backfills missing listings.
- `retrieve` restores an archive through an fzf picker whose preview reads
  the listing; `list` filters archives by name.
- `timecapsule-sync.sh` mirrors the playground onto a NAS with rsync over
  SSH, reads the NAS settings from a local config file, and stops to ask
  before a sync would delete more than 20 % of the mirror.

## The idea

- **One tool, several folders.** Manage one directory, or several configured
  ones, the way the playground is managed today, on one or more machines.
- **Rules in configuration.** After how many days something is archived, and
  which packaging and compression it gets.
- **Packaging providers.** Several formats behind one interface: tar, zip,
  zstd, xz and others. The first one is what exists today.
- **Console output** that says more than the scripts do; maybe a TUI, maybe
  not.
- **Search and selection.** The picker and the index move into the tool, so
  archive contents can be searched.
- **Sync to a backup store**, as the mirror onto the NAS does today.
- **Tiers.** For example: older than 30 days goes into a packed archive;
  older than 120 days lives only on the network store. Retrieve looks there
  too and fetches the archive automatically once the store is reachable.
- **Storage abstraction.** Stores could also be external, in some cloud.
  First a basic design, later more variants.
- **Naming.** Folder and archive naming kept generic or left to the user;
  possibly a ULID prefix that encodes the date.

## Decisions

Settled decisions live in ADR 0002 to ADR 0057 and in
`docs/specs/spec-v1.md`. What stays here shapes the remaining work:

- 2026-11-03, user: the remote-only tier (ADR 0007) is part of the
  first version.
- 2026-11-03, user: the old `.tar.xz` archives (313 on 2026-11-02,
  15 GB, all paired with a listing) are recompressed once with zstd,
  outside antiq, and then registered with `antiq import` (ADR 0019,
  ADR 0055); antiq reads no xz.
- 2026-11-03, user: until implementation starts, ADRs may be rewritten
  in place.
- 2026-11-02, user: the design goes in this order, each step open to
  change later: a rudimentary CLI first to get a feeling for what is
  needed, then the business-logic parts, then the extension points
  (ADR 0006), then onwards from there.
- 2026-11-02, user: things to address or think about later become todos
  as they come up, even if the outcome may be not to do them.

## Design draft and review

A design draft of v1 and its adversarial review (three reviewers, 67
findings, 2026-11-02) are in the session logbook at
`~/playground/2026_11_02_antiq-logbook/`:
`plans/design-draft-v2.md` (the draft with every fix the reviewers and
the lead agreed on), `plans/adversarial-ledger.md` (each finding with
evidence and outcome) and `plans/open-questions.md` (the questions and
the user's answers of 2026-11-03). The agreed fixes, the answers
and the research choices are in ADR 0030 to ADR 0053 and the spec.

A second adversarial review (2026-11-03) of the spec together with all
ADRs (Fable, Opus, Sonnet and Haiku, 85 findings) is in
`plans/adversarial-ledger-2.md` of the same logbook; its fixes and the
user's answers (remote-only tier after 180 days, the brake only on
deletions without a per-item check) are in the spec and the ADRs.

## Open questions

- **Several machines, live folders.** Bringing one live folder together
  across machines stays out of v1 (ADR 0008); a shared remote for
  archives is in (ADR 0017).
