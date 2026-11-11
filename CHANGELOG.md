# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/jakobwesthoff/antiq/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/jakobwesthoff/antiq/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/jakobwesthoff/antiq/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/jakobwesthoff/antiq/releases/tag/v1.0.0
