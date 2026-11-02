# 2. Write the tool in Rust as one crate with a library and a CLI binary

Date: 2026-11-02

## Status

Accepted

## Context

The tool replaces two bash scripts that manage the `~/playground`
directory: `archive.sh` archives, indexes and restores folders, and
`timecapsule-sync.sh` mirrors the directory onto a NAS.

## Decision

The tool is written in Rust, as one crate holding a library and a
command-line binary. The library (`src/lib.rs`) holds the logic. The
binary (`src/main.rs`) parses arguments and reports errors.

The binary parses arguments with `clap` (derive API) and propagates
errors with `anyhow`. The library defines its error types with
`thiserror`.

## Consequences

Building the tool needs a Rust toolchain of at least the version that
`Cargo.toml` declares as `rust-version`.
