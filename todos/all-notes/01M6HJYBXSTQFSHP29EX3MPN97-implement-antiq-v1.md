---
title: "Implement antiq v1"
kind: plan
component: project
origin: request
tags: [architecture]
---
# Implement antiq v1

This plan turns the version 1 design of antiq, `docs/specs/spec-v1.md`
and ADRs 0001 to 0057 at commit `ef3f47d`, into 39 work items in 11
waves plus one item that waits on the design of the picker's contents
view. Each item names the spec sections and ADRs it implements, the
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
picker runs alongside from wave 7. The plan also lists the risks, the
technical questions for the lead, the behaviour questions for the user,
and a table that maps every spec section and ADR to the items that
implement it.

## 1. Ground rules

These bind every item.

- Base: `main` at `ef3f47d`. `src/main.rs` holds an empty clap `Cli`,
  `src/lib.rs` a doc comment. Dependencies: anyhow 1.0.104, clap 4.6.7
  (derive), thiserror 2.0.21. `cargo test` runs 0 tests.
- Every commit passes `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings` and `cargo test`. Each
  behaviour change starts with a failing test. Every changed production
  line is covered (section 4.8). Docs (spec, ADRs, README) change in the
  same commit as the behaviour.
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
- ADRs may be rewritten in place until implementation starts; once code
  lands, a changed decision becomes a new ADR. The merge of I01 is the
  first code. Questions in section 8 that would change an ADR are
  therefore best settled before I01 merges.
- Nothing is pushed.
- An item edits only the files it owns plus the shared lines its entry
  names. I01 creates every file of the module tree as a stub, so later
  items fill files and never add module declarations.
- The README changes only at milestones (I16, I30, I39). Spec changes
  go into the section an item implements; JSON payloads go into the
  per-command placeholder lines I01 adds to "JSON output".

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
| `relpath` | `RelPath` and `Name` as bytes, escaped form, payload name part, machine part, temp names, lexical normalisation | spec "Metadata", "Storage" | I02 |
| `id` | `ObjectId`, `IdSource`, ID time | ADR 0014, 0039; spec "Objects" | I02 |
| `fsx` (`atomic`, `remove`, `moves`, `space`, `device`) | Atomic write, delete helper, moves with owner write, free space, device and filesystem type, birth time | spec "Storage", "Walks", "Operations" | I03 |
| `lock` | `.antiq/lock`, `RunLock` | spec "Operations" | I03 |
| `config` | `config.toml`: typed read, edits that keep comments | spec "Config" | I04 |
| `collection` | Finding `.antiq/` upwards, `Collection` with the local layout paths | ADR 0011; spec "Collections and pieces", "Storage" | I04 |
| `machine` | Machine name | spec "Config" | I04 |
| `select` | Selecting pieces; later behind `SelectionStrategy` | ADR 0012, 0028; spec "Selection" | I05, I38 |
| `walk` | The lstat walk of a piece | spec "Walks" | I05 |
| `idle`, `pin` | Idle verdict, empty, pins | ADR 0013, 0024, 0029, 0037, 0038; spec "Idle", "Pins" | I10 |
| `meta` | Metadata JSON | ADR 0053; spec "Metadata" | I08 |
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
| `remote` (`mod.rs`) | `Remote` trait, `Reachability`, opening the configured remote | ADR 0015, 0031 | I23, I38 |
| `remote::url`, `remote::marker`, `remote::path` | URL parsing, `antiq-remote.json`, path remote | ADR 0026, 0031, 0047 | I23 |
| `remote::commit`, `remote::cleanup`, `remote::testing` | Commit protocol, cleanup, fault-injecting remote for tests | ADR 0030 | I27 |
| `remote::ssh::session`, `::script`, `::classify` | `ssh` executable, master connection, `/bin/sh -s` scripts, away and broken | ADR 0048 | I24 |
| `remote::rsync` | rsync resolution, probe, command line, stats | ADR 0049 | I25 |
| `remote::ssh` (`mod.rs`) | `SshRemote`, the trait over ssh and rsync | ADR 0048, 0049 | I28 |
| `remote::policy`, `remote::away`, `remote::network` | `--if-reachable`, `away_warn_days`, `network_check` | spec "Remotes", "Dry runs, scheduling, exit codes" | I29 |
| `ops::<command>` | One module per command; `ops::archive`, `ops::unearth`, `ops::resolve` and `ops::sync` are directories with one file per part | spec "Operations", "Command line" | command items |
| `testing` (`cfg(test)`) | Fixtures shared by library tests | section 4 | I01 |

Binary, `src/main.rs` with modules under `src/cli/` (T20 in section 8):

| Module | Responsibility | Item |
|---|---|---|
| `cli` (`mod.rs`) | clap types for the whole command surface, dispatch to `cli::cmd` | I01 |
| `cli::output` | JSON envelope, error object, exit codes, colour, stdout and stderr | I01 |
| `cli::tty` | Controlling terminal, `TtyInteraction` implementing `interact::Interaction` | I01 |
| `cli::cmd::<command>` | One file per command: builds the call into `ops`, renders the console output | command items |
| `cli::picker` (`mod.rs`, `state.rs`, `layout.rs`) | The picker of `find` | I36, I40 |

### 2.2 Core types

- `ObjectId`: newtype over `ulid::Ulid`. Text form is the crate's upper
  case (T19). Parses either case.
- `RelPath`: a path relative to the collection root as bytes, since
  "paths and names are stored as the bytes the filesystem holds"
  (spec "Metadata"). Serialises in the escaped form. `Name` is one
  component.
- `Collection`: root path, `.antiq/` path accessors for every entry of
  the local layout (`config.toml`, `lock`, `objects/`, `tracking/`,
  `index/`, `tmp/`, `staging/`, `trash/`, `recovered/`), the parsed
  `Config` and the collection ULID.
- `Config`: `collection` policy, `machine`, `remotes: Vec<RemoteConfig>`
  with every key and default of the spec's config table.
- `TreeEntry` and `PieceTree`: the walk's result per entry: relative
  path, type, size, modification time in nanoseconds, ctime, inode,
  device, mode, link count, link target, birth time; sorted by path
  bytes; problems (`unreadable` with the first failing entry,
  `other_device` with the mount point path).
- `ListingEntry`: path, type, size, modification time in whole seconds,
  mode, link target, SHA-256 (spec "Listing"). I01 defines the struct so
  that the tar writer (I06) and the listing format (I08) can be built in
  parallel.
- `Metadata`, `Payload`, `HistoryEntry`, `Lifecycle`, `PayloadKind`,
  `CreatedSource`: the tables of spec "Metadata", with an `extra` map on
  every struct that can grow (foundations research §5).
- `TrackingEntry`: the fields of spec "Local state", plus the fields
  `status` needs to show stale backups and entries missing on the
  remote (I09 adds them to the spec).
