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

## Decided in round 6 (asqr 01M6K26GZBX00PR52WMGPP4YB5, 2026-11-04)

- The one-way command is `push`. It makes the target an exact mirror,
  and "backup" would promise to keep deleted data.
- Listing comment format: the readable, aligned variant, but with the
  exact byte size, not a rounded one. Columns get fixed generous
  widths so they line up, and the mode may be octal (the user leans to
  octal). The user asked why aligned columns should be harder to parse
  than the column variant; answered in round 7.
- `status`: no last push. The user wants the layout discussed at the
  start of the project: every piece one row, sorted (for example by
  path), a coloured state marker in the first column, then the state
  details. No grouping by state. An ANSI mock-up was requested.
- `grep`: measure an embedded search, using ripgrep's library crates
  (or a comparable Rust crate) inside antiq, not the system `grep`.

## Decided in round 7 (asqr 01M6K2MRX043QTZTSTCP22HKR9, 2026-11-04)

- Listing comment format as shown: `# antiq listing 1` header lines
  (piece, archived, archive file), then per entry an aligned comment
  line `# <type> <octal mode> <UTC time with T> <exact bytes or -> <path>`
  before the `<sha256>  <path>` line; directories end in `/`, links
  show `-> target`. Fixed generous widths; fields contain no spaces; the
  path comes last.
- `status` shows only pieces with a state (due, soon, pinned), one row
  each, sorted by path, the state coloured in the first column; plain
  live folders are only counted. No `--all`.
- "soon" means due within 3 days.

## Decided in round 8 (asqr 01M6K3PRWHFAZ9SE96KXKQQ435, 2026-11-04)

- grep engine: `regex::bytes` over each whole decoded listing. The user
  asked how complex the multithreaded code is and how much memory a
  search takes; answered in round 9.
- Listings stay `.sha256.zst`.
- The store root is `.antiq/pieces/`.
- grep output: the user wants to see several output variants before
  choosing.
- `forget` removes one specific version only. Removing all versions of
  a piece at once is at most an explicit exception (perhaps with
  `--force`).
- `import` stays, but changes: it should import a folder that lies
  outside the collection (not an archive file), so that it becomes an
  archived piece with the timestamps it has out there. The intended
  migration of old tar archives is to unpack each with its old
  timestamps somewhere temporary and run `import` on the folder. The
  user asked for ideas and another round.

## Decided in round 9 (asqr 01M6K440FS9VRZGA2HZ96AV5VR, 2026-11-04)

- grep runs on all cores but two.
- grep output: a table for now. The user asked for the lead's ideal
  proposal based on how the command is used, with the match highlighted
  in the output.
- `forget <piece> <version>` removes exactly one version (`-n` to see
  it first). All versions at once only with `--all-versions --force`.
  The next `push` mirrors the deletion.
- `import <folder> [--as <path>]` packs a folder from outside directly
  as an archived version. The version time is the newest mtime in the
  folder, and a parameter can set it explicitly. The source folder is
  kept.

## Decided in round 10 (asqr 01M6K4M2AHBZM65HTFZBA83ECB, 2026-11-04)

- grep output on a terminal follows the lead's proposal:
  - grouped by piece, with one line per in-piece path and the versions
    that hold it ("all 3 versions" or the dates);
  - the match highlighted, as ripgrep does;
  - a closing hint line that says how to continue with `unearth`.
- In a pipe, grep prints one tab-separated line per hit and version:
  `piece  version  path`. `--json` gives everything.
- Open (round 11, asqr 01M6K4TY1JS8DQZ1QA1NKBFJ6W): a match that lies
  wholly in the piece path. Proposal: one row for the piece, "piece
  name matches", with versions and the file count, and no file list.
  The user is also asked which topics still need a round before the
  concept.

## Decided in round 11 (asqr 01M6K4TY1JS8DQZ1QA1NKBFJ6W, 2026-11-04)

