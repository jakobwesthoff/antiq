---
title: "Check whether a piece with a non-UTF-8 name can still be unearthed by its exact path"
kind: bug
component: cli
origin: implementation
tags: [ux]
---
# Check whether a piece with a non-UTF-8 name can still be unearthed by its exact path

`unearth` and `forget` act only on the exact path of a piece; a
reference that matches only loosely is refused with the table of
matching pieces, so the user can copy the path from there. For a piece
whose name is not valid UTF-8, that table shows the name in an escaped
form, which cannot be pasted back as the same path. Reported by the
implementation of the exact-path rule and not reproduced yet: unknown
whether antiq reads the argument as raw bytes (then a shell form such as
`$'\xff'` would work) or only as text, in which case such a piece could
no longer be unearthed or forgotten by reference at all.

## To do

Reproduce with a piece named by invalid UTF-8 bytes. If it cannot be
reached, give the table a form that can be pasted, or name a way that
works in the error's fix.
