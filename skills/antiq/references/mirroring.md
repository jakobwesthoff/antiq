Part of the antiq skill. Read SKILL.md first, which holds the rules for every task; `antiq skill` prints it.

# antiq: mirroring

`push` makes one target equal to the whole collection: live folders,
versions and everything else in it. It is a mirror, not a backup. What is
gone in the collection, by antiq or by hand, is deleted on the target by
the next `push`, so after `forget` or `unearth` the next `push` deletes
that version on the target too.

## Setting up a target

Only on the user's request, and each step with their consent:

1. The user picks the target: an absolute path to an empty folder on
   another disk or a mounted share, or `[user@]host:path` over ssh. ssh
   options such as a port or a key go into `~/.ssh/config`. A path target
   must exist, must be a folder on the volume and not its root, and must
   not lie inside the collection.
2. `url` goes under `[target]` in `.antiq/config.toml`. Show the user the
   change before you save it.
3. `antiq push --init -n --json`, then `antiq push --init --json`. `--init`
   writes the marker `.antiq-target` into the empty folder and claims it
   for this collection. It refuses a folder that is not empty, and over
   ssh it creates the last missing level of the path.
4. Later runs are `antiq push -n --json` and `antiq push --json`. A
   scheduled run adds `--if-reachable`, which exits 0 instead of 4 when
   the target is offline.

`push -n` changes nothing in the collection or on the target, but it
connects to the target, over ssh too, and writes a run log. Run it only
when the task is about the mirror, not as a routine check.

## rsync

`push` needs rsync 3.2.4 or newer on this machine. It takes the first
`rsync` on `PATH` that is new enough, then the one in `/opt/homebrew/bin`
or `/usr/local/bin`. The `/usr/bin/rsync` of macOS (openrsync) is
refused. `rsync` under `[target]` names one by its absolute path. Over ssh
the other machine needs rsync too: `rsync_path` names a newer one there,
and only where none can be installed, such as on another Mac, `fsync =
false` lets `push` work without `--fsync`. The user decides on each of
these settings.

## When push fails

`push` refuses while a damaged or incomplete version exists: read
`references/damage.md` (`antiq skill damage`).

| Situation | What antiq does | What you do |
|---|---|---|
| no `[target] url` | error `no_target`, exit 1 | tell the user |
| the target folder is missing, or the disk or host is not reachable | `target_unreachable`, exit 4 (0 with `--if-reachable`) | tell the user the reason; do not create the folder |
| an empty folder without a marker | unreachable, with a warning naming `push --init` | tell the user; `--init` only with their consent |
| a folder that is not empty and has no marker | exit 1, `--init` refused | tell the user; never empty the folder |
| the marker belongs to another collection | exit 1, also with `--force` and `--take-over` | tell the user |
| the marker names this collection on another machine or at another path | exit 1, naming both | see below |

After a renamed machine or a moved collection, the marker names the old
machine or path. `push --take-over` rewrites the marker to this machine
and path and then pushes; the next `push` needs no flag. `push --force`
pushes once and leaves the marker as it is. Both only with the user's
consent: after a take-over, a scheduled `push` from the old machine or
path is refused.

A failed `push` names its run log, a zstd file that `zstd -dc <path> |
less` reads. On macOS the logs are in `~/Library/Logs/antiq`. Read it
when the error does not say enough.

`push` stays on the collection's volume and does not read the target
back: damage on the target stays unseen.

End of references/mirroring.md.