- A grep match that lies wholly in the piece path gives one row for
  the piece, "piece name matches", with its versions and file count, and
  no file list. A match in the in-piece path, or one that spans the
  boundary (`scanner/src`), lists the file. In a pipe, a name match is
  `piece  version  -`, one line per version.
- The user wanted to talk through the `archive` flow, the `push` flow
  and the configuration. Round 12 (asqr 01M6K4ZR68X7M4PW3ZF0Y9KW82)
  proposed them but was withdrawn unanswered, because the user then
  asked for the concept document first, with these points as its open
  questions.

## NEXT STEP (user order, 2026-11-04): write the concept document

The user asked for the concept document now, written as proposed. The
remaining open points, and any that come up while writing, are
discussed afterwards. Write it as a design document in the repository,
for example `docs/concept-v1-simplified.md`, and commit it. The user
reviews it in asqr. Include:
- what stays, what goes, what is rewritten;
- the store layout;
- the listing format;
- every command with its output: `init`, `archive`, `unearth`, `fork`,
  `forget`, `import`, `list`, `grep`, `status`, `pin`/`unpin`, `push`,
  `reconcile` if still needed;
- the `push` flow and the target marker;
- the configuration;
- the repairs and lock that remain;
- what happens to the built code and the paused branches;
- an open-questions section.

Proposals from the withdrawn round 12, to put in the concept as
proposals with open questions:
- **archive flow:**
  1. pieces are the folders matched by `patterns` (default `*`, each
     folder directly under the collection) minus `exclude`;
  2. due when the newest mtime, ignoring `ignore` lines, is older than
     `idle_days`; pinned folders (`.antiq-pin`) never;
  3. pack into `.antiq/tmp/` (`.tar.zst`, `.sha256.zst` listing,
     `.tar.zst.sha256`);
  4. verify by reading the whole archive against the listing and the
     folder; a folder that changes meanwhile is skipped;
  5. move into `.antiq/pieces/<path>/` and flush;
  6. then delete the folder through a trash inside `.antiq/`;
  7. one output line per folder and a summary; `-n` only shows.
- **`idle_days` default:** the spec says 14 today, the user's status
  example used 90; proposal 90, open.
- **push flow:**
  1. check the target marker: a foreign collection aborts, another
     host warns, a missing marker on an empty target is created, a
     missing marker on a non-empty target aborts;
  2. one `rsync -a --delete --delete-delay <collection>/ <target>/`
     without `.antiq/tmp/` and the lock (with `--delete-delay` an
     aborted run removes nothing that was there before);
  3. the target is a path (mounted NAS) or `user@host:path`;
  4. `-n` summarizes `--itemize-changes` per piece;
  5. the time and result of the push are recorded locally.
- **Verification:** open between checking only newly sent archives on
  the target, a separate `antiq verify` command, both, or neither.
- **Configuration:** `[collection]` with `id`, `patterns`, `exclude`,
  `ignore`, `idle_days` (90), `soon_days` (3); `[target]` with `url`
  and an optional `rsync` path. Everything tied to tiers, claims, live
  packages, the remote brake and the own ssh layer goes.

## Concept rounds (2026-11-04)

The concept is `docs/concept-v1-simplified.md` (70c25e0). Order agreed
with the user: follow-up questions in asqr until none remain, then an
adversarial review (fable, opus, sonnet, haiku), its questions in asqr,
repeat until clean.

Concept round 1 (asqr 01M6K53QJ44TT7ZAP6Q3WS1844), applied in f0db7b9:

- `idle_days` default 30, set per collection in its `config.toml`.
- No reclaim: an unearthed folder archived again is a new version.
- No `reconcile` command: repairs run at the start of every run.
- No state file at all.
- Paths escaped as GNU `sha256sum` does, in the comment lines too.
- Old `.antiq/objects/` stores are not converted.
- The user does not want to read the document whole; it is gone through
  question by question.

Concept round 2 (asqr 01M6K5JFV3VWECWFMV0X8BHE1Q and
01M6K5JMQHQ257VYAZS2ZNJ8WB):