- `IdleVerdict`: pinned, empty, active with the newest time, idle,
  only future times; future files listed.
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
      fn verify_strict(&self, package: &Path, tree_root: &Path, rules: &WalkRules)
          -> Result<Verified, PackError>; // with the lstat record of every entry
      fn list(&self, package: &Path) -> Result<Vec<ListingEntry>, PackError>;
      fn unpack(&self, package: &Path, staging: &Path, rules: &TargetRules)
          -> Result<Unpacked, PackError>;
  }
  ```

- `Remote` (ADR 0015: one trait, every implementation provides every
  operation). Spec "Remotes" lists reachability with three results, bulk
  metadata pull and push, payload put, get and delete, and exclusive
  creation; the commit protocol, cleanup and tiers need listing an
  object directory, a rename into place, the remote clock, a checksum of
  a remote file and a flush (NAS `sync`). Sketch, settled by I23:

  ```rust
  pub trait Remote: Send + Sync {
      fn reachability(&self) -> Result<Reachability, RemoteError>; // checks the marker
      fn begin_phase(&self) -> Result<(), RemoteError>;  // SSH: `-O check`, new master
      fn clock(&self) -> Result<Timestamp, RemoteError>;
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
  code. `ErrorKind` holds the kinds the spec names (`ambiguous_reference`
  with candidates, `unknown_reference`, `brake` with planned count, set
  size and up to ten items, `lock_held`, `remote_away`, `remote_broken`)
  and the ones the items need (`not_a_collection`, `usage`, others). I01
  also adds a transitional `not_implemented`; I39 removes it.
- Exit codes (spec "Dry runs, scheduling, exit codes"): 0 success; 1
  error, including remote broken, lock held, a held brake without a
  terminal, an unknown or ambiguous piece; 2 usage error (clap's own
  usage errors exit 2: `USAGE_CODE` in clap_builder 4.6.7); 3 partial,
  from `CommandReport::partial()`; 4 remote away, turned into 0 by
  `--if-reachable` as I29 implements.
- The binary propagates errors with anyhow and context (ADR 0005) and
  finds the `antiq::Error` in the chain to pick the exit code and JSON
  kind.

### 2.5 Console and JSON output

- antiq writes console output; the picker is its only full-screen part
  (ADR 0056). Data goes to stdout, messages to stderr; colour only on a
  terminal (spec "Dry runs, scheduling, exit codes").
- Every JSON output is one object `{"format": 1, "command": "<name>",
  ...}`; errors add `{"error": {"kind": ..., "message": ...}}`; a dry run
  carries `"dry_run": true` and `actions` (spec "JSON output"). The
  library's report types derive `Serialize`; `cli::output` wraps them in
  the envelope. Warnings travel in each report as a list and print to
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
  question on `/dev/tty`. One generic question type keeps the trait
  closed: items add questions without editing `interact.rs` (T21).
- Pending decisions live in the tracking entries (spec "Local state").
  `resolve` executes one answer at once, under the lock, with the loss
  computed at that moment; no answer is stored (spec "Decisions").

### 2.7 Dry runs

- Every state-changing command takes `--dry-run`, which prints the plan
  and changes nothing (ADR 0046). Read-only commands and `--dry-run`
  never touch `trash/`, `tmp/` and `staging/` (spec "Operations").
- Each state-changing op is split into `plan(...) -> Plan<Action>`,
  which reads local state (and the remote through read-only calls, T7)
  and decides, and `apply(plan, &RunGuard)`, which acts. `--dry-run`
  renders the plan; the normal path applies it. Steps whose outcome
  depends on an earlier step (a verify that may fail, an upload that may
  fail) appear in the plan as intended actions.
- The brake evaluates on the plan, so `--dry-run` reports where it would
  hold (ADR 0045).

### 2.8 Runs, the lock and proof tokens

- `run::RunGuard::begin` takes the lock (`lock::RunLock`, I03), writes
  pid, machine and start time, and does the start-of-run repairs (I17).
  Every function that writes into `.antiq/` or deletes a folder takes
  `&RunLock` or `&RunGuard`; `RunLock` has private fields and only
  `lock::acquire` makes one.
- Recommended type-state for the irreversible steps (T22): a folder is
  removed only through a function that takes a `DurableArchive`, made
  only by the store after a `Verified` (from the strict verify) and the
  durable writes of payload, listing, metadata and tracking (spec
  "archive" step 2, ADR 0033). A local archive is dropped only through
  a function that takes a `ConfirmedRemoteCopy`, made only by the check
  of ADR 0043. Each token gets a `compile_fail` doctest showing that it
  cannot be built outside its module.

### 2.9 Local metadata before `sync`

The spec says changes "are recorded locally and committed as a new
metadata version at the next `sync`" and that `.antiq/objects/<ulid>/`
has the remote's layout, but not where a recorded, uncommitted change
lives. The proposal (T1): `objects/<ulid>/meta.<N>` holds versions
known to be committed (pulled or committed by this machine);
`objects/<ulid>/meta.pending` holds the next version, written through
the atomic write. Local readers prefer `meta.pending`. `sync` commits it
as `meta.<N+1>` on the remote with the protocol of ADR 0030, with N the
version taken out (recorded in tracking), and then renames it locally
to `meta.<N+1>`. Without a remote it stays pending. I08 implements the
store side; if the lead accepts, I08 records it as an ADR.

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
| serde | 1.0.229 | derive | all formats | | I01 |
| serde_json | 1.0.154 | preserve_order | metadata, listings, tracking, JSON output | keeps unknown keys in input order, nested too (foundations §5) | I01 |
| toml | 1.1.8 | | typed config read; `deny_unknown_fields` names the bad key (foundations §6) | | I01 |
| toml_edit | 0.25.17 | | config edits that keep comments and order (foundations §6) | | I01 |
| jiff | 0.2.39 | serde | `Timestamp`, UTC by type (foundations §5) | fractional digits vary with the value; a fixed width needs a custom serializer (T15) | I01 |
| ulid | 3.0.0 | default (`std`) | IDs; `Ulid::from_datetime` per unit, not a shared `Generator`; `Ulid::from_parts` for fixed test IDs | a time before 1970 gives timestamp 0 without an error, so the 1990 rule runs before minting (foundations §4) | I01 |
| sha2 | 0.11.1 | | SHA-256 | pure Rust, hardware instructions detected at run time on aarch64; RUSTSEC-2021-0100 affects 0.9.7 only (foundations §3) | I01 |
| glob | 0.3.4 | | selection patterns | the only measured crate whose `require_literal_leading_dot` implements the dot rule; no braces (foundations §1) | I01 |
| ignore | 0.4.34 | | gitignore matching for the idle check (`Gitignore`) | measured with anchored rules and negation (foundations §2) | I01 |
| tar | 0.4.46 | default features off if it builds (T18) | tar writing and reading | at least 0.4.45 for RUSTSEC-2026-0067 and 0068; RUSTSEC-2021-0080 and 2018-0002 are older (tar research) | I01 |
| zstd | 0.14.3 | default features off; `experimental`, `zstdmt` | compression | `experimental` exposes `RSyncable`, `zstdmt` multithreading; at least 0.14.3 for RUSTSEC-2026-0391; `legacy` off, which holds the RUSTSEC-2026-0392 code (compression research) | I01 |
| zstd-sys | `=2.1.2+zstd.1.5.7` | | pins the bundled zstd 1.5.7 exactly (ADR 0052) | zstd-sys RUSTSEC-2026-0410 is wasm32 only (compression research) | I01 |
| filetime | 0.2.29 | | directory times in the final extraction pass; fixtures | used by the tar research probe | I01 |
| rustix | 1.1.6 | fs, process | plain `fsync`, `statvfs`, `statfs`, pid liveness, `setsid` in tests, dirfd-relative walking (R3) | probed in the filesystem research, which names RustSec entries only for `nix` and `remove_dir_all` | I01 |
| libc | 0.2.191 | | `sysconf(_SC_PHYS_PAGES)` for the memory budget (T12) | not researched; `cargo search` shows a 1.0 alpha, so the version is given explicitly | I01 |
| whoami | 2.1.3 | | machine name: `devicename()` on macOS, hostname elsewhere | `devicename()` on Linux reads `PRETTY_HOSTNAME` and fails when absent (foundations §7) | I01 |
| tempfile (dev) | 3.27.0 | | temporary collections and remotes | probed in the filesystem research | I01 |
| insta (dev) | 1.49.0 | filters | console output snapshots | not researched; ntropy uses it | I01 |
| unicode-normalization | 0.1.26 | | NFD folding for name collisions (ADR 0054) | not researched (T11) | I13 |
| crossterm | 0.29.0 | | picker drawing | ntropy's version (ADR 0057); advisories to check with `cargo audit` | I36 |
| nucleo | 0.5.0 | | picker matching | ntropy's version | I36 |
| unicode-width | 0.2.3 | | picker layout | ntropy pins 0.2.2 | I36 |
| unicode-segmentation | 1.13.x | | picker layout | ntropy's version 1.13.3 | I36 |

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
  hard links, a fifo, a sparse file (`set_len`), unreadable directories
  (mode 000, restored on drop so the temp directory can be removed).
- `TempCollection`: a temp directory made a collection through the
  `init` code once I11 lands; before that a minimal `.antiq/` written
  directly by the I01 helper.
- `FixedClock` with `advance`; `SeqIds` minting `Ulid::from_parts(ms,
  counter)`.
- `ScriptedInteraction`: a queue of expected questions and answers; it
  fails the test on an unexpected question or a leftover answer.
- Two machines: two `TempCollection`s with their own `machine.name` and
  collection ULIDs on one path remote directory.
- Binary tests cannot inject a clock: they set file times relative to
  the real time and set `idle_days`, `remote_only_days` and
  `grace_hours` to 0 where a test needs immediate eligibility.
- Helpers needed by one item stay in that item's test module; shared
  ones go into a new file under `src/testing/` or `tests/support/`.

### 4.3 Remote test doubles

- The path remote on a temp directory is a real implementation with no
  executables. It is the default remote of scenario and binary tests.
- `remote::testing::FaultRemote` (I27, `cfg(test)`) wraps a path remote:
  it fails the Nth call of an operation with an error, away or broken,
  and runs a callback between steps, so another machine can claim,
  commit or delete in the middle of a commit (ADR 0030).
- Seams for conditions an unprivileged test cannot create: a device
  boundary inside a piece, a mount point at the remote root, free
  space, the filesystem type (SMB and NFS warnings), `ENOTSUP` from
  `sync_all`. Each seam is an internal parameter (a probe trait or a
  function value), not a config key (T17).

### 4.4 SSH and rsync

What runs where, on this machine (macOS, no guaranteed sshd):

| Layer | Runs in | Covers |
|---|---|---|
| Unit tests | `cargo test` | ssh argv and `-o` options, ControlPath length, POSIX quoting, `/bin/sh -s` script text, the stderr classifier on recorded texts, rsync argv and environment, the version probe on `rsync  version 3.5.1  protocol version 33` and `openrsync: protocol version 29`, the stats parser on recorded output |
| Loopback shim | `cargo test` | `SshRemote` end to end without networking: `tests/fixtures/ssh-shim.sh` behaves like `ssh` (accepts the options antiq passes, simulates `-M -N -f`, `-O check` and `-O exit` with a socket file, runs the remote command with the local `/bin/sh`); rsync runs the shim through `-e`, which works because rsync runs an arbitrary `-e` program with plain argv (ssh research) |
| Fault modes | `cargo test` | the shim reads an environment variable to exit 255 with recorded error texts (away, broken); a fake rsync script (`machine.rsync` points at it) exits 23, 24, 30 or prints no stats |
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
- The terminal glue left over is covered by a small pty harness or
  listed as an accepted exception (T16).

### 4.6 Determinism

- A golden test hashes the live-package output for a fixed input
  (ADR 0052).
- A tar stability test: the same tree packs to the same bytes; a
  touched file changes its header (ADR 0050, tar research).

### 4.7 Linux

The filesystem research ran on Linux (OrbStack, ext4, btrfs, tmpfs,
overlay); antiq's own tests run there in I39 through
`scripts/test-linux.sh` (cargo test inside a Rust container).
Linux-only behaviour (non-UTF-8 names, `f_type` magic numbers) gets
`#[cfg(target_os = "linux")]` tests.

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
(one to two days), L 1000 to 1500 (two to three days). "First failure"
says what a test shows before the change. Where an entry has no Goal
line, its title is the goal.

Every item is accepted when its tests pass, the three check commands
pass on every commit, every changed production line is covered or
listed as an exception (4.8), the docs it names are updated in the
same commits, its JSON payload (if it adds a command's behaviour) is in
the spec, and its step todo is deleted in its finishing commit.
Acceptance lines in an entry add to this.

### Wave 1

#### I01 Lay down the module tree, shared types and the command surface

- Goal: a crate that builds, lints and tests clean, with every module
  file of section 2.1 present, the shared types complete, the whole
  command surface parsed, and every command answering
  `not_implemented` until its item lands. This is the rudimentary CLI.
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
  usage error.
- Owns: `Cargo.toml`, `Cargo.lock` (section 3), `src/lib.rs`,
  `src/main.rs`, `src/error.rs`, `src/clock.rs`, `src/interact.rs`,
  `src/plan.rs`, `src/report.rs`, stubs of every other module file,
  `src/cli/mod.rs`, `src/cli/output.rs`, `src/cli/tty.rs`, stubs of
  `src/cli/cmd/*.rs`, `src/testing/mod.rs`, `tests/support/{mod,cmd,tree}.rs`,
  `tests/cli.rs`, `scripts/check.sh` (fmt, clippy, test),
  `scripts/uncovered-lines.sh`; in the spec, one placeholder line per
  command under "JSON output".
- Shared types defined complete here: `ObjectId`, `RelPath` and `Name`
  (type and constructors; I02 adds the escaped form), `ListingEntry`
  and its entry type, `Clock` with `SystemClock` and `FixedClock`,
  `IdSource` (trait; I02 implements), `Interaction`, `Question`,
  `Choice`, `NonInteractive`, `ScriptedInteraction` (`cfg(test)`),
  `Plan<A>`, `CommandReport`, `Warning`, `ErrorKind`, `Error` with one
  `#[from]` variant per module error stub. Stubs that later items fill
  without touching their callers: `remote::probe_state(&Collection) ->
  RemoteState` returns `None`, `remote::away::status_warning(...)`
  returns no warning (filled by I23 and I29).
- Must not touch: any behaviour beyond parsing and the envelope.
- Depends on: none.
- Tests: `antiq --help` lists the thirteen subcommands, `pin` and
  `unpin` counted apart (first failure: no subcommands); `antiq status --dry-run`, `antiq find --force`,
  `antiq status --if-reachable` and `antiq resolve --mine` without a
  piece exit 2 (first failure: unknown arguments are not defined
  yet); `antiq --json status` prints `{"format":1,"command":"status",
  "error":{"kind":"not_implemented",...}}` and exits 1; unit tests map
  every `ErrorKind` to its exit code; the envelope serialises in key
  order `format`, `command`, then the payload; `setsid` helper in
  `tests/support/cmd.rs` proven by a test that `/dev/tty` cannot be
  opened in the child.
- Docs: spec "JSON output" placeholders; no README change.
- Acceptance: `scripts/check.sh` green; `cargo audit` clean or every
  finding listed; `shellcheck` clean; every file carries the MPL header.
- Size: L.

### Wave 2

#### I02 Relative paths, escaped forms and object IDs

- Goal: byte-exact relative paths with the escaped form, the payload and
  machine name parts, temp names, and minting IDs from a folder's
  times.
- Implements: spec "Metadata" (escaped form: valid UTF-8 stays, `%`
  becomes `%25`, every byte not part of valid UTF-8 becomes `%XX`),
  "Storage" (name part: every byte outside `[A-Za-z0-9._-]` as `%XX`,
  cut to 64 bytes without splitting an escape; machine part escaped the
  same way, cut to 32 bytes; temp name `.<final name>.<machine part>.part`),
  "Objects" (ID time: earliest of the directory's birth time and the
  birth and modification times of every entry, ignoring times before
  1990; source `birth`, `oldest-mtime`, `import`, `fork`; minted once),
  ADR 0014, 0039.
- Owns: `src/relpath.rs`, `src/id.rs`.
- Content: escape and unescape with a round trip for any bytes; lexical
  normalisation for references, `--to` and `--path` (resolve `.` and
  `..`, refuse a path above the root); `RandomIds` through
  `Ulid::from_datetime` per unit and `SeqIds` for tests (foundations
  §4); `id_time()` returning time and source, with the 1990 floor
  applied before minting. Hex digits in `%XX` are upper case (the spec
  writes `%25`), recorded in the spec.
- Must not touch: walking the disk (the caller passes the times).
- Depends on: I01.
- Tests: `b"a%\xffb"` escapes to `a%25%FFb` and back (first failure:
  stub); a name of 70 bytes with an escape across byte 64 is cut before
  the escape; `id_time` ignores a 1989 time and picks a 1995 mtime with
  source `oldest-mtime`; a birth time earlier than every mtime gives
  `birth`; times all before 1990 hit T2's rule; `..` past the root is
  refused.
- Docs: spec "Metadata" (hex case), T2's outcome if it adds a rule.
- Acceptance: property-style round-trip test over random bytes passes.
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
  macOS, which smbfs refuses); `remove_tree` (chmod u+rwx retry on
  `PermissionDenied`, refuses to descend into another device);
  `move_dir` (owner write around the rename, `CrossesDevices`
  reported as such); `available_space` (`statvfs`); device of a path,
  mount point test (`st_dev` against the parent, `statfs` mount name on
  macOS), filesystem kind (local, SMB, NFS, other) from `statfs`
  (`f_fstypename` on macOS, `f_type` magic numbers on Linux); birth time
  (std `created()`). Lock: open without truncating, `try_lock`, then
  `set_len(0)` and write pid, machine, start; `lock::holder()` for
  `status` and `find` reads the file and tests whether the pid is alive,
  taking no lock (T13 has the reason).
- Must not touch: callers.
- Depends on: I01.
- Tests: atomic write replaces content and leaves no temp file (first
  failure: stub); the fallback path runs when the flush seam returns
  `ENOTSUP`; `remove_tree` deletes a tree with a `0555` directory (std's
  `remove_dir_all` failed there, filesystem research §4);
  `move_dir` moves a `0555` directory to another parent and restores
  its mode (EACCES without owner write, reproduced in review 2, finding
  O4); a second `try_lock` on another open file description returns
  held with the first holder's pid; the device seam makes `remove_tree`
  stop at a foreign device.
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
  for a second `[[remote]]`; a `ConfigDoc` over `toml_edit` with the
  edits later items need (write a new config at `init`, set or remove
  the `[[remote]]` entry, set `machine.rsync`); `Collection::find(start)`
  and the layout accessors; machine name: `whoami::devicename()` on
  macOS falling back to the hostname, the hostname elsewhere.
- Must not touch: creating `.antiq/` (I11).
- Depends on: I01.
- Tests: discovery from a nested subdirectory finds the root (first
  failure: stub); `-C` start path; outside any collection gives
  `not_a_collection`; each default equals the spec table (one assertion
  per key); an unknown key under `[machine]` fails naming the key; two
  `[[remote]]` entries fail; an edit keeps comments and key order.
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
  `require_literal_leading_dot`; a top-down walk that stops at a match
  and descends only where a pattern can still match; exclude entries as
  literal relative paths with nothing below them a piece (T10);
  `walk_piece()` returning `PieceTree` sorted by path bytes, junk left
  out, problems recorded; dirfd-relative traversal so paths past the
  platform's path limit work (R3).
- Must not touch: idle rules (I10), packing.
- Depends on: I01.
- Tests: `*` skips `.git`, `.*` selects it, `projects/.*` selects
  `projects/.x` (first failure: stub); `**/*` selects what `*` selects;
  a symlinked directory is never a piece and is counted; `.antiq` is
  never a piece; an excluded path and its subtree are not pieces;
  entries come out in path-byte order (`dir`, `dir-y`, `dir/x`, tar
  research); a `000` directory makes the piece unreadable naming it; the
  device seam flags a foreign device with its path; a tree deeper than
  1,024 bytes walks completely.
- Docs: spec "Selection" (exclude semantics after T10).
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
- Content: hard links by `(dev, ino)` through `append_link`; the archive
  layout of members per T3 (top-level folder or not); worker count as a
  plain parameter (I07 supplies it).
- Must not touch: verification and extraction (I12, I13), live-package
  tolerance (I14).
- Depends on: I01.
- Tests: a `0600` file keeps its mode, uid 0, empty uname (first
  failure: stub); entries in path-byte order; packing the same tree
  twice gives the same bytes and touching a file changes them; a hard
  link reads back as `EntryType::Link`; a fifo is skipped and reported;
  a 1969 mtime is stored as 0 and reported; the zstd frame has the
  checksum flag and window log 27 and decodes with the 128 MiB limit; a
  2 GiB file made with `set_len` triggers the sparse warning; a reader
  that returns fewer bytes than the header size pads and marks the file
  changed.
- Docs: spec "Packaging" if T3 adds the member layout.
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
  allow it; dropping the lease returns its workers.
- Must not touch: the encoder (I06 takes the worker count).
- Depends on: I01.
- Tests: two leases never exceed the cores (first failure: stub); with
  1 GiB RAM at 25 % only one level-19 worker is handed out; a lease
  waits until another is dropped (thread test with a timeout); a piece
  below one job gets one worker.
- Docs: none.
- Size: S.

### Wave 3

#### I08 Metadata and listing formats and the local object store

- Goal: the formats shared through the remote and their local home.
- Implements: ADR 0053; spec "Metadata" (key order, payload object,
  keys left out where they do not apply, unknown fields kept, higher
  `format` read-only with a clear error on write, escaped paths),
  "Listing" (zstd-compressed JSON Lines, first line `{"format":1}`, one
  line per entry), "Storage" (`.antiq/objects/<ulid>/` with the
  remote's layout; payload files never overwritten, a put onto an
  existing name succeeds only when the full SHA-256 matches), the
  proposal of section 2.9, foundations §5.
- Owns: `src/meta.rs`, `src/listing.rs`, `src/store.rs`.
- Content: serde structs with `extra` maps; a first pass reading only
  `format`; `verified` with its three states (absent, null, a time);
  history actions named after the rows of spec "Metadata changes";
  listing writer and streaming reader (compression settings per T4);
  store: current metadata (the highest `meta.<N>` that parses, plus
  `meta.pending`), write pending, promote pending to `meta.<N>`, put a
  payload or listing from `.antiq/tmp/` under its final name, paths,
  object enumeration. Writes go through `fsx::write_atomic` and take
  `&RunLock`.
- Must not touch: remote code.
- Depends on: I02, I03.
- Tests: serialising a metadata value gives keys in the spec order
  (first failure: stub); unknown keys at the top and inside a payload
  survive a round trip; a `format: 2` file reads but refuses a write
  with `format_too_new`; `listing`, `verified`, `superseded`, `encoder`
  are absent for a live payload; a listing round trip keeps a path with
  a non-UTF-8 byte; the store picks the highest parsing version and
  reports the unparsable one; putting different bytes under an existing
  payload name fails; a `compile_fail` doctest shows a store write
  without a `RunLock`.
- Docs: spec "Listing" (key names), "Metadata" (history action names),
  ADR for section 2.9 if the lead accepts T1.
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
  `.antiq/tracking/<ulid>.json`; `PendingDecision` for the four kinds of
  the decision table; fields `status` needs: stale backup since,
  missing on the remote, diverged; `derive_state(entry, metadata,
  folder_present, own_collection)` for every row of the states table.
- Must not touch: live manifests (I14 owns their files).
- Depends on: I02, I03.
- Tests: one case per row of the states table (first failure: stub);
  round trip; a write without `RunLock` does not compile.
- Docs: spec "Local state" (the added fields).
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
  type of section 2.2; `pin::create`, `pin::remove`, `pin::is_pinned`.
- Must not touch: the commands `pin` and `unpin` (I18).
- Depends on: I04, I05.
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
- Docs: spec "JSON output" (`init` payload).
- Size: M.

#### I12 Strict verify and comparison with a listing

- Goal: the checks that gate every deletion and every move into place.
- Implements: ADR 0033, 0035; spec "Verifying" (stream integrity, then
  every member against the tree: type, size, content hash,
  modification time in whole seconds with the same clamp, mode, link
  target; equal path sets under the same walk rules; junk left out; the
  lstat of each entry recorded while reading: size, modification time
  in nanoseconds, ctime, inode), the listing comparison used by
  reclaiming and by the start-of-run repair of `unearthing` entries
  (path, type, size, modification time in whole seconds, mode, link
  target, content hash; directory times and junk do not count), tar
  research ("Strict verify").
- Owns: `src/pack/read.rs` (members of a package as a stream, listing
  from a package), `src/pack/verify.rs`.
- Must not touch: extraction (I13).
- Depends on: I05, I06.
- Tests: a fresh pack verifies with no differences (first failure:
  stub); an edited file, a deleted file, an added file, a changed mode
  and a repointed symlink are each reported (tar research case list); a
  truncated stream fails the integrity check; junk on disk is ignored;
  the lstat record holds every entry; the listing comparison ignores a
  newer directory time and reports a changed mtime.
- Docs: none.
- Size: M.

#### I13 Extraction into staging and name checks

- Goal: extract safely and refuse names the target cannot hold.
- Implements: ADR 0027 (a member path leading outside the piece is
  refused; unearthing never writes through a symlink), ADR 0034, ADR
  0050 (entry by entry with antiq's checks before the crate's, stop at
  the first refused member, directory modes and times in a final pass),
  ADR 0054 (check every member name before extracting: names the target
  cannot hold, name and path lengths, collisions under the target's
  case and Unicode rules from a probe of the staging volume; refuse
  with the list and the reason; at archive time warn for macOS and
  Linux rules), spec "unearth" (paths longer than the platform limit
  extracted directory by directory), tar research (owner `rwx` on
  directories until the final pass), names research.
- Owns: `src/pack/extract.rs`, `src/pack/names.rs`; adds
  `unicode-normalization` if T11 says so.
- Must not touch: the strict verify (I12).
- Depends on: I06.
- Tests: the six hostile archives of the tar research (absolute path,
  `../`, symlink then child, symlink to `/etc`, symlink then directory,
  hard link outside) are refused or stopped with nothing written
  outside staging (first failure: stub); a round trip keeps file,
  symlink and directory modes and times; a `0644` directory extracts its
  children; `README` and `Readme` in one directory are refused on the
  case-insensitive temp volume with both names listed; `café` in NFC
  and NFD likewise; a 256-byte name is reported for Linux at archive
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
- Owns: `src/pack/live.rs`, `src/pack/manifest.rs` (stored in
  `.antiq/tracking/`, file name per T5).
- Must not touch: `sync` (I32 drives it).
- Depends on: I05, I06.
- Tests: the golden hash for a fixed generated input (first failure:
  stub); one worker and four workers give the same bytes; a seam that
  changes a file between read and second lstat makes the packer
  restart, and after `live_retries` the piece is stale; a vanished file
  is dropped; the manifest diff reports a touched file and a new
  directory; repacking unchanged content reports the same SHA-256; the
  encoder record names zstd 1.5.7 and the parameters.
- Docs: spec "Change detection" if T5 places the manifest.
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
  follows U1.
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
  warning for a collection on SMB or NFS), "JSON output" (`status`
  payload: `collection`, `remote.state`, `pieces` with `id`, `path`,
  `name`, `state`, `flags`, `remote_only_in_days`), ADR 0040.
