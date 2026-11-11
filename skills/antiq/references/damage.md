Part of the antiq skill. Read SKILL.md first, which holds the rules for every task; `antiq skill` prints it.

# antiq: damaged versions

Each version is three files in `.antiq/pieces/<piece path>/`: the archive
(`.tar.zst`), its listing (`.sha256.zst`) and its checksum file
(`.tar.zst.sha256`). A version is **damaged** when one of them is missing
or changed from outside antiq, and **incomplete** when its archive is
missing.

Damage comes from outside antiq: a file deleted by hand, an incomplete
copy back from the target, a disk error. The one exception: a `forget`
that crashed or failed leaves an incomplete version; running the same
`forget` again removes it. Ask the user whether a `forget` was
interrupted before you offer to copy anything back.

## Where it shows

- `status --json`: `damaged_versions` (each with `piece`, `version` and
  `reasons`) and `incomplete_versions`.
- `find --json`: the version's `state` is `damaged` or `incomplete`, and
  `damage` lists the faults.
- `push` refuses to run while a damaged or incomplete version exists, and
  names the `rebuild` or `forget` command for each.
- `unearth` refuses a damaged version. Its error has no `fix`; the
  `message` names `fork`, `rebuild` and copying the file back from the
  target.

## What helps

Every step here changes the collection and needs the user's consent.
Tell the user what is missing and what each way costs before you run
anything.

- **Only the listing or the checksum file is missing.** `antiq rebuild
  <piece path> <stamp> -n --json`, then without `-n`. It checks the
  archive against what survives and writes the missing file.
- **The listing or the checksum file is there but broken.** `rebuild`
  refuses and names deleting that file, after which it can rebuild it,
  or copying it back from the target. Deleting it is the user's decision.
- **The archive is missing or has the wrong size.** Only a copy from the
  target, which holds a plain copy of the collection, saves the version.
  Ask the user whether the target still holds it, and copy it back only
  with their consent.
- **`forget`** deletes the version for good, and the next `push` deletes
  it on the target as well. Before you offer `forget`, tell the user that
  and ask whether the target still holds the version.
- **`fork`** extracts a damaged version with a warning and keeps it, so
  the user can rescue what is readable.

A version copied back by hand between an interrupted `unearth` and the
next antiq run is deleted by that run's repairs. Before you copy a
version back, run `antiq archive -n --json`: if its `actions` hold a
repair `delete_restored_version` for that piece and version, do not copy
it back; the unearthed folder already holds it.

## Restoring from the target

The target is a plain copy of the collection, live folders included, as
of the last `push`. Copy back only the files that are lost, from
`<target>/.antiq/pieces/<piece path>/` to the same place in the
collection, with `rsync -aH`; `-H` keeps hard links. Copy the whole
target only into an empty folder: over an existing collection it replaces
newer live files with the target's older ones. Show the user the command
first, and do this only on their request.

Before the next `push`, run `antiq status --json` and `antiq push -n
--json`. The `deleted` counts of the `mirror` action are what that push
would delete on the target, including anything the copy back missed.
Show them to the user before anything is pushed.

End of references/damage.md.
