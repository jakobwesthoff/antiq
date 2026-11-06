---
title: "Rework antiq v1 to the simplified concept"
kind: plan
component: project
origin: request
tags: [architecture]
---
# Rework antiq v1 to the simplified concept

This plan turns antiq from the multi-machine design of `docs/specs/spec-v1.md` at beb748f into the simplified single-machine concept `docs/concept-v1-simplified.md` (main beb748f), which went through three adversarial review rounds. It went through its own adversarial review (63 findings, all applied; ledger in the logbook, `plans/rework-plan-review-ledger.md`). Each item gets a step todo that is *Part of* this plan when the lead starts it.

Progress: complete. Final pass after the plan review applied to every
section; dependencies and the critical path re-checked (7.2).

Review: the adversarial review of this plan (63 findings, all settled,
PF5 withdrawn, no question for the user) is in
`plans/rework-plan-review-ledger.md`. Its fixes and the lead's decisions
of section 8.4 are applied in the body; finding IDs (PO, PF, PS, PH) are
cited where a fix lands.

Target: `docs/concept-v1-simplified.md` at main beb748f ("the concept").
Sections of the concept are cited as "concept <section>", lines as
"concept:<line>". The spec at beb748f is "old spec". Once D0 has merged,
both files are gone from the working tree; items read them with `git
show beb748f:docs/concept-v1-simplified.md` and `git show
beb748f:docs/specs/spec-v1.md`. Where the new spec and the concept
differ, the item reports the difference to the lead and the concept
decides until the spec is fixed (PS2, PO9).

Line numbers cited as "concept:<line>" are those of beb748f. The
concept's final text is at a19b83f (`push --force` instead of
`--take-over`, `rsync -H`; section 8.5); read the behaviour from `git
show a19b83f:docs/concept-v1-simplified.md` and use beb748f only to
follow a line citation.

## 1. Overview

### End state

One crate, one machine. `.antiq/` holds `config.toml`, `lock`, `tmp/`,
`trash/`, `doomed/`, `recovered/` and `pieces/<piece path>/` with three
files per version (`.tar.zst`, `.sha256.zst` listing, `.tar.zst.sha256`).
No object IDs, no metadata documents, no tracking, no index, no remote
layer. The command set is `init`, `archive`, `unearth`, `fork`,
`forget`, `rebuild`, `import`, `list`, `grep`, `status`, `pin`, `unpin`,
`push`. Every locked run starts with the six repairs of concept
"Repairs at the start of a run". `push` runs rsync against one target
and owns the `.antiq-target` marker. The spec is the concept, rewritten
as `docs/specs/spec-v1.md`; the concept file is gone.

### How the work is cut

Most of the built code serves the object store and the remotes. Nearly every command
module imports `meta`, `store` or `tracking` (rg, Appendix A.3, A.4),
so a module-by-module removal that keeps the old commands working would
mean surgery on code that is rewritten right after. The plan therefore:

1. removes, in one sequential item, everything the concept removes
   **and** the bodies of the commands it rewrites, leaving the old local
   commands as stubs that answer `not_implemented`; `init` keeps
   working;
2. lays down the new command surface and every new module file, so
   parallel items fill files and never fight over `lib.rs`, `error.rs`
   or `cli/mod.rs`;
3. builds the foundations (config and layout, the versions store, the
   listing format, rules and walk, idle clock and markers, repairs,
   damage and version naming, references, the rsync target layer),
   partly in parallel;
4. builds the commands on them, mostly in parallel;
5. finishes with the cleanup that needs every command in place
   (removing `not_implemented` by cherry-picking i42's 6a90f18, the
   README and the user documentation).

Store-free code that the rewritten commands reuse (the trash re-walk
comparison, the unearth target checks, the space ledger, the parallel
runner) is deleted in step 1 and ported back from beb748f by the item
that needs it, which names the exact source lines (`git show
beb748f:<path>`). Porting from history keeps the removal item
mechanical and puts each reuse decision next to the code that needs it.

### Phases

| Phase | Items | Runs |
|---|---|---|
| 0 Docs and measurement | D0, M1 | written in parallel with R1; M1 merges any time, D0 right after R1 |
| 1 Removal | R1 | alone on the code |
| 2 Surface | S1 | after R1 |
| 3 Foundations | F1 to F8, P1 | after S1; partly parallel (section 7) |
| 4 Commands | C1 to C11, P2 | after their foundations; mostly parallel |
| 5 Finish | Z | last |

Section 7 lists every item with its dependencies and the parallel
groups.

## 2. Removal

All of this is item R1 unless noted. "Refs" names the kept files that
import the removed thing (rg `crate::<module>` over the files that stay,
run on beb748f; Appendix A); R1 edits those so the crate builds. Files
that are themselves deleted are not listed as refs.

### 2.1 Library modules deleted whole

| Path | Concept | Refs in kept files and the edit there |
|---|---|---|
| `src/meta.rs` | "What remains": removed | `error.rs` (`Meta` variant, `FormatTooNew` kind mapping), `id.rs:7` (`CreatedSource`, only for `id_time`, which goes), `listing.rs:8` (`FileName`, `is_listing_file_name`, only for `listing::file_name`, which goes with its tests listing.rs:866-908) |
| `src/store.rs` | removed | `error.rs` (`Store` variant) |
| `src/tracking.rs` | removed | `error.rs` (`Tracking`), `reference.rs` (see 2.4) |
| `src/index.rs` | removed | `error.rs` (`Index`), `reference.rs` |
| `src/tombstone.rs` | removed | none outside deleted files |
| `src/brake.rs` | "Rules for every command" (brake keys go), "push" (no brake) | `error.rs` (`Brake` variant and kind, `insert_brake_hold` error.rs:276-298), `plan.rs:7,16` and its brake tests (126+), `cli/mod.rs:15, 436-440` |
| `src/repair.rs` (content) | rewritten (concept "Repairs") | `run.rs` imports `repair::{self, Effects, RealEffects, RepairError}` (run.rs:16) and passes `&dyn Effects` through `begin_with` (run.rs:51-65). R1 replaces the file by a stub with `run(collection, lock, clock) -> Result<Vec<Warning>, RepairError>` that returns no warnings and an empty `RepairError`; in `run.rs` it drops the `effects` parameter of `begin_with` and the two tests that need the old repairs, `a_repair_that_fails_is_an_error_of_the_run` (run.rs:173-181) and `a_real_run_empties_the_run_directories_before_it_starts` (run.rs:264-279). Every other user of `Effects` is a file R1 deletes (PO6, PH1). F6 writes the new file and the new run tests. |
| `src/commands/archive/` (mod, new, new/finish, durable, unearthed) | rewritten; reclaim removed (concept "unearth", "There is no reclaim") | `error.rs` (`Archive`), `testing/mod.rs` (`LockHeldWithoutHolder` impl for `ArchiveError`, testing/mod.rs:37-60) |
| `src/commands/unearth/` (mod, local, live, remote) | rewritten; live and remote go | `error.rs` (`Unearth`) |
| `src/commands/fork.rs` | rewritten | `error.rs` (`Fork`) |
| `src/commands/import.rs` | rewritten (folders, not archive files) | `error.rs` (`Import`) |
| `src/commands/status.rs` | rewritten | `error.rs` (`Status`) |
| `src/commands/pin.rs` | rewritten | `error.rs` (`PinOp`) |
| `src/commands/find.rs` | removed (`find` goes) | `error.rs` (`Find`) |
| `src/commands/sync/` | removed | `error.rs` (`Sync`) |
| `src/commands/reconcile/` | removed ("There is no `reconcile` command") | `error.rs` (`Reconcile`), `cli/mod.rs` (`pruned`) |
| `src/commands/resolve/` | removed (no conflicts) | `error.rs` (`Resolve`) |
| `src/commands/remote.rs` | removed | `error.rs` (`RemoteOp`, custom `From` error.rs:211-219) |
| `src/remote/{away,cleanup,commit,path,policy,testing}.rs`, `src/remote/ssh/` | removed (rsync replaces the remote layer) | `cli/mod.rs` (`policy::outcome`, `RemoteStop`, cli/mod.rs:422-429, 457-470), `error.rs` (`Remote`) |
| `src/remote/mod.rs` (all but the module lines of `rsync`, `marker`, `url`) | shrinks | `marker.rs` uses `Durable`, `RealDurable`, `recording` only for `write_marker` (marker.rs:12, 109-154). R1 deletes `write_marker`, `Durable`, `RealDurable`, `recording` and the marker write tests (marker.rs:253-389), and keeps the format tests (marker.rs:156-250) (PO3). |
| `src/archive/live.rs`, `src/archive/manifest.rs` | "What remains": `archive/` without them | none outside deleted files |
| `src/archive/frames.rs` | not in the concept table; its only user is the old `import` of archive files (`src/commands/import.rs:30, 273, 434`) | none outside deleted files. Flagged in section 8 (O3). |
| `src/cli/picker/` with `snapshots/` | removed | `cli/mod.rs:10` |
| `src/cli/cmd/{find,resolve,reconcile,remote}.rs`, `src/cli/cmd/sync/` | removed commands | `cli/mod.rs` (`Command` variants :41-71, `dispatch` :336, `name` :76, `if_reachable` :95, `guess_command` :515), `cli/cmd/mod.rs` |

Module declarations of deleted files also go from `src/archive/mod.rs`
(archive/mod.rs:8-11: `frames`, `live`, `manifest`) and from
`tests/support/mod.rs` (`objects`, `pieces`, `pty`) (PH10).

`src/interact.rs` and `src/cli/tty.rs` are removed by the concept too,
but `init` still asks through them (`src/commands/init.rs:14, 321-406`,
`cli/cmd/init.rs:29-30`). R1 leaves them; C2 (the new `init`) deletes
both, together with `Context::interaction` and `terminal`
(`cli/mod.rs:307-321`) and the `--json` help text "never ask a
question" (`cli/mod.rs:32`).

### 2.2 Parts of kept files deleted

| File | What goes | Why |
|---|---|---|
| `src/id.rs` | `id_time`, `IdTime`, `EntryTimes`, `ID_TIME_FLOOR` and their tests (id.rs:74-165 area) | object-ID time minting. `ObjectId`, `IdSource`, `RandomIds` and `SeqIds` stay, since `init` mints the collection ID through them (`commands/init.rs:13, 263, 414`, `cli/cmd/init.rs:10`); C2 decides whether the minting trait outlives the old `init` (PH11) |
| `src/relpath.rs` | `name_part`, `machine_part`, `temp_name`, `escape_part` (relpath.rs:208-250) and tests | payload and upload temp names; their callers are all deleted |
| `src/reference.rs` | `resolve` (index-based), the ULID prefix logic (`PREFIX_LENGTHS`, `is_prefix`, reference.rs:62-135), `ReferenceError::Index`, the index/tracking fixtures of its tests (reference.rs:254-278) | F8 rewrites references; `resolve_path` (reference.rs:149-190: `.`/`..` folding, root found by device and inode, no symlink followed) stays as it is |
| `src/plan.rs` | the `brake` field, its JSON, its lines, and its brake tests (plan.rs:101-171), which hold the file's only use of `reference::Candidate` (plan.rs:102, 132, 167, 171) (PO19) | concept "Rules for every command" |
| `src/collection.rs` | accessors for `objects/`, `tracking/`, `index/`, `remote-state.json`, `pruned.json`, `meta.pending`, `trash/<id>` (collection.rs:84-150) whose callers are deleted | F1 adds the new layout |
| `src/error.rs` | `Error` variants of every deleted module and of the stubbed commands' old error types (see 2.3); kinds `brake`, `no_remote`, `remote_away`, `remote_broken`, `unsupported`; the brake `details` arm; their rows in the tests (error.rs:300-488) | `not_implemented` stays: the stubs raise it until Z. `format_too_new` stays for now: `ListingError::FormatTooNew` still maps to it (listing.rs:281-304); F3 decides. |
| `src/cli/mod.rs` | the removed commands, `if_reachable`, `policy` handling, brake item lines, the `remote` case of `guess_command` | removed commands |
| `src/cli/cmd/status.rs` | everything but the table helpers | the helpers (`sanitize` 773, `mid` 790, `pad`, `pad_left`, `wrap`, `paint`, `spaces`, `Color`, `WIDE_FROM`) move to a new `src/cli/table.rs`. Their tests went with the deleted callers, so R1 adds direct unit tests for each moved helper, which keeps the moved lines covered (PO24). The helpers without a caller carry the dead-code allowance below. |
| `src/testing/mod.rs` | the `LockHeldWithoutHolder` impls for `ArchiveError` and the `Archive` arm (testing/mod.rs:37-60) | archive deleted |
| `tests/support/cmd.rs` | nothing: `path_without_real_ssh` (cmd.rs:34-56, used at cmd.rs:75) stays, so that no test reaches the network through a real `ssh`; P2 adds a test that proves the guard is active (PO7) | |

**Items left without callers (PO2, PS1, PO3).** R1 commit 2 leaves
`pub(crate)` items in the library, and `pub` items in the binary crate,
without a caller until a later item uses them again; `cargo clippy -D
warnings` refuses that as dead code. Rule for R1 and every later item:
such an item gets `#[allow(dead_code, reason = "used again by <item>")]`
naming the consumer item; it is never made `pub` to hide the warning;
the consumer removes the allowance; Z greps `allow(dead_code` under
`src/` and finds only the allowances that exist on beb748f
(`cli/mod.rs:293`, `cli/output.rs:211, 223`, `filesystem/device.rs:9`,
`testing/mod.rs:8`). Known cases and their consumers:

| Item | Consumer |
|---|---|
| `filesystem::space::{available_space, SpaceLedger, Reservation}` (space.rs:13-61) | C4 (`store_version`), C6 |
| `archive::extract::check_structure` (extract.rs:538) | C6 |
| `filesystem::remove::remove_tree_at` (remove.rs:48) | F6 (repair 3) |
| `filesystem::moves::move_dir` | F6 (repair 4, `give_back`) |
| `filesystem::device::is_mount_point` | P2 |
| `cli::output::Output::line_os` (output.rs:218) | C11 (raw paths in a pipe) |
| `cli::output::Output::stdout_style` (output.rs:176) | C1 |
| table helpers in `cli/table.rs` | C1, C3 to C11 (whichever uses each) |
| `testing::rerun_while_lock_unattributed`, `LockHeldWithoutHolder` (testing/mod.rs:28-90) | F6 |

The R1 agent adds any further case clippy reports, with its consumer.

### 2.3 Commands after R1

| Command | State after R1 |
|---|---|
| `init` | works as before (asks, `--yes`, old layout); C2 rewrites it |
| `status`, `archive`, `unearth`, `fork`, `import`, `pin`, `unpin` | stubs: find the collection (so "not a collection" still answers), then fail with `not_implemented` |
| `find`, `sync`, `reconcile`, `resolve`, `remote` | gone; a usage error |

S1 then replaces the surface with the new one (section 4.1).

### 2.4 Tests and fixtures

- Deleted test files (they test removed or stubbed behaviour; the items
  that rewrite a command port the store-free cases named in Appendix
  A.3 and A.4): `tests/archive.rs`, `tests/brake.rs`, `tests/find.rs`,
  `tests/fork.rs`, `tests/import.rs`, `tests/picker.rs`, `tests/pin.rs`,
  `tests/reconcile.rs`, `tests/reconcile_remote.rs`, `tests/rearchive.rs`,
  `tests/remote_path.rs`, `tests/remote_ssh.rs`, `tests/resolve.rs`,
  `tests/ssh_session.rs`, `tests/ssh_session_real.rs`, `tests/status.rs`,
  `tests/sync.rs`, `tests/tombstone.rs`, `tests/unearth.rs`,
  `tests/unearth_remote.rs`.
- Kept and edited: `tests/cli.rs` (the `COMMANDS` list :14-51, the
  removed-command cases, `remote help` :66, `--if-reachable` :386; the
  `accepts_and_needs_a_collection!` cases :127-275 keep working for the
  stubs), `tests/init.rs` (unchanged; C2 rewrites it).
