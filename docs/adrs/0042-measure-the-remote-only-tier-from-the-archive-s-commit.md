# 42. Measure the remote-only tier from the archive's commit

Date: 2026-11-03

## Status

Accepted

## Context

Archives move to the remote-only tier after an age. The age needs a
starting point.

## Decision

The archive tier starts from the piece's activity (ADR 0013). The
remote-only tier is measured from the time the archive was committed.

## Consequences

None further.
