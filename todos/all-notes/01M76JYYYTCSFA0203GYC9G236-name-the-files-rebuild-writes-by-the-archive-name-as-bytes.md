---
title: "Name the files rebuild writes by the archive name as bytes"
kind: bug
component: lib
impact: low
origin: review
tags: [error-handling]
---
# Name the files rebuild writes by the archive name as bytes

Found on 2026-11-12 in the review of the escaping rule for text output.
`rebuild` derives the names of the listing and checksum file it writes
from the archive name turned into text with `to_string_lossy`. An archive
name that is not UTF-8 then gives sidecars under another name than the
archive's, with U+FFFD in place of the bytes, so the version stays
damaged and gains stray files.

## Problem

Read in the code, not reproduced (2026-11-12): `write_missing` in
`src/commands/rebuild.rs` sets `archive_name` from
`version.files(VersionFile::Archive)[0].to_string_lossy()` and passes it to
`sidecar_name`, which strips the archive suffix and adds the sidecar's;
the files are staged and moved under those `String` names. `entry_of`
builds the reported `write` names the same way. The checksum file's
content (`ChecksumFile::new(..., archive_name)`) carries the lossy name
too, so `shasum -c` would not find the archive.

Version file names that antiq writes are ASCII only (`stem` in
`src/versions/names.rs` maps everything else to `-`), so only a version
file that came from outside antiq, such as one copied in by hand, can have
such a name.

## Suggested fix

Derive the sidecar names from the archive name's bytes (`OsStr`) and stage
them as `OsString`, and decide what the checksum file records for a name
that is not UTF-8 (the format `shasum` reads is text). Add a test with a
non-UTF-8 archive name whose listing is missing.
