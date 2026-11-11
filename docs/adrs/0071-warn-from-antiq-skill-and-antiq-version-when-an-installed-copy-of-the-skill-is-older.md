# 71. Warn from antiq skill and antiq --version when an installed copy of the skill is older

Date: 2026-11-11

## Status

Accepted

## Context

`antiq skill --install` copies the skill once (ADR 70). A copy stays as
it was when antiq is updated, and an agent runner loads the copy from its
skills directory in later sessions.

Agent runners read skills from directories of their own: `.agents/skills`,
`.claude/skills`, `.gemini/skills`, `.github/skills`, `.cursor/skills` and
`.codex/skills` in a project, and `~/.agents/skills`, `~/.claude/skills`,
`~/.gemini/skills`, `~/.copilot/skills`, `~/.cursor/skills` and
`~/.codex/skills` for a person. `CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
`COPILOT_HOME` and `GEMINI_CLI_HOME` move some of the personal ones.

## Decision

`antiq skill`, printing the core or a part, and `antiq --version` compare
the `antiq-version` of installed copies with their own version and warn
when a copy is older. No other command checks, and `skill --install` does
not either.

A copy is the directory `antiq` with a `SKILL.md` in one of the skills
directories above: the project ones in the directory the run starts in
and in each directory above it up to the git root, the personal ones in
the home directory and below a moving variable that holds an absolute
path. `--version` starts in the current directory, since it runs before
the command line, `-C` included, is read. A copy without `antiq-version`
counts as older. A copy of the same
or a newer version, one that cannot be read as a regular file and one
whose stamp is no `MAJOR.MINOR.PATCH` version give no warning. Copies
are told apart by the real path of their `SKILL.md`, and one warning
names every older copy.

The warning is a warning like the others: a `warning:` line on stderr in
text, and an entry of `warnings` with the code `antiq::skill::outdated`
under `--json`. `--version` prints text also under `--json`, so its
warning is always the line on stderr. Its fix names `antiq skill` and the `antiq skill
--install` command for each copy, and says to ask the user whether to
update it.

## Consequences

Each `antiq skill` without `--install` and each `antiq --version` looks for up to six skills
directories per directory from the start directory to the git root, six
in the home directory and one for each moving variable that is set.

A copy older than the antiq that runs goes unnoticed until someone runs
`antiq skill` or `antiq --version`.
