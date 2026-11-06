---
title: "Run remote commands through an SSH session with a loopback shim"
kind: feature
component: lib
origin: request
tags: [architecture]
---
# Run remote commands through an SSH session with a loopback shim

Work item I24 of the v1 implementation plan: the SSH session layer that runs commands on the NAS through the user's `ssh` with one control master per run, the classification of ssh failures into away and broken, and the loopback shim the tests run against. Its full entry is section 5, "I24", of the plan todo.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
