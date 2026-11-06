---
title: "Guard state-changing runs, repair at their start and render dry-run plans"
kind: feature
component: lib
origin: request
tags: [architecture]
---
# Guard state-changing runs, repair at their start and render dry-run plans

Work item I17 of the v1 implementation plan: the run guard every state-changing command runs in, with the lock, the start-of-run repairs in their fixed order, and the rendering of dry-run plans. Its full entry is section 5, "I17", of the plan todo.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