- Owns: `src/ops/status.rs`, `src/cli/cmd/status.rs`, `tests/status.rs`,
  README (first milestone).
- Content: every flag computed from the fields I08 and I09 define, so
  later items only set fields; `remote.state` through the I01 stub until
  I23 fills it; the away warning through the I01 stub until I29.
- Must not touch: data producers.
- Depends on: I04, I05, I08, I09, I10.
- Tests: a fixture with new, pinned, empty, unreadable and future-time
  pieces gives the expected JSON (first failure: `not_implemented`);
  fixture tracking and metadata give archived, remote-only with days,
  gone, live on another machine, superseded, stale, needing a decision,
  missing on the remote, recovered; a held lock prints the note; a
  console snapshot.
- Docs: spec "JSON output" if field details are added; README: `init`
  and `status` work.
- Size: L.

#### I17 Run guard, start-of-run repairs and dry-run plans

- Goal: the frame every state-changing command runs in.
- Implements: spec "Operations" (lock with pid, machine and start time;
  held lock exits 1 naming pid and start time; right after the lock:
  empty `trash/`, `tmp/`, `staging/` with one retry adding owner write,
  warn and continue; finish `removing` entries; an `unearthing` entry
  whose folder matches the version's listing becomes `live` with the
  entry's write time as unearthing time, any other `unearthing` entry
  is dropped and an unrecognised folder becomes a new piece reported
  once; a present folder of an ID this machine committed as archived is
  unearthed at that version, now; `recovered/` never emptied; read-only
  commands and `--dry-run` never touch these directories), ADR 0046
  (plan rendering, JSON `dry_run` and `actions`).
