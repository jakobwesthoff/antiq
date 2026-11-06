# 49. Mirror the collection with one rsync run of rsync 3.2.4 or later

Date: 2026-11-03

## Status

Accepted

## Context

A stock NAS runs rsync; no Rust crate implements the rsync sender
protocol (`docs/research/rsync.md`). `timecapsule-sync.sh` stops before
a sync would delete more than 20 % of the mirror.

## Decision

`push` runs the local rsync once against the target: `-a -H -x --delete
--delete-delay --itemize-changes`, excludes anchored at the collection
root for the marker, `.antiq/tmp/`, `.antiq/trash/`, `.antiq/doomed/`,
`.antiq/lock`, `.DS_Store` and the macOS volume-root directories,
`--timeout=300`, `--fsync` unless `[target] fsync = false`, and the ssh
options `BatchMode`, `ConnectTimeout`, `ServerAliveInterval` and
`ServerAliveCountMax`. The options of `[target] rsync_args` follow
verbatim. antiq requires rsync 3.2.4, the release that added `--fsync`,
and refuses openrsync, the default `/usr/bin/rsync` on macOS, and older
versions.

- `--delete-delay` makes deletions happen only at the end, so an interrupted
  run removes nothing that was on the target before.
- The macOS volume-root directories are left out because rsync cannot read
  them and any unreadable directory stops every deletion. `.DS_Store` at the
  root is left out because Finder visits would otherwise count as changes.
- The ssh options and `--timeout` keep a sleeping NAS from holding the lock
  for hours.
- `-x` keeps rsync on the collection's device.
- When the first run printed a `*deleting` line, a second pass runs, because
  on a case-insensitive target a folder renamed only in case is otherwise
  deleted at the end of the first run and copied again only by the next push.
- `push` takes the lock for its whole run, because without it rsync could
  scan a piece halfway through an `unearth` and leave the target with
  neither the folder nor the version.
- `push` has no brake: whatever is deleted locally, by antiq or by hand, is
  deleted on the target by the next push. The target is a mirror, not a
  backup with history.

## Consequences

With `--fsync` a power loss can at most lose the newest names, which the
next push sends again.
