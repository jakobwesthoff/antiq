# 62. Repair damaged versions only on request and refuse push while damage exists

Date: 2026-11-04

## Status

Accepted

## Context

Damaged and incomplete versions come from outside antiq: a file deleted
by hand, an incomplete copy back from the target, a disk error. antiq's
own crashes do not leave them behind for good (ADR 0059).

## Decision

No damaged or incomplete version is deleted or rewritten automatically;
the user looks at it first and may prefer to copy the missing file back
from the target. `rebuild` repairs a damaged version only on request,
writing only what is missing after checking the archive file against
what survives. `forget` removes an incomplete version.

`push` refuses while a damaged or incomplete version exists and prints
the exact `forget` or `rebuild` command for each, because the mirror
would otherwise delete or overwrite the target's good copy.

## Consequences

A scheduled `push` stops until the user acts.
