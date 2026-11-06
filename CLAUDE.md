# antiq

Your idle folders, kept like antiques. One Rust
crate with a library (`src/lib.rs`) and a command-line binary
(`src/main.rs`). Version 1 is implemented and not yet released; the
specification is `docs/specs/spec-v1.md`. Architecture decisions live
in `docs/adrs/`; deferred work and open design questions live in the
todo vault at `todos/`.

## Working on the code

- The development tasks are the recipes of the `Justfile`; use them
  (`just --list`). Every commit passes `just check` (formatting, clippy
  with warnings as errors, the tests).
- Tests: unit tests sit in their module; each command has a
  `tests/<command>.rs` that runs the binary; whole user tasks are
  scenarios in `tests/scenarios/`, run in text and in JSON mode (how to
  write one: `tests/support/scenario/mod.rs`).
- Tests start the antiq binary only through `tests/support/cmd.rs`. It
  holds a lock around every spawn, because a `pre_exec` child forked
  while another test thread initialises a macOS system library aborts
  before `exec` and shows up as a SIGKILL.
- A command error that wraps a lock, run or reference error uses
  `#[from]` or `#[source]`, never `#[error(transparent)]`: the JSON
  output finds `lock_held` and `candidates` by walking `source()`, and a
  transparent wrapper hides them.

## Licensing

antiq is licensed under the **Mozilla Public License 2.0**
(`LICENSE`). `Cargo.toml` declares it as `license = "MPL-2.0"`.

**Every source file must begin with the MPL-2.0 header comment.** This
covers Rust sources, tests, examples, build scripts and shell scripts.
For Rust files, use line comments:

```rust
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
```

Adapt the comment syntax to the file's language, keeping the wording
verbatim. In a script, the header goes directly below the shebang line.

Manifests and configuration files (`Cargo.toml`, `Cargo.lock`, CI
workflows), Markdown documents and the todo notes carry no header.

Third-party files copied into the repository keep their own license and
header and are listed here as exceptions, together with where their
license text lives.

- `assets/social/fonts/Inter-Variable.ttf`: Inter, SIL Open Font License
  1.1, text in `assets/social/fonts/Inter-OFL.txt`.
- `assets/social/fonts/VictorMono-Variable.ttf`: Victor Mono, SIL Open
  Font License 1.1, text in `assets/social/fonts/VictorMono-OFL.txt`. The
  social image and the captures of `docs/pages/capture/` use it.

## Documentation conventions

- ADRs (`docs/adrs/`) follow the rules in `docs/adrs/README.md`. Read
  them before writing or changing an ADR.
- The specification of version 1 lives in `docs/specs/spec-v1.md`. It
  describes what is decided, without the reasons, which the ADRs carry.
- Research on how antiq can do something (crates, C libraries,
  executables, measurements) lives in `docs/research/`, one document
  per topic. It records findings, not decisions; a decision taken on
  it becomes an ADR.
- Whatever needs to be addressed or thought about later, including
  things that may end up not being done, goes into the ntropy todo vault
  at `todos/` (format and commands in `todos/README.md`), one note per
  topic, as soon as it comes up. A todo is deleted once completed.
- When a decision lands or code changes make a doc stale, update the
  README, the spec and the affected todos in the same unit of work.
