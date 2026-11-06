# 9. Migrate the existing playground archives once through import

Date: 2026-11-02

## Status

Accepted

## Context

`archive.sh` has produced more than 300 archives in
`~/playground/archive/`. Each is a `<name>.tar.xz` with a member listing
in `archive/index/<name>.tar.xz.lst`.

## Decision

The existing archives are brought into the tool once. The tool does not
read the layout or the listings that `archive.sh` produces, and `init`
does not look for them. Each old archive is unpacked outside antiq with
its original timestamps and the folder is imported with `import` (ADR
0055).

## Consequences

antiq reads no xz.
