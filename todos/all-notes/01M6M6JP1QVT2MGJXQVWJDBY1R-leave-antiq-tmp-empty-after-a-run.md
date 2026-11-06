---
title: "Leave .antiq/tmp empty after a run"
kind: bug
component: project
origin: implementation
tags: [architecture]
---
# Leave .antiq/tmp empty after a run

Found by the E3 scenario tests on main 237d437: after a real `archive projects/scanner` the empty folder `.antiq/tmp/pack` stays. Repair 2 deletes whatever `.antiq/tmp/` holds, so every later locked run reports the repair (`note: repaired: ...` on stderr, a `repairs` entry in JSON) and every later dry run starts with `Would delete .antiq/tmp/pack` instead of its plan. The archive unit tests in `src/commands/archive/store.rs` assert that `tmp/pack/` holds nothing, not that it is gone.

## Fix

Every command that stages below `.antiq/tmp/` (archive's `pack/`, unearth's `extract/` and flush probe, rebuild's `rebuild/`, the marker temp files) leaves `.antiq/tmp/` empty when its run ends, success or handled failure, so the next run plans no repair 2. Parallel pieces of one archive run share `pack/`; it goes when the last one is done. A crash still leaves the leftovers for repair 2. Tests first: one per command that a run followed by a dry run plans no repair, and that `.antiq/tmp/` is empty.

## Relations

- Part of: [Rework antiq v1 to the simplified concept](01M6KFHS2FVQ9APAGQ947FP3CM-rework-antiq-v1-to-the-simplified-concept.md)
