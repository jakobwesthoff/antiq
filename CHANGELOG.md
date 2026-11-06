# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The first release of antiq: it keeps the idle folders of a collection as
`tar.zst` archives, finds them again and mirrors the collection to one
target.

### Added

- `init` makes a directory a collection, with a commented configuration
  in `.antiq/config.toml`
- `status` lists the pieces that are due, soon, pinned or refused, each
  with its reason; a piece is due when its newest file has not changed for
  30 days (`idle_days`)
- `archive` packs every due piece, or one piece now, into a `tar.zst`
  version, reads the archive back and compares it with the folder, and
  only then removes the folder; a folder that changed in the meantime is
  skipped
- `pin` and `unpin` keep a piece from being archived with a `.antiq-pin`
  file; `collection.ignore`, `collection.timeless`, `collection.exclude`
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
  target with rsync 3.2.4 or newer; `--init` claims the target,
  `--if-reachable` lets an unattended run skip an unreachable one
- A failed `push` names a compressed log of everything rsync printed
  (`zstd -dc <path> | less` reads it) and groups similar error lines
  under the exit code's meaning; antiq keeps the last 10 logs of a
  collection
- Progress cards with a bar for every piece on a terminal, plain lines in
  a pipe, and one JSON object with `--json`
- `-n` / `--dry-run` on every command that changes something
- Archives and listings are readable without antiq: `tar.zst` files and
  checksum lists that `shasum -c` reads once `zstd` has decompressed them
- `antiq --version` shows Archie beside the version on an interactive
  terminal with 24-bit colour; in a pipe it prints the plain version
- macOS and Linux
