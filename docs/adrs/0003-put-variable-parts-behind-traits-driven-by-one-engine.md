# 3. Put variable parts behind traits driven by one engine

Date: 2026-11-02

## Status

Accepted

## Context

Several parts of the tool come in more than one variant. Archives can be
packed in different formats, such as tar with xz, zstd or zip. Archives
can be kept in different stores, such as a NAS reached over SSH today and
external or cloud storage later. The first version implements only some
of these variants.

## Decision

Architecture and design come before implementation. Before the code is
written, the design settles which parts sit behind which traits, so that
further packaging formats, stores and other variants plug into the same
engine.

Which parts get a trait, and what each trait looks like, is decided in
later ADRs.

## Consequences

No packaging format or store gets implemented before its trait is
settled.
