---
title: "Escape control characters in the paths of error output"
kind: bug
component: cli
origin: implementation
tags: [ux, security, error-handling]
---
# Escape control characters in the paths of error output

Found on 2026-11-11 while planning the escape of control characters in
dry-run and result lines. The error report of a failed run prints paths
from the user's files raw, so a newline or ESC in a file name can forge
or hide lines of an error, on a terminal and on a pipe.

## Problem

Read in the code, not reproduced (2026-11-11). `report_error` in
`src/cli/mod.rs` prints `error_block::block_for(...)`
(`src/cli/error_block.rs`) on a terminal and its `Block::pipe` line
(`error: <chain>`) on a pipe. The title, the text, "How to fix it",
"What antiq was doing", the candidates and the `said` lines (excerpts of
the output of rsync and other programs) are built from `Rich` and
`Block` with library messages that hold raw paths, such as
`cannot walk {folder}`, `{path} is not a folder` and
`the newest file {path} is dated ...`. The tables of
`unresolved::group` already escape piece paths (`src/cli/unresolved.rs`,
around lines 249 and 374); the error title and help around them do not.

Left alone on purpose: clap's usage errors echo the command line, not
names from the collection; the JSON error envelope carries strings that a
newline cannot break; `grep`'s pipe lines print raw bytes as the spec
says.

## Suggested fix

Escape inside `Rich` (`push`) and `Block` (`pipe`, `message`,
`Para::Pre`, `said`, `doing`, `candidates`) with `sanitize` from
`src/cli/table.rs`, which the dry-run lines, result lines, notes and
warnings already use, so that every block escapes. Extend the general
rule in the spec's "Console output" section to error output.
