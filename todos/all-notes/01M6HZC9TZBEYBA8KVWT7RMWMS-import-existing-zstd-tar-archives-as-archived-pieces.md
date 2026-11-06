---
title: "Import existing zstd tar archives as archived pieces"
kind: feature
component: lib
origin: request
tags: [architecture]
---
# Import existing zstd tar archives as archived pieces

Work item I21 of the v1 implementation plan: `import`, which registers existing tar archives in zstd as archived pieces after checking codec, window and checksum of every frame. Its full entry is section 5, "I21", of the plan todo.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
