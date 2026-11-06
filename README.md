<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/emblem/archie-ledge-rim-light.svg">
    <img src="assets/emblem/archie-ledge.svg" width="256" height="256" alt="antiq's logo: Archie, a pangolin with obsidian scales and a gold face, peeking over a ledge with its paws on the edge">
  </picture>
</p>

# antiq

Your idle folders, kept like antiques.

Many developers have a folder where experiments pile up: spikes,
half-finished tools, a clone made to check one thing. The same happens in
the context directories of AI conversations and coding agents, where every
run leaves transcripts, intermediate files and scratch output you may want
again. You do not delete them, so they stay, and the folder grows.

antiq turns such a folder into a collection. A folder of it that nobody has
touched for 30 days (the default, configurable) is packed into a `tar.zst`
archive, read back and compared with the folder, and only then removed. You
find it again by the name of the folder or by the path of a file it held,
and bring it back. The whole collection, live folders and versions, can be
mirrored to one target with rsync.

![antiq in a terminal: status lists three due folders, archive packs them with a progress card each, grep finds server.py in the archive of recipe-api, unearth brings the folder back, and the version screen shows Archie](docs/pages/assets/shots/demo.gif)

antiq runs on macOS and Linux. Everything it writes can be read without it:
archives are `tar.zst` files, and a listing is a checksum list that
`shasum -c` reads once `zstd` has decompressed it.

## Where it helps

- **The folder where your experiments pile up.** Whatever its name, a
  folder with one subfolder per experiment is a collection. Old
  experiments leave the folder, and the search by name or by file finds
  them again.
- **The context directories of AI conversations and agent work.** Each run
  of a coding agent or each conversation leaves a folder of transcripts and
  intermediate data. A pattern in the collection's configuration selects
  whichever folders your tools write, and the ones nobody has touched for a
  while are archived instead of deleted.

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

<!-- docs:start -->
## A collection

A collection is a folder that antiq looks after. Each folder inside it that
you want archived is a piece. To try antiq without touching your own
folders, paste this. It makes a collection of six pieces, with file times
that put them 2 to 200 days in the past:

```bash
mkdir experiments && cd experiments
age() { touch -t "$(date -d "$1 days ago" +%Y%m%d%H%M 2>/dev/null || date -v-"$1"d +%Y%m%d%H%M)" "${@:2}"; }
for file in scanner/src/main.rs scanner/Cargo.toml scan-old/cli/main.rs \
    recipe-api/server.py recipe-api/requirements.txt ledger-cli/src/lib.rs \
    fresh-idea/notes.md keep-me/draft.txt; do
  mkdir -p "$(dirname "$file")" && echo "contents of $file" > "$file"
done
age 90 scanner/src/main.rs scanner/Cargo.toml
age 200 scan-old/cli/main.rs
age 45 recipe-api/server.py recipe-api/requirements.txt
age 28 ledger-cli/src/lib.rs
age 2 fresh-idea/notes.md
age 100 keep-me/draft.txt
```

The folders in the images hold a few hundred megabytes, so their sizes
differ from the ones you see. Everything else is the same. The commands
work on your own experiments folder too: run the dry run from "Archiving
idle folders" first and read what it would archive.

Make the folder a collection. `--pattern` says which folders are pieces:
`*` selects the folders directly inside it.

```bash
antiq init --pattern '*'
```

