# 32. Archive with a verified local archive and upload at the next sync

Date: 2026-11-03

## Status

Accepted

## Context

`archive.sh` archives without the NAS. If archiving waited for the
remote, nothing could be archived while the NAS is away.

## Decision

Archiving needs only a verified archive in local storage. Uploading it
is pending work for the next `sync`. The first version has no setting
that keeps a folder until its archive is on the remote.

## Consequences

Until the next sync, the local archive is the only copy.
