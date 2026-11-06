# 66. Run external programs through one runner that logs every run and summarises their errors

Date: 2026-11-06

## Status

Accepted

## Context

`push` recognised the failures of rsync by the exact texts of its error
output (`IO error encountered`, `unrecognized option`, `mkstemp ... Operation
not permitted`). Another rsync version words a line differently and the
recognition is gone. Every other failure showed rsync's raw lines, one for
each file. After an error nothing showed what rsync had printed during the
run.

## Decision

One runner starts every external program. Today that is rsync, which starts
ssh for an ssh target, so ssh's messages arrive in rsync's stderr. The runner
hands the standard output to its caller in segments while the program runs,
reads the standard error line by line, and returns the exit code or the
signal, the duration and both outputs.

**Run log.** Every antiq run that starts an external program writes one log.
Every call of the run appends to it: the command line, the start time, each
line of both streams marked by its stream and its offset from the start of
the call, the exit code or the signal, and the duration. A line carries the
time its first byte arrived, so a progress line can appear in the file after
a later error line. The log is complete: no line is left out and no size is
capped. The program's environment is not logged.

**Place.** On macOS the logs are in `~/Library/Logs/antiq`, where Console.app
shows them. Elsewhere they are in `antiq` in the state directory
(`$XDG_STATE_HOME`, else `~/.local/state`). The `directories` crate finds the
home and the state directory. Each collection has a directory of its own,
named by the collection's folder name and the first 8 hex digits of the
SHA-256 of its canonical path. A file is named `<command>-<local time>.log.zst`
and is a zstd stream, read with `zstd -dc <path> | less`. Directories are
private to the user (mode 0700) and files too (0600), as a log lists every
path of the collection.

**Kept logs.** antiq keeps the 10 newest logs of a collection. A run that
fails renames its log to end in `-failed.log.zst`, and the newest such log is
kept beyond the 10, so a collection holds at most 11. A log that cannot be
written is a warning, never a failure of the command.

**Stderr summary.** The lines on stderr are grouped. Each double-quoted part is
replaced by `"…"`, and lines with the same text after that form one group, in
order of first appearance. An error shows for each of the first 5 groups the
count, the line and what the first line of the group quoted, then how many
more kinds of lines there were, then the path of the log. The JSON error
carries all groups.

**Recognition.** The exit code decides that a run failed. The title of the
error names the meaning `rsync(1)` documents for the code (exit 23: "some
files or attributes were not transferred"; 255, which is ssh's code: "ssh
reported an error"). The special cases stay as extras that add a title or a
fix when their pattern matches: 24 (vanished files), 23 with `IO error
encountered`, 12 with an unknown `--fsync`, and a receiver that refuses
`mkstemp`, `chmod` or setting permissions, with the fix `rsync_args =
["--no-perms"]`. When a pattern does not match, the generic title, summary
and log appear.

## Consequences

The log of a run that is killed holds everything up to the last flush, and
`zstd -dc` reads such a file up to that point. Stderr lines and the start and
the end of each call are flushed at once. Standard output is flushed when a
line arrives a second or more after the last flush, so the last lines of a
program that then goes quiet can be missing.

The observed mirror lists every entry of the collection, so its log holds a
line for each entry on every push. Compression is the only limit on the size.

The JSON error of a failed push has `program_said` and, where a log exists,
`log` and `log_warning`.
