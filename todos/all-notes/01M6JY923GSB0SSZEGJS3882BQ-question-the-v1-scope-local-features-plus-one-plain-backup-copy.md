---
title: "Question the v1 scope: local features plus one plain backup copy"
kind: investigation
component: project
status: needs-discussion
impact: high
origin: discussion
tags: [architecture]
---
# Question the v1 scope: local features plus one plain backup copy

On 2026-11-04, during the implementation run, the user questioned
whether v1 is far too complicated for the problem. All implementation
work was paused (user order "Stop everything") and the scope is being
redesigned in asqr rounds. This note holds everything decided and
open, so the discussion survives without the conversation.

## The user's motivation (as stated)

- Wanted: all the local features, and syncing to one external backup
  target that holds one more duplicate of everything.
- The data should be reachable file by file without antiq, in a layout
  a person understands, so a restore is "copy these files back".
- Expected gain: far less state tracking, locking and synchronisation
  logic ("what if it is here but not there, or a process failed").
- Parts already built may be removed or thrown away.
- Realistic scenario: one main machine where almost everything happens;
  rarely a second machine needs an archived folder. That machine gets
  it manually from the backup, not through antiq.

## Decided so far (asqr scope rounds 1 to 3, 2026-11-04)

- One machine per backup target. No multi-machine support in v1:
  no version conflicts, no divergence, no claims, no versioned `meta.N`
  documents. May be reconsidered later; the new layout must not block
  that.
- The target marker holds the collection identity and the hostname. A
  target that belongs to another collection is refused (a mirror with
  deletes would otherwise move that collection away). Same collection
  from another host: a warning.
- No tiers: local archives are always kept; the target is an
  additional copy.
- Live backups stay, but as plain folders: the target is a mirror of
  the collection, live folders unpacked (rsync), archives as files.
  Copying the whole target back restores everything.
- Deletes: exact mirror (rsync `--delete`), no trash on the target (the
  user chose this after the `--backup-dir` option was explained).
- A path-based store under `.antiq/` (perhaps `.antiq/pieces/`), with
  archive and listing sharing one file-name prefix; the folder layout
  follows the piece's path.
- Re-archiving the same path is the same piece (a new archive of it).
- Old versions: `find` searches the newest; a key or flag adds all
  versions.

## Decided in round 4 (asqr 01M6K12ZP94T9VNSDT80ZY080W, 2026-11-04)

- `unearth` moves the piece out: after a successful extraction the
  archive is deleted, and the folder is an ordinary folder again.
- `fork` restores a working copy while the archive stays unchanged.
  `--name` is optional; without it the copy goes to the original path.
  Archiving it later gives a new version next to the old one.
- No picker in v1 unless `unearth` turns out uncomfortable without
  one. Every command has JSON output for scripts. `find` goes away.
  `list` and `grep` replace it, and `grep` searches the full paths,
  including the archive folder name and the position.
- No `status` command for now. The user asked what else it could
  show and where `-n` comes from.
- File names: normalize the path into the name and collapse repeated
  `-`.

## Open after round 4 (asqr round 5)

- `list <query>`: is the query fuzzy? Does it show all versions, or a
  count with `--versions` for details, or always all?
- `unearth` should take an unambiguous identifier, not a numbered
  choice. Which one: an exact path, path plus version, or a fuzzy name
  that must match exactly one piece?
- The name of `sync`: it is now a one-way backup to a configured
  target (maybe several targets later) and never fetches anything
  back, so it needs no conflict handling.
- The listing format. The user's idea is that the listing is itself a
  checksum file that standard tools can check, with antiq's metadata in
  comment lines. Verified on 2026-11-04: macOS `shasum -a 256 -c` and
  GNU `sha256sum -c` (coreutils 9.12, also with `--strict`) both skip
  lines that start with `#` and exit 0.
- The checksum: SHA-256 against BLAKE3 (`b3sum` is installed here).
- Two archives of the same path within the same second: rare, but it
  needs a rule.

## Decided in round 5 (asqr 01M6K1PKC40GHH05KFMTS1X80G, 2026-11-04)

- `list [query]`: the query is fuzzy on piece paths; every archived
  version is its own row, the newest first. There is no `--versions`.
- `unearth` takes a path (the newest version, or `--version` with a
  timestamp or a unique prefix of one), or a search word that matches
  exactly one piece; anything else is an error that lists the
  candidates.