![antiq init prints the collection's path and ID and says that the next archive would archive 4 folders](docs/pages/assets/shots/init.png)

`init` wrote `.antiq/config.toml`, the configuration of the collection.
Commands find the collection by searching upwards from the current
directory, or from the path given with `-C`.

## What is idle

```bash
antiq status
```

![antiq status shows a table: scan-old, scanner, recipe-api and keep-me are due, ledger-cli archives in 2 days](docs/pages/assets/shots/status.png)

A piece is due when its idle clock is older than 30 days. The idle clock is
the newest modification time of the piece's regular files. `ledger-cli`,
28 days idle, is `soon`: `status` shows that from 3 days before due.
`fresh-idea` has no state, so it is not listed. `status` lists the pieces
that are `due`, `soon`, `pinned` or `refused`, each with the reason.

## Keeping what must stay

`keep-me` is due, but it is a draft you are still using. Pin it:

```bash
antiq pin keep-me
antiq status
```

![antiq pin confirms, and status now lists keep-me as pinned, kept by .antiq-pin](docs/pages/assets/shots/keep.png)

`pin` creates a `.antiq-pin` file in the piece. A pinned piece is never
archived, and `push` still mirrors it. `unpin` removes the file.

Other ways to keep files out of the way are in the configuration, which
`init` wrote with a comment for every key:

- `collection.ignore` lists gitignore lines for paths that are not packed,
  not listed and not counted for the idle check. The default is
  `[".DS_Store"]`.
- `collection.timeless` lists gitignore lines for paths that are packed but
  not counted for the idle check, such as `.git/`. A piece whose only
  recent change is in `.git/` stays due.
- `collection.exclude` lists literal paths that are never pieces.
- `idle_days` and `soon_days` change the 30 and 3 days.

A piece can also carry `.antiqignore` and `.antiq-timeless` files with the
same gitignore lines. The reference below has the whole table.

## Archiving idle folders

Look at what the next run would do before it does it. A dry run, with `-n`,
changes nothing:

```bash
antiq archive -n
```

![antiq archive -n lists the three would archive lines in magenta and ends with a note that nothing was archived](docs/pages/assets/shots/dry-run.png)

Now archive. On a terminal each piece that is working has a card with a
progress bar, and a line stays behind for each piece that finished:

```bash
antiq archive
```

![the cards of recipe-api, scan-old and scanner while they pack](docs/pages/assets/shots/archive-live.png)

![the finished lines of the three pieces and the closing line](docs/pages/assets/shots/archive-done.png)

For each due piece `archive` packs a `tar.zst` version, reads the archive
back and compares it with the folder, and only then moves the folder away
and deletes it. A folder that changed in the meantime is skipped and its
files are kept. `archive <piece>` archives one piece now, however recently
it changed.

A piece is refused and never archived when its folder holds a mount point,
a nested collection, an unreadable or locked entry, an exclude entry, or a
control file that is not a regular file. `status` names the reason. A piece
without a file that counts for the idle check is empty and is not archived
either.

## Finding a version

The folders are gone from `experiments`, and every piece has a version.
`list` prints one row per version, and `grep` matches a regular expression
against the paths of all archived files:

```bash
antiq list scan
antiq grep 'main\.rs'
```

![antiq list scan shows the versions of scan-old and scanner; antiq grep lists the main.rs files with the version they are in](docs/pages/assets/shots/find.png)

`grep` reads the listings, so nothing is unpacked. A piece is named by its
path or by a search word, and a search word has to match exactly one piece.
`scan` matches two, so `list` shows both. Commands that take one piece fail
when the word matches none, and when it matches several they also list the
candidates. `unearth` and `forget` remove a version, so they act only on a
path: a word that fits one piece lists that piece and does nothing.

## Bringing a version back

`fork` extracts a working copy of a version and keeps the version:

```bash
antiq fork scanner
```

![antiq fork scanner prints the version it extracted and the path it wrote to](docs/pages/assets/shots/fork.png)

Change a file in the copy and archive the piece again. `archive <piece>`
works on a piece that is not due, and a piece can have several versions
side by side:

```bash
echo '// edited' >> scanner/src/main.rs
antiq archive scanner
antiq list scanner
```

![antiq list scanner shows two versions of scanner](docs/pages/assets/shots/versions.png)

`unearth <piece> [--version <version>]` brings a version back as a folder
at the piece's path and **removes that version** once the folder is
complete and verified. Without `--version` the newest version is taken. A
version is named by a prefix of its time, such as `2026-11-05` or
`2026-11-05T00`. `fork [--name <new path>]` writes the copy to another path
instead.

`forget <piece> <version>` deletes one version, and `forget <piece>
--all-versions --force` deletes all of them.

`import <folder> [--as <path>]` packs a folder from outside the collection
directly as a version and leaves the source in place.

## Mirroring

Everything in the collection, the live folders and the versions, can be
mirrored to one target. Add a `[target]` block to `.antiq/config.toml`,
create an empty folder for it, and let `push --init` claim it. The target
is an absolute path, such as a mounted disk or share, or `[user@]host:path`
reached over ssh. The path of the block below is the folder the images use:

```bash
mkdir -p /tmp/antiq-demo/target
```

```toml
[target]
url = "/tmp/antiq-demo/target"
```

```bash
antiq push --init
```

![antiq push --init claims the target and prints a table of what was created for each piece, then a closing line that starts with Claimed](docs/pages/assets/shots/push.png)

`--init` wrote the marker `.antiq-target` at the target's root. It holds
the collection ID, the machine name and the collection path, and keeps
another collection from pushing there by mistake. Later runs are `antiq
push`. `push -n` lists what would change, per piece, and changes nothing.

For an ssh target, `url = "backup@nas.local:/volume1/antiq/projects"`, the
ssh options such as a port go into `~/.ssh/config`. ssh is the recommended
way to reach a NAS.

> [!TIP]
> A NAS share that does not let rsync set file modes makes `push` fail
> with "the target does not let rsync set file modes". Tell rsync to leave
> the modes alone in `.antiq/config.toml`:
>
> ```toml
> [target]
> rsync_args = ["--no-perms"]
> ```
>
> Files in the live folders on the target then get the share's own modes,
> while the archived versions keep theirs.

A push that fails names the log of the run, a compressed file with
everything rsync printed. On macOS the logs are in `~/Library/Logs/antiq`,
elsewhere in `antiq` in the state directory, and the error shows the
command that reads one: `zstd -dc <path> | less`. antiq keeps the 10 newest
logs of a collection, and the newest log of a failed push beyond them.

The target is a mirror, not a backup: whatever is deleted in the
collection is deleted on the target by the next push. The reference below
says more under "Good to know".

## Running unattended

Run the two commands one after the other from a scheduler:

```bash
antiq archive; antiq push --if-reachable
```

The push runs after a partial archive too. The scheduler sees only the
push's exit status. `--if-reachable` makes an unreachable target exit 0
instead of 4. A held lock, a damaged version or any failure after the
target was reached still exits 1. The exit codes are in the reference.

## Damaged versions

A version is damaged when, among other causes, its archive lacks its
listing or checksum file. Files deleted by hand and incomplete copies from
the target cause this. Delete the listing of `scan-old` in the practice
collection to see what that looks like:

```bash
rm .antiq/pieces/scan-old/*.sha256.zst
antiq push
```

![antiq push is refused: the collection holds damaged versions, and the error names antiq rebuild with the version](docs/pages/assets/shots/error-damaged.png)

`push` is stopped by a damaged or incomplete version until you repair it,
and the error names the command. `rebuild` writes the missing listing after
checking the archive against what survives:

```bash
antiq rebuild scan-old 20
```

![antiq rebuild scan-old writes the missing listing](docs/pages/assets/shots/rebuild.png)

The version argument is a prefix of the version's time, so `20` is the one
version here. The reference lists what makes a version damaged or
incomplete, and `forget` removes an incomplete one.

## Reading archives without antiq

Everything antiq writes can be read without it. The files of a version are
in `.antiq/pieces/<piece>/`. Unpack the archive into a scratch directory,
go into the unpacked folder and check it against the listing. The names carry
the stamp of your version in place of the one shown:

```console
$ store=experiments/.antiq/pieces/scan-old
$ mkdir /tmp/check && cd /tmp/check
$ zstd -dc $store/scan-old--2026-11-05T150201.tar.zst | tar -x
$ cd scan-old
$ zstd -dc $store/scan-old--2026-11-05T150201.sha256.zst | shasum -a 256 -c -
cli/main.rs: OK
```

Use `shasum`: the `sha256sum` that macOS ships is not GNU's and fails on a
listing.

## Reference

### Concepts

- **Collection:** a directory with a `.antiq/` folder. Commands find it
  by searching upwards from the current directory, or from the path
  given with `-C`.
- **Piece:** a folder of the collection that antiq manages. The
  `patterns` of the configuration select pieces; its path relative to
  the collection is its identity.
- **Version:** one archived state of a piece. A piece can have several
  versions side by side.
- **Target:** the mirror location, an absolute path (a mounted disk or
  share) or `[user@]host:path` reached over ssh by rsync.

### Commands

```text
$ antiq --help
Your idle folders, kept like antiques

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
nothing. No command asks a question. `antiq --version` prints the version
on one line, and on a terminal with 24-bit colour it draws Archie beside it.

A piece is named by its path or by a search word. A search word must
match exactly one piece. The command fails when it matches none, and
when it matches several it also lists the candidates. `unearth` and
`forget` act only on a path: a word that fits one piece lists that piece
and does nothing.

### Output in a pipe

The images above are the output on a terminal. Into a pipe or a file
antiq prints plain lines, which scripts can read. `archive` prints one line
for each piece and a summary:

```console
$ antiq archive | cat
archived recipe-api
archived scan-old
archived scanner
Archived 3, skipped 0, refused 0.
```

`unearth` prints one line:

```console
$ antiq unearth scan-old | cat
Unearthed scan-old (version 2026-11-05T214151)
```

On a terminal `unearth` shows a card, leaves the line of the piece and ends
with `✔ Unearthed to ./scan-old   took 0:00` instead. The progress cards of `archive`,
`push`, `unearth`, `fork` and `rebuild` appear only on a terminal. A dry
run and a stdout that is not a terminal print the plain lines, and
`--json` prints its JSON object; none of them shows cards.

`list` and `grep` print one tab-separated line per version or hit when they
do not write to a terminal.

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
and its files are kept. `archive <piece>` archives one piece now, however
recently it changed.

`pin <piece>` creates a `.antiq-pin` file in the piece, which keeps it
from being archived while `push` still mirrors it. `unpin` removes it.
`import <folder> [--as <path>]` packs a folder from outside the
collection directly as a version and leaves the source in place.

A piece is refused, and never archived, when its folder holds a mount
point, a nested collection, an unreadable or locked entry, an exclude
entry, or a control file that is not a regular file. `status` names the
reason. A piece without a file that counts for the idle check is empty and
is not archived either.

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
the command.

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

`push` mirrors with one rsync run of `-a -H -x --delete --delete-delay`
(and one more pass when the first one deleted something and changes
remain), so the target ends up as a copy of the collection: live folders, versions and
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

**Checking a version by hand.** "Reading archives without antiq" shows the
commands. Use `shasum`: the `sha256sum` that macOS ships is not GNU's and
fails on a listing.

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

**Scheduling.** See "Running unattended".

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

The images of this README and of the project page are rendered from real
runs of antiq in a demo world. `just shots` renders them again into
`docs/pages/assets/shots/`; it needs `vhs`, `ttyd`, `ffmpeg`, `oxipng` and
an rsync of 3.2.4 or newer. `just demo` renders only the demo video
(`docs/pages/assets/demo.webm` and `demo.mp4`) and the GIF of the top of
this README. `just shots-test` tests the script that builds the demo world.

## About the name

The pangolin in the logo is [Archie](assets/archie-512.webp), antiq's
mascot: a small idol of obsidian and gold. Archie shares its name with
the first internet search engine, released in 1990, which indexed the
files on FTP archives so people could find them again. The search
engine's name, too, was a shortening of "archive".

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE).