- `tests/support/`: delete `objects.rs` after moving `hold_lock` and
  `snapshot` into `tests/support/cmd.rs` (they are store-free and the
  pin, fork and lock tests of later items need them); delete `pieces.rs`
  after moving `days_ago` into `tree.rs`; delete `pty.rs`; keep
  `cmd.rs` with its `path_without_real_ssh` guard (PO7), `tree.rs`,
  `sshd.rs` (P2's opt-in real-ssh tests use it).
- `tests/fixtures/`: keep `fake-rsync.sh`, `ssh-shim.sh`, `ssh-real.sh`,
  and of `ssh-texts/` the six texts push needs (`connect-refused`,
  `resolve`, `timeout`, `host-key-changed`, `host-key-unknown`,
  `permission-denied`); delete the other ten, whose only readers are
  the deleted classifiers (`src/remote/ssh/classify.rs:195`,
  `src/remote/ssh/mod.rs:3885-3888`).
- `src/remote/rsync.rs` keeps its tests. Most use `fake-rsync.sh`; two
  run a real rsync and are `#[ignore]`d (rsync.rs:1282-1283, 1344-1346)
  (PF9).

### 2.5 Cargo.toml

| Dependency | Removed in | Evidence |
|---|---|---|
| `crossterm` | R1 | only `src/cli/picker/mod.rs` |
| `unicode-segmentation` | R1 | only `src/cli/picker/layout.rs:19` |
| `rustix` feature `pty` | R1 | only `tests/support/pty.rs:363-371`; `termios` stays for `cli/output.rs:116, 129` |
| `nucleo` | kept | its users (`src/index.rs:23-24`, the picker) go, but F8 ports the query of `index.rs:500-540` into `src/query.rs` for the search-word references and `list` (O1, PO12). R1 keeps the dependency without users; cargo does not lint unused dependencies. |
| zstd feature `experimental` | F3 | needed only for `RSyncable` and `JobSize` of the live preset (`src/archive/zstd.rs:78-84`), which F3 removes |
| `zstd-sys` (`=2.1.2`, Cargo.toml:35-37) | F3 | the exact pin served byte-stable live packages (ADR 0052); lead decision O4 (8.4): pin and ADR go (PH4) |
| `regex` | added by C11 | `grep` (concept "grep"), with `cargo add` |

Everything else stays: `whoami` (`machine::detect`), `ulid` (collection
ID), `toml` (config parsing, config.rs:29, 267, 280), `toml_edit`
(`init` writes the config), `unicode-width` (tables), `glob`, `ignore`,
`filetime`, `sha2`, `tar`, `zstd`, `unicode-normalization`, `libc`,
`jiff`, `serde`, `serde_json`, `thiserror`, `anyhow`, `clap`; dev
`insta`, `tempfile` (PO19).

### 2.6 How R1 is cut into commits

Every commit passes `scripts/check.sh` (`cargo fmt --check`, `cargo
clippy --all-targets -- -D warnings`, `cargo test`).

1. Remove `find`, the picker, `crossterm` and `unicode-segmentation`.
   Self-contained: the picker's only entry is `cli/cmd/find.rs:48`.
2. Remove `sync`, `reconcile`, `resolve`, `remote`, the brake, the
   remote layer, `pty.rs`, the ssh test files and the unused ssh texts; stub
   `archive`, `unearth`, `fork`, `import`, `status`, `pin`, `unpin`;
   remove `meta`, `store`, `tracking`, `index`, `tombstone`, the old
   `repair`, `archive/{live,manifest,frames}` and the parts of 2.2. These
   cannot be split further without keeping the stubbed commands alive
   on the old store, since `commands/archive`, `commands/unearth` and `commands/status`
   reach into the remote layer and live packages (Appendix A.1, A.2).

The diff is almost all deletion. The reviewable part is the edits to
kept files (2.2), which the lead reviews with opus (section 7).

## 3. Changes to kept modules

What changes after R1, by module. "Tests" says which existing tests
change or go; new tests are in the items (section 7).

| Module | Item | Change | Tests |
|---|---|---|---|
| `src/config.rs` | F1 | Keys of concept "Configuration": `[collection]` `id`, `patterns`, `exclude`, `ignore` (default `[".DS_Store"]`), `timeless` (default empty), `idle_days` (default 30, today 14 at config.rs:154), `soon_days` (default 3, new); `[target]` `url` (kept as a string; P1 parses it), `rsync`, `rsync_path`, `fsync` (default true), `rsync_args` (default empty). The keys and tables concept:808-813 lists are refused, all of them named in one error. Other unknown `[collection]` keys keep today's warning (config.rs:76-83, 307-316). `ConfigDoc::new(id, patterns, exclude)` writes no `[machine]`; `add_remote`, `remove_remote`, `set_machine_rsync`, `MachineConfig`, `RemoteConfig`, `junk`, `memory_percent` and the range checks of removed keys go. | Tests of removed keys go (memory_percent config.rs:537, 577, 624, 809-862; machine and remote cases); defaults and key-list tests change |
| `src/collection.rs` | F1 | Layout of concept "Store layout": `config.toml`, `lock`, `tmp/` (`tmp/pack/<stamp>/<piece path>/` for archive), `trash/`, `doomed/`, `recovered/`, `pieces/`. `staging/` goes (unearth stages in `tmp/`). `machine_name()` goes; callers use `machine::detect`. | accessor tests (collection.rs:362 area) change |
| `src/machine.rs` | F1 | Only `detect` (machine.rs:21-38) stays; `resolve` goes (concept "What remains"). | `resolve` tests go |
| `src/run.rs` | F1, F6 | F1: the lock holder's machine name from `detect`. F6: repairs of concept "Repairs" after the lock; a dry run computes them read-only and returns them; a held lock in a dry run returns the holder and no repairs (concept "Rules for every command"). F6 also provides the test helper that holds the lock while a dry run runs, which the command items use (PO22). | dry-run tests change; R1 already removed the two repair-bound tests (2.1) |
| `src/lock.rs` | none | Unchanged: exclusive `flock`, holder pid, machine and start time. | unchanged |
| `src/plan.rs` | F6 | Actions of kind `repair` first, each naming what it deletes, renames back or moves (concept "Dry runs"). | new tests |
| `src/report.rs` | none | Unchanged (warnings, lock note, unknown-key note, SMB/NFS note; O9). | unchanged |
| `src/select.rs` | none | Patterns, dot rule, exclude by path and by (device, inode) carry over unchanged (concept "Selection"). | unchanged |
| `src/tree.rs`, `src/walk.rs` | F4 | Each entry gets a class: counted, timeless, ignored, control file. The walk enters ignored subtrees for the refusals and records them, so packing, the listing, the strict verify of a user's folder and the idle check can leave them out (concept "Ignored subtrees are still checked"). The junk list goes (walk.rs:327-329). New problems: `Locked { path }` for an immutable or append-only flag or an entry whose parent directory the run cannot write (concept "Locked entries"; O6), and `MarkerNotAFile { name }` for a non-file named `.antiq-pin` (other than a symlink) or `.antiq-restored` at the top (concept "Control files"). | junk tests go (walk.rs:327 cases); new class and problem tests |
| `src/rules.rs` (new, from `src/idle.rs`) | F4 | The gitignore matcher (idle.rs:11, 103-140, 272-337) moves here and serves two rule kinds: ignore (`ignore`, `.antiqignore`) and timeless (`timeless`, `.antiq-timeless`). Precedence, anchoring and the refusal of a broken rule file stay as built (idle.rs:288-316). | the ignore tests of idle.rs move with it |
| `src/idle.rs` | F5 | Idle clock of concept "Idle clock": later of the newest counted mtime and the `.antiq-restored` time; no one-day tolerance and no `OnlyFuture` (idle.rs:161, 184-212); future files and a future marker time are listed; control files at the top only; a marker that cannot be read or parsed refuses the piece; `soon` from `soon_days`. The `unearthed` parameter (idle.rs:144) becomes the parsed marker. | tolerance and `OnlyFuture` tests change; marker tests new |
| `src/pin.rs` | F5 | A pin is a regular file or a symlink named `.antiq-pin` at the top (pin.rs:56-62 today takes any non-directory). `create` writes through `.antiq/tmp/` and a rename, `remove` unlinks; both grant owner write to a top without it and put the mode back, then flush the top (concept "Markers"). A top on another device than `.antiq/tmp/` would make the rename fail with `EXDEV`; the marker write checks the device first and refuses with the reason "mount point" (PF3). | `is_pinned` tests for fifo/socket change (now a refusal, decided in the walk) |
| `src/filesystem/tree.rs` | F4 | `TreeEntry` gains the immutable/append-only flags: macOS `st_flags` `UF_IMMUTABLE`, `SF_IMMUTABLE`, `UF_APPEND`, `SF_APPEND`; Linux `statx` `STATX_ATTR_IMMUTABLE`, `STATX_ATTR_APPEND` from the call that already reads `STATX_ATTR_MOUNT_ROOT` (filesystem/tree.rs:584-640). | new |
| `src/filesystem/remove.rs`, `src/filesystem/moves.rs` | F4 | Refuse an entry with those flags with their own `FilesystemError` variant, before touching it (concept "What remains": `filesystem/`). `remove_tree` checks every entry it would delete. `move_dir` checks only what a rename touches: the moved directory and the parents it leaves and enters; the tree's contents are the walk's job (PF12). | new |
| `src/filesystem/atomic.rs`, `src/filesystem/space.rs` | F2 | A helper that creates a missing directory chain and flushes each new directory with its parent (concept "Store layout", flush rule). `is_out_of_space` (ENOSPC through the error chain, ported from `src/commands/archive/new.rs:271-283`) moves into `filesystem/space.rs` for C4, C5, C6, C7 (PO8); F4 alone edits `filesystem/mod.rs`. | new |
| `src/filesystem/space.rs` | F2 | `available_space`, `SpaceLedger` stay (concept "Free space"); `is_out_of_space` joins them (row above). | |
| `src/listing.rs` | F3 | Rewritten to concept "Listing format" (Appendix A.2: today zstd JSON lines). The in-memory `ListingEntry` stays, with `hard` entries carrying the SHA-256 of the file they link to. | almost all of listing.rs:306-908 replaced |
| `src/archive/tar_write.rs`, `read.rs`, `verify.rs`, `mod.rs` | F3, F4 | F3: hard-link entries carry the target's hash (tar_write.rs:334 sets none today); `read::list` yields the same entries from a tar alone (the listing `rebuild` regenerates); `files` and `bytes` counted once per stored file. F3 also owns the shared verification that C6 and C9 build on (PS5, PO8): the whole-file hash (`file_sha256`, ported from `src/commands/archive/new.rs:1063`, and the check against a checksum file, after `check_archive`, `src/commands/unearth/local.rs:243-267`), and the comparison of a tar's members with a listing without a folder (model `Arrived::check_listing`, `src/commands/unearth/remote.rs:110-134`). F4: `WalkRules.junk` becomes the ignore rules for a user's folder; staged trees are verified with no rules. | tar_write.rs:792-809, 871-892 (exact JSON) change |
| `src/archive/zstd.rs`, `src/testing/package.rs`, `Cargo.toml` | F3 | Live preset and constants go (zstd.rs:20-51, 78-84); a public `ZstdSettings::fast` (a low level, one worker) serves tests and the version fixture, so fixtures never run the level-19 encoder with its memory cost (pool.rs:22-27) (PO16); Cargo feature `experimental` and the `zstd-sys` pin go (O4, PH4). | zstd.rs ~176-186 and the `ZstdSettings::live(1)` call sites of Appendix A.2 change to `fast` |
| `src/archive/pool.rs`, `src/testing/pool.rs` | F3 | `Pool::for_machine()` takes no percentage; a constant of 25 % (concept "What remains": `archive/`). | pool tests with explicit percentages stay (they call `for_machine_with`) |
| `src/archive/names.rs` | none | Probe, folding, member checks and portability warnings carry over (ADR 0054). | unchanged |
| `src/archive/extract.rs` | C6 | A flush hook for every extracted file and directory, by the method of section 5.1. A file is flushed before it is closed (extract.rs:441). A directory is flushed after `finish` has set its final mode and times (extract.rs:460-471), since those changes dirty it again, and through the handle the extraction holds, since a final mode may keep the owner from opening it again (PF2). | new |
| `src/reference.rs`, `src/query.rs` (new) | F8 | New resolution against live pieces and store pieces (section 4.7). `resolve_path` stays. The nucleo query of `src/index.rs:500-540` (beb748f) is ported into `src/query.rs`, which `list` uses too (PO12, PH5). | rewritten |
| `src/remote/{rsync,marker,url}.rs` | P1 | Move to `src/target/`. rsync: floor 3.2.4 with patch compare (rsync.rs:88-93, 374-410), `[target] rsync`, argv of concept "push" steps 4 to 7, exit status, stdout and stderr returned for every status; `--ignore-times`, `--no-whole-file`, `--partial-dir`, `--stats` parsing go (rsync.rs:165-197, 282-299). marker: `.antiq-target`, `{format, id, host, path}`, rewrite allowed. url: absolute path (any characters a TOML string holds) or `[user@]host:path` (PF10). | rsync argv and stats tests go (rsync.rs:850-995 partly, 1065-1118, 1344); probe, env, resolve, failure tests stay; url tests mostly replaced (the marker write tests already went in R1) |
| `src/error.rs` | S1, then items | S1 creates the error enum of every new module and command, wires each into `error::Error` and adds the kinds `target_unreachable` (exit 4; O8) and `no_target` (exit 1; PS3); items fill their own enum and never edit `error.rs` (PH6). `details` finds a `LockError::Held` anywhere in the source chain, so a command error that wraps `RunError` still carries `pid` and `started` in its JSON (error.rs:275-283 matches only two shapes today) (PO15). | kind table test changes; a wrapped held-lock test |
| `src/lib.rs` | S1 | `extern crate self as antiq;`, so the version fixture shared through `#[path]` can name `antiq::` paths in both the library's tests and the binary tests (PO1). | |
| `src/cli/mod.rs`, `src/cli/cmd/*` | S1, then items | New command surface (section 4.1). | `tests/cli.rs` |
| `src/cli/output.rs` | none | Envelope unchanged (concept "Rules": the envelope stays). | unchanged |
| `src/id.rs` | C2 | Keeps `ObjectId`, `IdSource`, `RandomIds`, `SeqIds` for `init` (PH11); C2 removes what its new `init` no longer needs. | |
| `src/relpath.rs`, `src/clock.rs`, `src/sysmem.rs` | none after R1 | | |
| `tests/support/tree.rs` | F1 | `TempCollection::minimal()` (tree.rs:144-162) writes the new layout. | |

## 4. New code

Each subsection says what is built, which concept section it
implements, and the code it builds on. Behaviour the concept leaves open
is not filled in here; it is in section 8 with an "O" number.

### 4.1 Command surface and module tree (S1)

`-C <path>` and `--json` stay global (cli/mod.rs:25-38). Every
changing command takes `-n`/`--dry-run` (concept "Dry runs"). clap's
`help` subcommand stays disabled.

| Command | Arguments |
|---|---|
| `init` | `[--pattern <glob>]... [--yes] [-n]`; `--yes` stays only until C2, since the old `init` still asks (`src/commands/init.rs:327-344`) and `tests/init.rs` relies on it; C2 removes it and adds a test that `--yes` is a usage error (PH2, PO11) |
| `status` | none |
| `archive` | `[<piece>] [-n]` |
| `unearth` | `<piece> [--version <version>] [-n]` (`--to` goes) |
| `fork` | `<piece> [--version <version>] [--name <new path>] [-n]` |
| `forget` | `<piece> (<version> \| --all-versions --force) [-n]` |
| `rebuild` | `<piece> <version> [-n]` |
| `import` | `<folder> [--as <path>] [--time <time>] [-n]` |
| `list` | `[<query>]` |
| `grep` | `<pattern>` |
| `pin`, `unpin` | `<piece> [-n]` |
| `push` | `[--init] [--force] [--if-reachable] [-n]`; `--init` and `--force` are each refused together with `--if-reachable` (concept "push" step 5) |

`--version` takes a string; F7 matches it. `--time` takes a string;
C5 parses it (O11). S1 creates, as stubs, `src/cli/cmd/<name>.rs` for
every command (find the collection, then `not_implemented`; `init`
keeps working), and the new library files, each with its module doc
and nothing else: `src/versions/{mod,stamp,names,store,damage,naming}.rs`,
`src/rules.rs`, `src/markers.rs`, `src/query.rs`,
`src/commands/{archive,unearth,fork,forget,rebuild,import,list,grep,status,
pin,push,survey,restore}.rs`. `src/cli/table.rs` exists since R1. S1
also creates `tests/support/versions.rs` as a stub and its `#[path]`
line in `src/testing/mod.rs` and its line in `tests/support/mod.rs`, so
F7 only fills the file (7.2).

Errors (PH6): S1 creates the error enum of every new module (for
example `VersionsError`, `RulesError`, `MarkerError`, `QueryError`) and
of every command (`<Command>Error`), each empty, with `HasKind`, and
wires the command errors into `error::Error` (error.rs:132-209), so
items fill their own enum and never edit `error.rs`. Assumption:
`thiserror` derives an empty enum; if not, a single placeholder variant
that the item replaces. S1 adds the kinds `target_unreachable` (exit 4,
O8) and `no_target` (exit 1, PS3), and makes `details` find a
`LockError::Held` anywhere in an error's source chain (PO15).

S1 adds `extern crate self as antiq;` to `src/lib.rs`, so a fixture
shared between the library's tests and the binary tests through
`#[path]` can use `antiq::` paths in both (PO1).

Every stub is tested inside a collection (answers `not_implemented`,
exit 1), so its lines are covered (PO21).

### 4.2 Store layout and versions (F2)

Concept "Concepts" (version stamp), "Store layout".

- `versions::stamp`: format `YYYY-MM-DDTHHMMSS` and `-N` for N >= 2;
  parse refuses `-1`, `-0`, leading zeros in N; order by time, then
  counter as a number, none = 1. A new stamp for piece P at time t is
  t without a counter when no file in P's store folder and no directory
  `tmp/pack/<stamp>/<P>` uses it; otherwise t with counter 2, then t
  with counter 3, and so on, the first one unused (PH12). The version
  time of `archive` is the UTC second at which the piece's packing
  starts (O24, decided); the listing's `archived` holds it.
- `versions::names`: `<name>` from the piece path: every character
  outside `[A-Za-z0-9._]` becomes `-`, runs of `-` collapse; cut from
  the front at a `-` boundary, or inside one over-long component at a
  character boundary, so that `<name>--<stamp>.tar.zst.sha256` (the
  longest suffix) stays within 200 bytes. A file name is parsed only for
  its stamp and suffix: split at the last `--` (a collapsed `<name>`
  holds no `--`), suffix one of `.tar.zst`, `.sha256.zst`,
  `.tar.zst.sha256`. Files that do not parse are ignored by every
  command, keep a folder from counting as empty, and are listed by
  `status` as a note (O12, decided).
- `versions::store`: scan `pieces/` into store pieces (a folder holding
  at least one `.tar.zst`) and their versions by stamp with the files
  present. A folder holding sidecars only is no store piece, so `list`,
  `unearth` and `fork` do not offer it, but the scan reports its
  incomplete versions, `forget` addresses them by path and stamp, and
  repair 6 keeps the folder, since it is not empty (O28, PH9).
- Exact lookup (PO8): F2 owns the lookup of a piece path with the
  volume's own name equality, an `lstat` through the path, then the
  stored spelling read from the directory listing (port `lists_exactly`,
  `src/commands/unearth/mod.rs:871-883`). C1, C6, F6 and F8 build on it. No
  probe is used, so dry runs stay read-only.
- Operations, each flushing as concept "Store layout" says:
  `move_in` (listing, then checksum file, then `.tar.zst`, each flushed,
  then the directory; archive step 4); `move_back` (the reverse into
  staging; archive step 5 rollback); `delete_version` (`.tar.zst` first,
  then listing and checksum file, flushing between; used by `unearth`,
  `forget`, repair 5); removal of a store folder and its parents that
  became empty. Store folders and parents are created through the new
  `filesystem::atomic` chain helper. `is_out_of_space` moves into `filesystem/space.rs`
  (section 3).
- Builds on: F1 (the `pieces/` accessor and the new fixture layout,
  PO5), `filesystem::atomic::{flush_file, flush_dir}`, `TreeRoot`,
  `relpath::RelPath`.
- Test fixture `tests/support/versions.rs`, built by F7 (it needs F3's
  listing writer and F4's walk) and included by S1 in `src/testing/mod.rs`
  through `#[path]` as `tree.rs` is (testing/mod.rs:8-10), so library and
  binary tests share it; it uses only the public API through `antiq::`
  paths (PO1) and packs with `ZstdSettings::fast` (PO16). It writes a
  version of a `Tree` into `pieces/` (pack, listing, checksum file) with
  a given stamp, and helpers that remove or truncate one of its files.
  Every later item uses it instead of the old `tests/support/objects.rs`.

### 4.3 Listing format, checksum file and shared verification (F3)

Concept "Listing format", "Store layout" (checksum file).

- Writer: the header lines in the concept's order (`antiq listing 1`,
  `piece`, `archived`, `archive`, `archive-bytes`, `files`, `bytes`,
  optional `imported`, optional `rebuilt`, then `#`), entry comment
  lines with four fixed-width fields and the path at a fixed byte
  offset, `\t-> ` for links and `\t=> ` for hard links, a checksum line
  after each `file` and `hard` line; zstd level 3 with content checksum
  (as today, listing.rs:23). Field widths: the concept fixes the shape,
  not the numbers; F3 takes the example's widths (concept:228-233) and
  the spec records them.
- Escapes: checksum lines get a leading `\` and `\n`, `\\` only when the
  path holds a newline or a backslash; comment lines never start with
  `\` and write `\\`, `\n`, `\t`, `\r` inside paths and targets.
- `archive-bytes` is known only after the archive is written: the
  listing is written after packing finishes (today it is written after
  the verify, new.rs:841, so the order holds).
- Readers: `read_head` (stops at the `#` line; for `list`, `status`'s
  header counts and damage by head), `read_full` (to the end, so the
  zstd checksum is checked; yields entries), and a raw decode for
  `grep` (the whole decoded buffer, concept "grep").
