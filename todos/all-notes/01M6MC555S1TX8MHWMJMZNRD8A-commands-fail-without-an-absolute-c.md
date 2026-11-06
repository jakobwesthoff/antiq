---
title: "Commands fail without an absolute -C"
kind: bug
component: project
impact: high
origin: implementation
tags: [architecture]
---
# Commands fail without an absolute -C

Found by Z2 on main 3df335d while running the README examples. Inside a collection, `antiq unearth tools/scan-old` (no `-C`) prints `error: cannot find the piece: `.` is not an absolute path` and exits 1; so do `unearth scan-old`, `fork scan-old` and `-C . unearth scan-old`. With an absolute `-C` the same command works. The message comes from `RelPathError::NotAbsolute` in `src/relpath.rs`. Several commands default the directory to `PathBuf::from(".")` (`src/cli/cmd/{archive,init,import,rebuild,push,status,fork}.rs`), and some of them pass that relative path on to code that needs an absolute one. The tests and scenarios always pass an absolute `-C`, so none noticed.

## Fix

Tests first, through the binary with the process's current directory set to the collection: every command, once without `-C`, once with a relative `-C`, once from a subdirectory of the collection with a path argument relative to it. Then resolve the start directory to an absolute path in one place (the CLI context), so no command sees `.`. The scenario harness gains a way to run a command without `-C` and one scenario step uses it.

## Relations

- Part of: [Rework antiq v1 to the simplified concept](01M6KFHS2FVQ9APAGQ947FP3CM-rework-antiq-v1-to-the-simplified-concept.md)
