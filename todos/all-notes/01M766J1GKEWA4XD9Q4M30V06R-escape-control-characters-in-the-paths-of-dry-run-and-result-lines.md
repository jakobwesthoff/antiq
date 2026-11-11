---
title: "Escape control characters in the paths of dry-run and result lines"
kind: bug
component: cli
origin: implementation
tags: [ux, security]
---
# Escape control characters in the paths of dry-run and result lines

Found on 2026-11-11 while `archive -n` learned to list the entries a
version leaves out. A path that holds a control character, such as a
newline or ESC in a file name, is printed raw in the lines of a dry run.
A newline splits the line, so a file name can forge a line of antiq's
output; ESC can change what a terminal shows. A person or an agent that
reads the output then acts on a line antiq never wrote.

## Problem

Reproduced on 2026-11-11 in a throwaway collection: a piece `old` with an
ignored file named `evil` + newline + `Would archive fake` makes
`antiq archive -n` print, on a pipe:

```text
Would archive old
Would leave out .DS_Store
Would leave out evil
Would archive fake
```

The `Would leave out` lines of `archive -n` and `import -n` and the
`Would archive <piece>` lines print the path as it is. Other lines that
name a path may do the same; that has not been checked.

`find` already escapes such characters: `src/cli/table.rs` (around line
106) shows a control character as `%` and the hex digits of its bytes,
on a terminal and in a pipe, as the spec's section "find" says. The
escaped form of `src/relpath.rs`, `escape`, does not touch control
characters, since valid UTF-8 stays as it is.

## Impact

An agent that follows the antiq skill reads the dry run to decide what it
may run. A forged line can make it, or a person, believe the plan holds
something it does not.

## Suggested fix

Show every path in text output (terminal and pipe) with its control
characters escaped the way `find` does, in every line that prints a path:
start with the dry-run and result lines of `archive`, `import`,
`unearth`, `fork`, `forget`, `rebuild`, `pin`, `unpin` and `push`, and
check the notes and warnings that name paths. `--json` already carries
paths in their escaped form or as JSON strings, which a newline cannot
break; leave it unchanged. Update the spec where it describes these lines.