- Owns: `src/run.rs`, `src/repair.rs`, `src/plan.rs` (extends I01's
  type with rendering helpers).
- Must not touch: command files.
- Depends on: I03, I08, I09, I12.
- Tests: a held lock gives `lock_held` with the pid (first failure:
  stub); a leftover file in `trash/` is gone after `RunGuard::begin`; an
  entry that resists removal (seam) gives a warning and the run goes
  on; a `removing` entry is finished; an `unearthing` entry with a
  matching folder becomes `live` with the entry's time; one with a
  differing folder is dropped and the folder reported as new; a folder
  of an own archived ID becomes unearthed; a dry-run guard touches
  nothing.
- Docs: none.
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
  `abs_path`, `name`, `state`, `created`, `size`), "Pins" (`pin` and
  `unpin` refuse on a piece whose folder is not present; no metadata
  change), "reconcile" (rebuilds the index, does the start-of-run
  repairs; the remote part is I35).
- Owns: `src/ops/{find,pin,reconcile}.rs`,
  `src/cli/cmd/{find,pin,unpin,reconcile}.rs`,
  `tests/{find,pin,reconcile}.rs`.
- Content: interactive `find` prints the list until I36 lands.
- Depends on: I10, I15, I17.
- Tests: `find --json` lists objects with full ULIDs (first failure:
  `not_implemented`); a query narrows the list per U1; `find --print`
  without a terminal prints one absolute path per match, for an
  archived piece its stored path; `pin` creates `.antiq-pin` and a
  second run reports it already pinned; `pin` on an archived piece
  refuses; `reconcile` rebuilds a deleted index; `reconcile --dry-run`
  prints the plan.
