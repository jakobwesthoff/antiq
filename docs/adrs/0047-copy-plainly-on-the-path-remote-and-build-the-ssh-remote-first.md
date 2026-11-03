# 47. Copy plainly on the path remote and build the SSH remote first

Date: 2026-11-03

## Status

Accepted

## Context

When both ends of rsync are local paths and one is an SMB mount, rsync
reads the old file over SMB and writes a complete new one, so delta
transfer saves no network traffic. Delta transfer helps only when rsync
runs on the NAS.

## Decision

The path remote copies files plainly: a temporary name in the target
directory, written, flushed to disk and renamed. Delta transfer is a
property of the SSH remote. The SSH remote is built first; the path
remote stays in the first version.

## Consequences

Live packages on a path remote are uploaded in full on every
change.
