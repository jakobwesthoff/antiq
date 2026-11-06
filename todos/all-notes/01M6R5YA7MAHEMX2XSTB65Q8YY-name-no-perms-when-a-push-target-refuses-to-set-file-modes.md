---
title: "Name --no-perms when a push target refuses to set file modes"
kind: improvement
component: cli
origin: implementation
tags: [ux, error-handling]
---
# Name --no-perms when a push target refuses to set file modes

Seen on 2026-11-06 in a push to a real ssh target, a TrueNAS share
(rsync 3.2.7 on the target): the share refuses `chmod` ("Operation not
permitted"), so rsync with `-a` fails on every file with
`mkstemp ... failed: Operation not permitted` and exit 23. antiq then
refuses the push with rsync's raw lines, one per file. The cure is
already documented as an example in the configuration comment of the
spec, `rsync_args = ["--no-perms"]`; with it the same push worked, and
its card, table and pipe report agreed.

## Suggested change

When the mirror ends with exit 23 and rsync's lines show `Operation not
permitted` for `mkstemp` or `chmod` on the receiver, give the error a fix
that names `rsync_args = ["--no-perms"]` under `[target]` in
`.antiq/config.toml`, and shorten the repeated rsync lines to the first
few with a count of the rest.

## Open

Whether `--no-perms` loses anything a later restore from the target
needs: the archives keep the modes inside the tar, but the live folders
mirrored to the target would take the target's default modes.
