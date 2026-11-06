---
title: "Cap the lines and entries a listing reader accepts"
kind: improvement
component: lib
status: needs-discussion
origin: review
---
# Cap the lines and entries a listing reader accepts

`listing::Reader` (`src/listing.rs`) reads a listing without a limit on
the length of a line or on the number of entries. `unearth` through the
remote (I34) reads a listing fetched from the remote before the archive
is checked, so a damaged or hostile listing can make it allocate without
bound. Found in the plan review of I34 (finding O12), 2026-11-04.

## Open question

Which caps: a line length (a path is at most the platform's limits plus
escaping) and an entry count tied to something measurable. A cap needs
a number with a source; none is chosen yet.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
