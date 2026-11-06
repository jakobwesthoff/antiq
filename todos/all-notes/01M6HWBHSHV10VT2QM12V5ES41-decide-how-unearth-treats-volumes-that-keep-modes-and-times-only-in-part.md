---
title: "Decide how unearth treats volumes that keep modes and times only in part"
kind: investigation
component: lib
status: needs-discussion
origin: review
tags: [ux]
---
# Decide how unearth treats volumes that keep modes and times only in part

exFAT and FAT32 keep file modes and modification times only in part.
Unearthing a version onto such a volume restores neither exactly, so
the strict verify of the staged tree differs on every file. Whether
unearth goes on there, and how the verify treats the difference, is a
product decision that is still open.

## Facts

Measured on 2026-11-04 on macOS with disk images created by `hdiutil`:

- exFAT and FAT32 (MS-DOS): `chmod` succeeds, but every file reads
  back as 0700.
- FAT32 rounds modification times to 2 seconds: a time of
  1,000,000,001 reads back as 1,000,000,000. exFAT keeps it.
- Hard links fail with `ENOTSUP` on both. Symlinks work on macOS.

Not measured: Linux's exfat driver reports modes from its `fmask` and
`dmask` mount options rather than 0700, and Linux's exfat and vfat
drivers may refuse symlinks.

## Cases

- Archived on the volume and unearthed onto it on the same OS: the
  modes are already 0700 and the times already at the volume's
  granularity, so the version round-trips.
- Archived on another volume and unearthed onto exFAT or FAT32: modes
  are lost, and on FAT32 odd-second times are rounded.
- The same drive under macOS and then Linux: macOS stores 0700, Linux
  reports the mount's modes, so every mode differs.

## Already handled

The volume probe in `src/pack/names.rs` (I13) checks for hard links and
symlinks. A version holding members of a kind the volume cannot create
is refused before anything is written, together with the name problems.

## Options

- Refuse unearth onto a volume that does not keep modes, or that rounds
  times, with one clear message.
- Unearth with a warning that lists what was not restored, and have the
  verify compare modes and times only as far as the volume keeps them.
- Treat such volumes as unsupported for collections in v1, stated in
  the spec.

## Relations

- Relates to: [Offer a way to unearth pieces whose names the target cannot hold](01M6GQ88DMPMTZMHE5A592Q0K0-offer-a-way-to-unearth-pieces-whose-names-the-target-cannot-hold.md)
