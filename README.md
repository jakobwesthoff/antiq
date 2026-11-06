<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/emblem/archie-ledge-rim-light.svg">
    <img src="assets/emblem/archie-ledge.svg" width="256" height="256" alt="antiq's logo: Archie, a pangolin with obsidian scales and a gold face, peeking over a ledge with its paws on the edge">
  </picture>
</p>

# antiq

Your idle folders, kept like antiques.

antiq is a command-line tool written in Rust for directories where
working folders pile up, such as `~/playground`. It archives the folders
of a collection that nobody has touched for a while, finds them again by
name or by the files they hold, brings them back, and mirrors the whole
collection to one backup target. Everything it writes can be read
without antiq: archives are `tar.zst` files, and a listing is a checksum
list that `shasum -c` reads once `zstd` has decompressed it.

The pangolin in the logo is [Archie](assets/archie-512.webp), antiq's
mascot: a small idol of obsidian and gold. Archie shares its name with
the first internet search engine, released in 1990, which indexed the
files on FTP archives so people could find them again. The search
engine's name, too, was a shortening of "archive".

## Installation

Build from source with a Rust toolchain of version 1.97 or newer:

```bash
git clone https://github.com/jakobwesthoff/antiq
cd antiq
cargo install --path .
```

`antiq push` needs rsync 3.2.4 or newer on the machine that runs it.
The `rsync` that macOS ships is openrsync and is passed over; install the
real one, for example with `brew install rsync`. antiq takes the first
`rsync` that is new enough from `PATH`, then `/opt/homebrew/bin` and
`/usr/local/bin`. `[target] rsync` names one by its absolute path.

## Quick start

Make a directory a collection. `--pattern` says which folders are pieces,
here the folders two levels down:

```console
$ cd ~/playground
$ antiq init --pattern '*/*'
Created the collection at /home/me/playground
  id: 01M6MC8YED87SF9J43N47W07JE
  patterns: */*
The next archive would archive 2 folders.
```

See what is due, then archive it. A folder that nobody has touched for
30 days is due:

```console
$ antiq status
3 folders · 0 versions (0 B) · archive after 30 days idle

STATE    PIECE             IDLE  WHAT
due      projects/scanner  430d  archives next run
due      tools/scan-old    430d  archives next run
$ antiq archive
archived projects/scanner
archived tools/scan-old
Archived 2, skipped 0, refused 0.
```

Find a version by piece name or by the files it holds, and bring it back:

```console
$ antiq list scan
PIECE              VERSION               SIZE    FILES
projects/scanner   2026-11-05 01:09:48   120 B       1
tools/scan-old     2026-11-05 01:09:48   124 B       1
$ antiq grep 'main\.rs'
projects/scanner   1 version, 2026-11-05
  src/main.rs      2026-11-05
tools/scan-old     1 version, 2026-11-05
  cli/main.rs      2026-11-05

2 files in 2 pieces · antiq unearth <piece> [--version <version>]
$ antiq unearth scan-old
Unearthed tools/scan-old (version 2026-11-05 01:09:48)
```

To mirror the collection, add a `[target]` block to
`.antiq/config.toml`, create an empty folder on the target, and let
`push --init` claim it:

```toml
[target]
url = "/Volumes/nas/antiq/playground"
```

```console
$ antiq push --init
Claimed the push target /Volumes/nas/antiq/playground for this antiq collection.
Changed on /Volumes/nas/antiq/playground:
  projects/antiq: 1 created, 0 updated, 0 deleted
  projects/scanner: 3 created, 0 updated, 0 deleted
  tools/scan-old: 2 created, 0 updated, 0 deleted
  other files: 2 created, 0 updated, 0 deleted
```

<!-- docs:start -->
## Documentation

### Concepts

- **Collection:** a directory with a `.antiq/` folder. Commands find it
  by searching upwards from the current directory, or from the path
  given with `-C`.
