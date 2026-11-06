---
title: "Change into the piece folder through a shell integration"
kind: feature
component: cli
origin: discussion
tags: [ux]
---
# Change into the piece folder through a shell integration

After `find` (Enter unearths), `unearth`, `fork` or `init`, the user
usually wants to work in that folder. A program cannot change its
parent shell's directory, so antiq ships a shell function that wraps
the executable and changes directory when antiq names a target.
Decided with the user in asqr rounds 3 and 4 on 2026-11-04.

## Decisions

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
  `find` when Enter unearthed a piece, after `unearth`, after `fork`
  (the folder of the fork, also after `--name`) and after `init` (the
  new collection). Nothing else writes it.
- Without the integration, those commands print a copy-friendly line
  with the folder, such as `cd '/Users/jakob/projects/scan-tools'`, on
  a terminal and not under `--json`.
- `find --print` stays as a scripting output without side effects;
  the cd use case no longer needs it.

## Open points for the plan

- Quoting of the path in the hint line and in the file (newlines in
  names), and a path that cannot be written to the file.
- Fish's function and completion syntax.
- Spec "Command line" and README sections; an ADR for the protocol.
- Comes after the fix batch that makes `find --print` close on Enter,
  since both touch `find`.

## Relations

- Part of: [Implement antiq v1](01M6HJYBXSTQFSHP29EX3MPN97-implement-antiq-v1.md)
