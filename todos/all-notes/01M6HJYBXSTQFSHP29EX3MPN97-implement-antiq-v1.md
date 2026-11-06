---
title: "Implement antiq v1"
kind: plan
component: project
origin: request
tags: [architecture]
---
# Implement antiq v1

This plan turns the version 1 design of antiq, `docs/specs/spec-v1.md`
and ADRs 0001 to 0057 at commit `ef3f47d`, into 40 work items in 11
waves. Each item names the spec sections and ADRs it implements, the
files it owns, the items it depends on, the tests that prove it, the
docs it updates and its size, so that one implementation agent can take
it in its own worktree and write its own detailed plan from there. The
order follows the agreed sequence: first a skeleton that lays down the
module tree, the shared types and the whole command surface, then the
local business logic (selection, idle check, packing, archive, unearth,
import, decisions), then the extension points (the remote trait with
its path and SSH implementations and the commit protocol), then `sync`
and the multi-machine logic on top of them, then the ADR that records
the traits as built and an acceptance run on real SSH and on Linux; the
picker runs alongside from wave 7. The plan also records the technical
decisions taken on it, the risks, the behaviour questions for the user,
and a table that maps every spec section and ADR to the items that
implement it.

Ids in parentheses such as (F5), (O2), (S7) or (H6) name findings of the
plan's adversarial review of 2026-11-03; each finding, its evidence and
the agreed fix are in the review ledger,
`~/playground/2026_11_02_antiq-logbook/plans/impl/plan-review-ledger.md`.

## 1. Ground rules

These bind every item.

- Base: `main` at the commit that lands this plan (the design as of `ef3f47d` plus the ADR 0039 and 0045 fixes of `7c693c8`). `src/main.rs` holds an empty clap `Cli`,
  `src/lib.rs` a doc comment. Dependencies: anyhow 1.0.104, clap 4.6.7
  (derive), thiserror 2.0.21. `cargo test` runs 0 tests.
- The lead files each item's step todo through ntropy before its agent
  starts, with a *Part of* line to this plan's todo
  `01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md`. The agent deletes
  its step todo in its finishing commit.
- Every commit passes `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings` and `cargo test`. Each
  behaviour change starts with a failing test. Every changed production
  line is covered (section 4.8). Docs (spec, ADRs, README) change in the
  same commit as the behaviour.
- Tests assume a non-root user. A test that relies on a permission
  (an unreadable directory, a `0555` directory that blocks a rename)
  first asserts that the precondition holds and fails with a message
  naming it when it does not.
- Every source file starts with the MPL-2.0 header (`CLAUDE.md`), tests
  and test scripts included; in a script it goes below the shebang.
  Manifests and Markdown carry none. Scripts pass `shellcheck`.
- Dependencies are added with `cargo add`.
- In code with anyhow `Result`s (the binary), every `?` gets a
  `.context()` first, worded in simple present tense to complete
  "while attempting to ...", for example `.context("read the config file")`.
- `expect()` instead of `unwrap()`, with a short reason why it cannot
  fail.