- **Piece:** a folder of the collection that antiq manages. The
  `patterns` of the configuration select pieces; its path relative to
  the collection is its identity.
- **Version:** one archived state of a piece. A piece can have several
  versions side by side.
- **Target:** the backup location, an absolute path (a mounted disk or
  share) or `[user@]host:path` reached over ssh by rsync.

### Commands

```text
$ antiq --help
Usage: antiq [OPTIONS] <COMMAND>

Commands:
  init     Make a directory a collection
  status   Show the pieces that have a state
  archive  Archive due pieces, or one piece now
  unearth  Bring a version back and remove it
  fork     Extract a working copy of a version
  forget   Delete a version, or all of them
  rebuild  Repair a damaged version
  import   Pack a folder as a version
  list     List the versions
  grep     Search the paths of all archived files
  pin      Keep a piece from being archived
  unpin    Let a piece be archived again
  push     Mirror the collection to the target

Options:
  -C <PATH>      Run as if started in this directory
      --json     Print the result as one JSON object and never ask a question
  -h, --help     Print help
  -V, --version  Print version
```

Every command takes `-C <path>` and `--json`. Commands that change
something take `-n` / `--dry-run`, which prints the plan and changes
nothing. No command asks a question.

A piece is named by its path or by a search word. A search word must
match exactly one piece. The command fails when it matches none, and
when it matches several it also lists the candidates.

### Configuration

The configuration is `.antiq/config.toml`. `init` writes it with a
comment for every key, and with every key but the collection ID (and
`patterns`, when `--pattern` is given) commented out at its default.

| Key | Default | Content |
|-----|---------|---------|
| `collection.id` | minted at `init` | collection ULID, also written to the target's marker |
| `collection.patterns` | `["*"]` | glob patterns that select pieces; the shallowest match wins |
| `collection.exclude` | `[]` | literal paths that are never pieces |
| `collection.ignore` | `[".DS_Store"]` | gitignore lines for paths that are not packed, not listed and not counted for the idle check |
| `collection.timeless` | `[]` | gitignore lines for paths that are packed but not counted for the idle check, such as `.git/` |
| `collection.idle_days` | 30 | days a piece must be idle before it is due |
| `collection.soon_days` | 3 | days before due from which `status` shows a piece as `soon` |
| `target.url` | none | an absolute path or `[user@]host:path` |
| `target.rsync` | the first suitable `rsync` on `PATH`, then in `/opt/homebrew/bin` and `/usr/local/bin` | absolute path of the local rsync |
| `target.rsync_path` | none | the rsync on the target, passed as `--rsync-path` |
| `target.fsync` | `true` | pass `--fsync` to rsync |
| `target.rsync_args` | `[]` | extra rsync options, one string each |

A folder whose name starts with a dot is a piece only if a pattern names
the dot. Symlinks are never pieces. Besides the configuration, a piece
can carry `.antiqignore` and `.antiq-timeless` files with the same
gitignore lines as `ignore` and `timeless`.

### Archiving

A piece is due when its idle clock is older than `idle_days`. The idle
clock is the newest modification time of the piece's regular files,
leaving out ignored and timeless paths and antiq's own control files,
or the time `unearth` or `fork` wrote into the piece, if that is later.
`status` lists the pieces that are `due`, `soon`, `pinned` or `refused`
with the reason.

`archive` packs each due piece into a `tar.zst` version, reads the
archive back and compares it with the folder, and only then moves the
folder away and deletes it. A folder that changed meanwhile is skipped
and its files are kept. `archive <piece>` archives one piece now, however recently it
changed.

`pin <piece>` creates a `.antiq-pin` file in the piece, which keeps it
from being archived while `push` still mirrors it. `unpin` removes it.
`import <folder> [--as <path>]` packs a folder from outside the
collection directly as a version and leaves the source in place.

A piece is refused, and never archived, when its folder holds a mount
point, a nested collection, an unreadable or locked entry, an exclude
entry, or a control file that is not a regular file. `status` names the
reason. A piece without a file that counts for
the idle check is empty and is not archived either.