- Docs: spec "JSON output" (`pin`, `unpin`, `reconcile`).
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
- Content: parallel pieces through the I07 pool; the proof tokens of
  section 2.8 if T22 is accepted; the diverged refusal hook, a check of
  the tracking flag that I31 sets.
- Must not touch: unearthed pieces (I26 adds `unearthed.rs` and its
  routing).
- Depends on: I02, I06, I07, I08, I09, I10, I12, I13, I15, I17.
- Tests: an idle piece ends as payload, listing and pending metadata
  with lifecycle `archived`, and its folder is gone (first failure:
  `not_implemented`); a verify difference (seam) keeps the folder; a
  write after the verify, detected in the trash, renames the folder
  back as unearthed; the same with its path taken lands in
  `recovered/`; too little space (seam) skips with exit 3; pinned,
  empty, unreadable and mount-point pieces stay; `--dry-run` lists the
  pieces and changes nothing; a name colliding on macOS rules gives a
  warning and archives; two pieces run in parallel.
- Docs: spec "JSON output" (`archive`).
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
- Content: an archived ID without a local archive fails with a clear
  error until I34 adds fetching.
- Depends on: I08, I09, I12, I13, I15, I17.
- Tests: archive then unearth restores the tree with modes, times,
  symlinks and hard links, and tracking is `live` with the unearthing
  time (first failure: `not_implemented`); `--version 1` picks the
  first payload; `--to` outside the patterns refuses; a taken target
  refuses; a staged-tree difference (seam) moves nothing; a colliding
  name list refuses before extraction; unearthing twice reports the
  location.
- Docs: spec "JSON output" (`unearth`).
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
- Content: frame walking over headers and block headers, so every
  frame's window and checksum flag are checked without holding a frame
  in memory; `std::fs::copy` clones on macOS through `fclonefileat`
  with an `fcopyfile` fallback (std source, `sys/fs/unix.rs`).
- Depends on: I06, I08, I12, I13, I15, I17.
- Tests: a zstd tar made with the crate imports and `find` shows it
  (first failure: `not_implemented`); xz and gzip magic refuse naming
  the codec; window log 28 refuses naming `zstd -19 --long=27`; no
  checksum refuses; two top-level folders refuse; `../x` refuses; a
  second import of the same file is skipped; the input file is
  unchanged afterwards; a taken path warns and both objects are found.
- Docs: spec "JSON output" (`import`).
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
- Content: a `fork_unearthed()` core that I26 (answer `--fork`), I31
  and I34 call.
- Depends on: I15, I17, I20.
- Tests: fork of an unearthed piece gives a new ID with source `fork`
  and the old unearthed time (first failure: `not_implemented`); fork
  of a live piece refuses; `--name` renames; a taken name refuses;
  unearthing the old ID without `--to` refuses on the taken path.
- Docs: spec "JSON output" (`fork`).
- Size: S.

#### I23 Remote trait, path remote, marker and the `remote` commands