- Checksum file: one line `<hash>  <archive file name>` with a newline;
  reader accepts exactly that.
- Shared verification (PS5, PO8), which C4, C6 and C9 build on instead
  of each porting it: the whole-file hash of an archive (`file_sha256`,
  `src/commands/archive/new.rs:1063`) and its check against the checksum
  file and the listing's `archive-bytes` (after `check_archive`,
  `src/commands/unearth/local.rs:243-267`); the comparison of a tar's members
  (`read::list`) with a listing, without a folder (model
  `Arrived::check_listing`, `src/commands/unearth/remote.rs:110-134`).
- Tests first: the concept:264 test, a listing with tab, backslash,
  newline and carriage return in names passes
  `shasum -a 256 --strict -c` and GNU `sha256sum --strict -c` with exit
  0. `/usr/bin/shasum` exists on macOS. A bare `sha256sum` on `PATH` can
  be Apple's own tool, which fails on the listing (PO10), so the test
  takes `sha256sum` or `gsha256sum` only when its `--version` names GNU
  coreutils, and fails with a message naming the missing tool otherwise
  (rule of the old plan, section 1 there). Also: `shasum -a 256 -c
  <checksum file>` in the store folder.
- Builds on: `ListingEntry`, `is_sha256_hex`, the `Reader` skeleton and
  `HashingWriter` (Appendix A.2).

### 4.4 Damage and version naming (F7)

Concept "Store layout" (Damage), "Rules for every command" (Naming a
version).

- Classification per version: incomplete (sidecars, no `.tar.zst`);
  damaged by presence (a sidecar missing); damaged by head (listing head
  lacks a parsable `piece`, `archived` or `archive-bytes`; `.tar.zst`
  size differs from `archive-bytes`; checksum file not exactly one line
  for the archive's name); damaged by full decode (listing does not
  decode to its end). `list` uses presence and head; `status` and
  `push` add the full decode, in parallel on all cores but two like
  `grep` (concept:198-201).
- `--version` matching: input with `T` against stamps; otherwise each
  version's time rendered in the current zone (`jiff`, `TZ` honoured, so
  tests set `TZ`), as a text prefix; exact match wins; several matches
  name every stamp; text output adds the UTC offset where two versions
  of one piece render alike. Incomplete versions match only by stamp
  (`forget`, concept "forget").
- Builds on: F2, F3, F4 (the version fixture walks a tree with F4's walk,
  PO4).

### 4.5 Rules, walk, idle clock and markers (F4, F5)

Concept "Selection, walk and idle check", "Markers".