### Finding and bringing back

`list [<query>]` prints one row per version, newest first within a
piece. `grep <pattern>` matches a regular expression against the paths
of all archived files, read from the listings, so nothing is unpacked.
Without a terminal both print one tab-separated line per version or
hit.

`unearth <piece> [--version <version>]` brings a version back as a
folder at the piece's path and **removes that version** once the folder
is complete and verified. `fork <piece> [--version <version>] [--name
<new path>]` extracts a working copy and keeps the version. Without
`--version`, the newest version is taken. A version is named by a
prefix of its time, such as `2026-11-05` or `2026-11-05T00`.

`forget <piece> <version>` deletes one version; `forget <piece>
--all-versions --force` deletes all of them.

### Damaged and incomplete versions

A version is incomplete when its listing or checksum file exists without
the archive. It is damaged when the archive lacks its listing or
checksum file, when its size differs from the one the listing records,
or when a sidecar does not parse. Files deleted by hand and incomplete
copies from the target cause this. `list` marks such versions and
`status` reports them.

`rebuild <piece> <version>` writes the missing sidecar of a damaged
version after checking the archive against what survives, and
`forget` removes an incomplete one. A damaged or incomplete version
stops `push`, scheduled runs included, until you run `rebuild` or
`forget` or copy the missing file back from the target. The error names
the command:

```console
$ antiq push
error: Push refused: the store holds versions that are damaged or incomplete
  why: push copies the store as it is, so it only runs on a store without damage.
  fix: Repair or remove these versions first:
         projects/scanner 2026-11-05T000948: the listing is missing. Run `antiq rebuild projects/scanner 2026-11-05T000948`.
  while attempting to push the antiq collection
$ antiq rebuild projects/scanner 2026-11-05
Rebuilt version 2026-11-05 01:09:48 of projects/scanner: wrote projects-scanner--2026-11-05T000948.sha256.zst
note: the archive is the one checksummed when it was stored
```

### Setting up a target

Put `[target] url` into the configuration, create an empty folder on the
target, and run `antiq push --init`. It writes the marker
`.antiq-target` at the target's root and mirrors the collection. A path
target must be a folder on a volume, not the volume's root, and must not
lie inside the collection. `push` without a `url` fails with the error
kind `no_target`.

A path target that does not exist, or is an empty folder without a
marker, counts as unreachable: `push` exits 4, or 0 with
`--if-reachable`, and says why. A missing folder of a path target is
never created by antiq. Over ssh, `push --init` creates the last
missing level.

For an ssh target, `url = "backup@nas.local:/volume1/antiq/projects"`,
the ssh options such as a port go into `~/.ssh/config`. ssh is the
recommended way to reach a NAS.

### Pushing

`push` runs rsync once with `-a -H -x --delete --delete-delay`, so the
target ends up as a copy of the collection: live folders, versions and
everything else in it. Left out are antiq's temporary folders and lock
and the macOS volume-root folders. Over ssh the run uses `BatchMode`.
`push -n` lists what would change, per piece, and changes nothing.
`push --init` and `push --force` cannot be combined with
`--if-reachable`.

The marker `.antiq-target` holds the collection ID, the machine name and
the collection path. Before every push antiq compares it with the
collection:

- A target with a marker of another collection is refused.
- A marker that names another machine or another collection path is
  refused, and the message names both. `push --force` pushes anyway,
  leaves the marker as it is and prints a note naming its host and path.
- A non-empty folder without a marker is refused, and `--init` is
  refused there too, as it is wherever a marker exists.

antiq never rewrites the marker. After renaming the machine or moving
the collection, edit the marker's `host` or `path` by hand, or push with
`--force` each time.

### Good to know

**The target is a mirror, not a backup.** `push` has no brake: whatever
is deleted in the collection, by antiq or by hand, is deleted on the
target by the next push, and the target keeps no history.

