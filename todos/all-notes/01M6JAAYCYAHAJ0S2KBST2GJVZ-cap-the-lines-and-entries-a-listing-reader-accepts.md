---
title: "Cap the lines and entries a listing reader accepts"
kind: improvement
component: lib
status: needs-discussion
origin: review
---
# Cap the lines and entries a listing reader accepts

`listing::Reader` (`src/listing.rs`) reads a listing without a limit on
the length of a line or on the number of entries. `grep`, `list` and
`status` read listings, which can be copied back from the target by hand,
so a damaged or hostile listing can make them allocate without bound. Found in the plan review of I34 (finding O12), 2026-11-04.

## Open question

Which caps: a line length (a path is at most the platform's limits plus
escaping) and an entry count tied to something measurable. A cap needs
a number with a source; none is chosen yet.

## Relations

- Part of: [Rework antiq v1 to the simplified concept](01M6KFHS2FVQ9APAGQ947FP3CM-rework-antiq-v1-to-the-simplified-concept.md)
