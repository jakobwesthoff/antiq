# 68. Let push take over the target of its own collection

Date: 2026-11-06

## Status

Accepted

Amends [31. Mark the target with a .antiq-target file and check it before every push](0031-mark-the-target-with-a-antiq-target-file-and-check-it-before-every-push.md)

## Context

After a collection moves to another folder or a machine is renamed, the
marker names the old host or path and `push` refuses. ADR 0031 leaves two
ways on: editing the marker on the target by hand, which over ssh means a
command on the target host, or pushing with `--force` every time.

## Decision

`push --take-over` rewrites the marker to this machine and this collection
path when the marker names this collection's ID and another host or path.
It then pushes as usual. The marker is written anew as `--init` writes it,
by the same rsync run and before the mirror. A marker of another
collection, a missing marker and an unreadable marker are refused as
before.

`--take-over` is refused together with `--init`, `--force` and
`--if-reachable`, so an unattended run never changes a marker. A rewrite
prints a note that names the old and the new host and path.

Any collection with the ID in the marker can take the target over, a copy
of the folder included.

ADR 0031 stays in force except for its statement that antiq never rewrites
the marker and that the user edits it by hand.

## Consequences

The host and path the marker named before are refused by every push from
then on, `--if-reachable` included, until they take the target back. A push
from a copy mirrors the copy's state to the target.
