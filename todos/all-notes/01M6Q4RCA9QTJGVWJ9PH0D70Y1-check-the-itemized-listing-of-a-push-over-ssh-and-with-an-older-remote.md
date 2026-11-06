---
title: "Check the itemized listing of a push over ssh and with an older remote rsync"
kind: investigation
component: lib
status: needs-discussion
origin: implementation
tags: [testing]
---
# Check the itemized listing of a push over ssh and with an older remote rsync

Not confirmed. `push` on a terminal reads rsync's `-ii` listing and its
`--info=progress2` updates live, and the report of every push path reads
the itemized listing (`src/commands/push/observed.rs`,
`src/commands/push/summary.rs`, `src/target/rsync.rs`). All of this was
checked against a local target and against a local `rsync --server`
receiver started through `-e env`, with rsync 3.5.1 on both sides
(2026-11-05). A real ssh target and a remote rsync older than 3.5.1
(down to the 3.2.4 antiq requires) were not tried: ssh to localhost was
refused on this machine because its host key had changed, and the
lead did not override host key checking.

## Next step

Push once to a real ssh target with a recent rsync and once with a
3.2.x remote rsync, first push and no-change push, and compare the pipe
report with the terminal card and table. Delete this todo if they agree.