- Verification: rsync with `--fsync` and its own whole-file check,
  nothing read back. A full comparison is todo 01M6K6WN14S91E2X5TRQYZRHJ5.
- `unearth` writes `.antiq-unearthed` with the time; the idle clock
  starts there.
- Nothing records the last push; no warning.
- No brake on `push`. The user's note: it must be documented that push
  is only a mirror and not a backup, also in the README and user docs
  once they are written, so nobody gets the wrong idea.
- Scheduling: two commands, `archive` then `push --if-reachable`.
- Detail rules of selection, walk and idle check: to be gone through one
  by one (round 3).
- The 15 todos listed as obsolete are closed when the concept becomes the
  spec: tiers, version conflicts, claimed version number, unnamed local
  payloads, root live package, keep folder until on remote, chunked
  remote format, several remotes, switching the selection strategy,
  SQLite index, shared picker crate, find/unearthed-loss/prune
  decisions, one-shot command, the original v1 scope todo, and the plan
  "Implement antiq v1" (replaced by a rework plan).
- Shell integration kept for after v1 (todo rewritten, `horizon:
  someday`).
- ADRs: those of removed parts are deleted, the rest rewritten; numbers
  stay with gaps. The concept becomes the new spec.
- Paused branches i31, i33, i42, i44 saved as git bundles in the
  logbook (`plans/impl/bundles/`) and deleted; i42's two commits are
  carried over by the rework plan where they apply.
- The user's session note: is `archive` the right name for the command?
  To discuss (round 3).

Concept rounds 3 to 7 (asqr 01M6K6Z625AFW27THQM1V2BDWW,
01M6K7MDA54ATMFCZVQX3YSRY7, 01M6K848GKD8QHT8446P0Q8SCV,
01M6K8B4GVJ09ZJGKJ4HRDPJ2Y, 01M6K8EQHKPYXPG4Q16GRP1SMF, and chat):

- `archive <piece>` archives one piece now, however recent; pinned is
  refused until unpinned.
- Kept unchanged: patterns with `**` and shallowest match, the dot
  rule, exclude as literal paths, symlinks (never pieces, never
  followed), mount points, nested collections, unreadable entries,
  empty pieces.
- Symlinks: rsync copies them as links; whether something on the
  target follows them is the target's permission matter, documented,
  not solved.
- Future file times count normally; `status` points to the files so
  the user fixes them. The one-day tolerance rule goes.
- Ignore rules: `ignore` (config) and `.antiqignore` now mean "not
  packed, not counted, deleted with the folder" (the git meaning), the
  user's example being `node_modules/`; default `[".DS_Store"]`, the
  separate junk list goes. Rules that only keep paths out of the idle
  check are `timeless` (config) and `.antiq-timeless`. The user
  confirmed gitignore anchoring (`docs/*.log` is anchored).
- `push` mirrors live folders whole, ignored paths included.
- `init` creates without a question and without `archive.sh`
  detection; it prints the count of due folders only if that is a few
  lines on code that already exists (lead decides at implementation),
  else only that it was created.
- Command names: `archive` and `unearth` (the user tried `bury` and
  `shelve`, then preferred `archive`, since `shelve` does not go with
  `unearth`). The noun is "version" everywhere; "archive file" only for
  the `.tar.zst` on disk.
- Control files: `.antiqignore` like `.gitignore`, the others with a
  dash: `.antiq-timeless`, `.antiq-pin`, `.antiq-unearthed`. The tool
  name antiq stays.
- Go for the adversarial review of the concept (fable, opus, sonnet,
  haiku).

## Presented

- The grep study was presented in round 8
  (`docs/research/listing-grep.md`, main e2002a9):
  - the method: generated data, the read path, what is measured and
    what is extrapolated;
  - the engines compared;
  - choices: the engine (`regex::bytes` over the whole decoded listing
    is recommended), compressed or plain listings (zst about 3 times
    the search time, about 0.2-0.26 s for 300 versions in parallel),
    the thread count, and whether line numbers are reported.

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
