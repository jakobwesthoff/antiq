---
title: "Decide whether the --no-perms tip warns that mirrored folders lose their modes"
kind: investigation
component: project
status: needs-discussion
origin: implementation
tags: [config, ux]
---
# Decide whether the --no-perms tip warns that mirrored folders lose their modes

When a push target refuses to set file modes, the push error and the
README tip in "Mirroring" tell the user to add `rsync_args =
["--no-perms"]` under `[target]`. Not yet checked: what that costs a
later restore from the target. The archives keep the file modes inside
the tar, so versions lose nothing. The live folders that push mirrors
as they are would take the target's default modes instead, so an
executable bit, for example, would not survive a copy back from the
target.

## Open

- Confirm on a target with `--no-perms` which modes the mirrored live
  folders get (unconfirmed so far, only reasoned from rsync's
  documentation).
- Decide whether the push error's fix and the README tip mention it,
  and in which words.
