---
title: "Let push take over a target whose marker names another host or path"
kind: feature
component: lib
origin: request
tags: [ux]
---
# Let push take over a target whose marker names another host or path

The marker `.antiq-target` records the collection ID, the machine name and
the collection path, and antiq never rewrites it (ADR 0031, spec "Push",
step 5 "Marker"). After a renamed machine or a moved collection, `push`
refuses with exit 1 and names both, and the only ways on are editing the
marker by hand on the target or pushing with `--force` every time. On
2026-11-06 a collection moved from `~/playground-antiq` to `~/playground`
with its target on an ssh NAS, and the fix was an `ssh … sed -i` on the
NAS.

## Request

`push --take-over` rewrites the marker's `host` and `path` to this machine
and this collection, then pushes as usual. It works only when the marker's
collection ID is this collection's: a marker of another collection stays
refused, so a take-over can never claim someone else's target.

## Open details

- Combinations: refused together with `--init` (no marker to take over)
  and `--if-reachable` (an unattended run must not change the marker), as
  `--force` is today; whether `--take-over` and `--force` together is a
  usage error.
- A marker that already names this host and path: a plain push, with or
  without a note.
- Dry run: `Would take over the target: host <old> -> <new>, path <old> ->
  <new>`.
- The marker is written by the same rsync run `--init` uses, before the
  mirror.
- A new ADR that amends ADR 0031 (on `main`, so it gains only the status
  link).
