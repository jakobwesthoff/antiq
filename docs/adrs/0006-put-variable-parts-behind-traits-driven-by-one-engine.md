# 6. Put variable parts behind traits driven by one engine

Date: 2026-11-02

## Status

Accepted

## Context

Several parts of the tool come in more than one variant. Archives can be
packed in different formats, such as tar with xz, zstd or zip. They can
be kept in different stores, such as a NAS reached over SSH, or external
and cloud storage. The first version implements only some of these.

## Decision

The design settles which parts sit behind which traits before they are
implemented. Further variants then plug into the same engine.

Which parts get a trait, and what each trait looks like, is left to
later ADRs.

## Consequences

None further.
