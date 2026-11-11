---
title: "Escape names from the user's files in all text output by one shared rule"
kind: bug
component: lib
origin: discussion
tags: [ux, security, error-handling]
---
# Escape names from the user's files in all text output by one shared rule

A file or folder name can hold a control character such as a newline or
ESC. Printed raw, it can forge a line of antiq's output or change what a
terminal shows. Since `36df354`, dry-run lines, result lines, notes and
warnings escape control characters with `sanitize` from
`src/cli/table.rs`, but every call site has to remember it, error output
does not do it, and `grep`'s pipe lines print raw bytes by the spec. This
todo replaces the per-site escaping with one rule that holds for all text
output and is enforced, so that a new call site cannot forget it.

## Problem

Read in the code (2026-11-11): the error report of a failed run
(`report_error` in `src/cli/mod.rs`, `error_block::block_for` in
`src/cli/error_block.rs`, the `Block::pipe` line `error: <chain>`) prints
the title, the text, "How to fix it", "What antiq was doing", the
candidates and the `said` lines (output excerpts of rsync and other
programs) raw. About 250 `#[error(...)]` messages of the library embed
paths, 47 of them through `CodeSpan` (`src/code_span.rs`), and the code
has 89 calls of `.display()`. `grep` prints raw bytes in a pipe, so a
newline in an archived file name splits a hit into two lines.

## Decisions

- 2026-11-12, user: one general rule for paths and names from the user's
  files in all text output, errors, rsync excerpts and `grep`'s pipe
  lines included; the spec loses the `grep` exception.
- 2026-11-12, user: escape Unicode control characters (Cc), the bidi
  controls U+202A to U+202E and U+2066 to U+2069, and U+2028 and U+2029,
  each as `%` and the hex digits of its UTF-8 bytes.
- 2026-11-12, user: a `%` in a name is shown as `%25`, so the shown form
  is unambiguous and can be turned back into the exact bytes, as the
  escaped form of `src/relpath.rs` already does for `%` and bytes that
  are not UTF-8.
- 2026-11-12, user: the JSON `message` and `fix` of errors and warnings
  carry the escaped form, as they come from the same `Display` as the
  console; structured JSON fields such as `piece`, `path` and `left_out`
  stay as they are.
- 2026-11-12, user: enforce the rule with a `clippy.toml`
  `disallowed-methods` entry for `std::path::Path::display` outside tests,
  a `debug_assert!` where text reaches the console (`Rich` and
  `Output::line`) that no raw control character arrives except the tab of
  pipe tables, and a hostile test world with such names run through every
  command in text mode. No `Trusted`/`Untrusted` newtypes.

## Proposal

- A library module, for example `src/shown.rs`, with one escaping
  function and a `Display` adapter `Shown<T>` for `&str`, `&Path`,
  `&OsStr` and `&RelPath`, in the manner of `Path::display()`: the value
  stays raw, its display is escaped.
- `CodeSpan` escapes through it, which covers its 47 uses; the other
  `#[error]` messages format paths through `Shown`.
- In the binary, `Rich` segments and plain pipe lines escape values
  before styling is applied, so antiq's own ESC sequences survive and
  those of a name do not. `sanitize` and `shown_positions` in
  `src/cli/table.rs` move onto the shared function.
- The spec's "Console output" states the one rule without exceptions, and
  "JSON output" says that `message` and `fix` carry the escaped form.
- An ADR records the rule and its enforcement.
