# 43. Delete a local archive only after confirming its remote copy

Date: 2026-11-03

## Status

Accepted

## Context

In the remote-only tier the local archive is deleted (ADR 0007), which
leaves the remote copy as the only one.

## Decision

A local archive is deleted only after antiq has checked the remote's
identity, that the remote holds the same version, and the remote copy's
checksum: computed on the NAS over SSH, or read back on a path remote.
The read-back bypasses the client's file cache, since a read of a file
just written over a network mount may come from that cache instead of
from the server. The time of that check is recorded in the metadata.

## Consequences

None further.
