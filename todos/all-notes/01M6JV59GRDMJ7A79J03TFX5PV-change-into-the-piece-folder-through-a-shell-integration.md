---
title: "Change into the piece folder through a shell integration"
kind: feature
component: cli
horizon: someday
origin: discussion
tags: [ux]
---
# Change into the piece folder through a shell integration

After `unearth`, `fork` or `init`, the user usually wants to work in
that folder. A program cannot change its parent shell's directory, so
antiq would ship a shell function that wraps the executable and changes
directory when antiq names a target. Designed with the user in asqr
rounds 3 and 4 on 2026-11-04 around the picker of `find`. On
2026-11-04 the simplified v1 design (`docs/specs/spec-v1.md`) dropped
`find` and the picker; the user kept the idea for after v1, with `list`
and `grep` as the ways to find a piece. The picker did not come back:
`list` has since been renamed `find` (ADR 0069), without a picker.

## Design so far

- `antiq shell <zsh|bash|fish>` prints a shell function named `antiq`
  together with the tab completion for that shell. The user puts
  `eval "$(antiq shell zsh)"` into the shell's rc file.
- The shell code lives in the repository under `shell/`
  (`antiq.zsh`, `antiq.bash`, `antiq.fish`) and is embedded into the
  executable at compile time (`include_str!`). The completions come
  from the command definition.
- The function creates an empty temp file, runs `command antiq "$@"`
  with `ANTIQ_CD_FILE` set to it, and afterwards changes into the
  directory antiq wrote there, if any. stdout, stderr and the exit
  code stay as the executable left them. The pattern follows yazi
  (`--cwd-file`) and broot.
- antiq writes the absolute target folder into `ANTIQ_CD_FILE` after
  `unearth`, after `fork` (the folder of the fork, also after `--name`)
  and after `init` (the new collection). Nothing else writes it.
- Without the integration, those commands print a copy-friendly line
  with the folder, such as `cd '/Users/jakob/projects/scan-tools'`, on
  a terminal and not under `--json`.

## Open points

- Whether a command that only changes into a live piece found by a
  search word (such as `antiq cd <word>`) is wanted.
- Quoting of the path in the hint line and in the file (newlines in
  names), and a path that cannot be written to the file.
- Fish's function and completion syntax.
- Spec "Command line" and README sections; an ADR for the protocol.

## Revisit when

v1 of the simplified concept is implemented.
