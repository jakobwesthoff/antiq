# 27. Restore symlinks in pieces as they are

Date: 2026-11-03

## Status

Accepted

## Context

Unearthing refuses archive members that would land outside the piece.
Real folders contain symlinks whose targets lie outside the folder; the
313 archives `archive.sh` made hold 409 symlinks.

## Decision

Symlinks are archived and restored as they are, whatever their target.
Only a member's path can escape a piece, and an archive with such a
member is refused. Unearthing never writes through a symlink. rsync
copies links as links; on the target a link points to whatever its path
names there.

## Consequences

None further.
