# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `.antiq/README.md` tells whoever finds the folder what it is, what antiq
  is and where to get it, the main commands, how to read a version with
  `zstd`, `tar` and `shasum`, and that an AI agent runs `antiq skill`.
  `init` writes it. Every run that takes the lock writes it again when it
  is missing or differs from the text of the antiq that runs, so a
  collection made by an older antiq gets it with its first run, which
  reports `repaired: write .antiq/README.md`. A dry run lists the write
  too. antiq owns the file, so a change made by hand is replaced.

## [1.4.1] - 2026-11-12

### Fixed

- `rebuild` names the listing and the checksum file it writes by the
  bytes of the archive's file name. An archive name that is not UTF-8 no
  longer gets sidecars under a name with U+FFFD in it, and its checksum
  file names the archive, so `shasum -a 256 -c` finds it. A checksum file
  that an earlier `rebuild` wrote for such a name names a different file
  than the archive, so the version is now reported as damaged and `push`
  refuses it: delete that checksum file and run `rebuild` again, which
  checks the archive against the listing that is still there. A name with
  a line break is refused by `rebuild`, also in a dry run, instead of
  failing while the listing is written or leaving a checksum file that
  antiq reads as damaged

## [1.4.0] - 2026-11-12

### Added

- `antiq skill` prints an agent skill, a guide for AI coding agents to
  antiq's commands, their JSON output and which of them delete
  something. `antiq skill <part>` prints one of its parts (`mirroring`,
  `import`, `damage`), and `antiq skill --install <dir>` writes the skill
  to `<dir>/antiq/`, for a skills directory such as `.claude/skills`.
  `antiq skill` and `antiq --version` warn when they find an installed
  copy of the skill that is older than the antiq that runs

### Changed

- A `%` in a path or name is shown as `%25` in all text output, and a byte
  that is not UTF-8 as `%XX`, so that a shown name stands for exactly one
  name. The prose texts of `--json` carry the same form: the `message` and
  `fix` of errors and warnings, the `text` of refusal reasons, the
  `message` of skipped entries, the `damage` and `reasons` of versions and
  the notes. The structured fields (`piece`, `path`, `candidates`,
  `source`) are unchanged
- `grep` shows the piece and the path by the same rule on a terminal and in
  a pipe: a control character as `%XX` of its bytes, a `%` as `%25` and a
  byte that is not UTF-8 as `%XX`. A pipe line used to carry the raw bytes
  and a terminal the escapes of a listing (`\n`, `\t`, `\\`), so a newline
  in an archived path split a hit into two lines. The pipe and terminal
  forms now show a backslash as it is, and the match is still found in the
  raw path
- A pasteable command that names a path with a control character writes it
  in the `$'...'` quoting of a shell, as it did for a path that is not
  UTF-8, so the command names the exact path. The commands of the status
  fix, the refused push, the unpin hint of an archive card and the "did you
  mean" hint wrote a piece with a `%` as `%25`, which named another piece;
  they write the piece itself. A word that starts with `=` is quoted
- `archive -n` names, under each piece it would archive, the entries the
  version leaves out, as `import -n` does: a `Would leave out <path>` line
  for each ignored entry (a folder once, with a trailing `/`) and for
  `.antiq-restored`, and a `left_out` list in the action of `--json`.
  These entries are deleted with the folder without a copy. A real run
  prints what it printed before

### Fixed

- Error output shows a path or name with a control character, a bidi
  control or U+2028 and U+2029 in the same escaped form as the result
  lines, in the title, the text, "How to fix it", the candidates and the
  lines that rsync and other programs said, and in the usage error for an
  argument of the command line. A newline in a name could forge a line of
  an error, such as an `error:` line in a pipe
- An error message of several lines, such as the explanation of an invalid
  `config.toml`, the names a volume cannot hold or the regex error of
  `grep`, shows its first line as the title and the lines after it below.
  On a terminal they are lines under the title, in a pipe they are indented
  by two spaces under the `error:` line, and the `message` of `--json` keeps
  its line breaks. They were one line with `%0A` for each break
- A `%` or a byte that is not UTF-8 in a name shows as `%25` or `%XX` in the
  messages about damaged and doubled version files, a refused `forget`, a
  push refusal that names files to delete, the files of `rebuild` and
  `forget`, the skills directory of the outdated-skill warning, and the
  `-C` directory and typed piece of a command written to paste, which
  keep a name that is not UTF-8 by its bytes in `$'...'`
