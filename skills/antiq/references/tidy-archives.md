Part of the antiq skill. Read SKILL.md first, which holds the rules for every task; `antiq skill` prints it.

# antiq: keeping the rule files of a piece

Two rule files in a piece steer what an archive keeps. Both take
gitignore lines. Read this part before you write or change one, or
propose one to the user.

Everything under "We recommend" is a recommendation: what to propose or
write when the user has not said otherwise. The user's explicit wishes
come first, and so do the entries already in the piece's rule files and in
the `ignore` and `timeless` lines of `.antiq/config.toml`. Do not remove
or "correct" a line of the user's to fit the recommendation. Ask before
you would go against one.

## What the files do

- `.antiqignore`: what it matches is not packed and does not count as
  activity, and `archive` deletes it with the folder, without a copy.
- `.antiq-timeless`: what it matches is packed and kept, but its changes
  do not count as activity. It cannot keep a finished piece from going
  idle.

A path that both files match is ignored, so it is deleted.

- A rule file works at any level of the piece. Its lines are anchored at
  its own directory: `/target/` matches only there, `target/` at any depth
  below. A pattern with a `/` in the middle is anchored too.
- The nearest file wins, then those further out, then the `ignore` and
  `timeless` lines of the config; within one file the last matching line
  wins.
- A directory line hides everything below it, and a `!` line cannot bring
  back a path inside it. The rule files of an ignored directory are not
  read.
- A line antiq cannot read, or a rule file that is not UTF-8, refuses the
  whole piece: it is never due and `archive <piece>` does not archive it.

## We recommend for `.antiqignore`

Ignore only material that a command or a stable source brings back with
the same content, not byte for byte: a rebuilt jar counts. `archive`
deletes ignored paths without a copy, so a path that cannot be brought
back is lost. Typical:

- build output whose inputs and command the piece keeps, with its lock
  file or manifest present: `target/`, `build/`, `dist/`, `node_modules/`,
  `.venv/`, `__pycache__/`;
- dependency folders and caches that the tool fetches again:
  `.gradle/`, `.pytest_cache/`, `.mypy_cache/`.

A fetched archive whose lock file is missing does not count, since a
rebuild would fetch another version.

We recommend keeping out of `.antiqignore` anything that cannot be
brought back:

- logs, notes, plans, reports and measurements;
- API replies and model output, and every other run that cannot be
  repeated;
- copied material whose source can vanish;
- every git repository (clone, worktree and `.git/`), pushed or not,
  public or private, since a remote can be deleted, rewritten or become
  unreachable. Build output inside a clone, such as its `target/`, may
  still be ignored by the points above.

## We recommend for `.antiq-timeless`

Use it for data that is kept but rewritten by tools while the work stands
still. The case is the `.git/` of a clone with a remote: every `git fetch`
touches it, so without the line a finished piece never goes idle. A clone
without a remote needs no line.

## We recommend for keeping the files

- When unsure, leave a path in: a needless archive costs space, a wrong
  line costs data.
- Put a comment line above each entry or group that says what brings it
  back, for example `# rebuilt by cargo build`, or why the file is
  timeless, for example `# git fetch rewrites it`.
- Write plain patterns, since one line antiq cannot read refuses the
  piece.
- Do not repeat the `ignore` and `timeless` lines of the collection's
  config, and do not edit `.antiq/config.toml`: it is the user's.
- Add a line when the folder gains such a path and remove it when the path
  goes or changes its kind. This applies to lines you wrote, not the
  user's.
- After each change, run
  `antiq -C <collection root> archive -n --json <piece>`. In the action of
  the piece, `left_out` should list exactly the paths you meant to leave
  out, and `refused` should be empty. Do not run `archive` itself to test
  it.
- Tell the user what a new line will delete without a copy when the
  folder is archived.
- A piece that is already archived does not change when its live folder's
  rule files change. They take effect once the piece is unearthed and
  archived again, which is the user's call.

End of references/tidy-archives.md.