**What is not kept.** Extended attributes, ACLs, access times and
creation times are kept neither in versions nor on the target. Hard
links are kept in versions and in the mirror. Copying the target back
keeps them only with `rsync -aH`. Sparse files are stored in full in
versions and come back fully allocated.

**Symlinks on the target.** A symlink in a piece is copied as a symlink
and points to whatever its path names on the target.

**Checking a version by hand.** The files of a version are in
`.antiq/pieces/<piece>/`. Unpack the archive into a scratch directory,
go into the unpacked folder and check it against the listing:

```console
$ store=~/playground/.antiq/pieces/projects/scanner
$ mkdir /tmp/check && cd /tmp/check
$ zstd -dc $store/projects-scanner--2026-11-05T000948.tar.zst | tar -x
$ cd scanner
$ zstd -dc $store/projects-scanner--2026-11-05T000948.sha256.zst | shasum -a 256 -c -
src/main.rs: OK
```

Use `shasum`: the `sha256sum` that macOS ships is not GNU's and fails
on a listing.

**rsync compares only what it transfers.** rsync checks a whole-file
checksum of each file it sends. A file whose size and modification time
match is not sent and not compared, so damage on the target stays
unseen. antiq reads nothing back from the target.

**`fsync`, macOS and other targets.** With `--fsync` the target flushes
each file before it is renamed into place, so a power loss can at most
lose the newest names, which the next push sends again. Without it,
files that rsync reported as written can be empty or missing. A target
whose rsync is openrsync, which is every macOS machine, lacks `--fsync`
and needs `fsync = false`. Either set `rsync_path` to a 3.2.4 or newer
rsync there, or turn it off. Extra rsync options go into `rsync_args`
and override antiq's own: `rsync_args = ["--no-perms"]` follows `-a` and
stops the permissions from being set. `rsync_args` must not hold `-q` or
`--quiet`.

**Case-only renames and strict targets.** A folder renamed only in
case is copied in a second pass on a case-insensitive target. The
target must be as strict about names as the collection's volume. `push`
refuses while two piece paths of the collection differ only in case or
Unicode normalization. File names inside one live folder that differ
only in case are not checked.

**Mount points.** `push` stays on the collection's device (`-x`). A
directory that held files and later became the mount point of another
device arrives on the target as an empty directory, and its former
files are deleted from the target.

**Unreadable directories stop deletions.** When rsync cannot open a
directory of the collection, it deletes nothing on the target at all
until the directory is readable. `push` says that no deletion reached
the target and names the directories.

**Scheduling.** Run the two commands one after the other:

```bash
antiq archive; antiq push --if-reachable
```

The push runs after a partial archive too. The scheduler sees only the
push's exit status. `--if-reachable` makes an unreachable target exit 0
instead of 4. A held lock, a damaged version or any failure after the
target was reached still exits 1.

### Exit codes

| Exit | Meaning |
|------|---------|
| 0 | success |
| 1 | error, a held lock included |
| 2 | usage error |
| 3 | partial: a step failed for some pieces, or an `unearth` or `fork` that did not finish |
| 4 | target unreachable (`push` only) |

`--json` prints the result as one JSON object, errors included, and
never asks a question.
<!-- docs:end -->

## Development

The tests need rsync 3.2.4 or newer, found in `PATH`,
`/opt/homebrew/bin` or `/usr/local/bin`, `shasum`, and a GNU
`sha256sum` (named `gsha256sum` by brew's coreutils on macOS). They fail
with a message naming the missing tool.

The tasks live in the `Justfile` (`just --list` shows them). `just check`
runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
`cargo test`; every commit has to pass it.

Coverage needs `cargo-llvm-cov`. After a change, `just coverage
<base-commit>` lists the changed lines under `src/` that no test runs.

`docs/specs/spec-v1.md` specifies version 1, `docs/adrs/` records the
decisions behind it and `docs/research/` the research. Deferred work is
in the todo vault at `todos/`.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE).
