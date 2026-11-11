# 21. Search with list and grep

Date: 2026-11-02

## Status

Accepted

Amended by [69. Make find the command and keep list as its alias](0069-make-find-the-command-and-keep-list-as-its-alias.md)

## Context

`archive.sh` lists archives by name and restores one through an fzf
picker. The contents of every archive have to be searchable.

## Decision

`list` prints one row per version; its query matches piece paths fuzzily
and each row reads only the head of a listing. `grep` searches the paths
of every archived file in all listings with a regular expression.
Bringing a version back is the job of `unearth` and `fork`. There is no
picker.

## Consequences

`grep` decodes every listing. For 300 versions of 50 000 entries with
compressed listings it took about 0.2 to 0.26 s on a warm cache with 14
threads (`docs/research/listing-grep.md`).
