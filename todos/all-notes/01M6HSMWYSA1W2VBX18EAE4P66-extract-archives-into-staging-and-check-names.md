---
title: "Extract archives into staging and check names"
kind: feature
component: lib
origin: request
tags: [architecture]
---
# Extract archives into staging and check names

Work item I13 of the v1 implementation plan: extraction of a package into staging with antiq's own checks before the tar crate's, refusing members that lead outside or through a symlink, and the name checks against the target volume. Its full entry is section 5, "I13", of the plan todo.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
