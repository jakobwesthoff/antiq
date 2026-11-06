# 53. Store a version's metadata as comment lines of its listing

Date: 2026-11-03

## Status

Accepted

## Context

`list` and `status` need a version's piece path, time, archive name and
size, and file count and total size without reading the archive. A
listing is a SHA-256 checksum list, and `shasum -a 256 -c` (checked with
`/usr/bin/shasum` 6.02 and Homebrew shasum 6.04) and GNU `sha256sum -c`
(checked with coreutils 9.12) both skip lines that start with `#`, also
with `--strict`. A comment after the path on the same line does not
work: both tools read it as part of the file name.

## Decision

The metadata of a version are comment lines at the head of its listing:
the format line `# antiq listing 1`, then `piece`, `archived`,
`archive`, `archive-bytes`, `files` and `bytes`, and where they apply
`imported` and `rebuilt`. Each entry's type, mode, time and size are
comment lines of fixed width before its checksum line. `list` and
`status` read only the head.

## Consequences

A listing can be checked without antiq: `zstd -dc <listing> | shasum -a
256 -c -` inside the unpacked folder.