- Public items of the library are checked against the Rust API
  guidelines checklist (<https://rust-lang.github.io/api-guidelines/checklist.html>).
- Where the design enforces a rule through types (section 2.8), a
  `compile_fail` doctest proves it, one error condition per doctest,
  with the reason written next to it, checked once through a temporary
  example that shows the expected compiler error.
- ADRs may be rewritten in place until the first release is being
  built, which is announced when it starts; after that, a changed
  decision becomes a new ADR.
- Nothing is pushed.
- An item edits only the files it owns plus the shared edits its entry
  names. I01 creates every file of the module tree as a stub, including
  every `mod.rs` and every submodule file of `ops` and `cli::cmd`, so
  later items fill files and never add module declarations.
- Types that I01 defines complete (section 2.2) are frozen. A change to
  one goes through the lead, who schedules it so that no parallel item
  breaks.
- The README changes only at milestones (I16, I30, I39). Spec changes
  go into the section an item implements; JSON payloads and new error
  kinds go into the per-command paragraphs I01 adds to "JSON output".

## 2. Architecture

### 2.1 Module tree

One crate with a library and a binary (ADR 0003). The library holds the
logic; the binary parses arguments, renders output, talks to the
terminal and reports errors.

Library, `src/lib.rs`:

| Module | Responsibility | Implements | Item |
|---|---|---|---|
| `error` | Top-level `Error` wrapping one error enum per module; `ErrorKind` for exit codes and JSON error kinds | ADR 0005; spec "Dry runs, scheduling, exit codes", "JSON output" | I01 |
| `clock` | `Clock` trait, `SystemClock`, `FixedClock` | section 4 | I01 |
| `interact` | `Interaction` trait, `Question`, `Choice`, `NonInteractive`, scripted double | ADR 0020, 0025; spec "Decisions" | I01 |
| `plan` | `Plan<A>` for dry runs | ADR 0046 | I01, I17 |
| `report` | `CommandReport` trait, `Warning` | spec "JSON output" | I01 |
| `tree` | `TreeEntry`, `PieceTree` and the walk's problem types | spec "Walks" | I01 |
| `relpath` | `RelPath` and `Name` as bytes and the escape function (I01); unescape, payload name part, machine part, temp names, lexical normalisation (I02) | spec "Metadata", "Storage" | I01, I02 |
| `id` | `ObjectId` (I01); `IdSource` implementations and ID time (I02) | ADR 0014, 0039; spec "Objects" | I01, I02 |
| `fsx::tree` | `TreeRoot`: dirfd-relative opening, stat, reading and creating below a root, for paths past the platform's path limit | spec "Walks", "unearth" | I01 |
| `fsx` (`atomic`, `remove`, `moves`, `space`, `device`) | Atomic write, delete helper, moves with owner write, free space, device and filesystem type, birth time | spec "Storage", "Walks", "Operations" | I03 |
| `lock` | `.antiq/lock`, `RunLock` | spec "Operations" | I03 |
| `config` | `config.toml`: typed read, edits that keep comments | spec "Config" | I04 |
| `collection` | Finding `.antiq/` upwards, `Collection` with the local layout paths | ADR 0011; spec "Collections and pieces", "Storage" | I04 |
| `machine` | Machine name | spec "Config" | I04 |
| `select` | Selecting pieces; later behind `SelectionStrategy` | ADR 0012, 0028; spec "Selection" | I05, I38 |
| `walk` | The lstat walk of a piece | spec "Walks" | I05 |
| `idle`, `pin` | Idle verdict, empty, pins | ADR 0013, 0024, 0029, 0037, 0038; spec "Idle", "Pins" | I10 |
| `meta` | Metadata data types with their serde attributes (I01); format guard, reading and writing (I08) | ADR 0053; spec "Metadata" | I01, I08 |
| `listing` | Listing as zstd JSON Lines | spec "Listing" | I08 |
| `store` | Local object store `.antiq/objects/` | spec "Storage" | I08 |
| `tracking` | Tracking entries, derived states, pending decisions | spec "Local state", "States", "Decisions" | I09 |
| `index` | Derived, rebuildable index and search | ADR 0007; spec "Storage" | I15 |
| `reference` | Piece references (path or ULID prefix) | spec "Command line" | I15 |
| `pack` (`mod.rs`) | `Packager` trait, `PackageFormat` (`tar+zstd/1`) | ADR 0019; spec "Packaging" | I06, I38 |
| `pack::tar_write`, `pack::hashing`, `pack::zstd` | Tar writer with hand-built headers, hashing reader and writer, encoder and decoder settings | ADR 0050, 0051 | I06 |
| `pack::pool`, `sysmem` | Compression worker pool and memory budget | ADR 0051; spec "Packaging" | I07 |
| `pack::read`, `pack::verify` | Reading members and listings from a package, strict verify, comparison with a listing | ADR 0033, 0035 | I12 |
| `pack::extract`, `pack::names` | Extraction into staging, name checks | ADR 0027, 0034, 0050, 0054 | I13 |
| `pack::live`, `pack::manifest` | Live packages, tolerant verify, manifests | ADR 0018, 0052; spec "Change detection" | I14 |
| `pack::frames` | zstd frame inspection for `import` | ADR 0055 | I21 |
| `run`, `repair` | Run guard (lock plus start-of-run repairs) | spec "Operations" | I17 |
| `brake` | The brake | ADR 0045 | I35 |
| `remote` (`mod.rs`) | `Remote` trait, `Reachability` (I01), opening the configured remote with an `ssh` arm that answers `unsupported` (I23) | ADR 0015, 0031 | I01, I23, I38 |
| `remote::url`, `remote::marker`, `remote::path` | URL parsing, `antiq-remote.json`, path remote | ADR 0026, 0031, 0047 | I23 |
| `remote::commit`, `remote::cleanup`, `remote::testing` | Commit protocol, cleanup, fault-injecting remote for tests | ADR 0030 | I27 |
| `remote::ssh::session`, `::script`, `::classify` | `ssh` executable, master connection, `/bin/sh -s` scripts, away and broken | ADR 0048 | I24 |
| `remote::rsync` | rsync resolution, probe, command line, stats | ADR 0049 | I25 |
| `remote::ssh` (`mod.rs`) | `SshRemote`, the trait over ssh and rsync; the `ssh` arm of opening | ADR 0048, 0049 | I28 |
| `remote::policy`, `remote::away` | `--if-reachable`, `away_warn_days` | spec "Remotes", "Dry runs, scheduling, exit codes" | I29 |
| `ops::<command>` | One module per command; `ops::archive`, `ops::unearth`, `ops::resolve` and `ops::sync` are directories with one file per part | spec "Operations", "Command line" | command items |
| `testing` (`cfg(test)`) | Fixtures shared by library tests | section 4 | I01 |

Binary, `src/main.rs` with modules under `src/cli/` (T20):

| Module | Responsibility | Item |
|---|---|---|
| `cli` (`mod.rs`) | clap types for the whole command surface, dispatch to `cli::cmd` | I01 |
| `cli::output` | JSON envelope, error object, exit codes, colour, stdout and stderr | I01 |
| `cli::tty` | Controlling terminal, `TtyInteraction` implementing `interact::Interaction` | I01 |
| `cli::cmd::<command>` | One file per command: builds the call into `ops`, renders the console output; `cli::cmd::sync` is a directory (`mod`, `conflict`, `live`, `tiers`) | command items |
| `cli::picker` (`mod.rs`, `state.rs`, `layout.rs`) | The picker of `find` | I36, I40 |

### 2.2 Core types

I01 defines these complete; they are frozen after I01 (section 1).

- `ObjectId`: newtype over `ulid::Ulid`. Text form is the crate's upper
  case (T19). Parses either case.
- `RelPath`: a path relative to the collection root as bytes, since
  "paths and names are stored as the bytes the filesystem holds"
  (spec "Metadata"). Serialises in the escaped form. `Name` is one
  component. The escape function is part of I01 so that selection (I05)
  can match non-UTF-8 names through it (O16 fix, I05).
- `TreeEntry` and `PieceTree` (`src/tree.rs`): the walk's result per
  entry: relative path, type, size, allocated size, modification time
  in nanoseconds, ctime, inode, device, mode, link count, link target,
  birth time; sorted by path bytes; problems (`unreadable` with the
  first failing entry, `mount_point` with the path of the other
  device).
- `TreeRoot` (`src/fsx/tree.rs`): a directory handle through which every
  consumer opens, stats, reads and creates entries relative to a root,
  so that paths longer than the platform's path limit work (R3).
- `ListingEntry`: path, type, size, modification time in whole seconds,
  mode, link target, SHA-256 (spec "Listing").
- `Metadata`, `Payload`, `HistoryEntry`, `Lifecycle`, `PayloadKind`,
  `CreatedSource` (`src/meta.rs`): the tables of spec "Metadata" with
  their serde attributes: field order as the spec lists the keys,
  `skip_serializing_if` for the keys left out where they do not apply,
  `verified` with three states (absent, null, a time), and a flattened
  `extra` map on every struct that can grow (foundations research §5).
  `CreatedSource` includes `minted` (T2).
- `Reachability`: reachable, away, broken (spec "Remotes"), in
  `src/remote/mod.rs`.
- `Collection` and `Config` are defined by I04, `TrackingEntry` by I09:
  - `Collection`: root path, `.antiq/` path accessors for every entry of
    the local layout, the parsed `Config` and the collection ULID.
  - `Config`: `collection` policy, `machine`, `remotes:
    Vec<RemoteConfig>` with every key and default of the config table.
  - `TrackingEntry`: the fields of spec "Local state", the payload
    version a piece was unearthed from (F16), the time the entry was
    written (`written`, F22), and the fields `status` needs to show
    stale backups, entries missing on the remote and divergence.
- `IdleVerdict` (I10): pinned, empty, active with the newest time,
  idle, only future times; future files listed.
- `Plan<A>`: an ordered list of actions of a command-specific type `A`
  plus where the brake would hold.
- One report type per command, `Serialize`, implementing
  `report::CommandReport` (whether the run was partial, its warnings).

### 2.3 Traits

The engine of ADR 0006 is the set of `ops` functions; they reach the
parts that vary through these traits:

- `Clock` (`now()`), `IdSource` (`mint(time) -> ObjectId`) and
  `Interaction` (section 2.6). Defined complete in I01.
- `Packager` (spec "Packaging": "packs, verifies, lists and unpacks";
  ADR 0019). One implementation, `tar+zstd/1`. Sketch, settled by I06
  and I12 to I14:

  ```rust
  pub trait Packager {
      fn format(&self) -> PackageFormat;
      fn pack(&self, tree: &PieceTree, settings: &PackSettings, out: &mut dyn Write)
          -> Result<Packed, PackError>; // size, SHA-256, listing entries, reports
      fn verify_strict(&self, package: &Path, tree_root: &TreeRoot, rules: &WalkRules)
          -> Result<Verified, PackError>; // with the lstat record of every entry
      fn list(&self, package: &Path) -> Result<Vec<ListingEntry>, PackError>;
      fn unpack(&self, package: &Path, staging: &TreeRoot, rules: &TargetRules)
          -> Result<Unpacked, PackError>;
  }
  ```

- `Remote` (ADR 0015: one trait, every implementation provides every
  operation). Spec "Remotes" lists reachability with three results, bulk
  metadata pull and push, payload put, get and delete, and exclusive
  creation; the commit protocol, cleanup and tiers need listing an
  object directory, a rename into place, the remote clock, a checksum of
  a remote file and a flush (NAS `sync`). The bulk pull is one call;
  the push is the per-object commit protocol of ADR 0030 under the
  `remote_jobs` limit (F3). Listings and metadata files come through the
  small-file read (F2). Sketch, settled by I23:

  ```rust
  pub trait Remote: Send + Sync {
      fn reachability(&self) -> Result<Reachability, RemoteError>; // checks the marker
      fn begin_phase(&self) -> Result<(), RemoteError>;  // SSH: `-O check`, new master
      fn clock(&self) -> Result<Timestamp, RemoteError>;
      fn pull_metadata(&self, known: &KnownVersions) -> Result<Vec<MetaFile>, RemoteError>;
      fn list_objects(&self) -> Result<Vec<ObjectId>, RemoteError>;
      fn list_dir(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError>;
      fn read_file(&self, id: ObjectId, name: &str) -> Result<Vec<u8>, RemoteError>;
      fn create_exclusive(&self, id: ObjectId, name: &str) -> Result<Created, RemoteError>;
      fn write_temp(&self, id: ObjectId, temp: &str, bytes: &[u8]) -> Result<(), RemoteError>;
      fn rename_into_place(&self, id: ObjectId, temp: &str, name: &str) -> Result<(), RemoteError>;
      fn put_payload(&self, upload: &Upload<'_>) -> Result<Transfer, RemoteError>; // temp, final, seed
      fn get_payload(&self, id: ObjectId, name: &str, to: &Path) -> Result<Transfer, RemoteError>;
      fn delete(&self, id: ObjectId, name: &str) -> Result<(), RemoteError>;
      fn checksum(&self, id: ObjectId, name: &str) -> Result<Sha256, RemoteError>;
      fn flush(&self) -> Result<(), RemoteError>;
      fn finish(&self); // SSH: `-O exit`
  }
  ```

- `SelectionStrategy` (ADR 0012: "the selection strategy is a
  replaceable part"): extracted by I38 once selection has settled;
  until then `select` is a module with one strategy.

### 2.4 Errors and exit codes

- Each library module has its own `thiserror` enum in its own file;
  `error::Error` wraps them with `#[from]` (ADR 0005). I01 declares every
  wrapper variant against stub enums, so later items add variants only
  in their own files.
- `Error::kind() -> ErrorKind` gives the JSON error kind and the exit
  code. I01 lists each kind with its source: the kinds spec "JSON
  output" names (`ambiguous_reference` with candidates,
  `unknown_reference`, `brake` with the planned count, the set size and
  up to ten items, `lock_held`, `remote_away`, `remote_broken`),
  `not_a_collection` (spec "Collections and pieces": commands fail
  outside a collection), `usage` (T8), and the transitional
  `not_implemented`, which I39 removes. Items that add a kind add it to
  the spec's "JSON output".
- Exit codes (spec "Dry runs, scheduling, exit codes"): 0 success; 1
  error, including remote broken, lock held, a held brake without a
  terminal, an unknown or ambiguous piece; 2 usage error (clap's own
  usage errors exit 2: `USAGE_CODE` in clap_builder 4.6.7); 3 partial,
  from `CommandReport::partial()`; 4 remote away, turned into 0 by
  `--if-reachable` through `remote::policy` (I01 stub, I29 fills it,
  I30 places the first call site).
- `EMFILE` and `ENFILE` are errors of the run, never an unreadable
  piece (O11). `main` raises the soft `RLIMIT_NOFILE` before anything
  else (I01).
- The binary propagates errors with anyhow and context (ADR 0005) and
  finds the `antiq::Error` in the chain to pick the exit code and JSON
  kind.

### 2.5 Console and JSON output

- antiq writes console output; the picker is its only full-screen part
  (ADR 0056). Data goes to stdout, messages to stderr; colour only on a
  terminal (spec "Dry runs, scheduling, exit codes").
- All output goes through functions returning `io::Result`; a
  `BrokenPipe` on stdout stops the output without a panic and the run
  keeps its own exit code (lead decision on the I01 plan, 2026-11-03,
  replacing an earlier "exit 0"). Clippy's
  `print_stdout` and `print_stderr` lints are denied in the binary, so
  no `println!` can panic on a closed pipe (O13).
- Every JSON output is one object `{"format": 1, "command": "<name>",
  ...}`; errors add `{"error": {"kind": ..., "message": ...}}`; a dry run
  carries `"dry_run": true` and `actions` (spec "JSON output"). JSON
  errors go to stdout, plain messages to stderr (T9). The library's
  report types derive `Serialize`; `cli::output` wraps them in the
  envelope. Warnings travel in each report as a list and print to
  stderr on the console.
- Console rendering lives in `cli::cmd::<command>`, next to the call.

### 2.6 Interaction and decisions

- `--json` makes a command non-interactive. Otherwise a command is
  interactive when a controlling terminal is available, also when its
  output is redirected (ADR 0020, spec "Command line").
- The library asks through `Interaction::choose(&Question) ->
  Option<Choice>`. A `Question` is presentation-neutral data built by
  the library: a title, detail lines (such as the loss list of ADR 0036
  or up to ten brake items), the choices and the non-destructive
  default. `NonInteractive` answers nothing, so every decision point
  takes the non-destructive default and the object is listed as needing
  a decision (spec "Decisions"). `cli::tty::TtyInteraction` renders a
  question on `/dev/tty`. Items add questions without editing
  `interact.rs` (T21).
- Pending decisions live in the tracking entries (spec "Local state").
  `resolve` executes one answer at once, under the lock, with the loss
  computed at that moment; no answer is stored (spec "Decisions").

### 2.7 Dry runs

- Every state-changing command takes `--dry-run`, which prints the plan
  and changes nothing (ADR 0046). Read-only commands and `--dry-run`
  never touch `trash/`, `tmp/` and `staging/` (spec "Operations"). A
  dry run takes no lock and prints the same note as `status` when one is
  held (T7).
- Each state-changing op is split into `plan(...) -> Plan<Action>`,
  which reads local state (and the remote through read-only calls) and
  decides, and `apply(plan, &RunGuard)`, which acts. `--dry-run`
  renders the plan; the normal path applies it. Steps whose outcome
  depends on an earlier step (a verify that may fail, an upload that may
  fail) appear in the plan as intended actions.
- A dry run writes nothing on the remote, not even the probe file the
  path remote's clock needs; cleanup steps that depend on an age appear
  as "age unknown in a dry run" (F14).
- The brake evaluates on the plan, so `--dry-run` reports where it would
  hold (ADR 0045).

### 2.8 Runs, the lock and proof tokens

- `run::RunGuard::begin` takes the lock (`lock::RunLock`, I03), writes
  pid, machine and start time, and does the start-of-run repairs (I17).
  Every function that writes into `.antiq/` or deletes a folder takes
  `&RunLock` or `&RunGuard`; `RunLock` has private fields and only
  `lock::acquire` makes one.
- Type-state for the irreversible steps (T22): a folder is removed only
  through a function that takes a `DurableArchive`, made only by the
  store after a `Verified` (from the strict verify) and the durable
  writes of payload, listing, metadata and tracking (spec "archive"
  step 2, ADR 0033). A local archive is dropped only through a function
  that takes a `ConfirmedRemoteCopy`, made only by the check of ADR
  0043. The flows where the user's answer itself authorises a removal
  (the `theirs` choice of a version conflict sends the local archive
  through trash; `--theirs` on a diverged folder sends the folder
  through trash) take a `UserDecision`, made only by the decision
  handling of `resolve` and of the in-place questions (F9). Each token
  gets a `compile_fail` doctest showing that it cannot be built outside
  its module.

### 2.9 Local metadata before `sync`

The spec says changes "are recorded locally and committed as a new
metadata version at the next `sync`" and that `.antiq/objects/<ulid>/`
has the remote's layout, but not where a recorded, uncommitted change
lives. Decision (T1): `objects/<ulid>/meta.<N>` holds versions known to
be committed (pulled or committed by this machine);
`objects/<ulid>/meta.pending` holds the next version, written through
the atomic write. Local readers prefer `meta.pending`. `sync` commits
it with the protocol of ADR 0030, which gives the version number (one
above the highest committed version or claim on the remote, F15), and
then renames it locally to `meta.<N>` with that number. While a change
is pending, the tracking entry's "version taken out" reads `pending`;
I30 rewrites it to the committed number (F21). Without a remote the
change stays pending. I08 implements the store side and records the
decision as a new ADR.

### 2.10 Concurrency

Standard threads, no async runtime: scoped threads for parallel pieces,
the compression pool of I07 for workers and memory, a counting limit of
`remote_jobs` for remote operations (spec "Remotes"). OpenSSH's sshd
allows 10 sessions per connection by default (`MaxSessions`, ssh
research, "Not verified"); `remote_jobs` defaults to 4.

## 3. Dependencies

I01 adds every crate below except the ones marked for a later item, so
that `Cargo.toml` and `Cargo.lock` stay out of parallel work. At most
one item per wave edits `Cargo.toml` after that. I01 runs `cargo audit`
(cargo-audit 0.22.2 is installed) once the crates are in.

| Crate | Version | Features | For | Research and safety notes | Item |
|---|---|---|---|---|---|
| anyhow | 1.0.104 | | binary errors | present | in place |
| clap | 4.6.7 | derive | arguments (ADR 0004) | present | in place |
| thiserror | 2.0.21 | | library errors (ADR 0005) | present | in place |
| serde | 1.0.229 | derive | all formats | not researched; ntropy uses 1.0.228 | I01 |
| serde_json | 1.0.154 | preserve_order | metadata, listings, tracking, JSON output | keeps unknown keys in input order, nested too (foundations §5) | I01 |
| toml | 1.1.8 | | typed config read; `deny_unknown_fields` names the bad key (foundations §6) | | I01 |
| toml_edit | 0.25.17 | | config edits that keep comments and order (foundations §6) | | I01 |
| jiff | 0.2.39 | serde | `Timestamp`, UTC by type (foundations §5) | fractional digits vary with the value, so whole seconds need a custom serializer (T15) | I01 |
| ulid | 3.0.0 | default (`std`) | IDs; `Ulid::from_datetime` per unit, not a shared `Generator`; `Ulid::from_parts` for fixed test IDs | a time before 1970 gives timestamp 0 without an error, so the 1990 rule runs before minting (foundations §4) | I01 |
| sha2 | 0.11.1 | | SHA-256 | pure Rust, hardware instructions detected at run time on aarch64; RUSTSEC-2021-0100 affects 0.9.7 only (foundations §3) | I01 |
| glob | 0.3.4 | | selection patterns | the only measured crate whose `require_literal_leading_dot` implements the dot rule; no braces (foundations §1); matches only UTF-8 paths, so non-UTF-8 names go through their escaped form (O16) | I01 |
| ignore | 0.4.34 | | gitignore matching for the idle check (`Gitignore`) | measured with anchored rules and negation (foundations §2) | I01 |
| tar | 0.4.46 | default features off (T18) | tar writing and reading | at least 0.4.45 for RUSTSEC-2026-0067 and 0068; RUSTSEC-2021-0080 and 2018-0002 are older (tar research) | I01 |
| zstd | 0.14.3 | default features off; `experimental`, `zstdmt` | compression | `experimental` exposes `RSyncable`, `zstdmt` multithreading; at least 0.14.3 for RUSTSEC-2026-0391; `legacy` off, which holds the RUSTSEC-2026-0392 code (compression research) | I01 |
| zstd-sys | `=2.1.2+zstd.1.5.7` | default features off (`--no-default-features`) | pins the bundled zstd 1.5.7 exactly (ADR 0052) | zstd-sys 2.1.2's defaults are `legacy` and `zdict_builder`, which a direct dependency with defaults would switch back on through feature unification; RUSTSEC-2026-0410 is wasm32 only (compression research) | I01 |
| filetime | 0.2.29 | | directory times in the final extraction pass; fixtures | used by the tar research probe | I01 |
| rustix | 1.1.6 | fs, process | plain `fsync`, `statvfs`, `statfs`, dirfd-relative access (`fsx::tree`), `fcntl_nocache` and `fadvise` for read-back, pid liveness, `setrlimit`, `setsid` in tests | probed in the filesystem research, which names RustSec entries only for `nix` and `remove_dir_all` | I01 |
| libc | 0.2.191 | | `sysconf(_SC_PHYS_PAGES)` for the memory budget (T12) | not researched; `cargo search` shows a 1.0 alpha, so the version is given explicitly | I01 |
| whoami | 2.1.3 | | machine name: `devicename()` on macOS, hostname elsewhere | `devicename()` on Linux reads `PRETTY_HOSTNAME` and fails when absent (foundations §7) | I01 |
| tempfile (dev) | 3.27.0 | | temporary collections and remotes | probed in the filesystem research | I01 |
| insta (dev) | 1.49.0 | filters | console output snapshots | not researched; ntropy uses 1.48.0 | I01 |
| unicode-normalization | 0.1.26 | | NFD folding for name collisions (ADR 0054) | not researched (T11) | I13 |
| crossterm | 0.29.0 | | picker drawing | ntropy's version (ADR 0057); advisories to check with `cargo audit` | I36 |
| nucleo | 0.5.0 | | picker matching | ntropy's version | I36 |
| unicode-width | 0.2.3 | | picker layout | ntropy pins 0.2.2 | I36 |
| unicode-segmentation | 1.13.x | | picker layout | ntropy's version 1.13.3 | I36 |
| rustix features `pty`, `termios` | 1.1.6 | | pty harness for the terminal glue (T16) | `openpt`, `grantpt`, `unlockpt`, `ptsname`, `tcsetwinsize`, `ioctl_tiocsctty` exist in rustix 1.1.6 | I36 |

Not taken: `walkdir` and `jwalk` (the walk needs the device and
unreadable reports and long paths, R3; a plain `read_dir` walk measured
194 ms against walkdir's 187 ms on `~/playground`, filesystem research
§5), `globset` (no dot rule), `atomic-write-file` (failed with `ENOTSUP`
on smbfs and left its temp file), `sysinfo` (reported 362 GB free where
`statvfs` gave 314,873,352,192 bytes), the `openssh` crate and library
SSH clients (ADR 0048), xz crates (antiq reads no xz, ADR 0019).

## 4. Test strategy

### 4.1 Levels

1. Unit tests next to the code (`#[cfg(test)] mod tests`).
2. Library scenario tests inside `src/ops/**` and `src/remote/**`
   (`cfg(test)`): they reach private items, the scripted interaction and
   the fault-injecting remote. Most behaviour of `archive`, `unearth`,
   `sync` and `resolve` is proven here.
3. Binary tests in `tests/<command>.rs`: spawn the built binary
   (`CARGO_BIN_EXE_antiq`) detached from the terminal (4.5), assert exit
   codes, JSON (parsed with serde_json) and filesystem effects, and
   snapshot console text with insta filters for ULIDs, times and temp
   paths. They prove the command surface, the envelope, exit codes,
   stdout and stderr.
4. Opt-in tests, `#[ignore = "<what it needs>"]`: a real sshd on this
   machine, a Linux container. Run with `cargo test -- --include-ignored`.

### 4.2 Fixtures

- `testing::Tree` (library) and `tests/support/tree.rs` (binary tests):
  build directories, files with content, mode and modification time
  (filetime), symlinks (absolute, relative, dangling, pointing outside),
  hard links, a fifo, a small sparse file (`set_len`), paths longer than
  1,024 bytes (built through `TreeRoot`), unreadable directories (mode
  000, restored on drop so the temp directory can be removed).
- `TempCollection`: a temp directory with a minimal `.antiq/` written
  directly by the I01 helper, with a fixed collection ID that tests
  assert. A test of I11 checks that `init` produces the same layout and
  config keys, IDs and machine name aside.
- `FixedClock` with `advance`; `SeqIds` minting `Ulid::from_parts(ms,
  counter)`.
- `ScriptedInteraction`: a queue of expected questions and answers; it
  fails the test on an unexpected question or a leftover answer.
- Two machines: two `TempCollection`s with their own `machine.name` and
  collection ULIDs on one path remote directory.
- Binary tests cannot inject a clock: they set file times relative to
  the real time and set `idle_days`, `remote_only_days` and
  `grace_hours` to 0 where a test needs immediate eligibility.
- Compression in library tests runs through one process-wide test pool
  (I07), so parallel tests do not each start level-19 encoders (R5).
- Helpers needed by one item stay in that item's test module; shared
  ones go into a new file under `src/testing/` or `tests/support/`.
  `tests/support/mod.rs` allows dead code, since each test crate uses a
  part of it.

### 4.3 Remote test doubles

- The path remote on a temp directory is a real implementation with no
  executables. It is the default remote of scenario and binary tests.
- `remote::testing::FaultRemote` (I27, `cfg(test)`) wraps a path remote:
  it fails the Nth call of an operation with an error, away or broken,
  and runs a callback between steps, so another machine can claim,
  commit or delete in the middle of a commit (ADR 0030).
- Seams for conditions an unprivileged test cannot create: a device
  boundary inside a piece or at its top, a mount point at the remote
  root, free space, the filesystem type (SMB and NFS warnings),
  `ENOTSUP` from `sync_all`, the staging volume's case and Unicode
  rules. Each seam is an internal parameter (a probe
  trait or a function value), not a config key (T17).

### 4.4 SSH and rsync

What runs where, on this machine (macOS, no guaranteed sshd):

| Layer | Runs in | Covers |
|---|---|---|
| Unit tests | `cargo test` | ssh argv and `-o` options, the ControlPath limit of 86 bytes, POSIX quoting, `/bin/sh -s` script text, the command and mux classifiers on recorded texts, rsync argv and environment, the version probe on `rsync  version 3.5.1  protocol version 33` and `openrsync: protocol version 29`, the stats parser on recorded output |
| Loopback shim | `cargo test` | `SshRemote` end to end without networking: `tests/fixtures/ssh-shim.sh` behaves like `ssh` (accepts the options antiq passes, simulates `-M -N -f`, `-O check` and `-O exit` with a socket file, runs the remote command with the local `/bin/sh`); rsync runs the shim through `-e`, which works because rsync runs an arbitrary `-e` program with plain argv (ssh research) |
| Fault modes | `cargo test` | the shim reads an environment variable to exit 255 with recorded error texts (away, broken, mux failures); a fake rsync script (`machine.rsync` points at it) exits 23, 24, 30 or prints no stats |
| Real sshd | opt-in | the unmodified `ssh` against `/usr/sbin/sshd` started by the test as the user on 127.0.0.1 |
| Linux container | opt-in (I39) | a Debian container with sshd and rsync 3.x under OrbStack (`docker` 29.8.2 answers on this machine) |

- The ssh program is a constructor parameter of the session (default
  `ssh` from `PATH`); binary tests put a directory with an `ssh` link to
  the shim first in `PATH`. rsync's `-e` uses the same program.
- Real rsync 3.1 or later is needed by the loopback and sshd layers.
  This machine has rsync 3.5.1 at `/opt/homebrew/bin/rsync` and
  openrsync at `/usr/bin/rsync` (measured). Tests that need it fail with
  a message naming the missing executable; they do not skip silently.
- Measured for this plan in the scratchpad: `/usr/sbin/sshd -f <config>`
  run as the user on 127.0.0.1:22422 with generated host and user keys,
  `StrictModes no` and `UsePAM no` accepted a public-key login; a
  master (`-M -N -f -S`), `-O check`, `/bin/sh -s` with a quoted
  argument `'a b'`, a second `set -C` create refused with "cannot
  overwrite existing file", an rsync delta through the master (740
  literal bytes after appending 100 bytes to a 2 MB file) and `-O exit`
  all worked. The login shell found `/usr/bin/rsync` (openrsync), so the
  test config sets `machine.remote_rsync` to `/opt/homebrew/bin/rsync`.
  The test reaches the server through an `ssh` wrapper on `PATH` that
  adds `-F <test config>`, which keeps the user's own `~/.ssh/config`
  out of the test.
- The sshd harness (`tests/support/sshd.rs`, I24, T23) starts one
  `sshd -D -e` per test, stopped by a `Drop` guard, on a free port taken
  through port 0, with its own ControlPath, and polls the TCP port until
  the server answers before the test connects (O7).

### 4.5 Interactive parts without a terminal

- Binary tests spawn antiq after `setsid()` in `pre_exec` (rustix
  `process`). Measured: this agent sandbox has no controlling terminal
  (`/dev/tty`: "Device not configured"), and after `setsid()` opening
  `/dev/tty` fails with ENXIO. Without `setsid()`, a test run from a
  terminal hands the terminal to antiq, which then asks and blocks; the
  sandbox would hide that (R4).
- Decision logic is tested in the library with `ScriptedInteraction`.
- `cli::tty` renders and reads through functions generic over `BufRead`
  and `Write`, unit-tested with in-memory buffers. Only opening
  `/dev/tty` and switching raw mode stay outside them.
- The picker keeps ntropy's split: all interaction logic in
  `PickerState`, unit-tested without a terminal, drawing into
  `impl Write`; layout snapshots with insta.
- The terminal glue left over is covered by a pty harness in I36 (T16):
  `openpt`, `grantpt`, `unlockpt` and `ptsname` give a pty pair;
  `tcsetwinsize` sets a window size, since crossterm asks the terminal
  for it; the child runs `setsid()` and `ioctl_tiocsctty` on the slave
  in `pre_exec`, so the pty is its controlling terminal and `/dev/tty`
  opens it; the test writes keys to the master and reads the frames.

### 4.6 Determinism

- A golden test hashes the live encoder's output for a seeded stream
  generated in memory, with 1 and with 4 workers, so umask and clock
  play no part (ADR 0052, O8).
- A tar stability test: the same tree packs to the same bytes; a
  touched file changes its header (ADR 0050, tar research).

### 4.7 Linux

The filesystem research ran on Linux (OrbStack, ext4, btrfs, tmpfs,
overlay); antiq's own tests run there in I39 through
`scripts/test-linux.sh`: `cargo test` inside a Rust container as a
non-root user, with 4 GB of memory and `RUST_TEST_THREADS=4` (R5).
Linux-only behaviour (non-UTF-8 names, `f_type` magic numbers) gets
`#[cfg(target_os = "linux")]` tests. Tests of the staging volume's
real case and Unicode rules are macOS-only; elsewhere the rules come
through the seam (S11).

### 4.8 Coverage

- `cargo llvm-cov --all-targets --lcov --output-path target/llvm-cov/lcov.info`
  (cargo-llvm-cov 0.9.1 is installed). Spawned binaries count, since
  binary tests run the instrumented build. Opt-in layers add
  `-- --include-ignored`.
- `scripts/uncovered-lines.sh <base>` (I01) lists changed lines under
  `src/` (from `git diff -U0 <base>`) that lcov reports with zero hits.
