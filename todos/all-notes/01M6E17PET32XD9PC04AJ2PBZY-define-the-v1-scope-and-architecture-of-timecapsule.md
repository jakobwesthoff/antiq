---
title: "Define the v1 scope and architecture of timecapsule"
kind: investigation
component: project
status: needs-discussion
origin: idea
tags: [architecture]
---
# Define the v1 scope and architecture of timecapsule

timecapsule replaces the two playground scripts `~/playground/archive.sh`
and `~/playground/timecapsule-sync.sh` with a Rust tool that solves the
use case more generically. The idea came up on 2026-11-02 while finishing
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

- 2026-11-02, user: the tool gets its own repository,
  `jakobwesthoff/timecapsule`, and is named timecapsule.
- 2026-11-02, user: the first version reaches parity with the scripts
  (archiving by age, xz, index, search and retrieve with a picker, reindex,
  sync to the NAS) and includes tiers from the start.
- 2026-11-02, user: architecture and design come first. Decide up front
  which parts sit behind which traits and interfaces, so packaging formats,
  stores and the other parts can be added later and plug into the same
  engine.
- 2026-11-02, user: in the second tier, the local archive file is deleted
  and only the index and metadata stay local, so search keeps working and
  retrieve fetches the archive from the store when it is reachable.
- 2026-11-02, user: the first version has no syncing of the same folder
  between machines. The design makes sure it can be added later.
- 2026-11-02, user: the existing archives are migrated once; the tool does
  not need to read today's format itself.
- 2026-11-02, user: one crate with a library and a CLI binary; clap and
  anyhow for the CLI, thiserror for the library.

## Open questions

- **Several machines.** Probably configurable, so both are possible:
  separate targets per machine or per folder, or one playground on several
  machines brought together. Bringing them together might merge only the
  index, and a directory that exists only in the index is fetched from the
  store when needed. The same folder worked on in two places at once would
  need a merge, something git-like, which may be too complex to include.
- **Import.** Whether the tool gets a general import for existing folders,
  or for existing archives described by configuration, so that people with
  existing stores can bring them in. It does not have to be tailored to the
  playground's current format.
- **Migration** of the 311 existing `.tar.xz` archives and their listings:
  a one-off step, possibly built on that import feature.
- **A TUI** or console output only.
- **Naming scheme** for folders and archives, and whether it is generic or
  user-configured.
- **Binary name.** `tcap` is preferred, with `capsule`, `tcaps` and
  `timecap` as alternatives; a check whether each name is taken runs
  before one is adopted.
