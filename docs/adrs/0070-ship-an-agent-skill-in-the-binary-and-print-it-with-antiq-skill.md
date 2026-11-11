# 70. Ship an agent skill in the binary and print it with antiq skill

Date: 2026-11-11

## Status

Accepted

## Context

AI coding agents run antiq for the people they work with. An agent needs
to know how antiq works and which of its commands delete folders,
versions or files on the target. That knowledge has to match the antiq
that is installed.

## Decision

antiq ships an agent skill in the Agent Skills format at
`skills/antiq/`. It is a core, `SKILL.md`, and three parts under
`references/`: `mirroring`, `import` and `damage`. The core names each
part by its file and by its command at the step that needs it.

The skill is compiled into the binary. `antiq skill` prints the core,
`antiq skill <part>` prints one part, and `antiq skill --install <dir>`
writes the core and every part to `<dir>/antiq/`, so an agent reads the
skill of the antiq that runs. `--install` replaces `references/` whole
and leaves any other file in `<dir>/antiq/` alone. `skill` needs no
collection.

Under `--json`, `antiq skill` prints the envelope with `part` and the
text in `skill`, and `--install` the files it wrote in `installed`, since
every command takes `--json`. `skill` has no `-n`: dry runs are for the
commands that change a collection or its target.

The frontmatter of `SKILL.md` carries `antiq-version`, the crate version.
A test fails until it matches, so every copy of the skill says which
antiq it belongs to.

The skill lets an agent run `status`, `find`, `grep`, `skill` and any dry
run on its own when the task concerns archived folders. Every other
command runs only on the user's explicit request and after a dry run of
the same command line, since antiq deletes folders, versions and files on
the target.

## Consequences

A release that changes a command's behaviour changes the skill in the
same work, and a release commit raises `antiq-version`.

The published crate carries `skills/`.
