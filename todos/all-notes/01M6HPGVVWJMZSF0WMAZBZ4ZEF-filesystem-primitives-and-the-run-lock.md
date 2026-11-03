---
title: "Filesystem primitives and the run lock"
kind: feature
component: lib
origin: request
tags: [architecture]
---
# Filesystem primitives and the run lock

Work item I03 of the v1 implementation plan: the atomic write with its fsync fallback, the delete and move helpers, free space, device and filesystem kind, and the run lock. Its full entry (scope, owned files, tests, acceptance) is section 5, "I03", of the plan todo.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
