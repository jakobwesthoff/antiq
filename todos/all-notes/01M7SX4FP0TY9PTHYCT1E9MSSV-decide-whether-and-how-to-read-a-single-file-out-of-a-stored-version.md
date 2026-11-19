---
title: "Decide whether and how to read a single file out of a stored version"
kind: investigation
component: lib
status: needs-discussion
origin: idea
tags: [ux, architecture]
---
# Decide whether and how to read a single file out of a stored version

Raised on 2026-11-19. There is no quick way today to get at one file of an
archived piece. `grep` finds the path in the listings, but getting the file
means `fork`ing the whole version and picking the file out of the copy. A
command such as `antiq cat` or `antiq show` (or another name, see below)
could print one file of a given piece and version on stdout, or extract it
to a path. Before building it, settle whether the store format allows this
at an acceptable cost, or whether it has to change.

## Context

- A version is one `.tar.zst`: a tar stream written in-process (ADR 0050)
  and compressed as a whole with zstd at level 19, long-distance matching
  and a 128 MiB window (ADR 0051). Neither tar nor that zstd stream has an
  index, so reaching a member means decompressing everything before it. For
  a file near the end of an archive of several GB, `cat` costs about as
  much as `fork`, minus the writing.
- The listing (ADR 0053, "Listing format" in `docs/specs/spec-v1.md`)
  already holds every path with its type, mode, mtime and size. It holds no
  offset into the tar stream.
- The archive checksum covers the whole `.tar.zst`. Reading part of an
  archive cannot check it, so a damaged version would be read unverified.

## Options

1. **Stream until the member.** Decompress from the start and stop after
   the member. No format change, works for every existing version, cost
   grows with the member's position. Probably fine for small pieces and the
   right first step either way.
2. **Change the format for random access.** For example zstd's seekable
   format (independent frames plus a seek table) together with member
   offsets in the listing. Every frame starts with an empty window, so the
   ratio the 128 MiB window buys (ADR 0051, `docs/research/compression-bench.md`)
   shrinks with the frame size. Needs a new listing format version, and old
   versions keep the old format, so option 1 stays as the fallback.
3. **Cache.** Keep an extracted version (or the files read from it) in a
   cache after the first access, so reading several files of one version,
   or the same file again, decompresses once. Raises where the cache lives
   (`.antiq/tmp/` or a per-user cache directory), how large it may grow,
   and when it is dropped.

## Open questions

- Is option 1 fast enough on the real archives to make the format question
  moot? Measure on the largest versions before deciding anything.
- How large a frame keeps the compression loss acceptable, if option 2
  comes up again?
- Name: `cat` and `show` are recognisable; ADR 0023 asks for names that fit
  antiq while staying recognisable.
- Is it one command with a stdout mode and an extract mode, or two? Data on
  stdout follows ADR 0020; extraction to a path follows the destination
  rules of `unearth` and `fork`.
- How a file is named: `<piece> <path>` with `--version`, as `fork` takes
  it, or one `<piece>/<path>` argument as `grep` prints it.
- Several files or a directory at once, or exactly one file?
