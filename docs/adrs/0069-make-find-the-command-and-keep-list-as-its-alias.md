# 69. Make find the command and keep list as its alias

Date: 2026-11-11

## Status

Accepted

Amends [23. Name the commands to fit the name while staying recognizable](0023-name-the-commands-to-fit-the-name-while-staying-recognizable.md)

Amends [21. Search with list and grep](0021-search-with-list-and-grep.md)

## Context

Users kept typing `find`, `search` or similar because they did not
remember that `list` is the command that queries the piece names.

## Decision

The command that finds and lists the versions of pieces is `find`. Its
help line is "Find and list the versions of pieces".

`list` stays as an alias of `find` for backwards compatibility. The help
lists `find` alone, and the help and the usage of `list` read as those
of `find`.

`search` is not offered as a name. The help line of `grep` reads "Search
the paths of all archived files", and `antiq search main.rs` would list
pieces instead of searching archived files.

The `--json` envelope carries `find`, also for a run through `list` and
for a command line that fails to parse.

ADRs 21 and 23 stay in force, with `find` as the name of the command they
call `list`.

## Consequences

A script that checks the envelope for `"command": "list"` stops working.

A word close to `list` gets it as clap's suggestion, for example `lsit`
gets `Did you mean list?`. When a command line names no command, clap's
error lists `list` among the commands.
