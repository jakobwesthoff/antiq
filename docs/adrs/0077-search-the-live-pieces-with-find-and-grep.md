# 77. Search the live pieces with find and grep

Date: 2026-11-23

## Status

Accepted

Amends [21. Search with list and grep](0021-search-with-list-and-grep.md)

Amends [69. Make find the command and keep list as its alias](0069-make-find-the-command-and-keep-list-as-its-alias.md)

## Context

`find` and `grep` read the store only. A folder of the collection that has
no version, because it was never archived or because its last version was
unearthed, is found by neither, although it is in the collection.

## Decision

`find` and `grep` search the live pieces as well as the archived ones. A
live piece is a folder the patterns of the collection select. A search
finds everything the query matches, archived or live.

`--live` restricts a search to the live pieces and `--archived` to the
store. Both together are a usage error. Commands that need a version stay
on the store.

`find` lists a live piece as a row of its own. Live and archived pieces
share one ranking. A piece that is live and has versions lists its live
row directly before its versions. `find` walks nothing. Without a query it
lists every piece, live ones included.

`grep` searches in a live piece the files an archive of it would hold: it
walks the piece with the same rules as archiving, so ignored paths are not
searched. A problem in the walk of one piece is a warning that names the
piece and the reason. `grep` shows `live` as it shows a version.

The help line of `find` reads "Find and list the pieces and their
versions". The help line of `grep` reads "Search the file paths of live
pieces and archived versions".

## Consequences

A script that reads `stamp` or `version` from `find --json`, or the stamp
field of a `find` or `grep` pipe line, meets `null`, `-` or `live` for a
live piece. It needs `--archived` to see versions only.

`grep` walks every live piece whose name the pattern does not match, on top
of reading the listings.

A search that runs while `archive` or `unearth` finishes between the
reading of the live pieces and the reading of the store can miss the piece
that run moved.