- The one-way command is called `backup` for now; the user wavers
  between `push` and `backup` and asked for the lead's opinion. It is
  the only command that talks to the target.
- The listing is a checksum file (`.sha256.zst`) with antiq's metadata
  in `#` comment lines. The user finds the comment format proposed so
  far neither easy to parse nor easy to read, and wants a better one,
  or a middle way.
- Verified on 2026-11-04: a comment after the path on the same line
  does not work. Both `shasum -c` and GNU `sha256sum -c` read the rest
  of the line as part of the file name and fail.
- SHA-256 is the checksum.
- A second archive of the same path within the same second gets a
  counter suffix (`…T090211-2`).
- A small `status` stays and never talks to the target. Wanted in it:
  archive size, pinned folders, folders archived on the next run,
  folders due within a few days, perhaps the last backup time if it is
  stored locally. Pending backup changes belong to `backup -n`, not to
  `status`. The user asked for an output proposal.
- The tantivy index is likely not needed at first. `grep` searches the
  real listings; its speed is still to be measured. The index research
  stays in `docs/research/search-index.md`.

## Earlier open points (round 4, now mostly decided above), asqr 01M6K12ZP94T9VNSDT80ZY080W)

- `unearth` semantics. The user called "the archive stays and
  re-archiving makes a new version" wrong. Options put:
  - checkout: re-archiving replaces the archive;
  - move-out: the archive is deleted after unearthing;
  - keep plus a new version.
- `fork`: a working copy while the archive stays unchanged, and whether
  it needs `--to`. The user's earlier reading: fork restores while the
  archive keeps its current version; re-archiving then gives a separate
  entry.
- File names and checksums: the user wants the normalized path in the
  file name (anything outside readable characters becomes `-`).
  Proposed: `<normalized path>--<archive time>.tar.zst`, the same prefix
  `.listing.zst` (the listing already holds per-file SHA-256), and a
  `.tar.zst.sha256` sidecar for the archive in `shasum -c` format. The
  user doubts that the archive-time stamp adds much, but accepts it as
  extra information.
- The CLI and UX as a whole. The user listed the use cases:
  - archive everything due;
  - find by approximate name;
  - list a piece's versions;
  - restore by name;
  - restore a given version;
  - a working copy while the archive stays;
  - delete for good;
  - find the archives that hold a given file;
  - status;
  - sync to the backup.
  Proposed: one command per case with table output (`list`, `list
  --versions`, `unearth`, `fork --to`, `forget`, `grep`, `status`,
  `sync`). Whether the interactive picker is needed in v1 is open
  (proposal: only on ambiguity, `-i` for the full picker), and so is
  whether `status` stays (proposal: a slim one).
- The picker's contents search (I45): the user prefers a precomputed
  on-disk index used as a cache (as in their project Squirly, which
  uses tantivy), queried in the background, and content search kept
  separate from the name search. A study measures tantivy and an
  alternative (branch `search-index`, `docs/research/search-index.md`).
  First figures: a word index of 15 M paths takes 14 s to build, 166 MB
  on disk, 44 MB RSS and 3-37 ms per query, but substring queries reach
  290 ms; a trigram index takes 716 MB and answers substrings in
  0.2-2 ms.

## State of the work at the pause (main `df62f4b`)

- Merged:
  - the local commands, with the picker and its contents view;
  - path and ssh remotes;
  - versioned remote metadata with the commit protocol, claims and
    cleanup (I27, I30);
  - sync with uploads and compressed live packages (I30, I32);
  - unearth through the remote (I34);
  - the brake, the tombstone and prune (I35);
  - re-archiving and resolve (I26).
- Branches kept, unmerged:
  - `i33`: tiers, rebased, under an opus review with two minor findings
    open;
  - `i42`: decision fixes, 2 of 6 commits;
  - `i31`: version conflicts, a half-done commit saved as a patch in
    the logbook;
  - `i44`: a draft ADR for `forget`, uncommitted.
- Under the decisions above, much of I27, I30, I32, I34 and I35 and all
  of I31, I33 and I37 would be removed or rewritten. The local commands
  stay.

## Next step

After round 4: the lead writes a concept document (what stays, what
goes, the store layout, the `sync` flow, the CLI per use case, and how
the existing store and the built code are migrated or removed), the
user reviews it, and only then are rework items planned.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
