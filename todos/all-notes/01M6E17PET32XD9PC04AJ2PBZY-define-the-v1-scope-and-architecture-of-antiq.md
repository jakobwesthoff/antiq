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

Settled decisions live in ADR 0002 to ADR 0010 and in
`docs/specs/spec-v1.md`. What stays here shapes the remaining work:

- 2026-11-02, user: the tool gets its own repository and is named antiq
  (ADR 0010).
- 2026-11-02, user: the remote-only tier (ADR 0007) is wanted, but may
  not be part of the first version.
- 2026-11-02, user: the design goes in this order, each step open to
  change later: a rudimentary CLI first to get a feeling for what is
  needed, then the business-logic parts, then the extension points
  (ADR 0006), then onwards from there.

## Open questions

- **Several machines.** Probably configurable, so both are possible:
  separate targets per machine or per folder, or one playground on several
  machines brought together. Bringing them together might merge only the
  index, and a directory that exists only in the index is fetched from the
  store when needed. The same folder worked on in two places at once would
  need a merge, something git-like, which may be too complex to include.
- **Import.** Whether the tool gets a structured import for existing
  folders, or for existing archives described by configuration, so that
  people with existing stores can bring them in. ADR 0009 leaves this
  open; it does not have to be tailored to the playground's current
  format.
- **Migration** of the existing `.tar.xz` archives and their listings
  (313 on 2026-11-02): a one-off step, possibly built on that import
  feature.
- **A TUI** or console output only.
- **Naming scheme** for folders and archives, and whether it is generic or
  user-configured.