- F4 (section 3 rows for `tree.rs`, `walk.rs`, `rules.rs`, `filesystem`): the
  walk reads `.antiqignore` and `.antiq-timeless` as it descends (as
  `idle.rs` reads `.antiqignore` today), classifies every entry, and
  keeps checking ignored subtrees for mount points, nested collections,
  unreadable entries, exclude entries, depth, locked entries. A broken
  rule file of either kind refuses the piece as "rule file:
  <path>:<line>" (old spec "Idle", carried over; O7, decided). Locked
  means one of the flags of concept "Locked entries", or a directory
  (the top's parent included) that the user does not own and cannot
  write (O6, decided); a `0555` directory the user owns is not locked,
  since removal gives itself owner access.
- F5: `markers.rs` writes a marker by writing it into `.antiq/tmp/`,
  flushing it, granting owner write to the top if it lacks it, renaming
  over `<top>/<name>`, putting the mode back and flushing the top; a
  leftover stays in `tmp/`. It first compares the devices of the top and
  `.antiq/tmp/` and refuses with the reason "mount point" when they
  differ, instead of failing the rename with `EXDEV` (PF3).
  `.antiq-restored` holds one line, `restored <UTC time>[ version
  <stamp>]`; the parser is strict and reports the file on failure.
  `idle.rs` as in section 3.

### 4.6 Repairs, run and dry runs (F6)

Concept "Repairs at the start of a run", "Rules for every command".

- `run::start` takes the lock, then runs repairs 1 to 6 in order; a
  failed repair is a warning naming the path. `-n`: no lock, repairs
  computed read-only and returned as `repair` actions; a held lock
  returns the holder and no repairs. F6 provides the test helper that
  holds the lock from another process while a command's dry run runs;
  every command item uses it for its held-lock dry-run test (PO22).
- Repair 1: for each `tmp/pack/<stamp>/<path>/` with staged files,
  delete the sidecars of that stamp in `pieces/<path>/` when the
  `.tar.zst` is not there. The staging spelling of `<path>` is the store
  folder's spelling (archive stages under it), so the lookup is exact.
- Repair 2: empty `tmp/` (after repair 1, which reads it).
- Repair 3: delete every tree in `doomed/` without comparing; remove
  chains without a non-directory entry; empty parents up to `<stamp>/`.
- Repair 4: for each `trash/<stamp>/`, find the shortest path below it
  for which a version with that stamp exists (name equality through
  F2's exact lookup); rename the tree back to that path in the
  collection, or to `recovered/<stamp>/<path>/` when that fails for any
  reason; the rename back creates no missing parents, the chain inside
  `recovered/` is created and flushed (O13, decided); no version with
  that stamp: the first component goes back as it is.
- Repair 5: for each live piece whose top holds `.antiq-restored` with
  a stamp: delete what is left of that version (`delete_version`), then
  rewrite the marker to the time alone; name the deleted version.
- Repair 6: remove empty store folders and their empty parents.
- Giving a tree back (PO13): F6 owns `give_back` (rename back, else to
  `recovered/`), ported from `src/commands/archive/new/finish.rs:362-473` and
  `src/repair.rs:355-376` (beb748f), and exposes it; repair 4 and
  `archive` step 6 (C4) both call it.
- Builds on: F1, F2, F5.

### 4.7 Piece references (F8)

Concept "archive" (`<piece>`), "unearth" (`<piece>`). Rule decided in
8.4 (O1): every command that takes `<piece>` accepts a path or a search
word.

- Candidate sets: live pieces (selection) for `archive`, `pin`,
  `unpin`; store pieces (F2 scan) for `unearth`, `fork`, `rebuild`;
  store pieces plus sidecar-only folders for `forget` (O28).
- An input that names a candidate as a path (after `resolve_path`,
  reference.rs:149-190, and F2's exact lookup) is that piece; otherwise
  it is a search word, matched with the nucleo query ported into
  `src/query.rs` from `src/index.rs:500-540` (beb748f; smart case and
  smart normalization, index.rs:509-518), and must match exactly one
  candidate. None is `unknown_reference`; several are
  `ambiguous_reference` with the candidates (error.rs:266-288
  `details`). `src/query.rs` is public to `list` (C10) (PO12, PH5).

### 4.8 Commands

Every changing command: `run::start` (lock or dry run), repairs, its
work, its report; exit codes of concept "Exit codes". JSON payloads are
defined by the item and written into the spec section "JSON output" in
the same commit. A held lock in a real run is `lock_held` with `pid` and
`started` in the JSON (PO15).

- **status (C1)**: new `commands/survey.rs` judges every live piece (port the
  select, walk and judge loop of `src/commands/status.rs:385-395, 602-622` at
  beb748f) into `due`, `soon`, `pinned`, `refused` with every refusal of
  the walk as its reason (O14, decided), or plain; `commands/status.rs` adds
  the header (folder count, version count and the sum of
  `archive-bytes`, `idle_days`), the table sorted by path, and the notes
  of concept "status" (future times, damaged and incomplete versions with
  full decode, `recovered/`, `trash/`, `doomed/` leftovers, spelling
  differences, unparsable files in `pieces/`). A `doomed/` leftover is
  named with its first entry in walk order and a cause `status` can read
  (a locked flag, a directory it cannot write); the deletion's error
  itself is in the output of the run that failed and in later repair 3
  warnings (O2, decided). Lookups read only `NotFound` and
  `NotADirectory` as "missing" (the lesson of todo
  the status lookup todo, closed in C1). `soon` comes from `soon_days`. No lock;
  a held lock is a note.
- **init (C2)**: no question, no `archive.sh` detection
  (`has_archive_sh_layout`, `src/commands/init.rs:215-232`, goes), refusals
  as concept "init", the new layout and config, `-n` prints the config.
  After creating, it prints how many folders the next `archive` would
  archive, counted with C1's survey (PH7, decided; section 5.2).
  Deletes `interact.rs`, `cli/tty.rs`, `--yes`.
- **pin, unpin (C3)**: F8 reference to a live piece, F5 marker write,
  lock and repairs, `-n` says which. Port `PinAction`, `PinReport`,
  `Pinned`, `current_pin`, `folder_exists` (`src/commands/pin.rs` at beb748f,
  :48-62, 195, 287).
- **archive (C4)**: steps 1 to 8 of concept "archive", the crash notes,
  `archive <piece>`, `-n`. C4 exposes `store_version` (steps 2 to 4:
  pack into staging, verify, move in; the version time a parameter), which
  also holds the free-space check and reservation against the apparent
  size of the entries that are packed (O25, decided), so `import` gets
  the same rule (PS4, PO23). Builds on: F2 (stamp, `move_in`,
  `move_back`, exact lookup, `is_out_of_space` in `filesystem`), F3 (listing,
  checksum file, `file_sha256`), F4/F5 (walk, idle), F6 (run,
  `give_back`), F8 (reference), C1 (survey). Ports from beb748f: the
  parallel runner (`src/commands/archive/mod.rs:710-836`), `walk_and_judge`
  and `refused_by` (new.rs:466-525), `SpaceLedger` use (new.rs:631-644),
  `pack`/`write_package` (new.rs:730-774, 1031-1050), the trash re-walk
  (`differs`, `difference`, `walk_trashed`, finish.rs:48-305), and the
  store-free tests of Appendix A.3. New: staging in
  `tmp/pack/<stamp>/<path>/`, step 4 order, step 5 rollback, `doomed/`,
  parent-chain removal.
- **import (C5)**: concept "import" on C4's `store_version`; the source
  walked with the collection's rules and its own rule files, with the
  walk refusals; `# imported <time>`; `--time` (O11, decided); version
  time in the future refused naming the file. Ports `check_path`
  (`src/commands/import.rs:513-543`).
- **unearth (C6)** with the shared `commands/restore.rs`: concept "unearth"
  order, the flush method of section 5.1 (with the directory rule of
  section 3, `archive/extract.rs`), damaged refused through F3's shared
  verification, marker refusal, target refusals (port
  `check_target_on_disk`, `look_at`, `ancestors`, `TargetProblem`,
  `src/commands/unearth/mod.rs:146-167, 753-870`, with F2's exact lookup in
  place of `lists_exactly`; `check_target_names`, `make_parents`,
  `remove_created`, `move_into_place`,
  `src/commands/unearth/local.rs:437-573` without the tracking-bound
  `check_entries_on_volume` at 494-528), names per ADR 0054, free space for
  the listing's `bytes`, `.antiq-restored` with the stamp, version
  deletion, marker rewrite. Staging under `tmp/extract/<n>/` (plan
  decision; repair 2 empties it).
- **fork (C7)**: concept "fork" on `commands/restore.rs`; `--name <new
  path>` (a path now, not one name as in `src/commands/fork.rs:294-310`);
  refusal of a taken original path names `--name`; damaged version with
  a warning; without a usable listing the space check is skipped with a
  warning and the ENOSPC rule applies (O5, decided).
- **forget (C8)**: concept "forget"; `delete_version` per version;
  `--all-versions --force`; incomplete versions by stamp, also in a
  sidecar-only folder (O28); `-n` names the files.
- **rebuild (C9)**: concept "rebuild", all its cases and refusals, on
  F3's shared verification (`read::list`, whole-file hash, tar against
  listing) (PS5).
- **list (C10)**: concept "list"; head reads only; the query through
  `src/query.rs` (F8); SIZE in decimal units; `--json` adds unpacked
  size and file count; damaged and incomplete marked. No version, or no
  match for the query: exit 0 with a note on stderr (O27). A held lock
  is a note on stderr, and a `note` field under `--json`, exit 0.
- **grep (C11)**: concept "grep"; `regex::bytes` over each decoded
  listing, files in parallel on all cores but two (`std::thread::scope`
  with an atomic index, as `src/commands/archive/mod.rs:806-818` does);
  incomplete versions skipped; a listing that does not decode is skipped
  with a warning naming the version (O10, decided); terminal grouping,
  pipe lines, `--json`; unescaped matching, escaped display, raw pipe
  output; test with a tab and a backslash. No hit: exit 0 with a note on
  stderr; a pattern `regex` refuses: a usage error, exit 2 (O27). A held
  lock as for `list`.

### 4.9 Target and push (P1, P2)

Concept "push", "Configuration".

- P1 (`src/target/`): `url` (absolute path, any characters a TOML string
  holds, or `[user@]host:path`; a leading `/` means a path; host must
  not start with `-`) (PF10), `marker` (`.antiq-target`, one JSON line,
  fixed key order `format`, `id`, `host`, `path`; unknown keys ignored;
  higher `format` refused), `rsync` (resolve from `[target] rsync` or
  `PATH` as today, floor 3.2.4, openrsync refused naming the binary and
  `[target] rsync`; one runner returning exit status, stdout and stderr;
  argv builders for `--list-only <target>/`, the marker fetch, the
  marker send, the mirror and its `-n` pass, each with `-e`,
  `--rsync-path`, `--fsync` per `fsync`, and `rsync_args` after antiq's
  options; `LC_ALL=C` and the scrubbed environment of
  rsync.rs:422-434). `fake-rsync.sh` gains per-call answers (by call
  number) for multi-run tests.
- P2 (`src/commands/push.rs`): steps 1 to 9 of concept "push": repairs
  (lock for the whole run); store check (full damage scan, case and
  normalization clashes via `names::fold_path`, `trash/` not empty);
  rsync check; pre-check table (path target: canonicalize, refuse at or
  below the collection root by (device, inode) of its ancestors, refuse a
  mount root via `filesystem::device::is_mount_point`, errno classes of
  `src/remote/path.rs:145-163` at beb748f; ssh target: `--list-only`,
  exit 255 and 23 rules); marker read, compare, `--init`,
  `--force` (marker untouched); mirror with `-H`; second pass on `*deleting`; result codes 0,
  24, 23 with the IO-error line, 12 with the `--fsync` texts, 30;
  `-n` summary per piece from `--itemize-changes`; `--if-reachable`.
- No usable target (PS3): without `[target] url`, `push` fails with
  `no_target`, exit 1, naming the key and `push --init`; a `url` that
  P1's parser refuses is a config error naming the key, exit 1. The
  marker sent with `--chmod=F644` and without `-p` arrives with 0644
  masked by the receiving side's umask, which the spec says (PF8).
- Tests: the fake rsync for argv, exit statuses and texts; a real rsync
  3.2.4+ (Homebrew's `/opt/homebrew/bin/rsync` 3.5.1 here, Appendix A.1)
  on two temp directories for the path target, through `ssh-shim.sh`
  as `-e` for the ssh target without network; opt-in real sshd
  (`tests/support/sshd.rs`) for host-key and refused-key 255 lines.

### 4.10 Rules every item follows

- Lock, repairs, dry runs, markers, free space, exit codes and version
  naming as concept "Rules for every command"; the JSON envelope of
  `src/cli/output.rs:60-81` unchanged.
- Lookups that decide "not there" read only `NotFound` and
  `NotADirectory` as missing; any other error is reported (todo
  the status lookup todo, closed in C1).
- Directories antiq creates are flushed with their parent before a later
  step relies on them (F2 helper).

## 5. Measurements left to the plan

### 5.1 How `unearth` flushes the extracted tree (M1)

Concept "unearth" (Order): no version is deleted before the folder that
replaces it is durable; the method is measured here. Today nothing
flushes the extracted tree (`src/archive/extract.rs:397-481`,
`src/commands/unearth/local.rs` `place`; Appendix A.4). `flush_file` is
`File::sync_all`, which is `F_FULLFSYNC` on macOS, with a plain `fsync`
where the filesystem answers `ENOTSUP` (`src/filesystem/atomic.rs:20-46`).

Candidates on macOS:

- **A, per file**: `sync_all` on every extracted file before it is
  closed (hook at `extract.rs:441`), every directory flushed after
  `finish` set its mode and times (section 3, `archive/extract.rs`), then
  the parent after the rename.
- **B, one full flush**: plain `fsync` per file and directory, then one
  `F_FULLFSYNC` on a file of the staging volume before the rename. This
  rests on Apple's `fcntl(2)` (macOS 27, `man fcntl`), which M1's note
  quotes rather than re-derives (PF13): "`F_FULLFSYNC` Does the same
  thing as fsync(2) then asks the drive to flush all buffered data to
  the permanent storage device (arg is ignored). As this drains the
  entire queue of the device and acts as a barrier, data that had been
  fsync'd on the same device before is guaranteed to be persisted when
  this call returns. This is currently implemented on HFS, MS-DOS (FAT),
  Universal Disk Format (UDF) and APFS file systems."

Linux, without a measurement (PF4, lead decision): **C**, `syncfs` on the
staging volume once instead of per-file `fsync` (on Linux `sync_all` is
`fsync`, so A and B are the same there). `syncfs` reports writeback
errors only from Linux 5.8 on, and only those that happened since the
file descriptor it is given was opened (syncfs(2), man7.org, which M1's
note cites) (PO17). So the descriptor of the staging volume is opened
before the extraction starts, and on a kernel below 5.8 the flush falls
back to A.

M1, run by an agent; the lead starts it without asking the user again,
since the run is small (8.4):

- A throwaway crate outside the repository (as the grep study did,
  `docs/research/listing-grep.md`), release build. It does not need
  antiq's extraction: it writes the same tree shape with `std::fs`, then
  applies A or B.
- On this Mac's internal APFS volume only. Trees: T1 48 211 files of
  4 KiB each (about 200 MB; the file count of the `list` example of
  concept "list"), T2 1 000 files of 4 KiB, T3 one file of 100 MB. One
  run per strategy plus one unflushed control run per tree. At most
  about 2 GB of writes in total (8.4; PH3).
- Output: `docs/research/extract-flush.md` (findings only, per
  CLAUDE.md), committed by M1: the times, the `fcntl(2)` quotation, the
  `syncfs(2)` citation. M1 measures; it does not decide.

Decision rule, applied by the lead when M1 reports (thresholds accepted
in 8.4, O16): take A on macOS when its time for T1 is at most 1.5 times
the unflushed control and at most 30 s above it; otherwise B. A stays
where `F_FULLFSYNC` is refused (smbfs answers `ENOTSUP`,
atomic.rs:20-25). Linux takes C as above. C6 implements the chosen
method and states it in the spec section "unearth" in the same commit.

### 5.2 `init` prints the due count (C2)

Decided by the lead (PH7, 8.4): after creating the collection, `init`
prints how many folders the next `archive` would archive. Concept
"init" asked for it when it takes no more than a few lines on top of the
code `status` and `archive -n` run; in this plan both build on
`commands/survey.rs` (C1, section 4.8), so `init` opens the collection it
just wrote, calls the survey, counts `due`, prints one line and adds one
JSON field. C2 therefore depends on C1. The spec states the one
behaviour (concept "init", last paragraph).

## 6. Documentation and todos

### 6.1 Spec

D0 writes the new `docs/specs/spec-v1.md` from the concept and deletes
`docs/concept-v1-simplified.md` (concept "Documentation after
agreement"). Rules for D0:

- The spec states what is decided, without reasons (CLAUDE.md); the
  reasons go into the ADRs. Concept sentences that give reasons ("since
  the mirror would otherwise...") move to the ADR that carries the
  decision.
- Where the concept says a rule of the old spec applies unchanged
  (concept "Selection" mount points, unreadable entries; "Free space";
  "archive" step 6 and the write-after-walk and hard-link notes; "unearth"
  target and name checks; "Packaging" special files; "Collections and
  pieces" refusals of `init`), D0 copies that old-spec text into the new
  spec and strips what refers to removed parts (tracking entries, IDs,
  `--to`, remotes, live packages, junk). The old sections are
  "Collections and pieces" (old spec:46-78), "Walks" (136-190),
  "Operations" (1419-1430), "archive" (1454-1466, 1486-1491), "unearth"
  (1771-1820), "Packaging" (1140-1172, 1206-1226) and "Verifying"
  (1228-1266).
- Sections marked **Proposal** in the concept (archive flow, push flow,
  configuration) become plain spec text: the three review rounds agreed
  them (`plans/concept-review-ledger.md`, final passes).
- The spec keeps a "JSON output" section; D0 writes the envelope and
  the error-kind table as planned here (`not_implemented` included,
  marked as removed by Z; `target_unreachable` and `no_target` added),
  and each command item adds its payload.
- "Configuration" gains a short subsection on setting up a target: the
  `[target]` keys with an example, then `push --init` on an empty
  directory; `push` without `url` is `no_target` (PS3). The marker
  section says the marker arrives with mode 0644 masked by the
  receiving side's umask (PF8). `init` prints the due count (5.2).
- D0 delivers, in its report, a table from every concept section to the
  spec section that now carries it; the lead adds it to the plan todo,
  so later items can follow a concept citation of this plan into the
  spec (PS2, PO9).
- Items that refine a rule (field widths, error kinds, payloads, the
  flush method) change the spec in the same commit.

### 6.2 ADRs

Numbers stay; gaps are allowed; ADRs are rewritten in place until the
first release (ground rules of the old plan todo
01M6HJYBXSTQFSHP29EX3MPN97, its lines 68-70; concept "Documentation
after agreement").
D0 applies this table, which the lead confirmed for the review (O17,
8.4); 0012 and 0052 follow the decisions O18 and O4.

| ADR | Fate | Why |
|---|---|---|
| 0001-0005, 0010, 0022, 0028 | keep | untouched by the concept |
| 0006 traits | keep | still true (`Packager`); names no remote trait |
| 0007 index local for remote-only | delete | no tiers |
| 0008 no syncing across machines | rewrite | v1 has no multi-machine support at all (concept "Goal") |
| 0009 migrate playground archives | rewrite | through `import` of unpacked folders (concept "import") |
| 0011 `.antiq` directory | rewrite | the directory holds config, lock, staging and `pieces/`, no metadata or index |
| 0012 glob patterns | rewrite | the "selection strategy" sentences go (O18, decided): their todo closes in D0 and no strategy trait exists |
| 0013 idle by newest mtime | rewrite | ignore, timeless, `.antiq-restored`, future times count |
| 0014 ULID per folder | rewrite | identity is the path (concept "Concepts", "Piece") |
| 0015 local storage fixed, remotes behind a trait | delete | no remote layer |
| 0016 full metadata on remote | delete | no metadata documents |
| 0017 several machines share a remote | delete | one machine |
| 0018 back up live folders as packages | rewrite | live folders are mirrored as plain folders by rsync |
| 0019 zstd for archives and live packages | rewrite | archives only |
| 0020 `--json` implies non-interactive | rewrite | nothing asks; `--json` and data on stdout |
| 0021 search and bring back through one picker | rewrite | `list` and `grep`, no picker |
| 0023 command names | rewrite | `push` (not `backup`), `forget`, `rebuild`, `list`, `grep`; `find`, `sync`, `resolve`, `reconcile`, `remote` gone |
| 0024 pin file | rewrite | regular file or symlink; other entries refuse; written through `tmp/`; "backed up" becomes mirrored |
| 0025 resolve command | delete | no conflicts |
| 0026 remote add / init | delete | its subject moves into 0031 |
| 0027 symlinks as they are | rewrite | add: on the target a link points to whatever its path names there |
| 0029 `.antiqignore`, markers out of the idle check | rewrite | ignore now means not packed and deleted with the folder; timeless rules; control files only at the top |
| 0030 commit by claiming a number | rewrite | a version exists if and only if its `.tar.zst` exists; moved in last, deleted first |
| 0031 remote marker | rewrite | `.antiq-target`, its fields, `push --init`, `push --force` (marker never rewritten by antiq) |
| 0032 verified local archive, upload at next sync | rewrite | the archive flow with `trash/`, `doomed/`, rollback; `push` mirrors later |
| 0033 strict verify | rewrite | reclaim paragraph goes |
| 0034 unearth into staging | rewrite | no fetch; flush before rename; "already unearthed" sentence goes |
| 0035 reclaim | delete | no reclaim |
| 0036 ask before replacing | delete | nothing asks |
| 0037 idle clock of unearthed piece | rewrite | `.antiq-restored`, also written by `fork` |
| 0038 pieces without files | rewrite | "junk" becomes ignored paths and control files |
| 0039 ID time | delete | no IDs |
| 0040 other machines' live pieces | delete | one machine |
| 0041 back up nothing outside pieces | rewrite | `push` mirrors the whole collection |
| 0042, 0043, 0044 tiers and prune | delete | no tiers |
| 0045 brake | delete | no brake; the reason goes into 0049 |
| 0046 dry run everywhere | rewrite | the command list; repairs listed first |
| 0047 path remote first, 0048 ssh master | delete | rsync does the transport |
| 0049 rsync 3.1 | rewrite | one mirror run with rsync 3.2.4+, `--fsync`, `--delete-delay`, second pass, no brake |
| 0050 tar headers | rewrite | `hard` lines with checksums; reclaim mention goes |
| 0051 zstd level 19 | rewrite | only its reference to ADR 0052 goes (PH4) |
| 0052 live packages, pinned zstd | delete | live packages go and no output needs to be byte-stable; F3 removes the `zstd-sys` pin from Cargo.toml (O4, decided) |
| 0053 metadata as versioned JSON | rewrite | a version's metadata are the listing's comment lines |
| 0054 names | rewrite context | ADR 0017 goes; a version reaches another machine by copying from the target |
| 0055 import of archive files | rewrite | import of folders |
| 0056 console output and the picker | rewrite | no picker; `status` shows only pieces with a state |
| 0057 picker from ntropy | delete | no picker |
| 0058 pending metadata file | delete | no metadata |
| new 0059 | add | Repair interrupted runs at the start of every locked run (no `reconcile`, no state file) |
| new 0060 | add | Name versions by UTC stamp, file names from the piece path, version input rules |
| new 0061 | add | Remove a version once it is unearthed; `fork` keeps it |
| new 0062 | add | Repair damaged versions only on request (`rebuild`); `push` refuses while damage exists |

New ADRs are made with `adrs new --no-edit` (`docs/adrs/README.md`) and
follow its rules.

### 6.3 README

D0 replaces the plan bullets of `README.md` (README.md:13-34, today
naming `sync`, tiers, the picker and several machines) with the
concept's goal, and says the rework is in progress. Z writes the usage
section with the user documentation points below.

### 6.4 User documentation (Z)

A usage section in `README.md` carries them (O19, decided). Each point
of concept "Documentation after agreement" (concept:907-938)
becomes text there: mirror not backup and deletions; what is not kept;
symlinks on the target; checking a version by hand; rsync compares only
what it transfers; `fsync = false`, macOS targets, SMB and exFAT
`rsync_args`, ssh recommended; case-only renames and strict targets;
directories that became mount points; unreadable directories stop
deletions; damaged or incomplete versions stop `push`; `push --init`
and `--force`, and editing the marker's `host`/`path` by hand after a rename or move; copying back with `rsync -aH`; the scheduling line and its exit status; a path
target is a folder, not a volume root. Beyond the concept's list: how to
set up a target (`[target] url`, `push --init`; PS3), and that the hand
check uses `shasum`, since the `sha256sum` that macOS ships is not GNU's
and fails on a listing (PO10).

### 6.5 Todos

D0 closes (deletes, per `todos/README.md`) the 15 todos the user named
(scope todo, concept round 2):

| Todo | Title |
|---|---|
| 01M6JK2HCY29JY4HF66B53TPRH | Move confirmed archives to the remote-only tier |
| 01M6JK2HA4JTS15C6KNC74FPWX | Resolve version conflicts and divergence |
| 01M6JG79SGKMY0VGPHWC1R8RNF | Return the claimed version number with a commit error |
| 01M6J11464MD3X012QX4PX5BRQ | Remove local payloads that no metadata names |
| 01M6GKSF2151F4CHAT28AJYDG1 | Back up content outside every piece as a root live package |
| 01M6GKSF0CK8SHRE6M8RMJZM8Z | Keep a folder until its archive is on the remote |
| 01M6EYZM5B93BZY49ARNFF7H0Z | Chunked deduplicated remote format |
| 01M6EYBKGJZ22DSK9D77TCK82B | Several remotes per collection |
| 01M6EXZ2AKAS82GDR1N0EESRK1 | Switching the selection strategy |
| 01M6HXZ3WMPYJC28S3RB1CNKP1 | Index in a SQLite database |
| 01M6ESY4GEGYFC37C81N3PW6XG | Shared picker crate |
| 01M6JV59RPTP6QXMXST6WXHVHH | Find, unearthed-loss and prune decisions |
| 01M6ESY4F831GDCQY5Q70HD8AS | One-shot command for scheduled use |
| 01M6E17PET32XD9PC04AJ2PBZY | Define the v1 scope and architecture |
| 01M6HJYBXSTQFSHP29EX3MPN97 | Implement antiq v1 (replaced by the new plan todo) |

Further closings, decided by the lead (O20, 8.4):

- 01M6JY923GSB0SSZEGJS3882BQ (question the v1 scope): every point is
  decided in the concept; D0 closes it.
- the multi-machine forget todo, closed in C8 (forget command): the concept's `forget`
  replaces it; C8 closes it.
- The status lookup todo (closed in C1): the code it names
  goes in R1; the rule moves into section 4.10; C1 closes it.

Rewritten by D0 (they stay but name removed parts or point at the old
plan):

| Todo | Change |
|---|---|
| 01M6GPQTP37TBZD964TA2P7X9S xattrs | drop live packages; strict verify only |
| 01M6GQ88DMPMTZMHE5A592Q0K0 names the target cannot hold | context: a version copied from the target to another machine, not a shared remote |
| 01M6GX3A8T7TVYHDQZ90KM91AG sparse files | drop live packages |
| 01M6HN0XXCGG6VCX7WRK8RC9WN home network | `push --if-reachable` and ssh `ConnectTimeout`, not the remote |
| 01M6HWBHSHV10VT2QM12V5ES41 exFAT modes | "package" becomes version; still open |
| 01M6J68AS8PMAKFRX3BWJTAE8F hard-linked pieces | "comes back as unearthed" becomes "given back"; *Part of* the new plan |
| 01M6JAAYCYAHAJ0S2KBST2GJVZ listing caps | motivation: listings copied back by hand and read by `grep`; *Part of* the new plan |
| 01M6JK2HE4AM50SWDDNE2G7934 progress rows | `archive`, `unearth`, `push`; no `sync`; drop the pointer to I41 |
| 01M6JR4THG5RRC5JEB6CRNBY0Y one test binary | file counts after R1 |
| 01M6JV59GRDMJ7A79J03TFX5PV shell integration | point at the spec, not the concept |
| 01M6K6WN14S91E2X5TRQYZRHJ5 compare the target | point at the spec, not the concept |
| 01M6EWR3KTHNDDH9CDMZK9H19P named levels | "metadata" becomes "listing header" |

The new plan todo (kind `plan`, component `project`) holds this plan
after its review. The lead files it on main before R1, M1 and D0 start
(PO20); every item's step todo is *Part of* it, as in the old plan, and
D0 uses its ULID in place of 01M6HJYBXSTQFSHP29EX3MPN97 in the *Part of*
lines above.

## 7. Items

### 7.1 Ground rules for every item

Carried over from the old plan todo (its section 1) where they still
apply, plus the review's rules:

- The lead files the new plan todo on main before R1, M1 and D0 start
  (PO20). Each item runs in its own git worktree on a sonnet agent; the
  lead files its step todo (*Part of* the new plan todo) before it
  starts; the agent deletes the step todo in its finishing commit.
- Every commit passes `scripts/check.sh` (`cargo fmt --check`, `cargo
  clippy --all-targets -- -D warnings`, `cargo test`). Behaviour changes
  start with a failing test. Every changed production line is covered:
  `cargo llvm-cov --all-targets --lcov --output-path
  target/llvm-cov/lcov.info`, then `scripts/uncovered-lines.sh <base>`;
  lines no test can reach are listed with the reason in the delivery
  report.
- Items without callers yet: the dead-code rule of section 2.2 (an
  `allow(dead_code)` with a reason naming the consumer item, never `pub`
  to hide it; the named consumer removes it) (PS1, PO2).
- Docs travel with the code: the spec section an item implements, the
  ADR it touches, the todos it affects, in the same commit. After D0 the
  concept and the old spec are read with `git show beb748f:<path>` (the concept's final text with `git show a19b83f:<path>`, see the top note); a
  difference between the new spec and the concept goes to the lead, and
  the concept decides until the spec is fixed (PS2).
- MPL-2.0 header on every source, test and script file (CLAUDE.md);
  scripts pass `shellcheck`; dependencies with `cargo add`; `.context()`
  before every `?` in the binary; `expect` over `unwrap`.
- Tests assume a non-root user and assert their permission
  preconditions. Tests that need `shasum`, GNU `sha256sum` (checked by
  its `--version`, PO10) or rsync 3.2.4+ fail with a message naming the
  missing tool; they do not skip. A test that sets an immutable or
  append-only flag or takes a mode away clears or restores it in a drop
  guard that also runs on a panic, so the temp directory can always be
  removed (PF7, PO14). Fixtures pack with `ZstdSettings::fast` (PO16).
- An item edits only the files it owns plus the shared edits its entry
  names. S1 creates every new module file, so later items do not add
  module declarations. Two kinds of shared edit are allowed between
  parallel items, since they never touch the same lines: appending a
  helper to `src/cli/table.rs`, and removing one's own
  `allow(dead_code)` line.
- Ported code is taken from beb748f with `git show beb748f:<path>` and
  adapted; the delivery report names what was ported from where.
- Every command that takes the lock has a held-lock test of its real run
  (`lock_held`, `pid` and `started` in the JSON, PO15) and of its dry
  run (holder named, no repairs listed), the latter with F6's helper
  (PO22).
- Nothing is pushed.

### 7.2 Order and parallel groups

```text
before    the lead files the new plan todo on main
phase 0   M1 any time; D0 written alongside R1, merged right after it
phase 1   R1
phase 2   S1
phase 3   F1, then F2 and P1; F3 beside F1 and F2
          F4 after F1+F3; F5 after F4; F6 after F1+F2+F5
          F7 after F2+F3+F4; F8 after F2
phase 4   C1 after F5+F7; C2 after C1
          C4 after C1+F6+F8; C5 after C4
          C3 after F6+F8; C8, C9 after F6+F7+F8
          C6 after F6+F7+F8+M1; C7 after C6
          C10 after F7+F8; C11 after F7
          P2 after P1+F6+F7
phase 5   Z after all
```

Critical path: R1, S1, F1, F4 (with F3 done), F5, F6, C4, C5, Z. D0
must merge before F1 does. C1 waits for F7 (after F4) and F5 and runs
beside F6; it must be done before C4. M1 must have reported before C6.

| ID | Depends on | Parallel with |
|---|---|---|
| D0 | R1 merged; merges right after it (see its entry) | M1, R1 and S1 while being written; S1 and later items do not edit the docs and todos D0 rewrites |
| M1 | none | everything (writes only `docs/research/extract-flush.md`) |
| R1 | none | D0, M1 |
| S1 | R1 | D0, M1 |
| F1 | S1 | F3 |
| F2 | F1 (PO5) | F3, F4, F5, P1 |
| F3 | S1 | F1, F2, P1 |
| F4 | F1, F3 | F2, F8, P1 |
| F5 | F4 | F2, F7, F8, P1 |
| F6 | F1, F2, F5 | F7, F8, P1 |
| F7 | F2, F3, F4 (PO4) | F5, F6, F8, P1 |
| F8 | F2 | F4 to F7, P1 |
| P1 | F1 | F2 to F8 |
| C1 | F5, F7 | F6, F8, P1, C3, C6, C8 to C11, P2 |
| C2 | C1 | C3 to C11, P2 |
| C3 | F6, F8 | every other C item, P2 |
| C4 | C1, F6, F8 | C2, C3, C6 to C11, P2 |
| C5 | C4 | C2, C3, C6 to C11, P2 |
| C6 | F6, F7, F8, M1 | C1 to C5, C8 to C11, P2 |
| C7 | C6 | C1 to C5, C8 to C11, P2 |
| C8 | F6, F7, F8 | every other C item, P2 |
| C9 | F6, F7, F8 | every other C item, P2 |
| C10 | F7, F8 (PH5) | every other C item, P2 |
| C11 | F7 | F8, every other C item, P2 |
| P2 | P1, F6, F7 | C items |
| Z | all | none |

Ownership moves by the review, which the table reflects: F3 owns the
shared verification and `file_sha256` (PS5, PO8); F2 owns the exact
lookup and `is_out_of_space`, which goes into `filesystem/space.rs` so that F4
alone edits `filesystem/mod.rs` (`FilesystemError`); F6 owns `give_back` (PO13); F8
owns `src/query.rs` (PO12); C4 owns `store_version` (PS4); S1 adds the
`#[path]` lines for `tests/support/versions.rs` to `src/testing/mod.rs`
and `tests/support/mod.rs`, with the file as a stub, so F6 and F7 do not
both edit `src/testing/mod.rs`.

Every item from F1 on edits the new spec and so merges after D0. All C
items and P2 edit `docs/specs/spec-v1.md` in their own sections;
the lead merges them in the order they finish and resolves the
neighbouring-hunk conflicts.

### 7.3 Items

#### D0 Turn the concept into the spec, the ADRs and the todos

- Scope: section 6.1, 6.2, 6.3, 6.5 (6.2 and 6.5 as confirmed in 8.4).
- Files: `docs/specs/spec-v1.md`, `docs/concept-v1-simplified.md`
  (deleted), `docs/adrs/*`, `README.md`, `todos/all-notes/*`.
- Depends on: R1 merged. D0 merges right after R1 (8.4), because
  `src/meta.rs:1161-1178` reads the old spec's "Metadata" table in a
  test (`include_str!`), which D0's spec no longer has; R1 deletes that
  file.
- Tests first: none (docs). Check: every sentence of the new spec traces
  to a concept line or an old-spec line (validated-docs skill);
  `rg -n 'concept-v1-simplified' .` finds nothing; `adrs doctor` passes
  if installed.
- Report: the table from concept sections to spec sections (6.1).
- Risk: medium (a dropped rule becomes a silent gap for every later
  item). Opus review: yes, of the spec against the concept.

#### M1 Measure how to flush an extracted tree

- Scope: section 5.1 (macOS only, at most about 2 GB of writes).
  Throwaway crate outside the repository.
- Files: `docs/research/extract-flush.md` (new).
- Depends on: none; the lead starts it without asking the user again
  (8.4).
- Tests first: none (research).
- Risk: low. Opus review: no.

#### R1 Remove the multi-machine stack and the old store

- Scope: section 2 complete, in the two commits of 2.6, with the
  dead-code rule of 2.2.
- Files: everything in 2.1 to 2.5; `src/lib.rs`, `src/commands/mod.rs`,
  `src/archive/mod.rs` (module lines), `src/error.rs`, `src/run.rs`
  (repair call and two tests, 2.1), `src/cli/mod.rs`,
  `src/cli/cmd/mod.rs`, `src/cli/table.rs` (new, moved helpers),
  `tests/support/mod.rs` (module lines), `Cargo.toml`, `Cargo.lock`
  (PH10).
- Depends on: none (after the plan todo is filed).
- Tests first: the stub behaviour: `tests/cli.rs`
  `accepts_and_needs_a_collection!` for `status`, `archive`, `unearth`,
  `fork`, `import`, `pin`, `unpin` inside a collection answers
  `not_implemented`, exit 1 (new cases, failing before the stubs);
  `find`, `sync`, `reconcile`, `resolve`, `remote` are usage errors
  (exit 2); direct unit tests of every helper moved into
  `src/cli/table.rs` (PO24).
- Docs: none (D0 owns the spec; the old spec describes removed
  behaviour until D0 merges right after R1).
- Risk: high blast radius, low logic. Opus review: yes, of the edits to
  kept files (2.2) only.

#### S1 Lay down the new command surface and module tree

- Scope: section 4.1.
- Files: `src/cli/mod.rs`, `src/cli/cmd/*.rs`, `src/lib.rs` (module
  lines, `extern crate self as antiq;`), `src/commands/mod.rs`, every new
  module file of 4.1 (stubs with their empty error enums),
  `tests/support/versions.rs` (stub) with its lines in
  `src/testing/mod.rs` and `tests/support/mod.rs`, `src/error.rs`,
  `tests/cli.rs`.
- Depends on: R1.
- Tests first: `tests/cli.rs`: the help lists the 13 commands; each flag
  of 4.1 parses, `init --yes` included until C2; `push --init
  --if-reachable` and `push --force --if-reachable` are usage
  errors; `forget p` without a version and without `--all-versions
  --force` is a usage error; `forget p v --all-versions` is a usage
  error; every command but `init` outside a collection answers
  `not_a_collection`, and inside one `not_implemented`, exit 1 (PO21);
  `ErrorKind::ALL` holds `target_unreachable` (exit 4) and `no_target`
  (exit 1); a `lock_held` error wrapped in a command error still carries
  `pid` and `started` in its JSON (PO15).
- Docs: none; D0 writes "Command line" and the error-kind table as
  planned here.
- Risk: low. Opus review: no.

#### F1 Config, collection layout and machine name

- Scope: section 3 rows `config.rs`, `collection.rs`, `machine.rs`,
  `run.rs` (machine name), `tests/support/tree.rs`; `commands/init.rs` call of
  `ConfigDoc::new` and its `LAYOUT` (init.rs:123-131) so `init` writes
  the new layout and config. Shared edit in `src/remote/rsync.rs`:
  delete `Rsync::from_machine`, `TransferSettings::for_machine`, the
  `MachineConfig` import (rsync.rs:7, 146-152, 215-226) and their tests
  `from_machine_without_a_path_is_not_resolved` (rsync.rs:808),
  `the_settings_come_from_the_machine_config` (931),
  `the_default_machine_config_gives_the_default_timeout` (950),
  `from_machine_refuses_a_relative_path` (1309) and
  `from_machine_probes_the_stored_path` (1319) (PF11); they are the only
  users of `MachineConfig` outside `config.rs` after R1 (rg); P1 rebuilds
  them on `[target]`. Test fixtures that set `config.machine.name`
  (`src/run.rs:122`) change with it.
- Depends on: S1.
- Tests first: a config with each removed key (concept:808-813) is
  refused naming it, and one with several names all of them; defaults
  (`ignore = [".DS_Store"]`, `timeless = []`, `idle_days = 30`,
  `soon_days = 3`, `fsync = true`); `[target]` keys parse; an unknown
  `[collection]` or `[target]` key warns (O9); `init --yes` creates
  exactly `config.toml`, `tmp/`, `trash/`, `doomed/`, `recovered/`,
  `pieces/` (tests/init.rs; `--yes` until C2, PO11); the lock holder
  names `machine::detect()`.
- Docs: spec "Configuration", "Store layout".
- Risk: low. Opus review: no.

#### F2 The versions store

- Scope: section 4.2 without the real-version fixture (F7 fills it); the
  exact lookup and `is_out_of_space` (PO8).
- Files: `src/versions/{mod,stamp,names,store}.rs`, `src/filesystem/atomic.rs`
  (chain helper), `src/filesystem/space.rs` (`is_out_of_space`).
- Depends on: F1 (the `pieces/` accessor and the fixture layout, PO5).
- Tests first: stamps `2026-11-04T090211`, `-2`, `-10` parse and order
  (`-10` after `-9`); `-1`, `-0`, `-02` refused; a second version in the
  same second gets `-2`; names: `projects/scanner` gives
  `projects-scanner`, `a  b//c` style runs collapse, a 300-byte path is
  cut from the front at a `-` to fit 200 bytes with the longest suffix,
  a single 300-byte component is cut at a character boundary; parsing
  splits at the last `--`; scan finds versions, reports the incomplete
  versions of a sidecar-only folder without making it a store piece,
  ignores unparsable names; `move_in` order and flushes observed through
  a recording flush seam (pattern of `src/remote/mod.rs:63-160` at
  beb748f); `delete_version` deletes `.tar.zst` first; the last version's
  removal removes empty parents; the exact lookup under another case: the
  test probes the temp volume at run time (as `names.rs:1158-1175` does)
  and asserts the stored spelling on a case-insensitive volume and "not
  found" on a case-sensitive one, so it runs everywhere (PF6).
- Risk: high (the commit point). Opus review: yes.

#### F3 Listing format, checksum file, packing and shared verification

- Scope: section 4.3; section 3 rows for `listing.rs`, `archive/*`,
  `zstd.rs`, `pool.rs`, `testing/package.rs`, `testing/pool.rs`;
  Cargo feature `experimental` and the `zstd-sys` pin removed (O4,
  PH4); the shared verification and `file_sha256` (PS5, PO8).
- Depends on: S1.
- Tests first: the `shasum` and GNU `sha256sum --strict -c` test of
  concept:264 (fails today: the listing is JSON; `sha256sum` taken only
  when its `--version` names GNU coreutils, PO10); header field order
  and widths; escapes in both line kinds, including a name with a
  carriage return that `shasum` must read raw; a hard link gets a `hard`
  line and a checksum line equal to its target's; `files` and `bytes`
  count a hard-linked file once; `read::list` of a packed tar equals the
  packed listing entry for entry; `read_head` stops at `#`; `read_full`
  fails on a cut stream; checksum file round trip and `shasum -a 256 -c`
  in the folder; the archive check fails on another hash and on another
  size than `archive-bytes`; a tar compared with a listing that differs
  in one member names it; `ZstdSettings::fast` packs; `Pool::for_machine()`
  budgets 25 %.
- Docs: spec "Listing format".
- Risk: medium-high (an external format). Opus review: yes.

#### F4 Rules and the walk

- Scope: section 3 rows `tree.rs`, `walk.rs`, `rules.rs`, `filesystem/mod.rs`,
  `filesystem/tree.rs`, `filesystem/remove.rs`, `filesystem/moves.rs`, `archive/mod.rs` and
  `verify.rs` (`WalkRules`); section 4.5 F4 part.
- Depends on: F1 (keys), F3 (pack files).
- Tests first: `node_modules/` ignored is not packed, not listed, still
  walked: a mount point or nested `.antiq` inside it refuses the piece;
  `.git/` timeless is packed; `docs/*.log` in `a/.antiqignore` matches
  `a/docs/x.log`, not `a/b/docs/x.log` (concept:105-108); `.DS_Store`
  ignored by default; a broken `.antiq-timeless` refuses as "rule file:
  <path>:<line>"; an immutable file (`chflags uchg` on macOS, flags
  cleared by a drop guard, PF7) refuses as locked; the Linux flags, and
  a directory the user does not own and cannot write, come through a
  seam, since setting them needs root (PF1, PO14); a real `0555`
  directory the user owns is not locked and archives (mode restored by a
  guard); a fifo or directory named `.antiq-pin` or `.antiq-restored` at
  the top refuses ("is not a file"); the same names below the top are
  ordinary files; `remove_tree` refuses a locked entry anywhere in the
  tree, `move_dir` a locked moved directory or parent, before changing
  anything (PF12); a staged tree is verified with no rules.
- Docs: spec "Selection, walk and idle check".
- Risk: high (what is deleted with a folder). Opus review: yes.

#### F5 Idle clock and markers

- Scope: section 3 rows `idle.rs`, `pin.rs`; `markers.rs` (4.5).
- Depends on: F4.
- Tests first: clock = later of newest counted mtime and marker time; a
  file 1 h in the future makes the piece not due and is listed (old code
  ignores it past one day, idle.rs:161); exactly `idle_days` old is not
  due; timeless and control files do not count; a piece with only
  control files is empty; an unparsable `.antiq-restored` refuses naming
  it; marker write leaves no temp file in the top, restores a `0555` top's
  mode, and a crash before the rename leaves the temp in `tmp/`
  (seam); a top on another device than `.antiq/tmp/` is refused as
  "mount point" before any write (seam for the device, PF3); pin as
  symlink counts; `soon` within `soon_days`.
- Docs: spec "Idle clock", "Markers".
- Risk: medium. Opus review: no.

#### F6 Repairs, run and dry runs

- Scope: section 4.6; section 3 rows `run.rs`, `plan.rs`; `give_back`
  (PO13); the held-lock dry-run test helper (PO22).
- Files: `src/repair.rs`, `src/run.rs`, `src/plan.rs`,
  `src/testing/mod.rs` (lock helpers; removes their allowance),
  `tests/support/cmd.rs` (the helper for binary tests).
- Depends on: F1, F2, F5.
- Tests first, one per repair and per crash point (state built by hand
  with F2's operations): sidecars in `pieces/` plus a staged
  `.tar.zst` in `tmp/pack/` lose the sidecars (repair 1), while sidecars
  without a staged archive stay as an incomplete version; `tmp/` is
  emptied; a `doomed/` tree is deleted without comparison, empty chains
  go; a `trash/` tree goes back to its live path, to `recovered/` when the
  path is taken, its parent is missing or a file, with the chain inside
  `recovered/` created; the shortest prefix with a version decides the
  piece path, by name equality; no version: the first component goes
  back; a marker with a stamp deletes the rest of that version and
  rewrites the marker; a stamp naming nothing is dropped; empty store
  folders go and a sidecar-only folder stays; a failing repair is a
  warning; `-n` lists the repairs as `repair` actions and changes
  nothing; `-n` with the lock held names the holder and lists none.
- Docs: spec "Repairs", "Dry runs".
- Risk: high (deletes). Opus review: yes.

#### F7 Damage, version naming and the version fixture

- Scope: section 4.4; `tests/support/versions.rs` (section 4.2; S1
  created the stub and its module lines).
- Depends on: F2, F3, F4 (PO4).
- Tests first: each damage kind of concept "Damage" from the fixture
  (remove the listing, remove the checksum file, truncate the archive,
  cut the listing, head without `archive-bytes`, checksum file with two
  lines); incomplete = sidecars only; head-only check misses the cut
  listing, full check finds it; `--version 2026-11-04T090211`,
  `--version 2026-11-04`, `--version "2026-11-04 10:02"` under
  `TZ=Europe/Berlin`; exact wins over prefix; two matches list both
  stamps; the repeated hour at the end of daylight saving time adds
  the offset. The fixture itself is exercised from a library test and a
  binary test (PO1).
- Risk: medium. Opus review: no.

#### F8 Piece references

- Scope: section 4.7; the nucleo query ported into `src/query.rs`
  (PO12).
- Files: `src/reference.rs`, `src/query.rs`.
- Depends on: F2.
- Tests first: a path selects a live piece; a path with another case
  finds the store piece where the volume folds case (probed at run time
  as in F2); a search word matching one piece resolves, for each
  candidate set; two give `ambiguous_reference` with both paths; none
  gives `unknown_reference`; a path outside the collection gives
  `outside_collection` (existing kind); `forget`'s set includes a
  sidecar-only folder.
- Risk: low-medium. Opus review: no.

#### P1 Target layer: url, marker, rsync

- Scope: section 4.9 P1; `src/remote/` moves to `src/target/` (`git mv`),
  `src/lib.rs` line; `tests/fixtures/fake-rsync.sh` per-call answers.
- Depends on: F1.
- Tests first: url forms (absolute path with spaces and colons, `host:p`,
  `user@host:p`, `-host:p` refused, relative path refused); marker JSON
  byte-exact, unknown keys ignored, `format` 2 refused; version probe:
  `3.2.4` accepted, `3.2.3` and `3.1.0` refused (`3.1.0` passes today,
  rsync.rs:374-395), openrsync refused naming `[target] rsync`; argv of
  the mirror equals concept:708-717 with `rsync_args` after antiq's
  options; the runner returns stdout and stderr on exit 23.
- Risk: medium. Opus review: no.

#### C1 status

- Scope: section 4.8 status; `commands/survey.rs`, `commands/status.rs`,
  `cli/cmd/status.rs`, `tests/status.rs` (new); removes the allowance on
  `stdout_style`.
- Depends on: F5, F7.
- Tests first: the concept's example layout (concept:599-609) from a
  built collection: due, soon, refused with mount point (seam), with
  "locked" and with "rule file", pinned, plain counted only, the header
  line; notes for a future file, a damaged version, an incomplete one, a
  `recovered/` tree, a `doomed/` leftover with its first entry, a
  spelling difference, an unparsable file in `pieces/`; a held lock is a
  note; status writes nothing below `.antiq/` (port
  `status_changes_nothing...`, `tests/status.rs:428` at beb748f).
- Todos: closes the status lookup todo (O20).
- Risk: low-medium. Opus review: no.

#### C2 init

- Scope: section 4.8 init, section 5.2; deletes `interact.rs`,
  `cli/tty.rs`, `Context::interaction`/`terminal`, and `init --yes`
  (shared edit in `src/cli/mod.rs`); trims `id.rs` to what it still
  needs (PH11).
- Depends on: C1 (PH7).
- Tests first: `init` creates without a question and exits 0 (today it
  exits 3 without `--yes`, `tests/init.rs`); `init --yes` is a usage
  error (PO11); `-n` prints the config; the four refusals of concept
  "init"; no `archive.sh` proposal; the due count printed and in the
  JSON; the written `config.toml` carries a comment for every key and a
  commented-out `[target]` block with examples (8.5), and parses back
  to the defaults.
- Risk: low. Opus review: no.

#### C3 pin and unpin

- Scope: section 4.8 pin.
- Depends on: F6, F8.
- Tests first: pin by path and by search word; pinning twice says so; a
  piece without a live folder is refused; a top on another device is
  refused as "mount point" (PF3); `-n` says which and changes nothing;
  the held-lock cases of 7.1 (port the `tests/pin.rs` cases listed in
  Appendix A.6 at beb748f).
- Risk: low. Opus review: no.

#### C4 archive

- Scope: section 4.8 archive, `store_version` for C5 (PS4, PO23).
- Files: `src/commands/archive.rs` (may become `src/commands/archive/` with
  submodules declared inside it), `src/cli/cmd/archive.rs`,
  `tests/archive.rs`; removes the allowance in `filesystem/space.rs`.
- Depends on: C1, F6, F8.
- Tests first: an idle piece ends as three files in
  `pieces/<path>/`, its folder gone, `trash/` and `doomed/` empty; step 4
  order (listing, checksum, archive) by the flush seam; a refused rename
  in step 5 moves the files back in reverse and exits 3 (seam); a
  difference in step 6 renames the folder back, the version stays; with
  the path taken it lands in `recovered/<stamp>/<path>/`; a failing
  delete leaves the tree in `doomed/`; `archive <piece>` archives a piece
  that is not due, refuses a pinned one; ignored `node_modules/` is
  deleted with the folder and absent from the listing; two pieces in
  parallel; no room skips with exit 3, measured against the packed
  entries only; `-n` lists archives and refusals after the repairs.
  The six concept rules the review added (PS6): a live folder whose
  spelling differs from the stored one is archived into the stored
  folder (concept:133-138); a run whose pieces are only refused exits 0
  and names them (concept:312); a mount point that appears in an ignored
  subtree after the verify counts as a difference in step 6 and the
  folder goes back (concept:384-387); a change to an ignored file after
  the verify is no difference; `.antiq-restored` at the top is neither
  packed nor compared and goes with the folder; `archive <search word>`
  that matches exactly one live piece archives it (concept:354-356).
  Plus the ported store-free tests of Appendix A.3 and the held-lock
  cases of 7.1.
- Risk: high (deletes user folders). Opus review: yes.

#### C5 import

- Scope: section 4.8 import.
- Depends on: C4.
- Tests first: a folder outside becomes a version of its name, the
  source unchanged; `--as` with a path the patterns do not select is
  refused; a source inside the collection is refused; `.antiq-pin` at
  the source top refuses, `.antiq-restored` there is left out; an empty
  source is refused, also with `--time`; a future file time refuses
  naming it; `--time` sets the stamp; the listing has `imported`; the
  source's own `.antiqignore` applies and a mount point or nested
  `.antiq` in the source refuses (PH8); no room skips with exit 3
  (PO23); `-n` names the version and what it leaves out and changes
  nothing; the held-lock cases of 7.1 (PS7, PO22).
- Risk: medium. Opus review: no.

#### C6 unearth and the restore core

- Scope: section 4.8 unearth, the M1 decision.
- Files: `src/commands/restore.rs`, `src/commands/unearth.rs`,
  `src/archive/extract.rs` (flush hook), `src/cli/cmd/unearth.rs`,
  `tests/unearth.rs`.
- Depends on: F6, F7, F8, M1.
- Tests first: unearth restores the tree, deletes the version (archive
  first, by the flush seam), and leaves `.antiq-restored` with the time
  alone; every extracted file is flushed before it is closed and every
  directory after its final mode and times are set, before the rename
  (seam; PF2); a directory whose final mode keeps the owner out is still
  flushed; a crash after the rename leaves the stamp in the marker
  (repair 5 finishes; F6 test reused end to end); damaged version
  refused naming `fork`, `rebuild` and copying back; target refusals
  (port the cases of Appendix A.4); a top with a non-file marker name
  refused; no room exits 3; `--version` picks by stamp and by local
  date; `-n` names version, target and checks; the held-lock cases of
  7.1.
- Risk: high (deletes the version). Opus review: yes.

#### C7 fork

- Scope: section 4.8 fork.
- Depends on: C6.
- Tests first: fork to the original path keeps the version and writes
  the marker without stamp; a taken path names `--name`; `--name a/b`
  creates the parents; a damaged version extracts with a warning; no
  room for the listing's `bytes` exits 3, and a damaged version without
  a usable listing skips the check with a warning (PO23, O5); `-n` names
  version and target and changes nothing; the held-lock cases of 7.1
  (PO22).
- Risk: medium. Opus review: no.

#### C8 forget

- Scope: section 4.8 forget.
- Depends on: F6, F7, F8.
- Tests first: one version goes, archive first; the last version takes
  the store folder and empty parents; an incomplete version's sidecars
  go by stamp with the note, also in a sidecar-only folder; `--all-versions
  --force` removes all, incomplete ones included, naming each;
  `--all-versions` alone is a usage error (S1); `-n` names files and
  changes nothing; the held-lock cases of 7.1.
- Todos: closes the multi-machine forget todo, closed in C8 (O20).
- Risk: medium. Opus review: no.

#### C9 rebuild

- Scope: section 4.8 rebuild.
- Depends on: F6, F7, F8.
- Tests first: the three missing-file cases with their messages; a
  mismatch against the surviving checksum refuses with both hashes; a
  member differing from the surviving listing refuses; an archive size
  differing from `archive-bytes` refuses; a corrupt sidecar is not
  overwritten; a version that is not damaged is refused; the written
  listing has `rebuilt` and equals the archive's original listing
  otherwise; `-n` says what it would check and write and changes
  nothing; the held-lock cases of 7.1 (PO22).
- Risk: medium. Opus review: no.

#### C10 list

- Scope: section 4.8 list.
- Depends on: F7, F8 (PH5).
- Tests first: the concept's table (concept:545-550) from fixtures, newest
  first per piece, SIZE in decimal units, local time under a fixed `TZ`;
  the query through `src/query.rs`; damaged and incomplete marked;
  `--json` with UTC RFC 3339, unpacked size and file count; pipe output
  with stamps; no version and no match exit 0 with a note on stderr
  (O27); a held lock gives a note on stderr and a `note` field under
  `--json`, exit 0, text output as an insta snapshot (PS7, PO18).
- Risk: low. Opus review: no.

#### C11 grep

- Scope: section 4.8 grep; `cargo add regex`; removes the allowance on
  `line_os`.
- Depends on: F7.
- Tests first: the grouped terminal output of concept:567-575; name
  match row; pipe lines `piece\tstamp\tpath` and `-` for a name match;
  a name with a tab and a backslash: matched unescaped, shown escaped,
  printed raw in a pipe; incomplete versions skipped; an undecodable
  listing skipped with a warning; no hit exits 0 with a note on stderr;
  an invalid pattern is a usage error, exit 2 (O27); the held-lock note
  as for `list` (PS7, PO18).
- Risk: low-medium. Opus review: no.

#### P2 push

- Scope: section 4.9 P2; `tests/fixtures/ssh-shim.sh` trimmed of the
  master handling; removes the allowance on `is_mount_point`.
- Files: `src/commands/push.rs`, `src/cli/cmd/push.rs`, `tests/push.rs`,
  fixtures.
- Depends on: P1, F6, F7.
- Tests first: one test per pre-check row (concept:646-655); a damaged
  version refuses with the exact `rebuild`/`forget` command; a case clash
  between store folders refuses (case-sensitive volume needed, so a seam
  or Linux); a tree in `trash/` refuses; `--init` writes the marker with
  mode 0644 under a test umask of 022 (PF8) and leaves the root's mode
  and time; `--force` pushes despite a mismatching host or path and
  leaves the marker byte for byte as it was; a mismatching host refuses
  naming both; hard links of a live folder arrive as hard links (`-H`); the mirror with a real rsync keeps the target marker and deletes
  what the collection lost; the second pass after a case-only rename;
  exit 24 notes the vanished paths; exit 23 with the IO-error line says
  no deletion reached the target; exit 12 with the `--fsync` text names
  `rsync_path` and `fsync = false`; `--if-reachable` turns unreachable
  into 0 and leaves exit 1 alone; a held lock is exit 1 also under
  `--if-reachable`; `-n` summarizes per piece and writes no marker. No
  usable target (PS3): no `[target]` table, a table without `url`, an
  empty `url` and an unparsable `url` each exit 1, the first two with
  `no_target`. The no-real-ssh guard of `tests/support/cmd.rs`: an ssh
  target in a binary test without the shim reaches the stand-in and is
  unreachable (PO7).
- Risk: high (deletes on the target). Opus review: yes.

#### Z Finish

- Scope: cherry-pick i42's 6a90f18 (remove `not_implemented`;
  section 8, O15); grep `allow(dead_code` under `src/` and find only the
  allowances that exist on beb748f (2.2), removing any left; README
  usage and the user documentation (6.4); check `serde_json`'s
  `preserve_order` still serves a test (`tests/cli.rs:278-292` asserts
  envelope key order) before keeping or dropping it; a last pass of the
  spec against the code; delete the new plan todo.
- Depends on: every other item.
- Tests first: `ErrorKind::ALL` without `not_implemented`.
- Risk: low. Opus review: no.

## 8. Open points

Marks: **gap** (the concept says nothing), **contradiction** (two
statements disagree, or the concept disagrees with the code or the
bundles), **refinement** (the plan departs from the concept's wording
without changing behaviour), **lead** (a plan choice the lead confirms).
Each names the item that needs the answer before it starts.

State after the review: every point below is decided (8.4 and the
review ledger) and the body carries the decision; the entries stay as
the record of what was open and why.

### 8.1 Concept gaps and contradictions

- **O1 gap: what a search word matches.** `archive`, `unearth` (concept
  "archive", "unearth") take "a search word that matches exactly one
  piece"; `list` matches "fuzzily". The matching rule is not stated, nor
  whether `fork`, `forget`, `rebuild`, `pin` and `unpin` take search
  words, nor when an input is a path and when a search word. Decided as proposed:
  one rule for every `<piece>`: an input that names a candidate piece
  as a path (after `resolve_path`) is that piece; otherwise it is a
  search word, matched with the same `nucleo` pattern `list` uses
  (smart case, smart normalization, as `src/index.rs:509-518` built it),
  and must match exactly one candidate. Needed by F8, C10.
- **O2 gap: the error of a `doomed/` leftover in `status`.** Concept
  "archive" step 7: `status` reports the leftover "with the first entry
  that could not be deleted and the error". `status` is read-only and
  there is no state file, so it cannot know the error of a deletion it
  did not try. Decided as proposed: `status` names the leftover tree and its first
  entry in walk order, plus a likely cause it can read (a locked flag, a
  directory it cannot write); the error itself appears in the output of
  the run that failed and of each later run's repair 3 warning. Needed
  by C1.
- **O5 gap: free space for `fork` of a damaged version.** `fork` needs
  room for the listing's `bytes` (concept "Free space") and extracts a
  damaged version, whose listing may be missing or undecodable. Decided as proposed:
  without a usable listing, `fork` skips the space check with a warning
  and relies on the ENOSPC rule (step's staging removed, exit 3). Needed
  by C7.
- **O6 gap: "parent directory is not writable".** Concept "Locked
  entries". `remove_tree` gives itself owner access to directories it
  owns (old spec "Walks"; `src/filesystem/remove.rs`), so a `0555` directory
  owned by the user is no obstacle today. Decided as proposed: locked means a flag
  of the list, or a directory (the top's parent included) that the user
  does not own and cannot write (`faccessat` `W_OK` fails and the owner
  is someone else). Needed by F4.
- **O7 gap: a broken rule file.** The old spec "Idle" (old spec:214-218)
  makes a piece with a broken `.antiqignore` never idle, naming file and
  line; concept "Selection" carries rules over "unchanged, except where
  noted" but its `status` refusal list (concept:591-592) does not name
  it, nor `.antiq-timeless`. Decided as proposed: a broken rule file of either kind
  refuses the piece ("rule file: <path>:<line>") in `archive` and
  `status`. Needed by F4, C1.
- **O10 gap: `grep` over a listing that does not decode.** Concept
  "grep" skips incomplete versions only. Decided as proposed: a listing that fails
  to decode is skipped with a warning naming the version; the exit code
  stays 0. Needed by C11.
- **O11 gap: the format of `import --time`.** Decided as proposed: the input rules
  of "Naming a version" for a time: with `T` a UTC stamp, otherwise a
  local date-time with seconds (`2026-11-04 10:02:11`) in the current
  zone; a date alone is refused, since it names no second. Needed by C5.
- **O12 gap: foreign files in `pieces/`.** A file whose name does not
  parse as `<name>--<stamp><suffix>` (a `.DS_Store`, a hand copy) is not
  covered by "Damage". Decided as proposed: ignored by every command and by repair
  6 (a folder holding only such files is not "empty"), listed by
  `status` as a note. Needed by F2, C1.
- **O13 contradiction: parents under `recovered/`.** Concept "archive"
  step 6 says the tree moves to `recovered/<stamp>/<piece path>/`
  "(missing parents are not created)"; for a nested piece path the
  parents below `<stamp>/` never exist before. Decided as proposed: read the remark
  as belonging to "rename it back" (a missing live parent sends the tree
  to `recovered/`); the chain inside `recovered/` is created and flushed.
  Same for repair 4. Needed by F6, C4.
- **O14 contradiction: `status`'s refusal reasons.** Concept "status"
  lists mount point, nested collection, unreadable entry, exclude entry,
  unreadable marker; concept "Selection" adds "locked: <path>" and
  "<name> is not a file" (concept:93, 119) and O7 a broken rule file.
  Decided as proposed: `status` shows every refusal of the walk. Needed by C1.
- **O15 contradiction: what i42 carries.** Concept "What remains"
  (concept:896-898) says i42 carries "removing the unused error kind,
  the folder lookup fix". The bundle `i42.bundle` holds two commits
  beyond main e2491c5 (`git log e2491c5..f0fc5ec`): 6a90f18 (remove
  `not_implemented`) and f0fc5ec (a changed directory mode is no loss
  when an unearthed piece is archived again). The folder lookup fix
  (the status lookup todo, closed in C1) is in neither. Plan: 6a90f18 is
  cherry-picked in Z, not at the start, because the rework's stubs raise
  `not_implemented` until the last command lands; it touches only
  `src/error.rs` (`git show --stat 6a90f18`) and will conflict with S1's
  kind table, which Z resolves by hand. The objects are in the
  repository today (`git cat-file -t` answers `commit` for both); if gc
  has pruned them by then, `git fetch <bundle> i42:refs/tmp/i42` first.
  f0fc5ec does not apply: it changes the loss rule of reclaim
  (`src/commands/archive/unearthed.rs`, old spec "archive"), and reclaim is
  gone. The lookup fix is carried as the rule of section 4.10 and
  closes its todo in C1.
- **O27 gap: `list` and `grep` without a result, and a bad pattern.**
  Concept "list" and "grep" say nothing about no versions, no match or
  a pattern `regex` refuses. Decided by the lead (review, PS7): no hit
  or no version is exit 0 with a note on stderr; an invalid pattern is a
  usage error, exit 2. Needed by C10, C11.
- **O3 refinement: `archive/frames.rs` goes.** The concept's table keeps
  `archive/` "without `archive/live.rs`, `archive/manifest.rs`". `frames.rs`
  inspects zstd frames of archive files given to the old `import`
  (`src/commands/import.rs:30, 273, 434`, its only user); the new `import`
  takes folders, so R1 deletes it.
- **O21 refinement: `listing.rs` file-name helpers.** The concept's table
  says the listing's "file-name helpers move out of `meta.rs`". Those
  helpers (`meta.rs:80-139`, `listing::file_name`) name payloads by hash
  (`<16 hex>-<name>.tar.zst`); the new names (`<name>--<stamp>`) have
  nothing in common with them, so they are deleted and `versions::names`
  is written fresh.
- **O22 refinement: all of `cli/tty.rs` goes.** The concept removes "the
  question part"; every item in `cli/tty.rs` is question code
  (Appendix A.6), so C2 deletes the file.
- **O23 refinement: the flush method is measured by M1, not in the
  plan.** The plan was to be written without builds; section 5.1
  defines the measurement and the decision rule instead. Linux takes
  `syncfs` without a measurement (PF4).
- **O28 refinement: sidecar-only store folders.** Concept "Store layout"
  calls a folder under `pieces/` a piece of the store when it holds at
  least one `.tar.zst`; a folder with sidecars only is therefore no
  store piece, yet it holds incomplete versions that `status` reports
  and `forget` removes. The plan scans such folders for their
  incomplete versions, lets `forget` address them by path and stamp, and
  repair 6 keeps them, since they are not empty (PH9; sections 4.2,
  4.7).

### 8.2 Plan choices for the lead

- **O4 lead: ADR 0052 and the `zstd-sys` pin.** With live packages gone,
  no decision needs a pinned zstd (ADR 0052's reason is byte-stable
  live packages). `Cargo.toml` pins `zstd-sys = "=2.1.2"` citing it.
  Decided: D0 deletes ADR 0052 and drops 0051's reference to it; F3
  removes the pin. Needed by D0, F3.
- **O8 lead: the error kind for an unreachable target.** Decided as
  proposed: `target_unreachable`, exit 4; the old `remote_away` goes in
  R1. Review addition (PS3): `no_target`, exit 1, for `push` without a
  `[target] url`. Needed by S1.
- **O9 lead: notes the concept does not mention.** Today every command
  warns about unknown `[collection]` keys and about a collection on SMB
  or NFS (`src/report.rs`). The concept keeps "the JSON envelope of the
  current code" but names neither note. Decided as proposed: both stay,
  and unknown keys in `[target]` warn too. Needed by F1.
- **O16 lead: M1's thresholds** (1.5 times and 30 s, section 5.1).
  Decided: accepted, with M1 cut to about 2 GB of writes on macOS only.
- **O17 lead: the ADR table** (section 6.2), including the four new ADRs.
  Decided: as it stands, with O4 and O18 applied.
- **O18 lead: ADR 0012's "selection strategy" sentences.** The todo about
  switching strategies closes in D0; no strategy trait exists in the
  code (`rg Strategy src` finds nothing). Decided: dropped; D0 rewrites
  0012.
- **O19 lead: where the user documentation lives.** Decided as proposed:
  a usage section in `README.md`. Needed by Z.
- **O20 lead: closing three todos beyond the user's fifteen** (section
  6.5). Decided: closed as proposed (D0, C1, C8).
- **O24 lead: the version time of `archive`.** Decided as proposed: the
  UTC second at which the piece's packing starts (section 4.2). Needed
  by F2, C4.
- **O25 lead: the free-space figure of `archive`.** Concept "Free space"
  says "a piece's apparent size"; ignored paths are not packed. Decided
  as proposed: the apparent size of the entries that are packed, checked
  in C4's `store_version`, which `import` shares. Needed by C4.
- **O26 lead: the cutting approach.** The plan stubs the old local
  commands and ports store-free code from beb748f instead of keeping
  them alive through the rework (section 1). Between R1 and C4, C6, C1
  the binary cannot archive, unearth or report status. Decided:
  confirmed, since v1 is unreleased.

### 8.3 Questions for the lead (answered)

- D0 and R1: they merge back to back, R1 first (8.4).
- The new plan todo: the lead files it on main before R1, M1 and D0
  start (PO20; this replaces the first answer, "D0 files it").
- R1 stays one agent run with two commits (8.4).

### 8.4 Lead decisions (record)

Before the review (2026-11-04):

- Accepted as proposed: O1 (one rule for every `<piece>`, a path first,
  else a nucleo search word that must match exactly one piece), O2, O3,
  O5 to O15, O21 to O25.
- O4: ADR 0052 and the `zstd-sys` pin go.
- O16: thresholds accepted; M1 at most about 2 GB of writes (T1 48 211
  files of 4 KiB, T2 as planned, T3 one file of 100 MB, one run per
  strategy plus an unflushed control); the lead starts M1 without asking
  the user again.
- O17: the ADR table as it stands. O18: 0012's strategy sentences go.
  O19: a usage section in `README.md`. O20: the three extra todos close.
  O26: stub and port confirmed.
- 8.3: R1 and D0 back to back; R1 one run with two commits.

During the review (ledger `plans/rework-plan-review-ledger.md`):

- O27: `list`/`grep` without a hit exit 0 with a stderr note; an invalid
  `grep` pattern exits 2.
- PH7: `init` prints the due count from C1's survey; C2 depends on C1.
- PF4: Linux uses `syncfs` without a measurement; M1 runs on macOS only.
- PO20: the lead files the new plan todo on main before R1, M1 and D0.

### 8.6 M1's result and the lead's decision (2026-11-04)

M1 (`docs/research/extract-flush.md`, main 60d3387) measured on the
internal APFS SSD: T1 control 2.45 s, A 225.4 s, B 3.14 s. By the rule of
5.1, A fails both thresholds (91.9 times, 223 s above), so **C6 uses B on
macOS**: plain `fsync` per file and directory, then one `F_FULLFSYNC`
before the rename; **C on Linux** (one `syncfs` on an fd opened before
extraction, A on kernels before 5.8); A only where `F_FULLFSYNC` or
`syncfs` is refused. C6 states the method in the spec section "unearth".

### 8.7 Lead notes from delivered items (2026-11-04)

- From S1 (cef1a2b): a command error that wraps a `LockError` or a
  `RunError` is never `#[error(transparent)]`, since `find_held_lock`
  in `src/error.rs` walks the source chain and a transparent wrapper
  hides the lock (pinned by a test). Every C item follows this.
- C4 adds the end-to-end test that a wrapped `lock_held` keeps `pid`
  and `started` in the JSON envelope (PO15); S1 tests only the walker.
- F8 changes the `ambiguous_reference` candidates to a list of
  `{path}` as the spec says; the code still emits `{id, path}`.
- C2 removes `init --yes` (the spec's `init` has none).
- `forget --force` conflicts with `<version>` in clap (a `requires` on a
  bool flag is not enforced); the usage errors of 4.1 hold and are
  pinned case by case in `tests/cli.rs`.
- Tests that need external tools fail with a message naming the tool
  when it is missing, never skip: rsync 3.2.4 or later (P1's real-rsync
  tests) and GNU `sha256sum` (F3's listing tests; uutils and Apple's
  `sha256sum` are refused, PO10). Z documents these developer
  requirements in a "Development" section of the README,
  together with how to run `scripts/check.sh` and the coverage step.
- From F2: C4 keeps the staging tree when a move into the store ends in
  `PartlyMoved`, and on `StoredNotDurable` the piece stops before step
  5: its folder stays live and the run exits 3; the spec's "Spelling" section
  gets a sentence that on a case-folding volume a new store folder below
  an existing parent takes the parent's stored spelling (F8, which owns
  the lookup by spelling).
- From F2's review (VR8, accepted as delivered): `VersionsError` paths
  are relative to the root the function was given, so a staging path
  and a store path read alike. C4 and F6 wrap these errors with the step
  and the place ("while moving the version into the store", "while
  putting the files back into tmp/pack/...") so the user can tell where
  to look.
- From F8 (3428c8e): C items resolve pieces with
  `reference::resolve_piece` and the matching `CandidateSet` (live for
  archive, pin, unpin; store for unearth, fork, rebuild;
  store_and_sidecar_only for forget). A command error that wraps a
  `ReferenceError` uses `#[from]` or `source`, never
  `#[error(transparent)]`, or the `candidates` of an ambiguous reference
  are lost. An empty search word matches every candidate; each C item
  refuses an empty piece argument as a usage error (exit 2).
- For Z (found by C8): `init`'s dry-run action uses the key `action`
  where every other command uses `kind`; Z unifies it to `kind` with a
  test, and the spec's JSON output section with it; a real `init` also
  stops emitting the empty `actions` key, which the spec names for the
  dry run only (found by E1).
- For F7: sidecars without their `.tar.zst` are an incomplete version
  only when `versions::store::is_being_stored` finds no staged archive
  for them in `tmp/pack/`; otherwise they are repair 1's and not
  reported (spec "Repairs at the start of a run"). `status` and `list`
  run without the lock and see this state during a running `archive`.
- For P2: every `pub` item of `src/target/` has a caller once P2 lands
  (being `pub` hides dead code from Z's sweep); a collection root that
  is not UTF-8 is refused, since the marker stores strings; refusals of
  a url name the `[target] url` key.
- For P2 (spec gap found by E0): the spec does not give the JSON shape of
  `push --if-reachable` when the target is unreachable (exit 0, the
  reason printed "in text and `--json` every time"). P2 defines it in the
  spec's Push and JSON output sections; the scenario test asserts it.

### 8.5 User decisions after the plan review (2026-11-04)

- `--take-over` goes. `push --force` pushes despite a host or path
  mismatch and leaves the marker unchanged, so the machine it names
  pushes on as before; antiq never rewrites the marker; after renaming
  the machine or moving the collection the user edits the marker's
  `host` or `path` by hand. The user's reason: a hand copy used for a
  while and pushed back once must not take the target over.
- The mirror keeps hard links: `rsync -a -H`. The user's reason: without
  it hard links of live folders become separate files and waste space.
  Copying the target back keeps them only with `rsync -aH`.
- `init` writes a commented `config.toml` with examples: every
  `[collection]` key with its value and a short comment saying what it
  does, and a commented-out `[target]` block with example values for
  each key (url forms, `rsync`, `rsync_path`, `fsync`, `rsync_args`).
  The user's reason: the config explains itself where it is edited.
  Item C2; the spec's "init" and "Configuration" sections show the
  written file.
  Lead refinement (2026-11-04, C2): the keys `init` sets (`id`, and
  `patterns` or `exclude` when given) are active lines; every other
  `[collection]` key is written commented out with its default value and
  its comment, so a later change of a default still reaches existing
  collections. The user is told and may veto.
- `archive` with named pieces (asqr 01M6M0JCV8HAZA8RH73NRWAFXE,
  2026-11-04): a named piece that is pinned or refused makes the run exit
  1; when some named pieces are archived and others are not, it exits 3.
  `archive` without an argument keeps exit 0 for refused pieces. The
  user's reason: a script that names a piece learns that nothing
  happened. Item C4.

### 8.9 Module names (user decision, 2026-11-04)

Three modules get names that say what they hold: `src/fsx` becomes
`src/filesystem` (`FsxError` becomes `FilesystemError`), `src/ops`
becomes `src/commands` (the behaviour of each command; `src/cli/cmd`
stays), and `src/pack` becomes `src/archive` (the `.tar.zst` file of a
version: packing, reading, checking, extracting; the command stays
`commands::archive`). The user's reason: home-made shorthands such as
`fsx`, `ops` and `pack` make the code harder to read, while readable
short forms stay (`relpath`, `sysmem`, `id`, `cmd`); asqr's module
names are the model. New modules follow the same rule.

Item N1 does the rename as one mechanical step once F4 and E0 are
merged; items running then rebase onto it: `git mv`, every `use` line,
type and doc reference, the paths in this plan todo, and no behaviour
change; `scripts/check.sh` green.

### 8.8 Scenario tests (user decision, 2026-11-04, asqr 01M6KN9EE9TNBA3YM4XYS7Z2KB)

The use cases of the scope round, extended, become integration tests
that drive the real binary through a whole user task. Every scenario
runs twice, once with text output and once with `--json`: both runs end
in the same state, and every JSON object parses with the fields the spec
names. The user's reason: so that every use case is known to work for
scripting as well. Depth per scenario is discussed with the user before
it is written; a scenario that costs too much is dropped after asking.

Scenarios and the items each needs merged first:

| Scenario | Needs |
|---|---|
| init: the commented config parses back to the defaults | C2 |
| archive everything due: version in the store, folder gone, listing and checksum file present | C4 |
| archive one piece now, not yet due | C4 |
| dry run (`-n`) shows the plan and changes nothing | C4 |
| ignore and timeless: `node_modules` not packed and deleted with the folder; a fresh `.git` keeps nothing young | C4 |
| pin keeps a due folder; unpin lets the next run archive it | C3, C4 |
| find by search word: `list scan` finds `projects/scanner` | C10, F8 |
| list versions: local time in text, UTC in JSON | C10 |
| unearth by name: folder back byte for byte, version gone, `.antiq-restored` written | C6 |
| unearth a given version: by local date and by UTC stamp | C6 |
| fork: with `--name` and without; the version stays | C7 |
| forget one given version; forget all with `--all-versions --force`; the empty store folder goes | C8 |
| grep finds a path in two versions of two pieces | C11 |
| status shows due, soon, pinned and damaged | C1, C3, F7 |
| unearthed and later archived again: the idle clock restarts, a new version follows | C4, C6 |
| import a folder as a version, the folder stays | C5 |
| verify a version without antiq: `shasum -a 256 -c`, `tar`, listing against the files | C4 |
| a damaged version: status shows it, push refuses, rebuild repairs, push works | C9, P2, C1 |
| push to a path target: `--init`, the target is a copy, a second push removes what is gone | P2 |
| scheduled run with the target missing: `archive; push --if-reachable` exits 0 with a note | P2 |
| worked on elsewhere and pushed back: host or path mismatch refused, `--force` pushes and leaves the marker, a hand-edited marker pushes normally | P2 |
| restore everything from the target with `rsync -aH`: hard links kept, antiq works there, unearth works | P2, C6 |

Not chosen: an interrupted run repaired by the next one, two runs at
once.

Depth chosen by the user (asqr 01M6KYYV6Q0PYXD0KKD2EV13Z0, 2026-11-04):
- init, medium: the config is written and parses to the defaults, the
  due count is named, a second `init` is refused.
- find, deep: `list scan` finds both pieces, `list scanner` one, a word
  with no hit says so on stderr with exit 0; plus smart case, a path
  instead of a word, and an ambiguous word with `unearth` once it exists.
- list, medium plus the repeated hour of daylight saving time: newest
  first, local time on a terminal, stamps in a pipe, UTC in JSON,
  damaged versions marked; plus two versions in the repeated hour shown
  with their offsets.
- grep, medium: grouped terminal output, one pipe line per hit and
  version, a piece name match, no hit with exit 0.
- status, medium plus future times: every state in the table, a damaged
  version as a note, nothing written below `.antiq/`; plus a file with a
  time in the future (pinned joins once `pin` exists).

Depth of the other scenarios (asqr 01M6M06PAR8A9YSNDNQ4RDAXT7,
2026-11-04): deep for archive everything due (with hard links,
symlinks, an awkward name, the unpacked archive equal to the original
byte for byte), unearthed and archived again (changes made after
unearthing land in the new version, the old one is gone), unearth by
name (hard links, symlinks, modes and times kept) and unearth a given
version (with the repeated hour of daylight saving time); push medium
plus a folder whose name changes only in case (no `push -n` case); every
other scenario medium as offered.

Work: item E0 built the harness (main 0693f5b, `tests/support/scenario/`,
how to write a scenario is in its module documentation; scenarios go to
`tests/scenarios/<group>.rs`); the lead starts a scenario agent for each
group of scenarios once the items it needs are merged, extending the
harness as scenarios need it, so the harness keeps one structure. All
scenarios are in before Z.

## Appendix A. Code map (evidence for the sections above)

Gathered by reading main beb748f (rg, Read; no build). Line numbers are
those of beb748f.

### A.1 rsync, marker, url

- `src/remote/rsync.rs` (1373 lines, about 435 code). rsync comes from
  `machine.rsync` (`Rsync::from_machine`, rsync.rs:148-152); `resolve`
  searches a configured path, then `PATH` (absolute entries), then
  `/opt/homebrew/bin`, `/usr/local/bin` (rsync.rs:103, 142-143,
  338-371). `probe` runs `--version` with a scrubbed environment
  (rsync.rs:115-136); `parse_version_line` refuses openrsync and versions
  below 3.1 (rsync.rs:374-395); `RsyncVersion` has major, minor, patch
  (rsync.rs:88-93), so the 3.2.4 floor needs the patch in the compare.
  Refusal texts rsync.rs:27, 66-84. argv today: `--stats
  --no-human-readable --no-whole-file --ignore-times --timeout=N -e ...
  [--rsync-path] [--partial-dir=.antiq-partial] -- src dst`
  (rsync.rs:165-197). `join_rsh` quotes the `-e` words
  (rsync.rs:308-324, tested against real rsync rsync.rs:995).
  `scrub_environment` sets `LC_ALL=C`, drops `RSYNC_*` and `CVSIGNORE`
  (rsync.rs:422-434). `Transfer::run` uses `.output()` and parses stdout
  only on success; any nonzero exit is `RsyncError::Failed{code,stderr}`
  (rsync.rs:251-265). `parse_stats`/`TransferStats` are upload-only
  (rsync.rs:282-299). `TransferSettings::for_machine` reads
  `MachineConfig` (rsync.rs:215-226). Tests rsync.rs:436-1373 use
  `tests/fixtures/fake-rsync.sh` (`Fake`, rsync.rs:447): probe tests
  504-600, environment 602-657, resolve 660-815, argv 850-995, stats
  1065-1118, failures 1121-1280, real-rsync `#[ignore]` 1283, 1344.
- `src/remote/marker.rs` (389 lines): file `antiq-remote.json`
  (marker.rs:19), `Marker{format, id: ObjectId, layout}`
  (marker.rs:47-51), one JSON line plus newline (marker.rs:63-68);
  parse ignores unknown keys, refuses format/layout above 1 (`TooNew`)
  and 0 (`Invalid`) (marker.rs:73-89); `write_marker` is create-only
  through `TreeRoot::rename` and the `Durable` trait (marker.rs:109-154).
  Tests 156-250 (format) adapt; 253-389 (`Recording`, create-only) go.
- `src/remote/url.rs` (366 lines): absolute path or
  `ssh://[user@]host[:port]/path` (url.rs:43-70); path alphabet
  `[A-Za-z0-9._/-]` and component rules (url.rs:137-160); errors
  url.rs:12-37 (Empty, Form, Character, EmptyComponent, DotComponent,
  TrailingSlash, BareRoot, NoHost, NoPath, Name, HostOption, Port).
- `src/remote/mod.rs` (745 lines): `Durable`/`RealDurable`
  (mod.rs:42-61) and test `recording` (mod.rs:63-160) are used only by
  marker.rs and path.rs; `RemoteState`, `Reachability`, `Assessment`,
  `Remote` trait, `open`, `probe_state` (mod.rs:171-491), `RemoteError`
  (mod.rs:688-745). `RemoteState` is used by kept-but-rewritten code:
  `src/commands/status.rs:21-22`, `src/commands/archive/mod.rs:32`.
- Unreachable errno classification exists only in `src/remote/path.rs`
  `root_error_state` (path.rs:145-163: NotFound -> missing; NOTCONN,
  HOSTDOWN, HOSTUNREACH, NETUNREACH, TIMEDOUT -> away). It moves into
  the push pre-check before path.rs goes.
- ssh 255 classifier `src/remote/ssh/classify.rs:69-123` (away: `ssh:
  connect to host ... port ...:`, `ssh: Could not resolve hostname`,
  `Timeout, server`, banner timeout; broken: permission denied, host
  key). The concept's rule is simpler (two prefixes or "timed out").
  Fixture texts worth keeping for push tests: `connect-refused.txt`,
  `resolve.txt`, `timeout.txt`, `host-key-changed.txt`,
  `host-key-unknown.txt`, `permission-denied.txt`
  (`tests/fixtures/ssh-texts/`). ssh options today
  `src/remote/ssh/session.rs:644-699` (BatchMode, ConnectTimeout 30,
  ServerAliveInterval 15, ServerAliveCountMax 4, plus mux options).
- `tests/fixtures/fake-rsync.sh` (62 lines): answers from files beside
  its link (`version`, `version_exit`, `stdout`, `stderr`, `exit`,
  `signal`), logs argv to `calls.log` and env to `env.log`. One answer
  set per directory, so push tests with several rsync calls per run
  need per-call answers (an extension). `tests/fixtures/ssh-shim.sh`
  (272 lines) runs the remote command locally and works as rsync's
  `-e`; its `fault` files print an `ssh-texts/*.txt` and exit 255; its
  master/mux handling is dead weight. `tests/support/sshd.rs` (184
  lines) starts a real sshd for opt-in tests.
- `src/commands/remote.rs` (2565 lines) `remote add --init` checks: parent
  must exist, only the last component is made, a non-empty foreign
  directory is accepted unless it holds `objects` or `.antiq-clock.*`
  (commands/remote.rs:825-893). None of it matches the concept's table; the
  file goes whole.

### A.2 pack and listing

- The listing today is zstd JSON lines with a `{"format":1}` header
  (`src/listing.rs:18-23, 68-85, 148-150`), types
  `file|dir|symlink|hardlink` (listing.rs:27-36), `%`-escaped paths
  through `relpath::escape` (listing.rs:52-66; relpath.rs:19-63). API:
  `write` -> `Written{size,sha256}` (listing.rs:137-165), `Reader` and
  `read_all` (listing.rs:172-265), `file_name(payload_sha256)`
  (listing.rs:267-279), `check_entry` (listing.rs:109-131),
  `is_sha256_hex` (listing.rs:101), `ListingError` (listing.rs:281-304).
  Tests listing.rs:306-908 are almost all format tests. So `listing.rs`
  is a rewrite; the in-memory `ListingEntry`/`ListingType` model can
  stay (Hardlink maps to `hard`, Symlink to `link`).
- `Packager` trait and `TarZstd` (`src/archive/mod.rs:187-292`); `pack`
  takes a walked `PieceTree`, container name = last component of the top
  (mod.rs:233-261); `Packed{size, sha256, listing, notes}`
  (mod.rs:97-107). Tar headers `src/archive/tar_write.rs:80-114`; hard
  links become tar link members to the first path by (dev, inode)
  (tar_write.rs:170-177, 224-237, 319-335) and today a listing entry of
  type `hardlink` without sha256 (tar_write.rs:334). The concept wants a
  checksum line for `hard` entries too, so the packer must carry the
  first path's hash to its links. Special files skipped with
  `PackNote::SpecialFileSkipped` (tar_write.rs:239-245). Sparse warning
  tar_write.rs:450-496, used by `src/commands/archive/new.rs:49, 1133`.
- zstd: archive `ARCHIVE_LEVEL` 19, window log 27, decoder max 27
  (`src/archive/zstd.rs:11-26`); live-only `LIVE_LEVEL`, `LIVE_JOB_SIZE`,
  `Preset::Live`, `ZstdSettings::live`, live branches of `encoder()`
  (zstd.rs:20-51, 78-84). `ZstdSettings::live(1)` is also the cheap
  level of test helpers: `src/testing/package.rs:36, 57`,
  `src/archive/mod.rs:425`, `src/archive/extract.rs:686, 872`,
  `src/archive/frames.rs:444-446, 1077`, `src/commands/archive/new.rs:2363`.
  These move to a public `ZstdSettings::fast` (F3, PO16) before `live` goes.
- Pool: `Pool::for_machine(memory_percent)` -> `budget_for(ram, pct)`
  (`src/archive/pool.rs:123-129, 325-348`); archive costs pool.rs:20-28;
  live costs `src/archive/live.rs:28-39`. `memory_percent`:
  `src/config.rs:118-120, 142, 173-187, 299-300`, callers
  `src/cli/cmd/archive.rs:33`, `src/cli/cmd/resolve.rs:33`,
  `src/commands/sync/mod.rs:391`, `src/testing/pool.rs:20` (hard 25).
- `src/archive/frames.rs` is a zstd frame inspector used only by
  `src/commands/import.rs:30, 273, 434` (import of archive files). The new
  `import` takes folders, so frames.rs goes with the old import.
- `src/archive/manifest.rs` users: `src/commands/sync/live.rs:30, 1271`,
  `src/commands/sync/mod.rs:27`, `src/archive/live.rs:7`. `src/archive/live.rs`
  users: `src/commands/sync/live.rs`, `src/commands/unearth/mod.rs:27`,
  `src/commands/unearth/live.rs:157`, `src/commands/unearth/remote.rs:28`,
  `src/commands/archive/unearthed.rs:38` (`check_stream`, live.rs:224).
- Old payload names: `meta::FileName`, `is_payload_file_name`,
  `is_listing_file_name`, `check_file_names` (`src/meta.rs:80-139,
  260-276`); the payload name is built in `src/commands/archive/new.rs:795-802`.
  All obsolete under `<name>--<stamp>` naming.
- `src/testing/package.rs`: `pack_piece`, `pack_piece_without`,
  `on_disk`, `compress`, `raw_tar`/`RawMember` (package.rs:19-127);
  `src/testing/pool.rs` `shared_pool()`.

### A.3 archive flow, repairs, run

- Sizes: `src/commands/archive/new.rs` 4171 lines (production about 1140,
  tests from 1143), `src/commands/archive/unearthed.rs` 2431,
  `src/repair.rs` 4563 (production about 1290), `src/commands/archive/mod.rs`
  1275.
- Parallelism: `thread::scope` workers take candidates by an atomic
  counter (`src/commands/archive/mod.rs:800-836`); a hard failure stops new
  pieces (mod.rs:710-756). Candidates come from `status::status_with`
  (mod.rs:466-497).
- Per piece (`Piece::run`, new.rs:413-433): walk and idle re-check
  (`walk_and_judge`, new.rs:466-525, store-free); adoption of a crashed
  object (new.rs:534-598, store-bound); free space against
  `.antiq/tmp` with `SpaceLedger` (new.rs:631-644; `src/filesystem/space.rs:29-80`);
  pack to `tmp/<ULID>.tar.zst` with `sync_all` and a rehash
  (`write_package`, `file_sha256`, new.rs:730-774, 1031-1063); strict
  verify plus `Matched::of_packing` (new.rs:778-823,
  `src/commands/archive/durable.rs:94`); store via `store.put_file`, metadata,
  tracking `removing`, `durable::seal` (new.rs:665-940, durable.rs:137,
  all store-bound); removal in `src/commands/archive/new/finish.rs`:
  `move_to_trash` (finish.rs:200-221), refused rename handled in place by
  `remove_in_place` (finish.rs:225-274, which the concept replaces by
  undoing step 4), `rewalk`/`walk_trashed` (finish.rs:281-305),
  `differs` on (kind, size, mtime_ns, inode, ctime_ns), top on (kind,
  inode, mtime_ns) (finish.rs:48-77, store-free, matches concept step 6),
  `give_back` (finish.rs:362-400), `recover` to
  `recovered/<ULID>-<UTC>` (finish.rs:402-473; `recovered_name`,
  repair.rs:355), `delete_trashed` (finish.rs:311-340). There is no
  `doomed/` today (rg finds none outside a test variable).
- ENOSPC through the error chain (`is_out_of_space`, new.rs:271-283);
  exit 3 from `ArchiveReport::partial` (mod.rs:421;
  `src/cli/cmd/archive.rs:68`).
- The hard-link "kept instead of removed" rule of spec "archive" is a
  documented timing outcome, not code: only `apparent_size` counts a
  multi-name file once (mod.rs:500-510).
- Seams: `Seams` trait with `at(Step, piece)` (mod.rs:102-135), `Step`
  names include store steps that go; test harness `World`, `Scripted`,
  `Gate`/`Meeting` (new.rs:3933-3990) is store-bound and must be rebuilt.
- Store-free tests worth porting (new.rs): verify difference 1895,
  1914, 1931; refusals after status 1792-1870; space 1975-2135; trash
  re-walk 3132-3235, 3830; recovered 3306-3355, 3845-3888; parallel
  4022-4144, 2701; portability 2154-2189, 3767. `tests/archive.rs`
  (13 tests; :121 reads `.antiq/objects`) and `tests/rearchive.rs`
  (reclaim, goes).
- `repair::run` order today (repair.rs:292-337): `finish_drops`
  (tombstones), `finish_removals` (tracking-driven trash judge, 512-740),
  `empty_run_directories` (1109), `remove_write_leftovers` (1150),
  `settle_unearthing` (793), `catch_up_with_archives` (932),
  `restore_missing_entries` (999), `complete_imports` (1052),
  `remove_orphan_manifests` (1214). Only the tmp emptying, `move_trashed`
  (376), `recovered_name`, `is_write_leftover` and the `Effects` seam
  idea (66-100) carry over; concept repairs 1, 3, 5, 6 are new.
- `src/run.rs`: `RunGuard::begin_with` takes `collection.machine_name()`,
  the lock, then `repair::run` (run.rs:57-75); `start` returns
  `Started::Guarded` or `Started::DryRun{holder}` (run.rs:94-103). A dry
  run lists no repairs today. `RunLock` (`src/lock.rs:69`) is the proof
  token; `lock.rs` (428 lines) is self-contained; `Holder` records a
  machine name.
- `src/plan.rs`: `Plan<A>{actions, brake}`, `PlanAction::describe`,
  `DryRun`, `lines()` with brake lines (plan.rs:12-96, tests 126+).
- `src/report.rs`: `Warning`, `CommandReport`, notes (unknown keys,
  SMB/NFS, lock holder); store-free.
- `src/commands/archive/unearthed.rs` users outside itself: new.rs:415,
  mod.rs:566, 580 (`junk_of_piece`, `plan_action`),
  `src/commands/resolve/replace.rs:18`, `src/commands/resolve/mod.rs:393`.

### A.4 unearth, fork, import

- Nothing flushes the extracted tree today: `src/archive/extract.rs`
  (`Extraction`, 397-481; `drop(file)` at 441, directory modes in
  `finish` 460) and `src/commands/unearth/local.rs` `place`/`move_into_place`
  call no flush. `Environment` (local.rs:54-61) has seams for packager,
  probe, free space and rename, none for flushing.
- Reusable, store-free: `archive::extract::unpack`, `verify::verify_strict`,
  `verify::compare_with_listing` (folder vs listing, verify.rs:331),
  `read::list`/`read_package`/`read_package_from` (read.rs:39-111, the
  tar-derived listing that `rebuild` needs), `names::*`
  (`TargetRules::probe` names.rs:463, `fold_path` 228, `check_members`
  267, `portability_warnings` 316), `check_target_on_disk`, `look_at`,
  `lists_exactly`, `ancestors` (`src/commands/unearth/mod.rs:753-883`),
  `TargetProblem` (mod.rs:146-167), `check_target_names`,
  `make_parents`/`remove_created`, `move_into_place`, `check_archive`
  (whole-file hash, local.rs:243-267), `is_out_of_space`.
- Index- or tracking-bound: `check_planned_target` (mod.rs:766-809,
  claimant via `Index`), `check_entries_on_volume` (local.rs:494-528),
  `carry_out`/`place`/`Subject`/`Arrived` and most of `UnearthError`.
  `src/commands/unearth/live.rs` (1085) and `remote.rs` (2303) go whole.
- Store-free unearth tests worth porting (local.rs): name checks
  1220-1474, verify/recheck 1488-1560, parents and moves 870,
  1606-1732, archive checks 1147-1184, 989, space 1754-1832.
  `tests/unearth.rs` target refusals 346-422, damaged 475, dry runs
  521-593 carry over in spirit.
- `src/commands/fork.rs` (2377 lines, production about 650) renames a live
  unearthed folder and mints a new ID; nothing of it extracts. Only the
  `--name` parsing (fork.rs:294-310) and refusal texts survive. It and
  `src/commands/unearth/` import each other (`forked_new`, `mint`,
  `UnearthError::Fork`).
- `src/commands/import.rs` (2449 lines, production about 1050) imports
  `.tar.zst` files (`prepare_file` 407-510, `import_one` 921-1044); only
  `check_path` (513-543) and the `Hooks` seam idea survive.

### A.5 selection, walk, idle, config, layout

- Ignore: `ignore: Vec<String>` default empty, `junk` default
  `[".DS_Store"]` (`src/config.rs:96-98, 152, 241`). `.antiqignore`
  (`IGNORE_FILE`, `src/idle.rs:20`) read per directory unless ignored
  (idle.rs:272-337); the `ignore` crate only in idle.rs (11, 124);
  precedence nearest file, outer files, config (idle.rs:288-316).
  Ignore today means "left out of the idle check only" (idle.rs:180).
  The walk drops junk non-directories at any depth (`src/walk.rs:327-329`),
  so junk files skip the refusal checks. Nothing named `timeless` exists.
- Control files: `MARKER_NAMES = [.antiqignore, .antiq-pin]` at any
  depth (idle.rs:23, 169-191); `pin::is_pinned` is true for any
  non-directory (`src/pin.rs:56-62`); no `.antiq-restored`.
- Idle clock: `idle::check(.., unearthed: Option<Timestamp>, ..)`
  (idle.rs:141-216), unearth time from tracking
  (`src/commands/status.rs:492`, `src/commands/archive/new.rs:312, 388`); one-day
  future tolerance and `OnlyFuture` verdict (idle.rs:161, 184-212); due
  boundary `>` (idle.rs:200); default `idle_days` 14 (config.rs:154);
  no `soon_days`.
- Walk problems `Problem{Unreadable, MountPoint, Excluded,
  NestedCollection}` (`src/tree.rs:76-90`); mount detection
  walk.rs:162-186, 341-353 and `src/filesystem/tree.rs:584-690`; depth 512
  (walk.rs:73, 354-357); exclude by path or (dev, inode)
  (walk.rs:346-352, `src/select.rs:94-131`). No immutable/append or
  parent-writable check anywhere; Linux `statx` already returns
  `stx_attributes` but asks only for basic stats (filesystem/tree.rs:584-640),
  macOS `statat` gives `st_flags`.
- `src/filesystem/`: `remove_tree`/`remove_tree_at` (remove.rs:34-48, no flag
  handling), `move_dir` (moves.rs:36-60, owner write grant, no replace),
  `flush_file`/`flush_dir`/`write_atomic` (atomic.rs:44-70; `flush_file`
  is `sync_all` = `F_FULLFSYNC` on macOS, plain `fsync` on ENOTSUP),
  `available_space`, `SpaceLedger` (space.rs:13-61), `device_of`,
  `is_mount_point`, `fs_kind` (device.rs:22-135).
- `src/config.rs`: tables `[collection]`, `[machine]`, `[[remote]]`
  (config.rs:73-84); 16 collection keys (`COLLECTION_KEYS`
  config.rs:130-147); unknown collection keys only warned
  (config.rs:76-83, 307-316); `ConfigDoc` (about 370-480) writes `id`,
  `patterns`, `exclude`, `[machine] name` for `init`.
- `src/collection.rs`: paths `remote-state.json`, `pruned.json`,
  `objects/`, `tracking/`, `index/`, `tmp/`, `staging/`, `trash/<id>`,
  `recovered/` (collection.rs:84-150); `find_root` (25-43);
  `machine_name()` (70).
- `src/id.rs`: `ObjectId` (22), `IdSource`, `RandomIds`, `SeqIds`,
  `id_time` (74-165, uses `meta::CreatedSource`). Only `ObjectId` for
  the collection ID stays. `src/relpath.rs`: `escape`/`unescape`
  (19-70) stay; `name_part`, `machine_part`, `temp_name`, `escape_part`
  (208-250) serve removed temp names. `src/machine.rs`: `detect`
  (21-38) stays, `resolve(configured)` (40-52) goes.

### A.6 CLI, errors, dependencies, tests

- Surface today: `Cli` with global `-C`, `--json` (`src/cli/mod.rs:25-38`),
  13 commands (cli/mod.rs:41-71): `init --pattern --yes --dry-run`,
  `status`, `archive --dry-run`, `unearth <piece> --to --version <u32>
  --dry-run --if-reachable`, `find [query] --print`, `fork --name
  --dry-run`, `pin`/`unpin --dry-run`, `sync`, `resolve`, `remote
  add/show/remove`, `reconcile`, `import --path FILE...`. Dispatch
  cli/mod.rs:326-400; failure reporting `finish` 400-455 (policy and
  brake lines 422-440); usage envelope 475-515.
- Envelope `{"format":1,"command":..., ...}` and error object
  (`src/cli/output.rs:21, 60-81, 193`); data on stdout, messages on
  stderr (output.rs:212-241); broken pipe swallowed (output.rs:51).
  Dry runs are rendered per command today (`plan.rs` `Plan::dry_run`,
  `lines`; `cmd/pin.rs:70-88`, `cmd/import.rs:72-75`,
  `cmd/archive.rs:80-83`, `commands/init.rs:60-96`).
- `ErrorKind` (`src/error.rs:52-71`): usage (2), not_implemented,
  not_a_collection, ambiguous_reference, unknown_reference,
  outside_collection, brake, lock_held, no_remote, remote_away (4),
  remote_broken, format_too_new, unsupported, other; `exit_code`
  error.rs:113-119; exit 3 comes from reports (`cmd/archive.rs:68`).
  `Error` variants error.rs:132-209; `details` error.rs:266-288.
- `src/interact.rs` users: brake, commands/init (14, 321-406), commands/archive
  (mod.rs:25, 650; unearthed.rs:32, 754), resolve, remote, reconcile,
  cli/mod.rs (14, 17, 307-321, 578-582), cli/tty.rs, cmd/init.rs:29-30,
  cmd/archive.rs:36. `src/cli/tty.rs` is question code only (`open`,
  `TtyInteraction`, `ask`); colour and width live in `cli/output.rs:86-144`.
- `src/reference.rs`: `resolve(collection, &Index, cwd, given)` (80),
  `resolve_path` (149-190), ULID prefixes (62, 104-135); no fuzzy
  matching. nucleo is used in `src/index.rs:23-24, 509-518` and the
  picker.
- `src/commands/init.rs`: `plan` (144-211; refusals 236-255; `archive.sh`
  detection 215-232), `apply` (260-314; `LAYOUT` 123-131), questions
  (321-406). `machine::detect` from `cmd/init.rs:19`.
- `src/commands/pin.rs`: `holder_of`, `require_folder`, `target` read
  tracking, store and index (165-282); `PinAction` 48-62,
  `folder_exists` 195, `current_pin` 287 are store-free.
- `src/commands/status.rs` (3126 lines): the live part is `select` (385-395),
  `walk_piece_with` plus `judge` (602-622, 684), `recovered_names` (972),
  `presence`/`exists_exact` (1011-1043); the rest reads store, tracking,
  remote state.
- Table helpers in `src/cli/cmd/status.rs` (`sanitize` 773, `mid` 790,
  `pad`, `pad_left`, `wrap`, `paint`, `spaces`, `Color`, `WIDE_FROM`),
  imported by `cmd/archive.rs:7`, `cmd/pin.rs:7`, `cmd/fork.rs:7`,
  `cmd/unearth.rs:7`, `cmd/import.rs:7`, `cmd/find.rs:13`,
  `cli/mod.rs:439`.
- Dependencies by user: crossterm only `cli/picker/mod.rs`;
  unicode-segmentation only `cli/picker/layout.rs:19`; nucleo
  `index.rs`, picker; whoami only `machine.rs:20-23`; toml_edit only
  `config.rs:16`; glob `select.rs:11`; ignore `idle.rs:11`; rustix `pty`
  only `tests/support/pty.rs:363-371`, `termios` `cli/output.rs:116,
  129`; zstd features: `zstdmt` for `multithread` (zstd.rs:72),
  `experimental` for `RSyncable`/`JobSize` (zstd.rs:81-82, live only);
  insta snapshot files only under `src/cli/picker/snapshots/`.
- tests/support: `cmd.rs` (`Antiq`, `Run`, `detached`; drop
  `path_without_real_ssh` 38-56), `tree.rs` (`TempCollection::minimal`
  144-162 writes the old layout), `objects.rs` (meta/tracking helpers;
  `hold_lock` and `snapshot` are generic), `pieces.rs` (`days_ago`
  generic), `pty.rs` (picker, brake, reconcile tests), `sshd.rs`.
- Tests whose cases carry over in spirit:
  - `tests/cli.rs`: help and `COMMANDS` (14-51), help subcommand
    disabled (54), usage errors (89, 102), envelope keys (278), plain
    error (296), usage envelope (315, 323), `-C` (331), help/version
    under `--json` (346), global options after the subcommand (360),
    closed stdout (373), detached session (393), environment (410).
  - `tests/init.rs`: JSON and console creation, key order, repeated
    `--pattern`, `-C`, invalid pattern exit 2, dry run, refusals,
    the `**` note. Questions and `--yes` cases change.
  - `tests/pin.rs` at beb748f: pin creates the file, "already pinned",
    "not pinned", unpin, pin by path, outside-collection and unknown
    references, a directory named like the pin file refused, dry-run
    text and JSON, dry run refuses what a real run refuses, held lock
    (real run fails, dry run names the holder, no stderr under JSON),
    repairs as notes, unknown key note, `-C` relative references
    (565-600).
  - `tests/status.rs` at beb748f: new, pinned, empty, unreadable and
    future pieces (206), nothing written below `.antiq` (428), lock note
    (447), unknown key once (479), narrow table without colour (492),
    nothing to do (511), not a collection (526).
