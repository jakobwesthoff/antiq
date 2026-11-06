# 60. Name versions by UTC stamp and file names from the piece path

Date: 2026-11-04

## Status

Accepted

## Context

Versions of one piece have to order by time, and several can be made
within one second. File names have to name the piece for people and stay
within file-name limits.

## Decision

A version stamp is the UTC time of the version, written
`YYYY-MM-DDTHHMMSS`, with a counter `-2`, `-3` and so on when the same piece
has several versions within one second. `-1` is not a valid spelling.
Stamp and piece path together name a version exactly.

File names are `<name>--<version stamp>` plus the suffix. `<name>` is
the piece path with every character outside `[A-Za-z0-9._]` replaced by
`-` and repeated `-` collapsed. It is for people only: nothing parses it
and it need not be unique. It is cut from the front at a `-` boundary so
that the longest file name stays within 200 bytes.

An optional version is the flag `--version <version>`, a required one is
positional. An input containing `T` is matched against the version
stamps. Any other input is matched as a prefix of each version's time
rendered in the machine's current zone. The two are never mixed. An
exact match wins over prefix matches; several matches are an error
naming each version's stamp.

## Consequences

None further.