- `import -n` lists an ignored folder once when an ignored name sorts
  between the folder and its content, such as `cache.log` next to
  `cache/`. The content of the folder was listed as well
- A control character in a path, such as a newline or ESC in a file name,
  or a bidi control or the separators U+2028 and U+2029, is shown as `%`
  and the hex digits of its bytes in the `Would ...` lines of a dry run,
  in the result lines of `archive`, `import`, `push` and `init`, in notes
  and warnings, and in the fix lines of `status`, as `find` already did. A
  newline in a name could forge a line of a dry run, such as a `Would
  archive` line. `--json` carries the `message` and `fix` of errors and
  warnings in that form too; its other fields are unchanged

## [1.3.0] - 2026-11-11

### Changed

- `list` is now `find`, and its help line reads "Find and list the
  versions of pieces". `list` still works as an alias, which the help does
  not show. The `--json` envelope carries `"command": "find"` instead of
  `"command": "list"`, also for a run through `list`, so a script that
  checks for `list` has to change
- `find` marks the letters of each piece path that the query matched in
  bold red on a terminal with colour. The output without colour, in a
  pipe and with `--json` is unchanged

### Fixed

- A search word also matches a piece path that differs from it only in
  Unicode composition, such as a typed `é` against an `e` followed by a
  combining accent, and the letters it matched are marked on that path
- The closest pieces offered for an unknown search word include a path
  that differs from the word only in Unicode composition
- The tables of an ambiguous piece reference mark the letters the search
  word matched on the right letters when the piece path holds a control
  character or a decomposed accent

## [1.2.0] - 2026-11-06

### Added

- `push --take-over` rewrites the marker of the target to this machine and
  this collection path when it names this collection on another machine or
  at another path, and pushes. A marker of another collection stays
  refused

## [1.1.0] - 2026-11-06

### Added

- `import` shows a progress card on a terminal, as `archive` does. A pipe,
  `--json` and a dry run print what they printed before

## [1.0.0] - 2026-11-06

The first release of antiq: it keeps the idle folders of a collection as
`tar.zst` archives, finds them again and mirrors the collection to one
target.

### Added

- `init` makes a directory a collection, with a commented configuration
  in `.antiq/config.toml`
- `status` lists the pieces that are due, soon, pinned or refused, each
  with its reason. A piece is due when its newest file has not changed
  for 30 days (`idle_days`)
- `archive` packs every due piece, or one piece now, into a `tar.zst`
  version, reads the archive back and compares it with the folder, and
  only then removes the folder. A folder that changed in the meantime is
  skipped
- `pin` and `unpin` keep a piece from being archived with a `.antiq-pin`
  file. `collection.ignore`, `collection.timeless`, `collection.exclude`
  and the `.antiqignore` and `.antiq-timeless` files decide what is packed
  and what counts for the idle check
- `list` lists the versions, `grep` searches the paths of all archived
  files
- `unearth` brings a version back and removes it from the collection,
  `fork` extracts a working copy and keeps the version, `forget` deletes
  versions
- `import` packs a folder from elsewhere as a version
- `rebuild` repairs a damaged version whose listing or checksum file is
  missing or does not match
- `push` mirrors the live folders and the versions to a local or ssh
  target with rsync 3.2.4 or newer. `--init` claims the target, and
  `--if-reachable` lets an unattended run skip an unreachable one
- A failed `push` names a compressed log of everything rsync printed
  (`zstd -dc <path> | less` reads it) and groups similar error lines
  under the exit code's meaning. antiq keeps the last 10 logs of a
  collection and the newest one of a failed push
- Progress cards with a bar for every piece on a terminal, plain lines in
  a pipe, and one JSON object with `--json`
- `-n` / `--dry-run` on every command that changes something
- Archives and listings are readable without antiq: `tar.zst` files and
  checksum lists that `shasum -c` reads once `zstd` has decompressed them
- `antiq --version` shows Archie beside the version on an interactive
  terminal with 24-bit colour, and the plain version in a pipe
- macOS and Linux

[Unreleased]: https://github.com/jakobwesthoff/antiq/compare/v1.4.1...HEAD
[1.4.1]: https://github.com/jakobwesthoff/antiq/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/jakobwesthoff/antiq/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/jakobwesthoff/antiq/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/jakobwesthoff/antiq/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/jakobwesthoff/antiq/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/jakobwesthoff/antiq/releases/tag/v1.0.0
