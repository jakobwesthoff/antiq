# 31. Mark a remote root with a marker file and check it before every operation

Date: 2026-11-03

## Status

Accepted

## Context

A path remote on an unmounted share points at an empty local
directory. Without a check, antiq would read it as a remote without
archives, or write a second remote into the mount point.

## Decision

A remote root holds a marker file with the remote's ULID and the
layout version. The collection's config stores that ULID. Every remote
operation checks the marker first and refuses when it is missing or
holds another ULID. Only `remote add --init` creates a marker
(ADR 0026). A path remote whose root is not a mount point gets a
warning.

## Consequences

None further.