- Lines reachable only through a real terminal, a real NAS or a
  condition no seam reaches are listed with the reason in the item's
  delivery report; the lead accepts them or asks for a test.

## 5. Work items

Sizes: S up to about 500 lines with tests (up to a day), M 500 to 1000
(one to two days), L 1000 to 1500 (two to three days), XL above that
(only I01, which lands in two merges). "First failure" says what a test
shows before the change. Where an entry has no Goal line, its title is
the goal. "Shared edits" names files another item owns that this item
changes, and what it changes there.

Every item is accepted when its tests pass, the three check commands
pass on every commit, every changed production line is covered or
listed as an exception (4.8), the docs it names are updated in the
same commits, its JSON payload and new error kinds (if it adds a
command's behaviour) are in the spec, and its step todo is deleted in
its finishing commit. Acceptance lines in an entry add to this.

### Wave 1

#### I01 Lay down the module tree, shared types and the command surface

- Goal: a crate that builds, lints and tests clean, with every module
  file of section 2.1 present, the shared types of section 2.2
  complete, the whole command surface parsed, and every command
  answering `not_implemented` until its item lands. This is the
  rudimentary CLI.
- Lands in two merges. Part A: skeleton, command surface, envelope,
  errors, output rules, test support. Part B: the shared types of
  section 2.2 and `src/fsx/tree.rs`. Wave 2 starts after part B.
- Implements: ADR 0003, 0004, 0005, 0010, 0020, 0022 (terms in help
  texts), 0023, 0046 (`--dry-run` on every state-changing command),
  0055 (`import` in the surface), 0056; spec "Command line" (command
  table, flag rules, piece references parse as strings), "Dry runs,
  scheduling, exit codes" (codes), "JSON output" (envelope, error
  object, dry-run shape).
- Surface: `-C <path>` and `--json` on every command; `init
  [--pattern <glob>]... [--yes] [--dry-run]`; `status`; `archive
  [--dry-run]`; `unearth <piece> [--to <path>] [--version <n>]
  [--dry-run] [--if-reachable]`; `find [<query>] [--print]`; `fork
  <piece> [--name <name>] [--dry-run]`; `pin <piece> [--dry-run]`;
  `unpin <piece> [--dry-run]`; `sync [--dry-run] [--if-reachable]`;
  `resolve [<piece> <answer>] [--force] [--dry-run]` with the answers
  `--mine`, `--theirs`, `--fork`, `--replace`, `--skip`,
  `--drop-superseded` (one at most, only with a piece; ADR 0025);
  `remote add [--init] [--host-key-alias <alias>] <url> [--dry-run]`,
  `remote show`, `remote remove [--force] [--dry-run]`; `reconcile
  [--prune] [--force] [--dry-run] [--if-reachable]`; `import [--path
  <dir>] <file>... [--dry-run]`. A flag a command does not take is a
  usage error. clap's `help` subcommand is disabled
  (`disable_help_subcommand`), so the surface has exactly these
  subcommands (O18).
- Owns: `Cargo.toml`, `Cargo.lock` (section 3), `src/lib.rs`,
  `src/main.rs`, `src/error.rs`, `src/clock.rs`, `src/interact.rs`,
  `src/plan.rs`, `src/report.rs`, `src/tree.rs`, `src/fsx/tree.rs`, the
  type parts of `src/relpath.rs`, `src/id.rs`, `src/meta.rs` and
  `src/remote/mod.rs`, stubs of every other module file including every
  `mod.rs`, `src/ops/mod.rs` and all `ops` submodule files
  (`archive/{mod,new,unearthed}`, `unearth/{mod,local,remote,live}`,
  `resolve/{mod,replace,conflict,diverged,superseded}`,
  `sync/{mod,pull,upload,commit,conflict,live,tiers}`), `src/cli/mod.rs`,
  `src/cli/cmd/mod.rs` and stubs of every `src/cli/cmd` file (with
  `sync/{mod,conflict,live,tiers}`), `src/cli/output.rs`,
  `src/cli/tty.rs`, `src/testing/mod.rs`,
  `tests/support/{mod,cmd,tree,sshd}.rs` (`sshd.rs` a stub that I24
  fills), `tests/cli.rs`, `scripts/check.sh` (fmt, clippy, test),
  `scripts/uncovered-lines.sh`; in the spec, one paragraph per command
  under "JSON output", separated by blank lines, so parallel items edit
  different paragraphs (S9).
- Content: the shared types of section 2.2 with their serde attributes;
  the escape function on `RelPath`; `TreeRoot` with dirfd-relative
  open, stat, read-link, read-dir, create (`mkdirat`, `openat` with
  `O_NOFOLLOW`, `O_CREAT`, `O_EXCL`) and remove; `Reachability`;
  `ErrorKind` with each kind's source (section 2.4); stubs that later
  items fill without touching their callers:
  `remote::probe_state(&Collection) -> RemoteState` returns `None`,
  `remote::away::status_warning(...)` returns no warning,
  `remote::policy` returns the plain outcome (filled by I23 and I29),
  as plain stubs with unit tests; their call sites land with the first
  consumer (I16 calls `probe_state` and `status_warning`, I30 calls
  `remote::policy`; lead decision on the I01 plan, 2026-11-03); the
  `ListingEntry` key names and type values go into spec "Listing" with
  part B; a generic error kind `other` for failures without a specific
  kind; raising the soft `RLIMIT_NOFILE` in
  `main` (O11); output through `io::Result`, a `BrokenPipe` on stdout
  stopping the output while the run keeps its exit code, and the print
  lints denied (O13).
- Must not touch: any behaviour beyond parsing, the envelope and the
  shared types.
- Depends on: none.
- Tests: `antiq --help` lists the thirteen subcommands, `pin` and
  `unpin` counted apart, and no `help` subcommand (first failure: no
  subcommands); `antiq status --dry-run`, `antiq find --force`, `antiq
  status --if-reachable` and `antiq resolve --mine` without a piece
  exit 2 (first failure: unknown arguments are not defined yet); `antiq
  --json status` prints `{"format":1,"command":"status",
  "error":{"kind":"not_implemented",...}}` and exits 1; unit tests map
  every `ErrorKind` to its exit code; the envelope serialises in key
  order `format`, `command`, then the payload; the `setsid` helper in
  `tests/support/cmd.rs` proven by a test that `/dev/tty` cannot be
  opened in the child; output into a stdout pipe the reader has closed
  exits 0 without a panic; the soft `RLIMIT_NOFILE` after start is above 256;
  metadata types serialise in the spec's key order with the left-out
  keys absent and the three `verified` states; `escape(b"a%\xffb")` is
  `a%25%FFb`; `TreeRoot` creates, stats and reads a file at a depth
  past 1,024 bytes.
- Docs: spec "JSON output" paragraphs and error kinds with their
  sources; no README change.
- Acceptance: `scripts/check.sh` green; `cargo audit` clean or every
  finding listed; `cargo tree -e features -i zstd-sys` shows neither
  `legacy` nor `zdict_builder` (O1); `shellcheck` clean; every file
  carries the MPL header.
- Size: XL (two merges).

### Wave 2

#### I02 Relative paths, escaped forms and object IDs

- Goal: the payload and machine name parts, temp names, lexical
  normalisation, and minting IDs from a folder's times. Escape,
  unescape and the `RelPath`/`Name` validation are I01's (merged).
- Implements: spec "Metadata" (escaped form: valid UTF-8 stays, `%`
  becomes `%25`, every byte not part of valid UTF-8 becomes `%XX`),
  "Storage" (name part: every byte outside `[A-Za-z0-9._-]` as `%XX`,
  cut to 64 bytes without splitting an escape; machine part escaped the
  same way, cut to 32 bytes; temp name `.<final name>.<machine part>.part`),
  "Objects" (ID time: earliest of the directory's birth time and the
  birth and modification times of every entry, ignoring times before
  1990; source `birth`, `oldest-mtime`, `import`, `fork`, and `minted`
  per T2; minted once), ADR 0014, 0039.
- Owns: the functions of `src/relpath.rs` and `src/id.rs` beyond I01's
  types.
- Content: lexical normalisation for references, `--to` and `--path`
  (resolve `.` and `..`, refuse a path above the root); `RandomIds`
  through `Ulid::from_datetime` per unit and `SeqIds` for tests
  (foundations §4); `id_time()` returning time and source, with the 1990
  floor applied before minting and the T2 fallback. `SeqIds` needs an
  atomic counter, since `IdSource` is `Send + Sync`.
- Must not touch: walking the disk (the caller passes the times); I01's
  types.
- Depends on: I01.
- Tests: a name of 70 bytes with an escape across byte 64 is cut before the
  escape; `id_time` ignores a 1989 time and picks a 1995 mtime with
  source `oldest-mtime`; a birth time earlier than every mtime gives
  `birth`; times all before 1990 give the minting time, source
  `minted`, and a warning; `..` past the root is refused.
- Docs: keep spec "Storage" and "Objects" true.
- Size: M.

#### I03 Filesystem primitives and the run lock

- Goal: the helpers every writer and deleter uses.
- Implements: spec "Storage" (one atomic write: temp file in the same
  directory, `sync_all` with a fallback to plain `fsync` on `ENOTSUP`,
  rename, `fsync` of the directory), "Operations" (exclusive lock on
  `.antiq/lock` with pid, machine and start time; held lock fails at
  once naming pid and start time; moving a piece's top directory adds
  owner write first and restores the recorded mode; a trash entry that
  cannot be removed after one retry with owner write added stays with
  a warning; free space), "Walks" (the delete helper never crosses a
  device boundary), filesystem research (assessment).
- Owns: `src/fsx/{mod,atomic,remove,moves,space,device}.rs`,
  `src/lock.rs`.
- Content: `write_atomic`, `flush_file` and `flush_dir` (plain `fsync`
  through rustix on `ENOTSUP`; std's `sync_all` uses `F_FULLFSYNC` on
  macOS, which smbfs refuses); `remove_tree` through `TreeRoot` (chmod
  u+rwx retry on `PermissionDenied`, refuses to descend into another
  device); `move_dir` (owner write around the rename, `CrossesDevices`
  reported as such); `available_space` (`statvfs`); device of a path,
  mount point test (`st_dev` against the parent, `statfs` mount name on
  macOS), filesystem kind (local, SMB, NFS, other) from `statfs`
  (`f_fstypename` on macOS, `f_type` magic numbers on Linux); birth time
  (std `created()`). Lock: open without truncating, `try_lock`, then
  `set_len(0)` and write pid, machine, start; `lock::holder()` for
  `status`, `find` and dry runs reads the file and tests whether the pid
  is alive, taking no lock (T13).
- Must not touch: callers.
- Depends on: I01.
- Tests: atomic write replaces content and leaves no temp file (first
  failure: stub); the fallback path runs when the flush seam returns
  `ENOTSUP`; `remove_tree` deletes a tree with a `0555` directory (std's
  `remove_dir_all` failed there, filesystem research §4) and a tree
  deeper than 1,024 bytes; `move_dir` moves a `0555` directory to
  another parent and restores its mode (EACCES without owner write,
  reproduced in review 2, finding O4), after asserting the precondition;
  a second `try_lock` on another open file description returns held
  with the first holder's pid; the device seam makes `remove_tree` stop
  at a foreign device.
- Docs: none.
- Acceptance: all helpers used through these functions only.
- Size: M.

#### I04 Config, collection discovery and machine name

- Goal: find the collection and read and edit its config.
- Implements: ADR 0011 (search upwards like git; `-C` starts elsewhere;
  fail outside), ADR 0015 (config holds a list of remotes), spec
  "Collections and pieces" (first paragraph), "Config" (table and
  defaults; `[machine]` and `[[remote]]` parsed strictly and never
  uploaded; version 1 accepts exactly one remote), "Storage" (local
  layout paths), foundations §6 and §7.
- Owns: `src/config.rs`, `src/collection.rs`, `src/machine.rs`.
- Content: typed `Config` with every default of the table; strict
  structs for `machine` and `remote` (`deny_unknown_fields`); an error
  for a second `[[remote]]`; `memory_percent` validated to 1 to 100
  (O9); a `ConfigDoc` over `toml_edit` with the edits later items need
  (write a new config at `init`, set or remove the `[[remote]]` entry,
  set `machine.rsync`); `Collection::find(start)` and the layout
  accessors; machine name: `whoami::devicename()` on macOS falling back
  to the hostname, the hostname elsewhere.
- Must not touch: creating `.antiq/` (I11).
- Depends on: I01.
- Tests: discovery from a nested subdirectory finds the root (first
  failure: stub); `-C` start path; outside any collection gives
  `not_a_collection`; each default equals the spec table (one assertion
  per key); an unknown key under `[machine]` fails naming the key; two
  `[[remote]]` entries fail; `memory_percent = 0` and `101` fail; an
  edit keeps comments and key order.
- Docs: none.
- Acceptance: a `Collection` gives every path of the local layout.
- Size: M.

#### I05 Selection and the piece walk

- Goal: which directories are pieces, and one walk all consumers share.
- Implements: ADR 0012 (glob patterns, default `*`, shallowest match
  wins), ADR 0028 (dot entries only through a pattern with a literal
  leading dot in that component), spec "Selection" (never `.antiq/`,
  symlinks or excluded paths; lstat; never descend into a symlink;
  skipped symlinks and dot entries counted), "Walks" (lstat, no links
  followed, same device as the piece's top directory; a piece holding
  another device is flagged with the mount point path; an unreadable
  entry makes the piece unreadable with the first failing entry), the
  junk list (spec "Idle"), foundations §1, filesystem research §5.
- Owns: `src/select.rs`, `src/walk.rs`.
- Content: `glob::Pattern` with `require_literal_separator` and
  `require_literal_leading_dot`; valid UTF-8 paths match as they are,
  paths with non-UTF-8 bytes match through their escaped form (glob
  0.3.4 matches only UTF-8, O16); a top-down walk that stops at a match
  and descends only where a pattern can still match; exclude entries as
  literal relative paths with nothing below them a piece (T10);
  `walk_piece()` filling I01's `PieceTree` through `TreeRoot`, sorted by
  path bytes, junk left out, problems recorded, the allocated size read
  for each file; a piece whose top directory is on another device than
  the collection root is `mount_point` (O12); `EMFILE` and `ENFILE`
  are run errors, not `unreadable` (O11).
- Must not touch: idle rules (I10), packing; I01's tree types.
- Depends on: I01.
- Tests: `*` skips `.git`, `.*` selects it, `projects/.*` selects
  `projects/.x` (first failure: stub); `**/*` selects what `*` selects;
  a symlinked directory is never a piece and is counted; `.antiq` is
  never a piece; an excluded path and its subtree are not pieces;
  entries come out in path-byte order (`dir`, `dir-y`, `dir/x`, tar
  research); a `000` directory makes the piece unreadable naming it,
  after asserting it cannot be read; the device seam flags a foreign
  device inside a piece and at its top; a small sparse file reports an
  allocated size below its size; a tree deeper than 1,024 bytes walks
  completely; an `EMFILE` from the seam fails the run; on Linux, a
  non-UTF-8 name is selected through its escaped form.
- Docs: keep spec "Selection" true (exclude semantics; non-UTF-8 names
  match through their escaped form, where a `%` of the name reads
  `%25`) and keep spec "Walks" true (a piece whose top directory is on
  another device counts as containing a mount point).
- Acceptance: one walk API used by idle, packing, verify, change
  detection.
- Size: L.

#### I06 Packing archives: tar writer, zstd settings and the Packager trait

