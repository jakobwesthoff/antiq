# 26. Attach a remote with remote add and create a new one only with --init

Date: 2026-11-03

## Status

Accepted

## Context

A remote's root directory carries a marker file. A path that is not a
remote yet, such as the mount point of a share that is not mounted,
must not turn into one by accident.

## Decision

`antiq remote add <url>` attaches the collection to an existing remote
and refuses when the root has no marker. `antiq remote add --init <url>`
creates a new remote root with its marker. `antiq remote show` shows the
remote and `antiq remote remove` detaches it. The commands follow git's
`remote` commands.

## Consequences

None further.