- Goal: attach a path remote and offer the trait every later item uses.
- Implements: ADR 0015, 0016 (attaching copies the remote's metadata;
  archives fetched on demand), 0026, 0031, 0047; spec "Remotes" (URL
  forms `ssh://[user@]host[:port]/<path>` and an absolute local path;
  root path only `[A-Za-z0-9._/-]`; `remote add` refuses while a remote
  is attached and when a probe fails, writing nothing; `remote show`
  prints kind, URL, ULID, reachability and probed versions; `remote
  remove` detaches only and lists the remote-only pieces that become
  unreachable, the brake follows in I35; a path remote whose root is
  not a mount point gets a warning), "Path remote" (plain copy with
  `sync_all` and the `fsync` fallback for file and directory;
  reachability mapping: missing root, or a root that is not a mount
  point and has no marker, is away; another ULID, a mount point without
  marker or an I/O error on the marker is broken), "Storage" (remote
  layout; temp names; put onto an existing name only with a matching
  SHA-256), "Committing a version" (path remote clock from a probe
  file's modification time).
- Owns: `src/remote/{mod,url,marker,path}.rs`, `src/ops/remote.rs`,
  `src/cli/cmd/remote.rs`, `tests/remote_path.rs`; fills the
  `remote::probe_state` stub, so `status` shows `remote.state`.
- Depends on: I03, I04, I08, I15, I17.
- Tests: `remote add --init <dir>` writes the marker and the
  `[[remote]]` entry (first failure: `not_implemented`); `remote add`
  on a directory without marker refuses and writes nothing; a second
  `remote add` refuses; marker with another ULID gives broken; missing
  root gives away; the mount seam turns a missing marker into broken;
  attaching copies the remote's metadata into the store; `remote show
  --json`; `remote remove` detaches and keeps local metadata; payload
  put under an existing name with different bytes fails.
- Docs: spec "JSON output" (`remote add`, `show`, `remove`).
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
  same command), ssh research (option A; `sun_path` of 104 bytes).
- Owns: `src/remote/ssh/{session,script,classify}.rs`,
  `tests/fixtures/ssh-shim.sh`, `tests/ssh_session.rs`,
  `tests/support/sshd.rs` (opt-in harness of 4.4).
- Depends on: I01, I04.
- Tests: argv carries every `-o` option (first failure: stub); the
  ControlPath stays under 104 bytes; quoting survives `'`, spaces and
  newlines; the classifier maps each recorded stderr text (ssh
  research list) per T14; a stale socket of a dead pid is removed;
  through the shim, a master starts, `-O check` passes, a script runs
  with arguments, `set -C` refuses a second create; opt-in: the same
  against the real sshd.
- Docs: none.
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
- Owns: `src/ops/archive/unearthed.rs` and its routing line in
  `src/ops/archive/mod.rs`, `src/ops/resolve/{mod,replace}.rs`,
  `src/cli/cmd/resolve.rs`, `tests/{rearchive,resolve}.rs`.
- Depends on: I12, I19, I20, I22.
- Tests: an unearthed, unchanged piece is reclaimed with no new payload
  (first failure: routed to "new piece"); a reclaim without a usable
  archive copy is pending remote work; an added file replaces without
  asking; a removed file asks, and a scripted "fork" forks; without a
  terminal the piece is listed as needing a decision and the run exits
  3; `resolve <piece> --replace` replaces with the loss at that moment;
  `--mine` on this decision exits 2; `replace_without_asking` skips the
  question; a diverged piece is refused.
- Docs: spec "JSON output" (`resolve`).
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
- Depends on: I08, I23.
- Tests (with `FaultRemote`): a commit writes `meta.1` (first failure:
  stub); another machine's claim between steps gives a conflict; a
  higher version appearing after the rename gives a conflict; a claim
  older than the grace period is skipped and the next version uses
  N+2; an unreferenced payload older than the grace period is removed,
  a younger one stays, a superseded one stays; a directory with a
  `format: 2` file is left alone and reported; retention per T4.
- Docs: spec "Committing a version" for T4 and T6 outcomes.
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
- Owns: `src/remote/ssh/mod.rs`, `tests/remote_ssh.rs`; the SSH branch
  in `src/ops/remote.rs`.
- Depends on: I23, I24, I25.
- Tests (loopback shim with real rsync): `remote add --init
  ssh://...` probes and writes config (first failure: unsupported URL);
  openrsync as remote rsync refuses with the reason; a missing hash tool
  refuses; a put, a get, an interrupted put resumed from
  `.antiq-partial`; a seeded live upload leaves the old name's bytes
  unchanged; the size check catches a short file; the meta rename
  refuses an existing target; opt-in: the same against the real sshd.
- Docs: none.
- Size: L.

#### I29 Reachability policy: `--if-reachable`, the away warning and the network check

- Implements: spec "Remotes" (away longer than `away_warn_days` with
  remote work pending: `status` warns and `--if-reachable` no longer
  turns away into exit 0), "Dry runs, scheduling, exit codes"
  (`--if-reachable` for scheduled runs; exit 4), "Config"
  (`machine.network_check`: outside the home network the remote is
  away).
- Owns: `src/remote/{policy,away,network}.rs`; fills the
  `remote::away::status_warning` stub; the time the remote was last
  reachable is kept in a local file named per T24.
- Depends on: I23; the network check waits on U6.
- Tests: away with `--if-reachable` gives exit 0, without it exit 4
  (first failure: stub); away for eight days with pending work gives
  exit 4 under `--if-reachable` and a `status` warning; broken stays
  exit 1; the network seam reports "outside" and the remote is away
  without a connection attempt.
- Docs: the local layout in spec "Storage" if T24 adds a file.
- Size: M.

#### I36 The picker for `find`

- Goal: interactive `find` with every action except the contents view.
- Implements: ADR 0021, 0057 (start as a copy of ntropy's picker;
  unearthing on Enter, printing the path with `--print`, forking,
  pinning or unpinning), ADR 0020 and 0056, spec "Command line" (`find`
  opens a picker interactively).
- Owns: `src/cli/picker/{mod,state,layout}.rs`, the interactive branch
  of `src/cli/cmd/find.rs`, `Cargo.toml` (crossterm, nucleo,
  unicode-width, unicode-segmentation), `tests/picker.rs` if the pty
  harness is built.
- Content: copy `src/bin/ntropy/run/picker/` (ntropy is MPL-2.0 like
  antiq; the header stays): `mod.rs` (982 lines), `state.rs` (1,117) and
  `layout.rs` (1,014) including tests. The ties to ntropy are
  `layout.rs` (ntropy's `Candidate`) and the `open_tty()` call, both
  replaced; actions through ntropy's `AcceptKey` mechanism with the keys
  of U3; after the pick, the action runs through the same `ops`
  functions as the commands.
- Depends on: I18, I20, I22.
- Tests: ntropy's state tests carried over (first failure: module
  absent); layout snapshots for archived, live and remote-only rows; a
  pick with the print action prints the absolute path; the pty smoke
  test per T16.
- Docs: none beyond the README at I39.
- Size: L.

### Wave 8

#### I30 `sync`: pull, upload and commit

- Goal: exchange metadata and archives with the remote.
- Implements: spec "sync" (pull metadata first; upload pending
  archives; commit recorded metadata changes; at most `remote_jobs`
  remote operations at a time; compression may run ahead of uploads),
  "Several machines" (`sync` pulls metadata first; the machine whose
  version lost learns it at its next sync), "Objects" (`sync` mints IDs
  for new pieces), "Metadata changes" (`sync` row: `verified` times,
  history), "Remotes" (every operation checks the marker), ADR 0016,
  0017, 0032, 0030 (through I27).
- Owns: `src/ops/sync/{mod,pull,upload,commit}.rs`, stubs
  `src/ops/sync/{conflict,live,tiers}.rs` with their call sites (I31,
  I32, I33 fill them), `src/cli/cmd/sync.rs`, `tests/sync.rs`, README
  (second milestone).
- Content: a version conflict is recorded as a pending decision through
  the `conflict` stub and the piece is skipped (exit 3) until I31.
- Depends on: I19, I21, I27, I29.
- Tests (path remote, two machines): an archive made on A is uploaded
  and committed as `meta.1`, the pending file promoted (first failure:
  `not_implemented`); B pulls and `find` on B lists it; an import is
  uploaded; a failed upload (fault) leaves the piece pending and exits
  3; `--dry-run` reads the remote and writes nothing on either side;
  away gives exit 4, or 0 with `--if-reachable`; at most `remote_jobs`
  operations run at once (fault remote counter).
- Docs: spec "JSON output" (`sync`); README: archive, unearth, find,
  import and sync against a path remote work.
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
- Owns: `src/ops/unearth/{remote,live}.rs`, source selection in
  `src/ops/unearth/mod.rs`, `tests/unearth_remote.rs`.
- Depends on: I20, I22, I23, I29.
- Tests: a remote-only piece is fetched and unearthed (first failure:
  "no local archive"); a newer remote version is reported; with the
  remote away a local archive still unearths with a warning, and a
  remote-only piece exits 4; a gone piece comes back from its live
  package; another collection's live piece unearths only as a fork with
  a new ID; a stale package warns.
- Docs: spec "JSON output" (`unearth` additions).
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
  (`status` and `reconcile` report an unparsable highest `meta.<N>`),
  "JSON output" (error kind `brake` with the planned count, set size and
  up to ten items).
- Owns: `src/brake.rs`, the remote part of `src/ops/reconcile.rs`, the
  brake in `src/ops/remote.rs` (`remote remove`), `tests/{brake,reconcile_remote}.rs`.
- Depends on: I18, I23, I27.
- Tests: 3 of 10 planned removals with the default 20 % hold the brake
  and list them (first failure: stub); `--force` passes; a scripted
  "yes" passes; `--dry-run` reports the brake; an archive missing on
  the remote marks the entry missing and `--prune` drops it; a pending
  upload is never pruned; `remote remove` with remote-only pieces goes
  through the brake; a commit by another machine is reported.
- Docs: spec "JSON output" (`reconcile` additions).
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
  `tests/sync_conflict.rs`.
- Depends on: I26, I30.
- Tests (two machines): both archive the same unearthed ID, the second
  sync gets a conflict (first failure: recorded only); "mine" writes a
  new version with theirs superseded; "theirs" sends the local archive
  through trash and tracking follows; "fork" makes the local archive
  version 1 of a new ID; without a terminal the decision is pending and
  `resolve <piece> --theirs` executes it; a remote version from another
  machine marks the unearthed folder diverged and `archive` refuses it.
- Docs: none beyond JSON additions.
- Size: L.

#### I32 Live backups in `sync`

- Implements: ADR 0018, 0047 (full upload on a path remote), 0049
  (seeding); spec "sync" (refresh the live packages of changed pieces;
  delete a live package only after its archive's metadata is committed
  on the remote), "Change detection", "Verifying" (tolerant verify,
  stale), "States" (gone piece keeps its live package), "Pins" (pinned
  pieces still backed up), `collection.backup_live`,
  `collection.live_exclude`, "Metadata changes" (`sync` row: live
  payload replaced).
- Owns: `src/ops/sync/live.rs`, `tests/sync_live.rs`.
- Depends on: I14, I30.
- Tests: a live piece gets an ID and a live package (first failure:
  stub phase); an unchanged piece transfers nothing; a touched file
  with the same content only replaces the manifest; a changed piece
  uploads a new package, through the seed on the SSH shim; a file
  changing during reads ends as "backup stale since"; the live package
  of an archived piece is deleted only after the archive's commit;
  `backup_live = false` uploads nothing; the first sync after
  unearthing packs once.
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
- Owns: `src/ops/sync/tiers.rs`, `tests/sync_tiers.rs`.
- Depends on: I30.
- Tests: with `remote_only_days = 0` a committed, confirmed archive
  leaves local storage through the trash and `verified` is set (first
  failure: stub phase); a checksum mismatch keeps it; a collection
  without a remote keeps it; an unreferenced local archive is never
  dropped; a newer remote archive within the local tier is fetched;
  `status` shows "remote-only in N days".
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
- Depends on: I31, I34, I35.
- Tests: `--theirs` on a diverged folder without a terminal and without
  `--force` refuses (first failure: unsupported answer); with `--force`
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
  unknown format), `src/remote/mod.rs` (opening by URL form), new ADRs
  through `adrs new --no-edit`.
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
  Linux container, including the non-UTF-8 name tests.
