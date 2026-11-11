Part of the antiq skill. Read SKILL.md first, which holds the rules for every task; `antiq skill` prints it.

# antiq: importing a folder

`import` packs a folder from outside the collection as a version, checks
the archive the way `archive` does, and leaves the folder where it is.
It is the way to bring old projects from another disk, a downloads folder
or an old archive file into the collection's archive. `import -n` is fine
on your own; the import itself only on the user's request.

```sh
antiq -C <collection root> import <absolute folder> --as <piece path> -n --json
antiq -C <collection root> import <absolute folder> --as <piece path> --json
```

- Give the folder as an absolute path: a relative one is read against the
  `-C` directory, not against your working directory. It must lie
  outside the collection and must be a folder, not a symlink to one.
- `--as` is the piece's path in the collection. Without it the piece is
  named after the folder. The patterns of the collection must select that
  path, and it must not lie inside another piece.
- The version is named by the newest file in the folder that counts. A
  folder that was copied or unpacked without its original times gets
  today's time; tell the user. `--time` gives the version another time,
  either as a stamp without a counter, `2026-11-04T090211`, or as local
  time with seconds, `"2026-11-04 10:02:11"`. A date alone, a prefix of a
  stamp or a time in the future is refused.
- The ignore and timeless rules of the collection apply to the folder.
  The dry run's action lists in `left_out` the paths the version would
  leave out; show them to the user.
- A folder with an `.antiq-pin` at its top, or without a file that
  counts, is refused.
- Old archive files: unpack each into a temporary folder outside the
  collection, with the files' original times as `tar` and `unzip` keep
  them, and import the folder that comes out. The source and the unpacked
  folder stay after the import; delete them only if the user asks.

The result has `imported` with `piece`, `stamp` and the sizes, or
`skipped` with `reason` and `message` (exit 3), for example when there is
no room.

End of references/import.md.
