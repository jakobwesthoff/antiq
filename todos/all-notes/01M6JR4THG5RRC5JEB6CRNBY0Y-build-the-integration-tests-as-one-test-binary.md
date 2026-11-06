---
title: "Build the integration tests as one test binary"
kind: improvement
component: project
origin: discussion
tags: [testing]
---
# Build the integration tests as one test binary

Each of the 19 files in `tests/` is its own test binary, linked against
the whole library. On a build after a change, `ld` links every one of
them again, and macOS scans every new binary (XProtect, syspolicyd).
On 2026-11-04 an `fs_usage` capture of 31 s during the parallel agent
builds showed `ld` writing 370 MB and `syspolicyd` plus
`XprotectService` reading 241 MB. The test binaries in one worktree
measured 10 to 42 MB each.

## Suggested change

Move the files into modules of one binary, for example
`tests/it/main.rs` with `mod sync; mod picker; ...`, and keep
`tests/support/` as a shared module. Then one test binary is linked
per build instead of 19.

## To check before doing it

- Tests that rely on running in their own process: environment
  variables, the current directory, global state, or `#[ctor]`-like
  setup. Name each and keep it apart, or make it safe to share.
- The pty tests in `tests/picker.rs` and their harness
  `tests/support/pty.rs`.
- `cargo test --test <name>` invocations in scripts, docs and briefs
  change to `cargo test --test it <module>::`.
- Measure the link output and the build time before and after.

## Timing

After the running wave 9 items (I32, I33, I35) are merged, as the
change touches every test file. Discussed with the user on
2026-11-04 as part of lowering the SSD writes of the builds; they
asked for a todo and may want to discuss it again.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
