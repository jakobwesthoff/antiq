# 9. Migrate the existing playground archives once

Date: 2026-11-02

## Status

Accepted

## Context

`archive.sh` has produced more than 300 archives in
`~/playground/archive/`. Each is a `<name>.tar.xz` with a member listing
in `archive/index/<name>.tar.xz.lst`.

## Decision

The existing archives are migrated into the tool once. The tool does not
read the layout or the listings that `archive.sh` produces.

A general import of existing folders or archives is not ruled out by
this.

## Consequences

None further.