- Goal: pack a piece into `tar+zstd/1` with its listing in one read.
- Implements: ADR 0019, 0050, 0051; spec "Packaging" (hand-built GNU
  headers, mode masked to permission bits, uid and gid 0, empty owner
  names, modification time as the floor in whole seconds, times before
  1970 clamped to 0 and reported, no extended attributes, entries
  sorted by path bytes, hard links stored as links, special files
  skipped and reported, reads limited to the header size with a short
  read padded with zeros and marked as changed, SHA-256 per file in the
  same read, the sparse warning above 1 GiB; archives at level 19 with
  long-distance matching, window log 27, default job size, content
  checksum; every decoder sets the maximum window to 128 MiB), tar
  research assessment, compression research ("Archive settings through
  the crate").
- Owns: `src/pack/mod.rs` (trait, format id, settings types),
  `src/pack/tar_write.rs`, `src/pack/hashing.rs`, `src/pack/zstd.rs`
  (encoder settings for archives and live packages as named constants,
  decoder constructor with `window_log_max(27)`).
- Content: one top-level directory, the container, named after the
  folder when packed, holding the piece's entries; listing paths are
  relative to the container (T3, F5); files read through `TreeRoot`;
  hard links by `(dev, ino)` through `append_link`; the sparse warning
  as a pure function of the tree's sizes and allocated sizes (S10);
  worker count as a plain parameter (I07 supplies it).
- Must not touch: verification and extraction (I12, I13), live-package
  tolerance (I14).
- Depends on: I01.
- Tests: a `0600` file keeps its mode, uid 0, empty uname (first
  failure: stub); entries in path-byte order below the container;
  packing the same tree twice gives the same bytes and touching a file
  changes them; a hard link reads back as `EntryType::Link`; a fifo is
  skipped and reported; a 1969 mtime is stored as 0 and reported; the
  zstd frame has the checksum flag and window log 27 and decodes with
  the 128 MiB limit; the sparse warning function fires for an apparent
  size 1 GiB above the allocated size and not below; a reader that
  returns fewer bytes than the header size pads and marks the file
  changed; a file past 1,024 bytes of path packs.
- Docs: keep spec "Packaging" and "Listing" true (the container;
  listing paths below it).
- Acceptance: listing entries equal what I12 reads back.
- Size: L.

#### I07 Compression worker pool and memory budget

- Goal: one pool per run that hands out workers.
- Implements: spec "Packaging" (one pool of one worker per core shared
  by all compression; a piece smaller than one job gets one worker,
  larger pieces up to the free workers, several pieces in parallel;
  compressors in flight limited by about 256 MiB per level-19 worker
  against `memory_percent` of RAM), ADR 0051, `collection.memory_percent`.
- Owns: `src/pack/pool.rs`, `src/sysmem.rs`.
- Content: cores from `std::thread::available_parallelism`; RAM from
  `sysconf(_SC_PHYS_PAGES)` times the page size (T12); `Pool::lease(want,
  bytes_per_worker)` blocks until at least one worker and the memory
  allow it; when nothing is leased, one worker is always granted, with a
  warning when the budget is below one worker (O9); dropping the lease
  returns its workers; a process-wide pool for library tests (O15).
- Must not touch: the encoder (I06 takes the worker count).
- Depends on: I01.
- Tests: two leases never exceed the cores (first failure: stub); with
  1 GiB RAM at 25 % only one level-19 worker is handed out; with a
  budget below one worker one worker is still granted and a warning
  given; a lease waits until another is dropped (thread test with a
  timeout); a piece below one job gets one worker.
- Docs: keep spec "Packaging" true (the one-worker floor).
- Size: S.

### Wave 3

#### I08 Metadata and listing IO and the local object store

- Goal: read and write the formats shared through the remote, and their
  local home.
- Implements: ADR 0053; spec "Metadata" (key order, keys left out where
  they do not apply, unknown fields kept, higher `format` read-only with
  a clear error on write, escaped paths), "Listing" (zstd-compressed
  JSON Lines, first line `{"format":1}`, one line per entry, file name
  `<sha256 16 hex>.listing.zst`), "Storage" (`.antiq/objects/<ulid>/`
  with the remote's layout; payload files never overwritten, a put onto
  an existing name succeeds only when the full SHA-256 matches), section
  2.9, foundations §5.
- Owns: the functions of `src/meta.rs` beyond I01's types,
  `src/listing.rs`, `src/store.rs`.
- Content: a first pass reading only `format`; times in whole seconds
  (T15); history actions named after the rows of spec "Metadata
  changes", the committing version's entry carrying the commit time
  (T6); listing writer at level 3 with the content checksum and a
  streaming reader through the 128 MiB decoder (T4); store: current
  metadata (the highest `meta.<N>` that parses, plus `meta.pending`),
  write pending, promote pending to `meta.<N>` with the number the
  protocol gave, put a payload or listing from `.antiq/tmp/` under its
  final name, paths, object enumeration. Writes go through
  `fsx::write_atomic` and take `&RunLock`.
- Must not touch: remote code; I01's metadata types.
- Depends on: I01, I02, I03, I04.
- Tests: a written metadata file has keys in the spec order (first
  failure: stub); unknown keys at the top and inside a payload survive a
  read and write; a `format: 2` file reads but refuses a write with
  `format_too_new`; a listing round trip keeps a path with a non-UTF-8
  byte; the store picks the highest parsing version and reports the
  unparsable one; promoting `meta.pending` gives `meta.<N>`; putting
  different bytes under an existing payload name fails; a `compile_fail`
  doctest shows a store write without a `RunLock`.
- Docs: spec "Listing" (key names; keep the compression settings
  true), "Metadata" (history action names; keep whole seconds true);
  keep ADR 0058 true (section 2.9, T1).
- Size: L.

#### I09 Tracking entries and derived states

- Goal: the local record per folder and the state every command shows.
- Implements: spec "Local state" (path, ID, metadata version taken out,
  unearthed time, state `unearthing`, `live`, `removing`, `gone`, local
  payloads, metadata versions this machine committed, pending
  decisions; remote-only derived), "States" (table), "Decisions" (a
  pending decision carries its options), ADR 0040 (live on another
  machine).
- Owns: `src/tracking.rs`.
- Content: `TrackingEntry` with serde and atomic writes to
  `.antiq/tracking/<ulid>.json`; "version taken out" as a number or
  `pending` (F21); the payload version unearthed (F16); `written`, the
  time the entry was written (F22); `PendingDecision` for the four kinds
  of the decision table; fields `status` needs: stale backup since,
  missing on the remote, diverged; `derive_state(entry, metadata,
  folder_present, own_collection)` for every row of the states table,
  over I01's metadata types.
- Must not touch: live manifests (I14 owns their files).
- Depends on: I01, I02, I03, I04.
- Tests: one case per row of the states table (first failure: stub);
  round trip with `pending` as version taken out; `written` set on each
  write; a write without `RunLock` does not compile.
- Docs: keep spec "Local state" true (the added fields).
- Size: M.

#### I10 Idle check and pins

- Goal: decide per piece whether it is pinned, empty, active or idle.
- Implements: ADR 0013, 0024, 0029, 0037, 0038; spec "Idle" (newest
  regular-file modification time older than `idle_days`; directory
  times do not count; times more than one day ahead do not count and
  are listed; ignore rules from the config and `.antiqignore` files at
  any level, for this check only; changes to `.antiqignore` and
  `.antiq-pin` are no activity; junk left out; an unearthed piece's
  clock starts at the later of its newest time and its unearthing;
  empty when no file counts; only future files is not empty and not
  idle), "Pins" (`.antiq-pin` at the top; checked before the idle and
  empty tests), foundations §2.
- Owns: `src/idle.rs`, `src/pin.rs`.
- Content: `ignore::gitignore::Gitignore` from `collection.ignore` lines
  and one per `.antiqignore`, anchored at its directory; the verdict
  type of section 2.2; `pin::create`, `pin::remove`, `pin::is_pinned`
  (writes through I03's helpers).
- Must not touch: the commands `pin` and `unpin` (I18).
- Depends on: I03, I04, I05.
- Tests: a pinned empty piece is pinned (first failure: stub); a piece
  whose only files are ignored, markers or junk is empty; a newer
  `.antiqignore` is not activity; a file dated two days ahead is
  ignored and listed; only future files gives "not idle, not empty"; a
  newer directory time does not count; the unearthed clock wins over
  old file times; `sub/*.log` with `!sub/keep.log` in a nested
  `.antiqignore` hides `sub/b.log` and keeps `sub/keep.log`
  (foundations §2 example).
- Docs: none.
- Size: M.

#### I11 `init`

- Goal: make a directory a collection.
- Implements: spec "Collections and pieces" (`init` paragraph: mint the
  collection ULID and store it; list what the patterns select and ask
  before creating; detect `archive/` with `*.tar.xz` and `index/*.lst`
  and propose excluding it; note a pattern containing `**`; without a
  terminal or with `--json` create nothing and print the plan unless
  `--yes`, exit 3; `--pattern` repeatable; every other setting at its
  default), "Config" (`machine.name` stored at `init`), ADR 0011, 0046.
- Owns: `src/ops/init.rs`, `src/cli/cmd/init.rs`, `tests/init.rs`;
  creates the `.antiq/` layout directories.
- Must not touch: config parsing (I04).
- Depends on: I02, I03, I04, I05.
- Tests: `antiq init --yes --json` creates `.antiq/config.toml` with a
  collection ULID, `patterns` and the machine name (first failure:
  `not_implemented`); without `--yes` and without a terminal it exits 3
  and creates nothing; a scripted "no" creates nothing; the
  `archive.sh` layout yields an exclude proposal (with `--yes` per U5);
  `**` produces the note; `--dry-run` creates nothing; `init` inside a
  collection follows U5.
- Docs: spec "JSON output" (`init` paragraph).
- Size: M.

#### I12 Strict verify and comparison with a listing

- Goal: the checks that gate every deletion and every move into place.
- Implements: ADR 0033, 0035; spec "Verifying" (stream integrity, then
  every member against the tree: type, size, content hash,
  modification time in whole seconds with the same clamp, mode, link
  target; equal path sets under the same walk rules; junk left out; the
  lstat of each entry recorded while reading: size, modification time
  in nanoseconds, ctime, inode), the listing comparison used by
  reclaiming and by the start-of-run repairs (path, type, size,
  modification time in whole seconds, mode, link target, content hash;
  directory times and junk do not count), tar research ("Strict
  verify").
- Owns: `src/pack/read.rs` (members of a package as a stream, listing
  from a package), `src/pack/verify.rs`.
- Content: both comparisons work on the paths below the container, so
  the container's name and the folder's name do not count (T3, F5);
  the tree side goes through `TreeRoot`.
- Must not touch: extraction (I13).
- Starts from: I06 left `verify_strict(package, root, rules)` in `src/pack/verify.rs` and `list(package)` in `src/pack/read.rs` as stubs returning `PackError::NotImplemented`, called by `TarZstd`; fill their bodies and extend the unit structs `Verified`, `Unpacked`, `WalkRules`, `TargetRules` in `src/pack/mod.rs` as needed.
- Depends on: I05, I06.
- Tests: a fresh pack verifies with no differences (first failure:
  stub); an edited file, a deleted file, an added file, a changed mode
  and a repointed symlink are each reported (tar research case list); a
  truncated stream fails the integrity check; junk on disk is ignored;
  the lstat record holds every entry; the listing comparison ignores a
  newer directory time and reports a changed mtime; a folder renamed
  after packing still verifies; a tree past 1,024 bytes of path
  verifies.
- Docs: none.
- Size: M.

#### I13 Extraction into staging and name checks

- Goal: extract safely and refuse names the target cannot hold.
- Implements: ADR 0027 (a member path leading outside the piece is
  refused; unearthing never writes through a symlink), ADR 0034, ADR
  0050 (members read with the crate, each written after antiq's own
  checks through directory handles that never follow a symlink, stop at
  the first refused member, directory modes and times in a final pass),
  ADR 0054 (check every member name before extracting: names the target
  cannot hold, name lengths, collisions under the target's
  case and Unicode rules from a probe of the staging volume; refuse
  with the list and the reason; at archive time warn for macOS and
  Linux rules), spec "unearth" (paths longer than the platform limit
  extracted directory by directory), tar research (owner `rwx` on
  directories until the final pass), names research.
- Owns: `src/pack/extract.rs`, `src/pack/names.rs`; adds
  `unicode-normalization` (T11).
- Content: the container's contents are extracted into the target
  directory, whatever its name (F5); paths past the platform limit are
  created through `TreeRoot` (`mkdirat`, `openat` with `O_NOFOLLOW`,
  `O_CREAT`, `O_EXCL`) (O5); the staging volume's rules come from a
  probe behind a seam (S11).
- Must not touch: the strict verify (I12).
- Starts from: I06 left `unpack(package, staging, rules)` in `src/pack/extract.rs` as stubs returning `PackError::NotImplemented`, called by `TarZstd`; fill their bodies and extend the unit structs `Verified`, `Unpacked`, `WalkRules`, `TargetRules` in `src/pack/mod.rs` as needed.
- Depends on: I06.
- Tests: the six hostile archives of the tar research (absolute path,
  `../`, symlink then child, symlink to `/etc`, symlink then directory,
  hard link outside) are refused or stopped with nothing written
  outside staging (first failure: stub); a round trip keeps file,
  symlink and directory modes and times; a `0644` directory extracts its
  children; with the seam's case-insensitive rules, `README` and
  `Readme` in one directory are refused with both names listed, and
  `café` in NFC and NFD likewise; a macOS-only test runs the real probe
  on the temp volume; a 256-byte name is reported for Linux at archive
  time; a 1,300-byte path extracts.
- Docs: none.
- Size: L.

#### I14 Live packages and change detection

- Goal: rsync-friendly live packages and knowing when to make one.
- Implements: ADR 0018, 0052 (level 3, rsyncable, job size 512 KiB, at
  least one worker, no long-distance matching, pinned zstd, golden test,
  version and parameters recorded); spec "Verifying" (tolerant verify:
  a file whose lstat changes during its read, or that reads short,
  discards the package and restarts the piece up to `live_retries`
  times, then the piece is skipped and shown as "backup stale since";
  vanished files dropped; stream checksum verified before upload),
  "Change detection" (manifest of path, type, size, modification time
  in nanoseconds, inode, ctime and directories; repack when the tree
  differs; same SHA-256 means only the manifest is replaced), the live
  exclude list (spec "Packaging"), compression research (byte identity;
  `NbWorkers(0)` ignores rsyncable).
- Owns: `src/pack/live.rs`, `src/pack/manifest.rs` (stored as
  `.antiq/tracking/<ulid>.manifest.json`, T5).
- Must not touch: `sync` (I32 drives it).
- Depends on: I05, I06.
- Tests: the golden hash of the live encoder over a seeded in-memory
  stream, the same with 1 and 4 workers (first failure: stub, O8); a
  seam that changes a file between read and second lstat makes the
  packer restart, and after `live_retries` the piece is stale; a
  vanished file is dropped; the manifest diff reports a touched file and
  a new directory; repacking unchanged content reports the same
  SHA-256; the encoder record names zstd 1.5.7 and the parameters.
- Docs: keep spec "Storage" true (manifest file name).
- Size: L.

### Wave 4

#### I15 Index and piece references

- Goal: search all objects and resolve what a user types.
- Implements: spec "Storage" (index derived from metadata and listings,
  plain files, rebuildable, behind a module interface), "Command line"
  (a reference is a path or a ULID; paths resolved against the current
  directory, normalised, relative to the root, outside refused,
  archived pieces resolve the same way; a ULID prefix of at least 6
  characters unique among the objects this machine knows; several
  matches refuse with the list, none is an error, both exit 1; JSON
  kinds `ambiguous_reference` with candidates and `unknown_reference`),
  ADR 0007 (remote-only archives searchable like local ones).
- Owns: `src/index.rs`, `src/reference.rs`.
- Content: rebuild from the store and tracking; an update call for
  state-changing commands; read-only callers compute in memory when the
  index is missing (T13); search behind one function whose matching
  follows U1, including member paths from listings if U1 says so.
- Paths: a path the user typed has `.` and `..` folded lexically; then
  only the shortest ancestor that is the collection root itself (same
  device and inode) is replaced by the root, so another spelling of the
  root (macOS `/var` and `/private/var`) still lies below it, and the
  rest stays lexical: symlinks below the root are never followed. I02's
  `below_root` stays lexical (lead decision, 2026-11-04, after a review
  finding of I02's plan).
- Depends on: I08, I09.
- Tests: a unique 6-character prefix resolves (first failure: stub); 5
  characters are refused; two objects sharing a prefix give
  `ambiguous_reference` with both; the path of an archived piece
  resolves; a path outside the collection is refused; a rebuilt index
  equals the incrementally updated one.
- Docs: none.
- Size: M.

#### I16 `status`

- Goal: show every piece by state.
- Implements: spec "status" (read-only; pinned, new, empty, gone,
  unreadable, containing a mount point, future modification times,
  stale backups, diverged, needing a decision, superseded archives,
  pending remote work, missing on the remote, recovered trees, skipped
  symlinks and dot entries, "remote-only in N days"), "Selection" (a
  folder at a path no tracking entry claims is a new piece, shown next
  to an archived piece of the same path), "Objects" (a piece without an
  ID shows as new), "Operations" (a note while a run holds the lock; a
  warning for a collection on SMB or NFS), "Committing a version"
  (`status` reports an unparsable highest `meta.<N>`), "JSON output"
  (`status` payload: `collection`, `remote.state`, `pieces` with `id`,
  `path`, `name`, `state`, `flags`, `remote_only_in_days`), ADR 0040.
- Owns: `src/ops/status.rs`, `src/cli/cmd/status.rs`, `tests/status.rs`,
  README (first milestone).
- Content: every flag computed from the fields I01, I08 and I09 define,
  so later items only set fields; `remote.state` through the I01 stub
  until I23 fills it; the away warning through the I01 stub until I29.
- Must not touch: data producers.
- Also: print one warning per unknown `[collection]` key from
  `Config::unknown_collection_keys` (section 11, I04).
- Depends on: I04, I05, I08, I09, I10.
- Tests: a fixture with new, pinned, empty, unreadable and future-time
  pieces gives the expected JSON (first failure: `not_implemented`);
  fixture tracking and metadata give archived, remote-only with days,
  gone, live on another machine, superseded, stale, needing a decision,
  missing on the remote, recovered; an unparsable highest version is
  reported; a held lock prints the note; a console snapshot.
- Docs: spec "JSON output" (`status` paragraph, if field details are
  added); README: `init` and `status` work.
- Size: L.

#### I17 Run guard, start-of-run repairs and dry-run plans

- Goal: the frame every state-changing command runs in.
- Implements: spec "Operations" (lock with pid, machine and start time;
  held lock exits 1 naming pid and start time; the start-of-run
  repairs; `recovered/` never emptied; read-only commands and
  `--dry-run` never touch the run directories), ADR 0046 (plan
  rendering, JSON `dry_run` and `actions`).
- Owns: `src/run.rs`, `src/repair.rs`, `src/plan.rs` (adds rendering
  helpers to I01's type).
- Content: the repairs in this order (F4): first finish `removing`
  entries, for a folder still in place and for a tree in
  `trash/<ulid>` alike, by comparing it with the listing of the
  committed version and then deleting it, since the verify's lstat
  record does not survive a crash; then empty `trash/`, `tmp/` and
  `staging/`, with one retry adding owner write, warning and continuing
  on failure; turn an `unearthing` entry whose folder matches the
  version's listing into `live` with the entry's `written` time as
  unearthing time (F22), drop any other `unearthing` entry and report an
  unrecognised folder once as a new piece; treat a present folder of an
  ID this machine committed as archived as unearthed at that version,
  now.
- Must not touch: command files.
- Also: remove the temp files an interrupted `write_atomic` left in
  `.antiq/` (`.<name>.<pid>.<n>.tmp`, outside `recovered/`), with a
  test (section 11, I03).
- Depends on: I03, I04, I08, I09, I12.
- Tests: a held lock gives `lock_held` with the pid (first failure:
  stub); a `removing` entry with its tree in `trash/<ulid>` is
  finished before the trash is emptied, and one with its folder in place
  likewise; a leftover file in `trash/` is gone after `RunGuard::begin`;
  an entry that resists removal (seam) gives a warning and the run goes
  on; an `unearthing` entry with a matching folder becomes `live` with
  the entry's `written` time; one with a differing folder is dropped and
  the folder reported as new; a folder of an own archived ID becomes
  unearthed; a dry-run touches nothing.
- Docs: keep spec "Operations" true (the repair order: `removing`
  entries before emptying the trash).
- Size: L.

### Wave 5

#### I18 `find` without the picker, `pin`, `unpin` and local `reconcile`

- Goal: list and search, pin, and rebuild the index.
- Implements: ADR 0021, 0024; spec "find" paragraph of "Command line"
  (lists and searches all objects; `--print` prints the absolute path
  of the picked piece's folder, for an archived piece the path it would
  be unearthed to; without a terminal the path of every match, one per
  line; with `--json` the matches and `--print` adds nothing), "JSON
  output" (`find` payload: `query`, `matches` with `id`, `path`,
  `abs_path`, `name`, `state`, `created`, `size`), "Operations"
  (`status` and `find` print a note while a run holds the lock), "Pins"
  (`pin` and `unpin` refuse on a piece whose folder is not present; no
  metadata change), "reconcile" (rebuilds the index, does the
  start-of-run repairs; the remote part is I35).
- Owns: `src/ops/{find,pin,reconcile}.rs`,
  `src/cli/cmd/{find,pin,unpin,reconcile}.rs`,
  `tests/{find,pin,reconcile}.rs`.
- Content: interactive `find` prints the list until I36 lands.
- Depends on: I10, I15, I17.
- Tests: `find --json` lists objects with full ULIDs (first failure:
  `not_implemented`); a query narrows the list per U1; `find --print`
  without a terminal prints one absolute path per match, for an
  archived piece its stored path; `find` prints the lock note while a
  run holds the lock (H4); `pin` creates `.antiq-pin` and a second run
  reports it already pinned; `pin` on an archived piece refuses;
  `reconcile` rebuilds a deleted index; `reconcile --dry-run` prints the
  plan.
- Docs: spec "JSON output" (`pin`, `unpin`, `reconcile` paragraphs).
- Size: M.

#### I19 `archive` for pieces that are not unearthed

- Goal: archive idle pieces and remove their folders safely.
- Implements: spec "archive" steps 1 to 5 (pack into `tmp/` and verify
  strictly; store payload and listing and record the metadata change,
  all durable before the next step; tracking `removing` and rename into
  `trash/`; walk the trashed tree against the verify's lstat record and
  on a difference rename it back and treat it as unearthed at the
  committed version, or move it to `recovered/<ulid>-<time>/` when its
  path is taken; delete and clear the entry; when a rename fails, walk
  and compare in place and delete there with the entry `removing`;
  upload left to `sync`), "Objects" (`archive` mints IDs for new
  pieces), "Operations" (free space against the apparent size with
  parallel pieces reserving their sum; `ENOSPC` removes that step's
  temp output and skips the piece; exit 3), "Metadata changes"
  (`archive` row), ADR 0032, 0033, 0040 (other collections' live pieces
  untouched), 0045 (`archive` outside the brake), 0046, 0050 (sparse
  warning), 0054 (name warnings, archived anyway).
- Owns: `src/ops/archive/{mod,new}.rs`, `src/cli/cmd/archive.rs`,
  `tests/archive.rs`.
- Content: the trashed tree is named `trash/<ulid>`, so the repair of
  I17 finds it (F4); the tracking entry's "version taken out" reads
  `pending` until `sync` commits (F21); parallel pieces through the I07
  pool; the proof tokens of section 2.8; the diverged refusal hook, a
  check of the tracking flag that I31 sets; the trash re-walk through
  `TreeRoot`.
- Must not touch: unearthed pieces (I26 adds `unearthed.rs` and its
  routing).
- Depends on: I02, I06, I07, I08, I09, I10, I12, I13, I15, I17.
- Tests: an idle piece ends as payload, listing and pending metadata
  with lifecycle `archived`, its tracking says `pending`, and its folder
  is gone (first failure: `not_implemented`); a verify difference
  (seam) keeps the folder; a write after the verify, detected in the
  trash, renames the folder back as unearthed; the same with its path
  taken lands in `recovered/`; too little space (seam) skips with exit
  3; pinned, empty, unreadable and mount-point pieces stay; `--dry-run`
  lists the pieces and changes nothing; a name colliding on macOS rules
  gives a warning and archives; two pieces run in parallel; a piece
  with a path past 1,024 bytes archives.
- Docs: spec "JSON output" (`archive` paragraph).
- Size: L.

#### I20 `unearth` from a local archive

- Goal: bring an archived piece back from a local archive.
- Implements: spec "unearth" (current archive or payload version `n`;
  checksum check; extract into staging; strict verify; tracking
  `unearthing`; target checked again, missing parents created, a file,
  symlink or piece in the path refuses; rename into place; top
  directory's mode; entry `live` with the unearthing time; an ID
  already unearthed does nothing and says where; target is the stored
  path or `--to`, inside the collection and selected by the patterns;
  a taken target refuses; name checks before extracting), "Operations"
  (space for the listing's total size; owner write around moves),
  "Metadata changes" (`unearth` row), ADR 0034, 0037, 0054.
- Owns: `src/ops/unearth/{mod,local}.rs`, `src/cli/cmd/unearth.rs`,
  `tests/unearth.rs`.
- Content: the tracking entry records the payload version unearthed
  (F16); an archived ID without a local archive fails with a clear
  error until I34 adds fetching.
- Paths: a path the user typed has `.` and `..` folded lexically; then
  only the shortest ancestor that is the collection root itself (same
  device and inode) is replaced by the root, so another spelling of the
  root (macOS `/var` and `/private/var`) still lies below it, and the
  rest stays lexical: symlinks below the root are never followed. I02's
  `below_root` stays lexical (lead decision, 2026-11-04, after a review
  finding of I02's plan).
- Depends on: I08, I09, I12, I13, I15, I17.
- Tests: archive then unearth restores the tree with modes, times,
  symlinks and hard links, and tracking is `live` with the unearthing
  time and payload version (first failure: `not_implemented`);
  `--version 1` picks the first payload; `--to` outside the patterns
  refuses; a taken target refuses; a staged-tree difference (seam)
  moves nothing; a colliding name list refuses before extraction;
  unearthing twice reports the location; archive and unearth of a piece
  with a path past 1,024 bytes end to end (O5).
- Docs: spec "JSON output" (`unearth` paragraph).
- Size: L.

#### I21 `import`

- Goal: register existing zstd tar archives as archived pieces.
- Implements: ADR 0055, ADR 0019 (no xz); spec "import" (refuse
  anything but tar in zstd by magic bytes, naming the codec; refuse a
  frame with a window above 128 MiB naming `zstd -19 --long=27`, and
  any frame without the content checksum; read the whole stream, size,
  SHA-256 and listing as `archive` does; exactly one top-level folder;
  members leading outside refused; name warnings; a file whose SHA-256
  is already a payload is skipped and reported; copy (a clone where the
  filesystem supports one, never a hard link) into `tmp/`, flush,
  rename to the payload name, record a new ID with the oldest member
  modification time, source `import`, the top folder's name, payload
  version 1; `--path` default the top folder's name at the root, a path
  the patterns do not select refuses, a path taken by a folder or object
  warns; the input is never moved or deleted; upload at the next
  `sync`), compression-bench ("Decompression flags").
- Owns: `src/ops/import.rs`, `src/pack/frames.rs`,
  `src/cli/cmd/import.rs`, `tests/import.rs`.
- Content: the archive's single top-level folder is the container of
  `tar+zstd/1` and the listing holds the paths below it (F5); frame
  walking over headers and block headers, so every frame's window and
  checksum flag are checked without holding a frame in memory;
  `std::fs::copy` clones on macOS through `fclonefileat` with an
  `fcopyfile` fallback (std source, `sys/fs/unix.rs`).
- Paths: a path the user typed has `.` and `..` folded lexically; then
  only the shortest ancestor that is the collection root itself (same
  device and inode) is replaced by the root, so another spelling of the
  root (macOS `/var` and `/private/var`) still lies below it, and the
  rest stays lexical: symlinks below the root are never followed. I02's
  `below_root` stays lexical (lead decision, 2026-11-04, after a review
  finding of I02's plan).
- Depends on: I06, I08, I12, I13, I15, I17.
- Tests: a zstd tar made with the crate imports and `find` shows it
  (first failure: `not_implemented`); xz and gzip magic refuse naming
  the codec; window log 28 refuses naming `zstd -19 --long=27`; no
  checksum refuses; two top-level folders refuse; `../x` refuses; a
  second import of the same file is skipped; the input file is
  unchanged afterwards; a taken path warns and both objects are found;
  an import with `--path` to another name later reclaims against its
  listing.
- Docs: spec "JSON output" (`import` paragraph).
- Size: L.

### Wave 6

#### I22 `fork`

- Implements: spec "fork" (only unearthed pieces; new ID with the time
  of the fork, source `fork`; tracking keeps its unearthed time;
  `--name` renames within the parent, a taken name refuses; the old
  ID's live package is deleted, recorded as the `fork` row of
  "Metadata changes"; unearthing the old ID later needs `--to`), ADR
  0014.
- Owns: `src/ops/fork.rs`, `src/cli/cmd/fork.rs`, `tests/fork.rs`.
- Content: a core that only mints the new ID (F1). The `fork` row of
  "Metadata changes" (old ID: lifecycle `archived`, live payload
  removed; new ID created) is written by the `fork` command and by I26's
  `--fork` answer. I31's conflict fork and I34's fork of another
  collection's live piece call the core but never write the old ID.
- Depends on: I15, I17, I20.
- Tests: fork of an unearthed piece gives a new ID with source `fork`
  and the old unearthed time, and records the `fork` row for the old ID
  (first failure: `not_implemented`); fork of a live piece refuses;
  `--name` renames; a taken name refuses; unearthing the old ID without
  `--to` refuses on the taken path.
- Docs: spec "JSON output" (`fork` paragraph).
- Size: S.

#### I23 Remote trait, path remote, marker and the `remote` commands

- Goal: attach a path remote and offer the trait every later item uses.
- Implements: ADR 0015, 0016 (attaching copies the remote's metadata;
  archives fetched on demand), 0026, 0031, 0047; spec "Remotes" (URL
  forms `ssh://[user@]host[:port]/<path>` and an absolute local path;
  root path only `[A-Za-z0-9._/-]`; `remote add` refuses while a remote
  is attached and when a probe fails, writing nothing; a failing
  exclusive create is one of the probes; `remote show` prints kind, URL,
  ULID, reachability and probed versions; `remote remove` detaches only
  and lists the remote-only pieces that become unreachable, the brake
  follows in I35; a path remote whose root is not a mount point gets a
  warning), "Path remote" (plain copy with `sync_all` and the `fsync`
  fallback for file and directory; reachability mapping: missing root,
  or a root that is not a mount point and has no marker, is away;
  another ULID, a mount point without marker or an I/O error on the
  marker is broken), "Storage" (remote layout; temp names; put onto an
  existing name only with a matching SHA-256), "Committing a version"
  (path remote clock from a probe file's modification time).
- Owns: the trait and opening in `src/remote/mod.rs` (beyond I01's
  types), `src/remote/{url,marker,path}.rs`, `src/ops/remote.rs`,
  `src/cli/cmd/remote.rs`, `tests/remote_path.rs`; fills the
  `remote::probe_state` stub, so `status` shows `remote.state`.
- Content: opening by URL with an `ssh` arm that answers `unsupported`
  until I28 (S4); the path remote's checksum reads the file back past
  the client cache, through `fcntl_nocache` on macOS and `fadvise` with
  "don't need" on Linux (O17); `pull_metadata` in one call.
- Depends on: I03, I04, I08, I15, I17.
- Tests: `remote add --init <dir>` writes the marker and the
  `[[remote]]` entry (first failure: `not_implemented`); `remote add`
  on a directory without marker refuses and writes nothing; a failing
  exclusive create (seam) refuses `remote add` and writes nothing (F18);
  a second `remote add` refuses; marker with another ULID gives broken;
  missing root gives away; the mount seam turns a missing marker into
  broken; attaching copies the remote's metadata into the store;
  `remote show --json`; `remote remove` detaches and keeps local
  metadata; payload put under an existing name with different bytes
  fails; an `ssh://` URL answers `unsupported`.
- Docs: spec "JSON output" (`remote add`, `show`, `remove`
  paragraphs).
- Size: L.

#### I24 SSH session layer and the loopback shim

- Goal: run commands on the NAS through the user's `ssh`.
- Implements: ADR 0048; spec "SSH remote" (system `ssh`; a master per
  run with `ControlPersist` from `machine.control_persist`, a short
  `ControlPath` in a per-user directory named with the run's pid;
  sockets of runs that no longer exist removed at start; `-O check`
  before each remote phase and a new master when it fails; `-O exit` at
  the end; a fallback to a direct connection is acceptable;
  `BatchMode=yes`, `ConnectTimeout`, `ServerAliveInterval`,
  `ServerAliveCountMax`, `HostKeyAlias` when configured; every command
  as `/bin/sh -s` with the script and quoted values on stdin; `set -C`
  for exclusive creation; exit 255 and the error text decide away or
  broken), "Committing a version" (SSH clock from `date +%s` in the
  same command), ssh research (option A).
- Owns: `src/remote/ssh/{session,script,classify}.rs`,
  `tests/fixtures/ssh-shim.sh`, `tests/ssh_session.rs`,
  `tests/support/sshd.rs` (fills I01's stub; harness of 4.4).
- Content: the ControlPath directory is `/tmp/antiq-<uid>/`, mode
  `0700`, checked before use; the ControlPath is at most 86 bytes,
  since `ssh -M` binds the path plus a 17-character suffix, and a
  longer one is antiq's own error (O2). Restarting a master after a
  failed `-O check` is `-O exit`, then removing the socket file (only a
  socket, only in antiq's directory), then `-M`, then `-O check`; if
  that fails too, commands run as direct connections with a warning
  (O3). Two classifiers (O4): a command's result is classified only on
  exit 255 and only by lines with ssh's own prefixes, ignoring "Control
  socket" lines; mux failures are classified apart; "Connection
  refused" counts as away only in the "ssh: connect to host ...
  Connection refused" form (T14); a `mux_client_` line saying the
  session was refused (sshd's `MaxSessions`) gives one warning per run.
- Depends on: I01, I04.
- Tests: argv carries every `-o` option (first failure: stub); an
  86-byte ControlPath is accepted and an 87-byte one refused; a
  directory with the wrong mode or owner is refused; quoting survives
  `'`, spaces and newlines; the classifiers map each recorded text (ssh
  research list, mux texts) as above; a stale socket of a dead pid is
  removed; through the shim, a master starts, `-O check` passes, a
  script runs with arguments, `set -C` refuses a second create, a dead
  master is replaced by the restart sequence; opt-in: the same against
  the real sshd, plus `kill -9` of the master followed by the restart.
- Docs: keep spec "SSH remote" true (the 86-byte limit and the
  ControlPath directory).
- Size: M.

#### I25 rsync resolution, probe and command line

- Goal: run rsync the way ADR 0049 fixes.
- Implements: ADR 0049; spec "Transfers" (3.1 or later, openrsync
  refused; local rsync resolved at `remote add` from the configured
  path, else the first of `PATH`, `/opt/homebrew/bin`, `/usr/local/bin`
  that passes the probe, stored as an absolute path, probed every run;
  `LC_ALL=C`, every `RSYNC_*` variable and `CVSIGNORE` removed,
  `--stats`, `--no-human-readable`, `--no-whole-file`, `--timeout` from
  `machine.rsync_timeout`, never `--inplace` or `--append`;
  `--partial-dir=.antiq-partial` for archive uploads; `--rsync-path`
  from `machine.remote_rsync`; any non-zero exit fails), rsync research
  (version line forms, stats labels under `LC_ALL=C`, exit 30 on
  timeout).
- Owns: `src/remote/rsync.rs`, `tests/fixtures/fake-rsync.sh`.
- Depends on: I01, I04.
- Tests: the probe accepts `rsync  version 3.5.1  protocol version 33`
  and refuses `openrsync: protocol version 29` and 3.0.9 (first
  failure: stub); resolution order with fake executables on a temp
  `PATH`; the environment lacks `RSYNC_PASSWORD` and `CVSIGNORE`; argv
  has the fixed flags and never `--inplace`; the stats parser reads
  `Literal data: 1408 bytes` and treats missing stats as unknown; exit
  23, 24 and 30 from the fake each fail the transfer.
- Docs: none.
- Size: M.

### Wave 7

#### I26 Re-archiving unearthed pieces: reclaim, replace and the loss decision

- Goal: archive unearthed pieces again without losing content.
- Implements: ADR 0025, 0033 (reclaim gate), 0035, 0036; spec
  "archive" (an unchanged unearthed piece is reclaimed: folder removed
  the same way, archive kept, no new payload and no upload; it needs a
  local archive that passes the stream check or a recorded remote
  check, else pending remote work; a changed one replaces its archive;
  no loss means no question; loss is removed paths or a file's content,
  mode, link target or modification time changed; ask replace, fork or
  skip; without a terminal skip and list as needing a decision;
  `replace_without_asking`; a diverged piece is refused), "Decisions"
  (`resolve` interactive over pending decisions; `resolve <piece>
  <answer>` at once under the lock with the loss computed then; nothing
  stored; listing without a piece and without a terminal, or with
  `--json`, exit 3 when any; an answer that does not fit is a usage
  error; the "replacing with loss" row), "Metadata changes"
  (reclaiming row).
- Owns: `src/ops/archive/unearthed.rs`, `src/ops/resolve/{mod,replace}.rs`,
  `src/cli/cmd/resolve.rs`, `tests/{rearchive,resolve}.rs`.
- Shared edits: `src/ops/archive/mod.rs` (I19): the line that routes
  unearthed pieces to `unearthed.rs`; `src/cli/cmd/archive.rs` (I19):
  rendering of reclaimed pieces and decisions.
- Content: reclaim and loss compare against the listing of the payload
  version the piece was unearthed from (F16); the `--fork` answer writes
  the `fork` row through I22's core (F1); the routing `match` in
  `resolve/mod.rs` over the four decision kinds, with
  `conflict.rs`, `diverged.rs` and `superseded.rs` answering
  `unsupported` until I31 and I37 fill them (S7); `resolve <piece>`
  without an answer walks that piece's decisions (T25).
- Depends on: I12, I19, I20, I22.
- Tests: an unearthed, unchanged piece is reclaimed with no new payload
  (first failure: routed to "new piece"); a piece unearthed with
  `--version 1` is compared with version 1's listing; a reclaim without
  a usable archive copy is pending remote work; an added file replaces
  without asking; a removed file asks, and a scripted "fork" forks;
  without a terminal the piece is listed as needing a decision and the
  run exits 3; `resolve <piece> --replace` replaces with the loss at
  that moment; `--mine` on this decision exits 2; `resolve <piece>`
  without a terminal lists that piece's decisions and exits 3;
  `replace_without_asking` skips the question; a diverged piece is
  refused.
- Docs: spec "JSON output" (`resolve` paragraph).
- Size: L.

#### I27 Commit protocol and remote cleanup

- Goal: commit metadata versions safely between machines.
- Implements: ADR 0030, 0017; spec "Committing a version" (payload
  under its temp name, flushed, renamed; claim N+1, one above the
  highest committed version or claim, by exclusive creation, an
  existing claim is a conflict; metadata under a temp name; check that
  the claim still exists, that no `meta.<M>` with M >= N+1 exists and
  that the payload is present with its size; rename (no-replace on
  SSH); list again, a higher committed version is a conflict; readers
  read only `meta.<N>`), cleanup rules (abandoned claims; payload
  removal only when no retained version references it, older than the
  grace period and no open claim; superseded payloads kept; only in
  directories where every `meta.<N>` parses with a known format; own
  temp names and partial files early only in directories whose
  committed metadata names this collection, while the lock shows no
  other run; ages on the remote's clock), the unparsable highest
  version rule.
- Owns: `src/remote/{commit,cleanup,testing}.rs`.
- Content: retention per T4 (the highest committed version kept; older
  ones removed once a higher one exists and they are older than the
  grace period; listings follow their payload); the own-temp-name rule
  also covers `<temp>.` followed by six characters of `[A-Za-z0-9]`,
  which rsync 3.5.1 leaves when killed hard, and such names of other
  machines go after the grace period (O6); in a dry run, age-dependent
  steps report "age unknown in a dry run" (F14).
- Depends on: I08, I23.
- Tests (with `FaultRemote`): a commit writes `meta.1` (first failure:
  stub); another machine's claim between steps gives a conflict; a
  higher version appearing after the rename gives a conflict; a claim
  older than the grace period is skipped and the next version uses
  N+2; an unreferenced payload older than the grace period is removed,
  a younger one stays, a superseded one stays; a directory with a
  `format: 2` file is left alone and reported; an old `meta.<N>` is
  removed per T4; an own `<temp>.Ab12Cd` is removed early and another
  machine's only after the grace period.
- Docs: keep spec "Committing a version" true (retention per T4, the
  rsync leftover names) and keep spec "Metadata" true (the commit time
  per T6).
- Size: L.

#### I28 The SSH remote

- Goal: the `Remote` trait over ssh and rsync, and `remote add` for SSH
  URLs.
- Implements: ADR 0048, 0049; spec "SSH remote" (`sync` on the NAS
  after a run's temp files and before their renames, and after the
  renames), "Transfers" (archive upload onto its temp name with
  `--partial-dir`, resumable; fetch into `.antiq/tmp/`; a new live
  package seeded as a hard link to the current one and transferred onto
  it; no live-package transfer deletes files; size on the NAS checked
  in the same command as the rename; NAS rsync and hash tool probed at
  `remote add` through the same ssh command), "Remotes" (`remote add`
  probe failures: no rsync 3.1, openrsync, no SHA-256 tool, a failing
  exclusive create checked by a second create, unreachable; nothing
  written then), "Committing a version" (rename refusing an existing
  target: `ln` to the final name, then remove the temp name).
- Owns: `src/remote/ssh/mod.rs` (including the `ssh` arm of opening),
  `tests/remote_ssh.rs`.
- Shared edits: `src/remote/mod.rs` (I23): the `ssh` arm of the opening
  `match` calls into `remote::ssh` (S4); `src/ops/remote.rs` and
  `src/cli/cmd/remote.rs` (I23): the SSH probes of `remote add` and the
  probed versions in `remote show`.
- Depends on: I23, I24, I25.
- Tests (loopback shim with real rsync): `remote add --init
  ssh://...` probes and writes config (first failure: `unsupported`);
  openrsync as remote rsync refuses with the reason; a missing hash tool
  refuses; a put, a get, an interrupted put resumed from
  `.antiq-partial`; a seeded live upload leaves the old name's bytes
  unchanged; the size check catches a short file; the meta rename
  refuses an existing target; opt-in: the same against the real sshd.
- Docs: none.
- Size: L.

#### I29 Reachability policy: `--if-reachable` and the away warning

- Implements: spec "Remotes" (away longer than `away_warn_days` with
  remote work pending: `status` warns and `--if-reachable` no longer
  turns away into exit 0), "Dry runs, scheduling, exit codes"
  (`--if-reachable` for scheduled runs; exit 4). The network check is
  out of v1 (U6).
- Owns: `src/remote/{policy,away}.rs`; fills I01's
  `remote::policy` and `remote::away::status_warning` stubs, whose call
  sites I16 and I30 place (S5); the time the remote was last reachable is
  kept in `.antiq/remote-state.json` (T24).
- Depends on: I23.
- Tests (library level; the binary exit test is in I30): away with
  `--if-reachable` gives the exit-0 outcome, without it `remote_away`
  (first failure: stub); away for eight days with pending work gives
  `remote_away` under `--if-reachable` and a `status` warning; broken
  stays an error.
- Docs: keep the local layout in spec "Storage" true
  (`remote-state.json`).
- Size: M.

#### I36 The picker for `find`

- Goal: interactive `find` with every action except the contents view.
- Implements: ADR 0021, 0057 (start as a copy of ntropy's picker;
  unearthing on Enter, printing the path with `--print`, forking,
  pinning or unpinning), ADR 0020 and 0056, spec "Command line" (`find`
  opens a picker interactively).
- Owns: `src/cli/picker/{mod,state,layout}.rs` and their snapshot files,
  `Cargo.toml` (crossterm, nucleo, unicode-width, unicode-segmentation,
  rustix `pty` and `termios`), `tests/picker.rs` (pty harness).
- Shared edits: `src/cli/cmd/find.rs` (I18): the interactive branch.
- Content: copy `src/bin/ntropy/run/picker/` (ntropy is MPL-2.0 like
  antiq; the header stays): `mod.rs` (982 lines), `state.rs` (1,117),
  `layout.rs` (1,014) including tests, and the `snapshots/` directory
  (one `.snap` file), renamed to antiq's module path (H2). The ties to
  ntropy are `layout.rs` (ntropy's `Candidate`) and the `open_tty()`
  call, both replaced; actions through ntropy's `AcceptKey` mechanism
  with the keys of U3; after the pick, the action runs through the same
  `ops` functions as the commands. The pty harness follows 4.5 (T16).
- Depends on: I18, I20, I22.
- Tests: ntropy's state tests and snapshot carried over (first failure:
  module absent); layout snapshots for archived, live and remote-only
  rows; a pick with the print action prints the absolute path; through
  the pty, Enter on a row unearths and Esc ends without action.
- Docs: none beyond the README at I39.
- Size: L.

### Wave 8

#### I30 `sync`: pull, upload and commit

- Goal: exchange metadata and archives with the remote.
- Implements: spec "sync" (pull metadata first; upload pending
  archives; commit recorded metadata changes; at most `remote_jobs`
  remote operations at a time; compression may run ahead of uploads),
  "Storage" (listings fetched incrementally), "Several machines"
  (`sync` pulls metadata first; the machine whose version lost learns
  it at its next sync), "Metadata changes" (`sync` row: `verified`
  times, history), "Remotes" (every operation checks the marker), ADR
  0016, 0017, 0032, 0030 (through I27).
- Owns: `src/ops/sync/{mod,pull,upload,commit}.rs`,
  `src/cli/cmd/sync/mod.rs`, `tests/sync.rs`, README (second
  milestone); the call sites of the phase stubs.
- Content: the phases in the spec's order (F7): pull metadata and fetch
  missing listings (F2); refresh live packages (I32, first call site);
  upload pending archives; commit recorded metadata changes, rewriting
  the tracking's `pending` to the committed number (F21); delete live
  packages whose archive is committed (I32, second call site); apply
  the tiers (I33). A version conflict goes through the `conflict` stub,
  which records a pending decision and skips the piece (exit 3) until
  I31. A dry run writes no probe file (F14).
- Depends on: I19, I21, I27, I29.
- Tests (path remote, two machines): an archive made on A is uploaded
  and committed as `meta.1`, the pending file promoted and tracking
  rewritten (first failure: `not_implemented`); B pulls metadata and
  listings and `find` on B lists it; an import is uploaded; a failed
  upload (fault) leaves the piece pending and exits 3; `--dry-run`
  reads the remote and writes nothing on either side; through the
  binary, away exits 4, and 0 with `--if-reachable` (S5); at most
  `remote_jobs` operations run at once (fault remote counter).
- Docs: spec "JSON output" (`sync` paragraph); README: archive,
  unearth, find, import and sync against a path remote work.
- Size: L.

#### I34 Unearthing through the remote

- Implements: spec "unearth" (when the remote is reachable, pull the
  ID's metadata first and say when the remote has a newer version; with
  the remote away proceed from local data and warn; fetch the archive
  into `tmp/` and check its checksum; an ID without an archive, a gone
  piece or another collection's live piece unearthed as a fork, comes
  from its current live package with its stream checksum checked and
  the staged tree verified against the package's members; a fork of
  another collection's piece is a new ID owned here; warn when the
  package was marked stale or made with the live exclude list; no
  archive payload until archived), "fork" (a fork made while unearthing
  another collection's piece uses that unearthing time), "Operations"
  (space for the archive's size when fetched), ADR 0007, 0040.
- Owns: `src/ops/unearth/{remote,live}.rs`, `tests/unearth_remote.rs`.
- Shared edits: `src/ops/unearth/mod.rs` (I20): source selection;
  `src/cli/cmd/unearth.rs` (I20): the new messages.
- Content: the listing is fetched with the archive (F2); a fetched
  archive stays as the local archive while the piece is within the
  local tier, with its `verified` record kept (F17); a fork of another
  collection's live piece never writes that collection's ID (F1).
- Depends on: I20, I22, I23, I29.
- Tests: a remote-only piece is fetched and unearthed, and the archive
  stays local (first failure: "no local archive"); a newer remote
  version is reported; with the remote away a local archive still
  unearths with a warning, and a remote-only piece exits 4; a gone piece
  comes back from its live package; another collection's live piece
  unearths only as a fork with a new ID, and its metadata is unchanged;
  a fork of a live piece without a package fails with a clear error
  (F8); a stale package warns.
- Docs: keep spec "unearth" true (the fetched archive kept within the
  local tier); spec "JSON output" (`unearth` additions).
- Size: L.

#### I35 The brake, remote `reconcile` and detaching

- Implements: ADR 0044, 0045; spec "Safety" (brake for dropping entries,
  `remote remove` while pieces exist only on the remote, and
  `resolve --drop-superseded`; when planned removals exceed
  `brake_percent` of the affected set the step stops and lists up to
  ten; asks on a terminal, refuses without one unless `--force`;
  `--dry-run` reports where it would hold), "reconcile" (rebuild the
  index from local and remote metadata; remove leftovers per the commit
  protocol; report IDs whose committed version another machine wrote;
  an entry without a local archive whose archive, verified or pulled
  before, is gone from the remote is marked missing; `--prune` drops
  such entries and gone pieces; an entry with a local archive not yet
  uploaded is pending upload and never pruned), "Committing a version"
  (`reconcile` reports an unparsable highest `meta.<N>`), "JSON
  output" (error kind `brake` with the planned count, set size and up to
  ten items).
- Owns: `src/brake.rs`, `tests/{brake,reconcile_remote}.rs`.
- Shared edits: `src/ops/reconcile.rs` and `src/cli/cmd/reconcile.rs`
  (I18): the remote part; `src/ops/remote.rs` and
  `src/cli/cmd/remote.rs` (I23): the brake of `remote remove`.
- Content: the planned removals and the affected set per step (F20):
  for `reconcile --prune`, the entries to drop against all entries this
  machine tracks or knows from metadata; for `remote remove`, the
  remote-only pieces that become unreachable against all objects of the
  collection; for `resolve --drop-superseded`, the superseded payloads
  to drop against the ID's archive payloads (with one current payload,
  at least half, so it always holds at the default; U7). Age-dependent
  cleanup in a dry run reports "age unknown in a dry run" (F14).
- Depends on: I18, I23, I27.
- Tests: 3 of 10 planned removals with the default 20 % hold the brake
  and list them (first failure: stub); `--force` passes; a scripted
  "yes" passes; `--dry-run` reports the brake; an archive missing on
  the remote marks the entry missing and `--prune` drops it; a pending
  upload is never pruned; `remote remove` with remote-only pieces goes
  through the brake; a commit by another machine is reported; an
  unparsable highest version is reported.
- Docs: keep spec "Safety" true (the set of each brake step); spec
  "JSON output" (`reconcile` additions).
- Size: L.

### Wave 9

#### I31 Version conflicts and divergence

- Implements: ADR 0017, 0025; spec "Several machines" (putting an ID
  back with a metadata version that is no longer current is a conflict;
  choices mine, theirs and fork with the table's effects; a superseded
  archive stays referenced; a local archive that no remote metadata
  references is a conflict and never dropped), the diverged rule (the
  remote's current version is neither the one taken out nor one this
  machine committed since; `archive` and `sync` refuse until resolved),
  "Decisions" (version conflict row; `sync` asks in place on a
  terminal).
- Owns: `src/ops/sync/conflict.rs`, `src/ops/resolve/conflict.rs`,
  `src/cli/cmd/sync/conflict.rs`, `tests/sync_conflict.rs`.
- Shared edits: `src/cli/cmd/resolve.rs` (I26): rendering of version
  conflicts.
- Content: a pending change that touches only metadata (such as
  `verified` times or history) is rebased onto the remote's newer
  version instead of raising a conflict: `verified` is dropped when its
  payload is no longer referenced, the local copy is kept, history
  entries merge (F6). The `fork` choice is its own operation: the local
  archive becomes version 1 of a new ID through I22's core, and the old
  ID is not written (F1). The `theirs` choice removes through a
  `UserDecision` (F9).
- Depends on: I26, I30.
- Tests (two machines): both archive the same unearthed ID, the second
  sync gets a conflict (first failure: recorded only); "mine" writes a
  new version with theirs superseded; "theirs" sends the local archive
  through trash and tracking follows; "fork" makes the local archive
  version 1 of a new ID and leaves the old ID's metadata as it was;
  without a terminal the decision is pending and `resolve <piece>
  --theirs` executes it; a `verified`-only pending change after another
  machine's commit is rebased, not a conflict; a remote version from
  another machine marks the unearthed folder diverged and `archive`
  refuses it.
- Docs: keep spec "Several machines" true (the rebase of metadata-only
  changes); JSON additions.
- Size: L.

#### I32 Live backups in `sync`

- Implements: ADR 0018, 0047 (full upload on a path remote), 0049
  (seeding); spec "sync" (refresh the live packages of changed pieces;
  delete a live package only after its archive's metadata is committed
  on the remote), "Objects" (`sync` mints IDs for new pieces), "Change
  detection", "Verifying" (tolerant verify, stale), "States" (gone piece
  keeps its live package), "Pins" (pinned pieces still backed up),
  "fork" (the old ID's live package is deleted), `collection.backup_live`,
  `collection.live_exclude`, "Metadata changes" (`sync` row: live
  payload replaced).
- Owns: `src/ops/sync/live.rs`, `src/cli/cmd/sync/live.rs`,
  `tests/sync_live.rs`.
- Content: both call sites of I30 (refresh before the uploads, deletion
  after the commit); new pieces get their ID here, and with
  `backup_live = false` they still get an ID and payload-less live
  metadata (F8); the old ID's live package is deleted once the fork's
  change is committed (F10).
- Depends on: I14, I28, I30.
- Tests: a live piece gets an ID and a live package (first failure: stub
  phase); with `backup_live = false` it gets an ID and metadata without
  a payload; an unchanged piece transfers nothing; a touched file with
  the same content only replaces the manifest; a changed piece uploads a
  new package, through the seed on the SSH shim; a file changing during
  reads ends as "backup stale since"; the live package of an archived
  piece is deleted only after the archive's commit; after a fork, the
  old ID's live package is deleted; the first sync after unearthing
  packs once.
- Docs: keep spec "sync" true (IDs and payload-less metadata with
  `backup_live = false`).
- Size: L.

#### I33 Tiers and local archive retention

- Implements: ADR 0007, 0042, 0043, 0045 (tiering outside the brake);
  spec "sync" (remote-only tier measured from the archive's commit; a
  local archive moves there only after the remote's identity, the
  payload and the remote copy's checksum are confirmed, over SSH after
  a NAS `sync` that followed its upload; deleted through the trash with
  the time of the check recorded), "Several machines" (a local archive
  is dropped only when its payload, by checksum, is referenced by the
  remote's metadata and its remote check is recorded; the remote's
  newer current archive is fetched while the piece is within the local
  tier, otherwise only metadata), "unearth" (the local archive is
  always kept without a remote).
- Owns: `src/ops/sync/tiers.rs`, `src/cli/cmd/sync/tiers.rs`,
  `tests/sync_tiers.rs`.
- Content: the commit time comes from the history entry of the
  committing version (T6); the drop goes through a
  `ConfirmedRemoteCopy` (section 2.8).
- Depends on: I28, I30.
- Tests: with `remote_only_days = 0` a committed, confirmed archive
  leaves local storage through the trash and `verified` is set (first
  failure: stub phase), on the path remote and through the SSH shim; a
  checksum mismatch keeps it; a collection without a remote keeps it;
  an unreferenced local archive is never dropped; a newer remote archive
  within the local tier is fetched; `status` shows "remote-only in N
  days".
- Size: M.

### Wave 10

#### I37 Resolving diverged folders and superseded archives

- Implements: spec "Decisions" (diverged row: `--theirs` shows the
  folder's differences to the listing of the version taken out and asks,
  without a terminal only with `--force`, then the folder goes through
  trash and the current version is unearthed in its place; `--mine`
  archives the folder as the next version after the loss check against
  the remote's current listing, the other becomes superseded; `--fork`
  gives a new ID and clears the divergence; superseded row:
  `--drop-superseded` through the brake, in a new version), "Several
  machines" (`unearth --version` brings a superseded archive back;
  `status` lists pieces with superseded archives), ADR 0045.
- Owns: `src/ops/resolve/{diverged,superseded}.rs`,
  `tests/resolve_remote.rs`.
- Shared edits: `src/cli/cmd/resolve.rs` (I26): rendering of these two
  decisions.
- Content: `--theirs` removes the folder through a `UserDecision` (F9).
- Depends on: I31, I34, I35.
- Tests: `--theirs` on a diverged folder without a terminal and without
  `--force` refuses (first failure: `unsupported`); with `--force`
  the folder is in trash and the current version is unearthed;
  `--mine` with a loss asks; `--fork` clears the divergence;
  `--drop-superseded` of 1 of 2 payloads holds the brake without
  `--force` and commits a new version with it.
- Size: M.

#### I38 Extension points: the selection strategy and the ADR of the traits

- Goal: record the traits as built and close ADR 0006's open part.
- Implements: ADR 0006 ("which parts get a trait, and what each trait
  looks like, is left to later ADRs"), ADR 0012 (selection strategy as
  a replaceable part), ADR 0015, ADR 0019.
- Owns: `src/select.rs` (extract `SelectionStrategy`), `src/pack/mod.rs`
  (packager lookup by `package_format`, with a clear error for an
  unknown format), `src/remote/mod.rs` (refactor of the opening that I23
  and I28 built; no new arm), new ADRs through `adrs new --no-edit`.
- Must not touch: behaviour; every existing test passes unchanged.
- Depends on: I05, I06, I23, I28, and the end of wave 9, since the
  refactor touches modules that waves 6 to 9 call.
- Tests: a payload with an unknown `package_format` gives a clear error
  (first failure: no lookup); a test strategy plugged into selection is
  used.
- Docs: one ADR per trait (Remote, Packager, SelectionStrategy) as
  built, each naming ADR 0006.
- Size: M.

### Wave 11

#### I39 Acceptance: real SSH, Linux and the README

- Goal: v1 proven end to end on both remotes and both systems.
- Owns: `tests/acceptance.rs` (opt-in), `scripts/test-linux.sh`,
  `scripts/sshd-container.sh` if used, README (final), removal of the
  `not_implemented` kind.
- Content: a two-collection scenario on one remote (archive on A,
  `find` and unearth on B as a fork, a version conflict, a tier move,
  `reconcile --prune` through the brake) on the path remote, on the
  loopback shim, against the local sshd (opt-in) and against a Linux
  container with sshd and rsync 3.x (opt-in); `cargo test` inside a
  Linux container as a non-root user with 4 GB of memory and
  `RUST_TEST_THREADS=4`, including the non-UTF-8 name tests; a check on
  the NAS share that the path remote's read-back checksum reads the
  server's data (R8).
- Linux container: the image needs `setfattr` (package `attr`), which I06's xattr test calls, and `CARGO_MANIFEST_DIR` set for the binary tests; `ps` is not needed (the session test uses `kill -0 -$$`).
- Depends on: every other item.
- Docs: README (what v1 does, how to set up a remote), spec status line
  ("Draft") per the lead.
- Size: M.

### After the picker

#### I40 The picker's contents view

- Implements: ADR 0057 and spec "Command line" (a key expands the
  selected piece in place into a tree of its contents from its listing;
  members whose paths match the query show as its children), U1 (the
  query matches member paths). Design picture:
  `~/playground/2026_11_02_antiq-logbook/design/mockups/png/n80-contents-tree.png`.
- Owns: `src/cli/picker/` (after I36), `tests/picker.rs`.
- Depends on: I15 (listing search), I36.
- Size: M.

## 6. Dependency graph and waves

The added dependency edges (I04 for I08, I09 and I17; I03 for I10; I28
for I32 and I33) all point at earlier waves, so the waves are unchanged.
I01 lands in two merges; wave 2 starts after part B.

| Item | Depends on | Wave |
|---|---|---|
| I01 | none | 1 (parts A and B) |
| I02, I03, I04, I05, I06, I07 | I01 | 2 |
| I08, I09 | I01, I02, I03, I04 | 3 |
| I10 | I03, I04, I05 | 3 |
| I11 | I02, I03, I04, I05 | 3 |
| I12 | I05, I06 | 3 |
| I13, I14 | I06 (I14 also I05) | 3 |
| I15 | I08, I09 | 4 |
| I16 | I04, I05, I08, I09, I10 | 4 |
| I17 | I03, I04, I08, I09, I12 | 4 |
| I18 | I10, I15, I17 | 5 |
| I19 | I02, I06, I07, I08, I09, I10, I12, I13, I15, I17 | 5 |
| I20 | I08, I09, I12, I13, I15, I17 | 5 |
| I21 | I06, I08, I12, I13, I15, I17 | 5 |
| I22 | I15, I17, I20 | 6 |
| I23 | I03, I04, I08, I15, I17 | 6 |
| I24, I25 | I01, I04 | 6 |
| I26 | I12, I19, I20, I22 | 7 |
| I27 | I08, I23 | 7 |
| I28 | I23, I24, I25 | 7 |
| I29 | I23 | 7 |
| I36 | I18, I20, I22 | 7 |
| I30 | I19, I21, I27, I29 | 8 |
| I34 | I20, I22, I23, I29 | 8 |
| I35 | I18, I23, I27 | 8 |
| I31 | I26, I30 | 9 |
| I32 | I14, I28, I30 | 9 |
| I33 | I28, I30 | 9 |
| I37 | I31, I34, I35 | 10 |
| I38 | I05, I06, I23, I28; after wave 9 | 10 |
| I39 | all others | 11 |
| I40 | I15, I36 | 8 |

Waves and the files that keep them apart:

| Wave | Items | Shared files and how they stay apart |
|---|---|---|
| 1 | I01 | creates every file; two merges |
| 2 | I02 to I07 | disjoint modules; I02 adds functions to I01's `relpath.rs` and `id.rs` without touching the types |
| 3 | I08 to I14 | disjoint modules; I08 adds functions to I01's `meta.rs`; only I13 edits `Cargo.toml` |
| 4 | I15, I16, I17 | disjoint; README only in I16 |
| 5 | I18 to I21 | own `ops` and `cli::cmd` files; one spec "JSON output" paragraph per command |
| 6 | I22 to I25 | disjoint; I23 fills the `remote::probe_state` stub, no edit of `status.rs` |
| 7 | I26 to I29, I36 | I26 edits I19's `ops/archive/mod.rs` routing and `cli/cmd/archive.rs`; I28 edits I23's `remote/mod.rs` arm, `ops/remote.rs` and `cli/cmd/remote.rs`; I36 edits I18's `cli/cmd/find.rs` and is the only `Cargo.toml` editor |
| 8 | I30, I34, I35, I40 | I40 only in `src/cli/picker/`; `ops/sync/*` and `cli/cmd/sync/mod.rs` to I30; `ops/unearth/*` and `cli/cmd/unearth.rs` to I34; `ops/reconcile.rs`, `cli/cmd/reconcile.rs`, `ops/remote.rs` and `cli/cmd/remote.rs` to I35 |
| 9 | I31, I32, I33 | one sync phase file and one `cli/cmd/sync` file each, behind I30's call sites; only I31 edits `cli/cmd/resolve.rs` |
| 10 | I37, I38 | I37 in `ops/resolve/` and `cli/cmd/resolve.rs`, I38 in `select.rs`, `pack/mod.rs`, `remote/mod.rs` |
| 11 | I39 | |

The longest chain is I01, I03, I08, I17, I23, I27, I30, I31, I37,
I39; with I23 placed in wave 6 it gives the 11 waves. I24 and I25
depend only on I01 and I04 and could start from wave 3 if an agent is
free; I23 could start in wave 5. Both moves pull extension-point work
ahead of the local business logic, against the agreed order, so the
plan keeps them in wave 6.

## 7. Risks

- R1 ADR window. ADRs may be rewritten in place until the first
  release is being built. Decided: ADR 0045's last sentence gains the
  explicit `resolve --drop-superseded` case that spec "Safety" has, and
  ADR 0039's source list gains `fork` and `minted` (T2), in place.
- R2 Console snapshots. The design pictures fix the look; the command
  items' insta snapshots are reviewed against them before acceptance.
- R3 Long paths. On macOS, 44 members of a 1,300-byte path failed past
  `PATH_MAX` of 1,024 with the `tar` crate (names research), and spec
  "unearth" extracts such paths directory by directory. Every consumer
  that reads or writes those trees therefore goes through I01's
  `TreeRoot`: the walk (I05), the delete helper (I03), packing (I06),
  verify (I12), extraction (I13), live packing (I14), the trash re-walk
  (I19) and unearthing (I20), each with a test past 1,024 bytes.
- R4 Terminal in tests. Measured: the agent sandbox has no controlling
  terminal. A binary test that spawns antiq without `setsid()` passes
  here and blocks on a prompt when the user runs `cargo test` in a
  terminal. The `tests/support` command builder always detaches (I01),
  and no test spawns the binary another way.
- R5 Slow and large tests. Level 19 with a 128 MiB window took 17.7 s
  for a 150 MB input (compression research), and a level-19 encoder
  reached about 150 MB of resident memory even for a 10 KB input
  (measured in the review), so parallel tests can run a container out
  of memory. Fixtures stay at kilobytes, library tests share one
  process-wide pool (I07), `scripts/test-linux.sh` gives the container
  4 GB and sets `RUST_TEST_THREADS=4`, and any test above a few seconds
  gets the opt-in mark.
- R6 zstd pin. The golden-hash test of ADR 0052 fails when the bundled
  zstd changes; `cargo update` cannot move zstd-sys past the exact pin
  `=2.1.2+zstd.1.5.7`, so only a deliberate change of the pin does.
  With its default features off, the pin does not switch `legacy` back
  on (O1).
- R7 rsync on the far side. The loopback and sshd layers run the far
  side's rsync on this Mac, where the login shell finds openrsync
  (measured); tests set `machine.remote_rsync`. The real NAS's rsync
  version, `PATH`, hash tool and `--rsync-path` behaviour are not
  verified (rsync research, "Not verified"; ssh research, "Not
  verified"). Recommendation: a manual first run against the NAS by the
  user, with `remote add` probes, before relying on it.
- R8 SMB semantics. Exclusive create and locking from two machines on
  smbfs were not tested, and whether a successful `fsync` there reaches
  the server's disk is unknown (filesystem research, "Not verified").
  The commit protocol leans on exclusive create (ADR 0030). A read-back
  of a file just written over SMB may come from the client's cache, not
  from the server, which would weaken the check of ADR 0043 on the path
  remote; I23 reads past the cache and I39 checks it on the NAS share.
- R9 Size of the `sync` items. I30 and I31 sit at the top of L.
  Recommendation: the lead splits I30 at item-plan time if its plan
  exceeds about 1,500 lines (for example uploads apart from pull and
  commit).
- R10 Parallel merges of docs. Spec and README edits collide when
  items merge one after another. The per-command paragraphs and the
  README milestones reduce this; the lead rebases trivial conflicts.
- R11 Static Linux builds. Under musl, std's `created()` returns
  `Unsupported`; `rustix::fs::statx` gives the birth time (filesystem
  research, Linux). Only relevant if a static build is made.
- R12 File descriptors under launchd. The soft limit there is 256 (measured
  in the review with `launchctl limit`), so a scheduled run can hit
  `EMFILE`. `main` raises the soft limit (I01), and `EMFILE` fails the
  run instead of marking pieces unreadable (I05).

## 8. Technical decisions

- T1 Uncommitted metadata lives in `objects/<ulid>/meta.pending`,
  promoted after the commit with the number the ADR 0030 protocol gives;
  tracking says `pending` until I30 rewrites it (section 2.9). I08
  records it as a new ADR.
- T2 When no time from 1990 on exists, the ID time is the time of
  minting, with a warning, and the source is `minted`; added to the
  spec's `created_source` list and to ADR 0039 in place before I01
  merges.
- T3 Archives hold one top-level container; listings, verify and
  reclaim compare the paths below it (F5).
- T4 The highest committed `meta.<N>` is kept; older ones are removed
  once a higher version exists and they are older than the grace
  period; listings follow their payload. Listing names are the spec's;
  listings are zstd level 3 with the content checksum, read with the
  128 MiB window limit.
- T5 The live manifest is `tracking/<ulid>.manifest.json`.
- T6 The history entry of the committing version carries the commit
  time; the remote-only tier reads it.
- T7 A dry run takes no lock and prints the lock note; a dry `sync`
  reads the remote and writes nothing on either side, not even the
  clock's probe file; age-dependent cleanup reports "age unknown".
- T8 With `--json` in the arguments, a usage error prints the envelope
  with kind `usage` and exits 2.
- T9 JSON errors go to stdout, plain messages to stderr.
- T10 `collection.exclude` entries are literal relative paths; nothing
  below them is a piece.
- T11 The staging volume is probed for case and normalisation
  insensitivity, behind a seam; names are compared after NFD
  (`unicode-normalization`) and `str::to_lowercase`; the strict verify
  stays the backstop.
- T12 RAM is `sysconf(_SC_PHYS_PAGES)` times the page size.
- T13 Read-only commands compute a missing index in memory without
  writing it; a running run is detected from the lock file's pid, with
  no `try_lock` probe.
- T14 "Connection refused" means away only in the "ssh: connect to host
  ... Connection refused" form (I24).
- T15 Metadata and history times are written in whole seconds.
- T16 The terminal glue is covered by the pty harness of 4.5 in I36.
- T17 Devices, free space, filesystem type, `ENOTSUP`, the volume's name
  rules go through seams; no disk-image test.
- T18 `tar` without its default features.
- T19 ULIDs are written in upper case; references accept either case.
- T20 The binary stays `src/main.rs`, its modules under `src/cli/`.
- T21 One generic `Question` type in the library.
- T22 Proof tokens `DurableArchive`, `ConfirmedRemoteCopy` and
  `UserDecision` (section 2.8), each with `compile_fail` doctests.
- T23 SSH tests: loopback shim in `cargo test`; real sshd (harness of
  4.4) and Linux container opt-in, both run before v1 is called done
  (I39).
- T24 The time the remote was last reachable is kept in
  `.antiq/remote-state.json`.
- T25 `resolve <piece>` without an answer walks that piece's decisions
  interactively; without a terminal it lists them and exits 3.
- O12 A piece whose top directory is on another device than the
  collection root counts as containing a mount point (a reading of spec
  "Walks"); I05 records it in the spec.

## 9. Questions for the user

Answered by the user on 2026-11-03 (chat), binding for the items:

- U1 `find <query>` also matches member paths from the listings
  (I15 search, I18). Fuzzy matching as the picker; objects only; JSON
  `size` is the current payload's size.
- U2 Console look, designed with the user from rendered terminal
  pictures and written into spec "status" and "Command line": output
  adapts to the terminal's width, a wide layout where there is room
  (the wider design reads better) and a narrow one at 80 columns with
  long paths shortened in the middle; `status` lists
  only what needs the user or changes at the next run, as a table
  (STATE, PIECE, AGE, WHAT), "nothing to do" otherwise, every piece via
  `find`; long runs show one row per piece (path, size, bar, operation,
  detail), on one line when the path fits and two lines otherwise;
  `sync` shows its phases as a checklist. Pictures in
  `~/playground/2026_11_02_antiq-logbook/design/mockups/png/`:
  `n80-status-d.png`, `status-f-clean.png`, `n80-archive-h1-oneline.png`,
  `n80-archive-h1-twoline.png`, `n80-sync-s2-oneline.png`,
  `n80-sync-s2-twoline.png` for 80 columns, `status-d-attention-table.png`,
  `progress-k-h1.png` and `progress-o-sync-phases.png` for wide
  terminals; their sources are `gen.py` and `ans/` beside
  them, rendered with `design/ansi-shot`. Command items match them in
  their console snapshots.
- U3 Picker flow: after pin, unpin or print the picker stays open;
  unearth and fork close it. Keys and the fork name follow ntropy's
  accept keys and keep the name unless the U4 design says otherwise.
- U4 The contents view is a tree inside the list (ADR 0057, spec
  "Command line"; picture `n80-contents-tree.png`). I40 builds it.
- U5 Accepted as recommended: `init` where `.antiq/` exists or inside a
  collection refuses, naming the existing root; `init --yes` takes the
  proposed `archive/` exclude; `sync` without a remote fails with exit 1
  and kind `no_remote`, `remote show` reports none and exits 0,
  `reconcile` does its local part, `--if-reachable` without a remote
  changes nothing; an interactive "skip" ends with exit 3; `status`
  exits 0 unless the command itself fails.
- U6 `machine.network_check` is out of v1; a someday todo keeps the
  gateway IP and MAC options and the ARP finding (O14).
- U7 Every `resolve --drop-superseded` holds the brake (asks on a
  terminal, needs `--force` without one), as the brake's set rule gives.

## 10. Coverage of the spec and the ADRs

Spec sections:

| Spec section | Items |
|---|---|
| Purpose, Scope | all; checked end to end in I39 |
| Scope, not in version 1 | no item; I04 refuses a second remote; I05 has one strategy |
| Collections and pieces (marker, discovery, `-C`) | I04 |
| Collections and pieces (`init`) | I11 |
| Selection | I05; counts shown by I16 |
| Walks | I01 (`TreeRoot`), I05; delete helper I03 |
| Idle | I10 |
| Pins | I10; commands I18 |
| Objects (IDs, ID time, ownership) | I02; minting in I19, I21, I22, I32; ownership in I09, I19, I32, I34 |
| Versions | I08 (payload version), I27 (metadata version) |
| Metadata | I01 (types), I08 (IO) |
| Listing | I08 (format), I06 (generation), I12 (comparison) |
| Local state | I09 |
| States | I09 (derivation), I16 (display) |
| Metadata changes | I19 (`archive`), I26 (reclaiming), I20 (`unearth`), I22 and I26 (`fork`), I30 to I33 (`sync`), I21 (`import`), I26, I31, I37 (`resolve`); `pin` none (I18) |
| Storage (local layout, remote layout, payload names, atomic write) | I01, I04, I11 (local layout), I23 (remote layout), I02 and I08 (names), I03 (atomic write) |
| Storage (index, listings fetched incrementally) | I15; I30 and I34 fetch listings |
| Config | I04; written by I11, I23, I25 |
| Committing a version | I27; unparsable highest version reported by I16 and I35 |
| Remotes | I23; SSH probes I28; brake on `remote remove` I35; `remote_jobs` I30; `away_warn_days` I29 |
| Path remote | I23 |
| SSH remote | I24, I28 |
| Transfers | I25, I28 |
| Several machines | I30, I31, I33, I34 |
| Packaging | I06, I07, I14 |
| Verifying | I12 (strict, listing), I14 (tolerant), I26 (reclaim gate) |
| Change detection | I14, I32 |
| Operations (lock, start of run, space, moves) | I03, I17; space in I19, I20, I34 |
| archive | I19, I26 |
| sync | I30, I31, I32, I33 |
| unearth | I20, I34; names and staging I13 |
| fork | I22; old live package I32; fork while unearthing I34 |
| import | I21 |
| reconcile | I18 (local), I35 (remote) |
| status | I16 |
| Safety | I35; exemptions in I19, I32, I33 |
| Command line (table, flags, references) | I01, I15 |
| Command line (console, interactive rule, `find`, picker) | I01, I18, I36, I40 |
| Decisions | I26 (framework, replacing with loss), I31 (version conflict), I37 (diverged, superseded) |
| Dry runs, scheduling, exit codes | I01 (codes), I17 (plans), I29 (`--if-reachable`), I30 (exit test) |
| JSON output | I01 (envelope, errors, kinds), I16 (`status`), I18 (`find`), each command item for its payload |

ADRs:

| ADR | Items |
|---|---|
| 0001 Record architecture decisions | process; I08, I38 write ADRs |
| 0002 Write the tool in Rust | I01 |
| 0003 One crate with a library and a CLI binary | I01 |
| 0004 clap | I01 |
| 0005 Typed errors and anyhow | I01; every item |
| 0006 Traits driven by one engine | I06, I23 (traits), I38 (ADRs of the traits) |
| 0007 Index and metadata local for remote-only archives | I15, I33, I34 |
| 0008 No syncing one folder across machines | non-goal; I34 unearths other machines' live pieces only as forks |
| 0009 Migrate the playground archives once | non-goal for the code; I21 is not tied to the `archive.sh` layout; I11 proposes excluding it |
| 0010 Name the project antiq | I01 |
| 0011 `.antiq` directory | I04, I11 |
| 0012 Glob patterns, shallowest match | I05, I38 |
| 0013 Idle by newest file time | I10 |
| 0014 ULID that survives cycles | I02, I19, I22 |
| 0015 Fixed local storage, one Remote trait | I04, I23, I38 |
| 0016 Full metadata set on the remote | I23, I30 |
| 0017 Several machines share one remote | I30, I31 |
| 0018 Live folders as packages | I14, I32 |
| 0019 zstd for archives and live packages | I06, I21 (no xz) |
| 0020 `--json` implies non-interactive | I01, I36 |
| 0021 One picker for search and bring-back | I18, I36 |
| 0022 Collection and piece | I01 (help texts), all |
| 0023 Command names | I01 |
| 0024 `.antiq-pin` | I10, I18 |
| 0025 `resolve` | I26, I31, I37 |
| 0026 `remote add` and `--init` | I23, I28 |
| 0027 Symlinks as they are | I06, I13 |
| 0028 Dot entries only by leading-dot pattern | I05 |
| 0029 `.antiqignore`, marker files not activity | I10 |
| 0030 Commit by claim and rename | I27, I28 |
| 0031 Remote marker checked first | I23 |
| 0032 Verified local archive, upload at next sync | I19, I30 |
| 0033 Strict verify before deleting | I12, I19, I26 |
| 0034 Staging and verify before moving into place | I13, I20 |
| 0035 Reclaim without repacking | I12, I26 |
| 0036 Ask before replacing with loss | I26 |
| 0037 Idle clock from unearthing | I10, I20 |
| 0038 Pieces without files left alone | I10 |
| 0039 ID time from oldest birth or modification time | I02 (with `minted`, T2) |
| 0040 Other machines' live pieces, fork only | I16, I19, I32, I34 |
| 0041 Nothing outside pieces backed up | non-goal; selection (I05) never makes loose files a piece |
| 0042 Remote-only tier from the commit | I33 |
| 0043 Local archive deleted after remote confirmation | I23 (read-back past the cache), I33 |
| 0044 Missing entries kept until `reconcile --prune` | I35 |
| 0045 The 20 % brake | I35; exemptions I19, I32, I33 |
| 0046 `--dry-run` everywhere | I01, I17, every state-changing item |
| 0047 Path remote copies plainly; SSH first | I23 |
| 0048 ssh executable with a master | I24, I28 |
| 0049 rsync 3.1 or later | I25, I28 |
| 0050 Tar in process with hand-built headers | I06, I13 |
| 0051 zstd 19, 128 MiB window | I06, I07 |
| 0052 Live packages at a pinned zstd | I01 (pin), I14 |
| 0053 Versioned JSON metadata keeping unknown fields | I01, I08 |
| 0054 Name checks before unearthing, warnings when archiving | I13, I19, I20, I21 |
| 0055 `import` | I21 |
| 0056 Console output and the picker | I01, I36 |
| 0057 Picker from ntropy's picker | I36, I40 |


## 11. Decisions taken during implementation

Lead decisions on questions of the implementation agents and their
reviewers, dated. Where a decision changes behaviour or a format, the
item that implements it writes it into the spec or the ADR named, in
the same commit; the lead checks that at acceptance.

### I01 (merged)

- 2026-11-03: a `BrokenPipe` on stdout or stderr stops that stream
  without a panic and the run keeps its exit code (spec "Dry runs,
  scheduling, exit codes", written).
- 2026-11-03: the envelope's `command` is the command words joined by
  one space; failures without a specific kind have the kind `other`;
  `remote::policy` has its call site in I01's dispatch, `probe_state`
  and `status_warning` get theirs in I16 (spec "JSON output", written).
- 2026-11-03: `ListingEntry` types are `file`, `dir`, `symlink`,
  `hardlink` (spec "Listing", written).
- 2026-11-04, review R of part B (ledger `review-r-ledger.md` in the
  logbook's `plans/impl/`): `TreeRoot` never follows a symlink when
  changing a mode; the directory chain opens search-only; `open_read`
  takes regular files only; a symlink changed while read gives its own
  error; `sub()` carries its prefix and reports paths relative to the
  original root, and walkers descend with `sub()`; `Payload.file` and
  `Payload.listing` are validated names (spec "Storage" grammar for
  format 1); `TreeRoot::rename` refuses an existing target. Fix batch by
  the I01 agent, then an opus review of the diff.

### I02 (merged)

- 2026-11-04: `below_root` stays lexical; root and given path come in
  the same spelling. I15, I20 and I21 fold `.` and `..` lexically, then
  replace only the shortest ancestor that is the collection root itself
  (device and inode) by the root, so symlinks below the root are never
  followed (see their Paths lines).
- 2026-11-04: equal birth and modification times give the source
  `oldest-mtime` (I02 writes it into ADR 0039 and spec "Objects").

- 2026-11-04 (I02 report): a relative `--to` or `--path` resolves
  against the current directory like a piece reference; a path outside
  the collection is refused with the new error kind
  `outside_collection`, exit 1 (I15 adds the kind to `ErrorKind`; spec
  "Command line" and "JSON output" written by the lead). Two machine
  names alike in their first 32 escaped bytes give the same temp name;
  accepted, since only an object's owner uploads it.

### I03

- 2026-11-04: the filesystem primitives take no lock token; they are
  `pub(crate)`, and the tokens sit at the store and ops layer.
- 2026-11-04: `move_dir` moves between two `TreeRoot` handles through
  `TreeRoot::rename` (no-replace), so pieces deeper than the path limit
  move too. Where the filesystem refuses the no-replace flag (smbfs),
  `rename` checks the target with an lstat and renames plainly; the
  window in between is documented, and antiq's moves run under the run
  lock.
- 2026-11-04: `write_atomic` leaves `.<name>.<pid>.<n>.tmp` behind when
  a run dies mid-write, in object and tracking directories that no
  cleanup covered. The start-of-run repairs remove them (I17; spec
  "Operations" says so, written by the lead).
- 2026-11-04, review T of the merged diff (ledger `review-t-ledger.md`
  in the logbook's `plans/impl/`): `remove_tree` and `move_dir` refuse
  the root path of their tree; `TreeEntry` gains `mount_root` (Linux:
  statx `STATX_ATTR_MOUNT_ROOT` when the kernel reports it; macOS:
  `f_mntonname` of the directory against its parent's), so the delete
  helper refuses a same-filesystem bind mount and the walk and selection
  report it as a mount point. This replaces the I05 statement that bind
  mounts are not seen; the bind-mount todo closes with the fix batch,
  which lands before I19. On macOS a firmlink at the top of the system
  volume compares as a mount root (safe direction, stated in the spec).
  `remove_tree` works iteratively and stops at the walk's 512 levels
  with its own error, and extraction refuses members deeper than that
  (I13); a lock file that is not UTF-8 reads as an unknown holder; tests
  added for the EPERM holder, the Linux mount-root branch and the
  listing retry on macOS (T3 to T7).

### I04

- 2026-11-04: `collection.exclude` entries are written in the escaped
  form of spec "Metadata" and parsed into `RelPath`.
- 2026-11-04: an unknown `[collection]` key gives a warning naming it;
  range checks with an error naming the key: `memory_percent` 1 to 100,
  `remote_jobs` at least 1, `brake_percent` 0 to 100, timeouts and
  `live_retries` above the value that would disable them; an empty
  machine name is refused; a `.antiq` that is a symlink or a file is no
  marker. I04 writes these into spec "Config" and "Collections and
  pieces".

- 2026-11-04 (I04 report): `Config::unknown_collection_keys` lists the
  unknown keys; the first command that reads the config, `status` (I16),
  prints one warning per key, and so does every later command. Printed
  collection roots are canonical (`find_root` and `find` canonicalise
  their start).

### I05

- 2026-11-04: a directory selection must descend into but cannot list
  is reported as unreadable for selection and skipped; skipped
  symlinks are symlinks a pattern matched, skipped dot entries are
  directories only the dot rule kept out; the junk list applies to
  entries that are not directories; patterns with empty, `.` or `..`
  components are invalid; an exclude entry that exists also matches by
  device and inode, so it works on case-insensitive volumes; an entry
  that vanishes during a walk makes the piece unreadable for that run.
  I05 writes these into spec "Selection", "Walks" and "Idle".
- 2026-11-04: glob 0.3.4 lets `*.txt` match `.txt`, so I05 applies the
  dot rule in its own per-component matcher (and records the glob
  behaviour in `docs/research/foundations.md`).
- 2026-11-04: patterns and names compare as bytes, so an NFC pattern
  does not match an NFD name (I05 writes it into spec "Selection").
- Out of scope, filed as a someday todo: bind mounts on the same
  filesystem keep `st_dev`, so the device rule cannot see them.

- 2026-11-04 (I05 delivery, accepted): a selection candidate that is
  deleted during the run is skipped silently; any other `lstat` failure
  during selection puts its parent directory among the unreadable ones
  (spec "Selection", written by I05). The bind-mount limit is stated in
  spec "Walks".

- 2026-11-04, review Q of the finished diff (ledger `review-q-ledger.md`):
  a piece that contains an existing exclude entry is never archived,
  reclaimed or packed for backup, and status lists it with the excluded
  path, as with a mount point (lead's safe default, told to the user;
  detected by device and inode through a new `Problem::Excluded`); walk
  and selection stop at a depth of 512 levels, past which the piece is
  unreadable, and run iteratively; a directory that vanishes between
  listing and opening is skipped. I05 writes these into spec
  "Selection" and "Walks".

### I06

- 2026-11-04: I06 defines the complete `Packager` trait (format, pack,
  verify_strict, list, unpack, taking `&TreeRoot` where they touch a
  tree); the tar+zstd implementation delegates verify, list and unpack
  to free functions in `pack/verify.rs`, `pack/read.rs` and
  `pack/extract.rs`, stubs until I12 and I13 fill them.
- 2026-11-04: the container directory is the first member, with the top
  directory's mode and time, and has no listing line; listing `size` is
  the tar header size, 0 for directories, symlinks and hard links; the
  first path of a hard-linked inode in path-byte order is the file,
  later ones link to `<container>/<first path>` with listing type
  `hardlink` and `link` the first path; the sparse warning fires above
  1 GiB, strictly; a file that grew is cut at the header size, a short
  read pads with zeros and marks the file changed, a vanished file fails
  the pack naming it; `PackSettings` carries the archive or live preset.
  I06 writes the listing and member rules into spec "Packaging" and
  "Listing".

- 2026-11-04 (I06 delivery, accepted): the container's name is the last
  component of the tree's top (a root top is refused); `pack` refuses a
  tree with problems or without its top entry before writing a byte;
  link names are written literally, with GNU long-name records, since
  the tar crate's `append_link` rewrites targets; nothing calls the
  sparse warning before I19. Spec "Packaging" and "Listing" written by
  I06.

### I08

- 2026-11-04: history actions are `archive`, `reclaim`, `unearth`,
  `fork` (on both IDs), `sync`, `import`, `resolve`; a listing file
  carries the first 16 hex of its payload's SHA-256, and a put onto an
  existing listing name compares the full SHA-256 of both files; an
  unreadable `meta.pending` is an error, never a fallback;
  `write_pending` is refused over an unparsable or too-new highest
  committed version; `promote_pending` refuses a number not above the
  highest local committed one. I08 writes these into spec "Metadata",
  "Storage", "Metadata changes" and ADR 0058.

### I09

- 2026-11-04, I09 plan review (ledger `i09-ledger.md`, 18 findings
  agreed): archive step 5 sets the entry from `removing` to `gone`,
  keeping its local payloads; a gone entry whose own metadata is live and
  whose folder is back at its path keeps its ID; an entry without
  metadata, and own live metadata without an entry, derive to an error
  per entry that `status` lists and I17 repairs. I09 writes these into
  spec "States", "Local state" and "archive". Local payloads are
  identified by file; an entry has a `removing_payload` field;
  `read_all` returns the entries and the damaged files.
- Write orders the later items follow (from I09): archive, unearth and
  minting write the metadata first, then the entry (I19, I30, I32, I34;
  removing sets `removing_payload`); I20 unearths as entry
  `unearthing`, rename, entry `live`, then metadata; a fork (I22, I26,
  I31, I37) writes the old ID's archived metadata, the old entry `gone`
  keeping its local payloads, the new metadata, then the new entry; I21
  writes a `gone` entry with the local archive at version 1, version
  taken out `pending`. I17's repair writes the missing live entry for
  own live metadata with a folder, uses `removing_payload`, removes
  tracking temp files, and removes manifests through I14's code (as does
  I35); I16 and I17 list damaged entries, and a damaged ID blocks
  minting at its path.

### I10

- 2026-11-04: `collection.ignore` lines are anchored at each piece's
  top; any entry named `.antiq-pin` at the top that is not a directory
  pins, a directory of that name does not and `unpin` refuses it; the
  pin is created with `TreeRoot::create_file` (O_EXCL, an empty marker
  needs no atomic write) and removed with `remove_file`; a tree with walk
  problems is not idle (an error) unless pinned. I10 writes these into
  spec "Idle", "Pins" and "Config".

### I11

- 2026-11-04: commands get `out: &mut Output` as the last parameter of
  `cmd::*::run`, and `Output` gains `line` (stdout) and `note` (stderr);
  I11 lands this as its own first commit. `TempCollection` stays the
  I01 helper (section 4.2). The `archive.sh` exclude is proposed only
  when `archive/` is a directory holding a `*.tar.xz` and
  `archive/index/` holds a `*.lst` (ADR 0009), and the patterns would
  select `archive` (I11 writes it into spec "Collections and pieces").
- 2026-11-04 (I11 plan review, S2): `init` refuses when a selected
  piece's top holds a collection marker, naming each nested collection
  root, exit 1; the walk reports a collection marker deeper inside a
  piece as a problem, so such a piece is never archived (I03 follow-up
  batch).

### I14

- 2026-11-04: `collection.live_exclude` entries are paths relative to
  the collection root in the escaped form, parsed into `RelPath` like
  `collection.exclude`; an entry and everything below it is left out of
  the live package and the manifest, an entry at or above a piece's top
  leaves that piece without a live package, an empty entry is an error;
  the packer reports whether anything was left out (for `unearth`'s
  warning). `live_retries` counts restarts, so the default 3 gives four
  attempts. A file that vanishes after its read counts as changed;
  only a file gone before it is opened is dropped. The caller (I32)
  holds the pool lease; a restart re-walks the piece. I14 writes these
  into spec "Packaging", "Verifying" and "Config".
- 2026-11-04: what a live package left out reaches other machines
  through a payload key `left_out` (escaped paths, live payloads only,
  omitted when empty), not through the `encoder` record. I14's packer
  returns the list; I32 adds the key to `Payload` and to spec
  "Metadata" when it writes live payloads; I34 warns on it.

### I12

- 2026-11-04, I12 plan review (ledger `i12-ledger.md`, 8 findings
  agreed): a staged verify skips only the top's mode (the caller sets it
  after the rename, I20); `compare_with_listing` also returns the lstat
  record, so reclaiming feeds archive step 4 with it; `Verified` holds
  the package's entries (`Verified::listing`), which I19 stores as the
  listing; negative base-256 tar times list as 0. For I19: the top
  entry's ctime in the record changes when antiq moves the piece, so
  step 4 compares kind, inode and `mtime_ns` for the top.
- 2026-11-04: on a user's tree the strict verify does not compare
  directory modification times, as the listing comparison does not,
  since a junk file such as `.DS_Store` changes its directory's time
  without changing what is archived; on staging it compares them. I12
  writes it into spec "Verifying".

### I13

- 2026-11-04: no path-length refusal before unearthing; only name
  lengths are checked, since extraction works relative to directory
  handles. I13 removes "and path" from ADR 0054 and spec "unearth".
- 2026-11-04, I13 plan review (ledger `i13-ledger.md`, 17 findings
  agreed): three passes over the stream (raw scan, gather, extract),
  each drained so the checksum is checked; macOS name limit in UTF-16
  units; the case fold is NFD, lower, upper, lower, NFD.
- 2026-11-04 (O12): the final pass stays inside `unpack` (ADR 0050). A
  directory whose mode denies its owner read or search can then hide
  its children from the strict verify, so `WalkRules` says whether the
  tree is antiq's own staging; only there may the verify give such a
  directory owner access for the read and put its mode back afterwards
  (I12). The staging cleanup already grants owner access (I03's
  `remove_tree`).
- 2026-11-04 (O13): Linux cannot set a symlink's mode, so the strict
  verify and the listing comparison do not compare symlink modes on any
  platform (I12 writes it into spec "Verifying").

### I07

- 2026-11-04: two lines in `src/testing/mod.rs` export the shared test
  pool; the Linux container run relies on `RUST_TEST_THREADS=4`, since
  `sysconf` sees the host's memory, not a container limit (R5 holds
  only with that).
- Open, assigned: the bytes per worker of a level-3 lease is decided in
  I14's plan.
- 2026-11-04: listing compression takes no pool lease at all. A listing
  is compressed by one single-threaded level-3 encoder of a few MiB,
  next to whatever payload lease the thread holds, so the
  one-lease-per-thread rule never trips (I08).
- 2026-11-04, review P of the finished diff (ledger `review-p-ledger.md`):
  an archive encoder costs about 430 MiB plus 140 MiB per worker
  (measured), so a lease is charged 448 MiB fixed plus 160 MiB per
  worker instead of 256 MiB per worker (spec "Packaging" and
  `docs/research/compression.md` written by the lead); the
  one-lease-per-thread check is an `assert!`; tests added for FIFO among
  blocked waiters, both wake-up paths, the want-0 clamp and the
  machine's figures.

## Relations

- Relates to: [Define the v1 scope and architecture of antiq](01M6E17PET32XD9PC04AJ2PBZY-define-the-v1-scope-and-architecture-of-antiq.md)