- Depends on: every item except I40.
- Docs: README (what v1 does, how to set up a remote), spec status line
  ("Draft") per the lead.
- Size: M.

### Waiting on the user

#### I40 The picker's contents view

- Blocked by the needs-discussion todo
  `01M6ESY4GEGYFC37C81N3PW6XG-design-how-the-picker-shows-a-pieces-contents.md`.
  ADR 0057 leaves open how the contents are shown and does not assume a
  plain preview pane; the user named an overlay or a tree integrated
  into the picker as directions. The item starts after that decision
  and after I36.
- Depends on: I36, the user's design decision (U4).

## 6. Dependency graph and waves

| Item | Depends on | Wave |
|---|---|---|
| I01 | none | 1 |
| I02, I03, I04, I05, I06, I07 | I01 | 2 |
| I08, I09 | I02, I03 | 3 |
| I10 | I04, I05 | 3 |
| I11 | I02, I03, I04, I05 | 3 |
| I12 | I05, I06 | 3 |
| I13, I14 | I06 (I14 also I05) | 3 |
| I15 | I08, I09 | 4 |
| I16 | I04, I05, I08, I09, I10 | 4 |
| I17 | I03, I08, I09, I12 | 4 |
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
| I32 | I14, I30 | 9 |
| I33 | I30 | 9 |
| I37 | I31, I34, I35 | 10 |
| I38 | I05, I06, I23, I28; after wave 9 | 10 |
| I39 | all but I40 | 11 |
| I40 | I36, U4 | after the decision |

Waves and the files that keep them apart:

| Wave | Items | Shared files and how they stay apart |
|---|---|---|
| 1 | I01 | creates every file |
| 2 | I02 to I07 | disjoint modules |
| 3 | I08 to I14 | disjoint modules; only I13 may edit `Cargo.toml` |
| 4 | I15, I16, I17 | disjoint; README only in I16 |
| 5 | I18 to I21 | own `ops` and `cli::cmd` files; spec JSON placeholders per command |
| 6 | I22 to I25 | disjoint; I23 fills the `remote::probe_state` stub, no edit of `status.rs` |
| 7 | I26 to I29, I36 | I26 owns `ops/archive/mod.rs` routing; I28 the SSH branch of `ops/remote.rs`; I36 the only `Cargo.toml` editor |
| 8 | I30, I34, I35 | `ops/sync/*` to I30, `ops/unearth/*` to I34, `ops/reconcile.rs` and `ops/remote.rs` to I35 |
| 9 | I31, I32, I33 | one sync phase file each, behind I30's stubs |
| 10 | I37, I38 | I37 in `ops/resolve/`, I38 in `select.rs`, `pack/mod.rs`, `remote/mod.rs` |
| 11 | I39 | |

The longest chain is I01, I03, I08, I17, I23, I27, I30, I31, I37,
I39; with I23 placed in wave 6 it gives the 11 waves. I24 and I25
depend only on I01 and I04 and could start from wave 3 if an agent is
free; I23 could start in wave 5. Both moves pull extension-point work
ahead of the local business logic, against the agreed order, so the
plan keeps them in wave 6.

## 7. Risks

- R1 ADR window. The first code that lands closes the in-place rewrite
  of ADRs. Text fixes worth making before I01 merges: ADR 0045 ends with
  "No archive on the remote is deleted except when a newer, verified and
  committed version of the same ID replaces it", while its own decision
  and spec "Safety" also allow the explicit drop of a superseded
  version; ADR 0039 names the sources `birth`, `oldest-mtime` and
  `import`, while the spec adds `fork`. Recommendation: fix both in
  place before I01 merges.
- R2 Gaps that block items. T1 blocks I08; T3 blocks I06; T10 blocks
  I05; U1 blocks the search part of I15 and I18; U5 touches I11; U6
  blocks the network check in I29. Recommendation: settle them while
  wave 1 runs.
- R3 Long paths. On macOS, 44 members of a 1,300-byte path failed past
  `PATH_MAX` of 1,024 with the `tar` crate (names research), and spec
  "unearth" extracts such paths directory by directory. The walk, the
  strict verify and the delete helper read the same trees, so they also
  need dirfd-relative calls (rustix `fs`) instead of full paths.
  Recommendation: build that into I05 and I03 from the start, with a
  deep-path test in each.
- R4 Terminal in tests. Measured: the agent sandbox has no controlling
  terminal. A binary test that spawns antiq without `setsid()` passes
  here and blocks on a prompt when the user runs `cargo test` in a
  terminal. Recommendation: the `tests/support` command builder always
  detaches (I01), and no test spawns the binary another way.
- R5 Slow tests. Level 19 with a 128 MiB window took 17.7 s for a 150 MB
  input (compression research). Recommendation: fixtures of kilobytes;
  any test above a few seconds gets the opt-in mark.
- R6 zstd pin. The golden-hash test of ADR 0052 fails when the bundled
  zstd changes; `cargo update` cannot move zstd-sys past the exact pin
  `=2.1.2+zstd.1.5.7`, so only a deliberate change of the pin does.
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
  The commit protocol leans on exclusive create (ADR 0030).
- R9 Size of the `sync` items. I30 and I31 sit at the top of L.
  Recommendation: the lead splits I30 at item-plan time if its plan
  exceeds about 1,500 lines (for example uploads apart from pull and
  commit).
- R10 Parallel merges of docs. Spec and README edits collide when
  items merge one after another. The per-command placeholders and the
  README milestones reduce this; the lead rebases trivial conflicts.
- R11 Static Linux builds. Under musl, std's `created()` returns
  `Unsupported`; `rustix::fs::statx` gives the birth time (filesystem
  research, Linux). Only relevant if a static build is made.

## 8. Open technical questions for the lead

Each with a recommendation.

- T1 Where a recorded, uncommitted metadata change lives (section 2.9).
  Recommendation: `objects/<ulid>/meta.pending`, promoted to
  `meta.<N+1>` after the commit; an ADR from I08.
- T2 ID time when every time is before 1990 or none is available. The
  spec and ADR 0039 give no fallback and none of the four sources fits.
  Recommendation: the time of minting, with a warning, and a decision
  on the source value (possibly a new value, which changes the spec's
  `created_source` list).
- T3 Member layout of antiq's own archives: whether members carry the
  piece's folder name as one top-level directory. Imported archives
  have exactly one top-level folder (ADR 0055), and the 313 `archive.sh`
  archives each have one named after the unit (tar research). The spec
  does not say for archives `archive` makes. Recommendation: one
  top-level directory named after the folder, so both kinds share one
  shape under `tar+zstd/1`; recorded in spec "Packaging".
- T4 Retention of old `meta.<N>` files and of listings. ADR 0030 removes
  "old metadata files" after the grace period; the spec's cleanup list
  does not say which versions are retained. Recommendation: keep the
  highest committed version; remove older ones once a higher version
  exists and they are older than the grace period; listings follow the
  payload they describe. Also the listing file name prefix:
  recommendation, the SHA-256 prefix of its payload, so they pair. And
  the listing's zstd settings, which the spec does not give:
  recommendation, level 3 with the content checksum, read through the
  decoder with the 128 MiB window limit.
- T5 Where the live manifest is stored. Spec layout: `tracking/<ulid>.json`
  holds "tracking entries, and live manifests". Recommendation: a
  separate `tracking/<ulid>.manifest.json`, so a large manifest does not
  rewrite with every tracking change.
- T6 The archive's commit time for the remote-only tier (ADR 0042). The
  metadata tables have no commit time. Recommendation: the history entry
  written with the committing version carries the commit time; the
  tier reads it from there.
- T7 `--dry-run` and the lock, and `--dry-run` of `sync`. The spec says
  read-only commands take no lock and `--dry-run` never touches the
  run directories, not whether a dry run takes the lock.
  Recommendation: no lock, the same note as `status` when one is held;
  a dry `sync` reads the remote but writes nothing on either side.
- T8 Usage errors under `--json`. clap prints text and exits 2.
  Recommendation: when `--json` is in the arguments, print the JSON
  envelope with kind `usage` and exit 2.
- T9 The channel of JSON errors. Recommendation: stdout, as data, since
  the envelope is the command's output; messages in plain text to
  stderr.
- T10 `collection.exclude` semantics. The spec calls them "paths that
  are never pieces" and `init` proposes excluding `archive/`.
  Recommendation: literal relative paths; nothing below an excluded path
  is a piece.
