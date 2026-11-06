---
title: "Consider storing extended attributes in archives"
kind: feature
component: lib
horizon: someday
origin: discussion
---
# Consider storing extended attributes in archives

Versions store no extended attributes in v1 (ADR 0050): macOS
attributes such as `com.apple.provenance` and any `user.*` attributes are
dropped when a piece is packed, and an unearthed piece comes back without
them. The mirror on the target keeps none either. The 313 archives `archive.sh` made carry
none either (no pax records, `docs/research/tar.md`).

## Decisions

- 2026-11-03, user: store none for now; a todo for a possible future
  feature, should it turn out to be needed.

## Notes

- Storing them means pax `SCHILY.xattr.*` records written through the
  `tar` crate's `append_pax_extensions`; this is custom code and was not
  tested (`docs/research/tar.md`).
- `bsdtar` on macOS writes `._*` AppleDouble members plus
  `LIBARCHIVE.xattr.*` and `SCHILY.xattr.*` records for files with
  attributes, measured during the tar research.
- Attributes would also enter the strict verify (ADR 0033).

## Revisit when

A piece loses something that matters because an attribute was dropped,
or a user asks to keep attributes.
