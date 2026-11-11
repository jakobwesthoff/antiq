Part of the antiq skill. Read SKILL.md first, which holds the rules for every task; `antiq skill` prints it.

# antiq: damaged versions

Each version is three files in `.antiq/pieces/<piece path>/`: the archive
(`.tar.zst`), its listing (`.sha256.zst`) and its checksum file
(`.tar.zst.sha256`). A version is **damaged** when one of them is missing
or changed from outside antiq, and **incomplete** when its archive is
missing. antiq's own crashes do not cause this; the next locked run
repairs what an interrupted run left behind.

## Where it shows

- `status --json`: `damaged_versions` (each with `piece`, `version` and
  `reasons`) and `incomplete_versions`.
- `find --json`: the version's `state` is `damaged` or `incomplete`, and
  `damage` lists the faults.
- `push` refuses to run while a damaged or incomplete version exists, and
  names the `rebuild` or `forget` command for each. The command in a JSON
  `fix` has no `-C`; add `-C <collection root>` before you run it.
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
- **The archive is missing, or a file is there but broken.** The version
  can only be saved by copying the file back from the target, which holds
  a plain copy of the collection. Ask the user whether the target still
  holds it, and copy it back only with their consent.
- **`forget`** deletes the version for good, and the next `push` deletes
  it on the target as well. Before you offer `forget`, tell the user that
  and ask whether the target still holds the version.
- **`fork`** extracts a damaged version with a warning and keeps it, so
  the user can rescue what is readable.

A version copied back by hand between an interrupted `unearth` and the
next antiq run is deleted by that run's repairs: copy back only when no
run was interrupted, and run `antiq status --json` first.

## Restoring from the target

The target is a plain copy of the collection. A lost file, or a whole
lost collection, comes back with `rsync -aH` from the target; `-H` keeps
hard links. Do this only on the user's request, and run `antiq status
--json` before the next `push`, since a `push` from an incomplete
collection deletes on the target what is missing locally.

End of references/damage.md.
