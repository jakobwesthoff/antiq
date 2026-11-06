# 54. Check names against the volume before unearthing and warn when archiving

Date: 2026-11-03

## Status

Accepted

## Context

Names are stored byte for byte (ADR 0050), and a version can reach
another machine by copying its files from the target. On macOS, a Linux
name that is not UTF-8 fails to extract. Two Linux names that differ
only in case or in Unicode normalisation extract silently as one file,
the later one replacing the earlier. Linux refuses names longer than 255
bytes, which macOS allows (`docs/research/names.md`). A case-insensitive
target would merge two pieces whose paths differ only in case or
normalization.

## Decision

Before extracting anything, unearthing checks every member name against
the rules of the volume it extracts to: names the volume cannot hold,
name lengths, names that collide under the volume's case and Unicode
rules, and hard links or symlinks on a volume that cannot create them.
Paths are not checked for length: a path longer than the platform's path
limit is extracted directory by directory. It refuses the piece with a
list of the offending names and the reason. The strict verify (ADR 0033)
stays as the backstop.

Archiving checks the same rules for both macOS and Linux and warns about
names that would not survive on the other system. It does not refuse.

`push` refuses, naming both, when two piece paths or two store folders
of the collection differ only in case or Unicode normalization.

## Consequences

A piece with such names can be unearthed only on a machine whose rules
fit it. The first version offers no other way out.
