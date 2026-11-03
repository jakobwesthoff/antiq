---
title: "Lay down the module tree, shared types and the command surface"
kind: feature
component: lib
origin: request
tags: [architecture]
---
# Lay down the module tree, shared types and the command surface

Work item I01 of the v1 implementation plan: the crate skeleton with
every module file, the shared types complete, the whole command surface
parsed with every command answering `not_implemented`, the JSON
envelope, exit codes and test support. Its full entry (scope, owned
files, tests, acceptance) is section 5, "I01", of the plan todo. It
lands in two merges (part A skeleton and surface, part B shared types
and `src/fsx/tree.rs`); wave 2 starts after part B.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
