# 6. Migrate the existing playground archives once

Date: 2026-11-02

## Status

Accepted

## Context

`archive.sh` has produced more than 300 archives in
`~/playground/archive/`, each a `<name>.tar.xz` with a member listing in
`archive/index/<name>.tar.xz.lst`.

## Decision

The existing archives are migrated into the tool once. The tool does not
read the layout or the listings that `archive.sh` produces.

This does not rule out a structured import mechanism that brings
existing folders or archives into the tool. Whether there is one, and
what it looks like, is open.

## Consequences

The existing archives need a one-off migration step before the tool
replaces `archive.sh`.