- T11 Unicode collisions (ADR 0054). The names research left the volume
  probe unbuilt and no crate was researched for normalisation or case
  folding. Recommendation: probe the staging volume for
  case-insensitivity and normalisation-insensitivity with two test
  files each, then compare names folded with `unicode-normalization`
  (NFD) and `str::to_lowercase`, leaving the strict verify as the
  backstop the ADR names.
- T12 RAM size for the memory budget, not researched. Recommendation:
  `libc::sysconf(_SC_PHYS_PAGES)` times the page size, present on macOS
  (libc 0.2.191 defines `_SC_PHYS_PAGES` for Apple) and Linux.
- T13 Read-only commands with a missing or stale index, and the lock
  note. Recommendation: compute in memory without writing; detect a
  running run by reading the lock file and testing the pid, since a
  `try_lock` probe would take the lock for a moment and make a starting
  run fail.
- T14 Classifying "Connection refused". ssh research requirement 6
  makes away "no route, timeout" and broken "answers but refuses". A
  refused TCP connection fits neither cleanly. Recommendation: away, so
  a NAS whose sshd is still starting does not fail scheduled runs.
- T15 Timestamp format. jiff writes as many fractional digits as the
  value needs (foundations §5). Recommendation: a custom serializer
  with whole seconds for metadata and history times, which matches the
  listing's whole seconds; ID times keep milliseconds inside the ULID.
- T16 Coverage of terminal glue (raw mode, `/dev/tty`). Recommendation:
  a small pty harness with rustix's `pty` functions in I36 for one
  picker run and one prompt; if it turns out fragile, the remaining
  lines are listed as accepted exceptions.
- T17 Seams versus opt-in tests for devices, free space and `ENOTSUP`.
  Recommendation: seams for `cargo test`; an opt-in macOS test with a
  disk image (`hdiutil`) for the device boundary only if the lead wants
  one.
- T18 `tar` without its default `xattr` feature. antiq writes no
  extended attributes (ADR 0050). Recommendation: `default-features =
  false` if it builds and the tests pass.
- T19 ULID text case. The crate writes upper case; the todo vault's file
  names use upper case. Recommendation: upper case in metadata, file
  names and JSON; references accept either.
- T20 Where the binary's modules live. ADR 0003 and `CLAUDE.md` name
  `src/main.rs`. Recommendation: keep `src/main.rs` and put its modules
  under `src/cli/`, with no module of that name in the library.
- T21 One generic `Question` type in the library instead of one trait
  method per decision. Recommendation: accept (section 2.6).
- T22 Proof tokens for removing a folder and dropping a local archive
  (section 2.8). Recommendation: accept, with `compile_fail` doctests.
- T23 SSH test layers (section 4.4). Recommendation: loopback shim in
  `cargo test`, real sshd and Linux container opt-in, both run before
  v1 is called done (I39).
- T24 Where the time the remote was last reachable is kept, for
  `away_warn_days`. Recommendation: `.antiq/remote-state.json` through
  the atomic write, added to the spec's local layout.
- T25 `resolve <piece>` without an answer flag. ADR 0025 and the spec
  define `resolve` alone and `resolve <piece> <answer>`. Recommendation:
  the interactive walk restricted to that piece; without a terminal it
  lists that piece's decisions and exits 3. This edges into behaviour;
  the lead may pass it to the user.

## 9. Questions for the user

Behaviour, look and taste that the spec and ADRs leave open. Each with
a recommendation.

- U1 What `find <query>` matches, and what it lists. The spec says
  `find` "lists and searches all objects" and the scope names an index
  of each archive's contents. Open: whether the query matches the
  name and path only or also member paths in listings; fuzzy (as the
  picker) or substring; whether new pieces without an ID appear; and
  which size the JSON `size` field carries. Recommendation: name and
  path, matched the same way as the picker; objects only; `size` the
  current payload's size; contents search together with the contents
  view (U4).
- U2 Console look of `status`, `archive`, `sync` and the other commands,
  including progress during long runs. Recommendation: plain grouped
  text first (the rudimentary CLI), reviewed once real output exists.
- U3 Picker keys and flow. Which keys trigger fork, pin or unpin, print
  and the contents view (ntropy reserves Ctrl with c, d, n, p, u and w);
  whether the picker closes after each action, as ntropy's does; whether
  forking from the picker asks for a new name. Recommendation: ntropy's
  accept keys, every action ends the picker, fork keeps the name.
- U4 The contents view of the picker (todo
  `01M6ESY4GEGYFC37C81N3PW6XG`, needs-discussion). Blocks I40 only.
- U5 Small command-line gaps:
  - `init` where `.antiq/` already exists, or inside a collection.
    Recommendation: refuse, naming the existing collection's root.
  - `init --yes` and the proposed `archive/` exclude. Recommendation:
    `--yes` accepts the proposal as part of the plan.
  - `sync`, `remote show`, `reconcile` and `--if-reachable` with no
    remote attached. Recommendation: `sync` fails with exit 1 and a
    `no_remote` kind; `remote show` reports none and exits 0;
    `reconcile` does its local part.
  - The exit code after an interactive "skip". Recommendation: 3, since
    the table's 3 means "some objects skipped"; nothing stays pending.
  - The exit code of `status` when it shows problems. Recommendation: 0
    unless the command itself fails.
- U6 What `machine.network_check` compares. The spec says "router
  address of the home network". Open: the default gateway's IP address
  or its hardware (MAC) address; private gateway IPs such as
  `192.168.1.1` repeat across networks. Recommendation: the gateway's
  MAC address. Blocks the network check in I29.

## 10. Coverage of the spec and the ADRs

Spec sections:

| Spec section | Items |
|---|---|
| Purpose, Scope | all; checked end to end in I39 |
| Scope, not in version 1 | no item; I04 refuses a second remote; I05 has one strategy |
| Collections and pieces (marker, discovery, `-C`) | I04 |
| Collections and pieces (`init`) | I11 |
| Selection | I05; counts shown by I16 |
| Walks | I05; delete helper I03 |
| Idle | I10 |
| Pins | I10; commands I18 |
| Objects (IDs, ID time, ownership) | I02; minting in I19, I21, I22, I30; ownership in I09, I19, I32, I34 |
| Versions | I08 (payload version), I27 (metadata version) |
| Metadata | I08 |
| Listing | I08 (format), I06 (generation), I12 (comparison) |
| Local state | I09 |
| States | I09 (derivation), I16 (display) |
| Metadata changes | I19 (`archive`), I26 (reclaiming), I20 (`unearth`), I22 (`fork`), I30 to I33 (`sync`), I21 (`import`), I26, I31, I37 (`resolve`); `pin` none (I18) |
| Storage (local layout, remote layout, payload names, atomic write) | I01, I04, I11 (local layout), I23 (remote layout), I02 and I08 (names), I03 (atomic write) |
| Storage (index) | I15 |
| Config | I04; written by I11, I23, I25 |
| Committing a version | I27 |
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
| fork | I22; fork while unearthing I34 |
| import | I21 |
| reconcile | I18 (local), I35 (remote) |
| status | I16 |
| Safety | I35; exemptions in I19, I32, I33 |
| Command line (table, flags, references) | I01, I15 |
| Command line (console, interactive rule, `find`, picker) | I01, I18, I36, I40 |
| Decisions | I26 (framework, replacing with loss), I31 (version conflict), I37 (diverged, superseded) |
| Dry runs, scheduling, exit codes | I01 (codes), I17 (plans), I29 (`--if-reachable`) |
| JSON output | I01 (envelope, errors), I16 (`status`), I18 (`find`), each command item for its payload |

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
| 0039 ID time from oldest birth or modification time | I02 |
| 0040 Other machines' live pieces, fork only | I16, I19, I32, I34 |
| 0041 Nothing outside pieces backed up | non-goal; selection (I05) never makes loose files a piece |
| 0042 Remote-only tier from the commit | I33 |
| 0043 Local archive deleted after remote confirmation | I33 |
| 0044 Missing entries kept until `reconcile --prune` | I35 |
| 0045 The 20 % brake | I35; exemptions I19, I32, I33 |
| 0046 `--dry-run` everywhere | I01, I17, every state-changing item |
| 0047 Path remote copies plainly; SSH first | I23 |
| 0048 ssh executable with a master | I24, I28 |
| 0049 rsync 3.1 or later | I25, I28 |
| 0050 Tar in process with hand-built headers | I06, I13 |
| 0051 zstd 19, 128 MiB window | I06, I07 |
| 0052 Live packages at a pinned zstd | I14 |
| 0053 Versioned JSON metadata keeping unknown fields | I08 |
| 0054 Name checks before unearthing, warnings when archiving | I13, I19, I20, I21 |
| 0055 `import` | I21 |
| 0056 Console output and the picker | I01, I36 |
| 0057 Picker from ntropy's picker | I36, I40 |

## Relations

- Relates to: [Define the v1 scope and architecture of antiq](01M6E17PET32XD9PC04AJ2PBZY-define-the-v1-scope-and-architecture-of-antiq.md)
