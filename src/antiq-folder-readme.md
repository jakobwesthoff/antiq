# antiq collection

This folder, `.antiq/`, belongs to [antiq](https://antiq.westhoffswelt.de),
a tool that archives idle folders. antiq creates and changes what is in it.
Change nothing here by hand, except editing `config.toml`. Copying a
version back from the target of a `push` and taking a folder out of
`recovered/` or `trash/` are your own steps. antiq writes this README
again whenever it differs from its own text, so edits to it are lost.

- `config.toml` holds the settings of the collection.
- `pieces/` holds the archives: one folder per archived folder, named by
  its path. A complete version is three files.
- `recovered/` and `trash/` hold folders that antiq could not put back.
  `antiq status` lists them.
- `lock`, `tmp/` and `doomed/` are antiq's working files.

## Working with it

Install antiq with `cargo install antiq` or
`brew install jakobwesthoff/tap/antiq`. From the folder that holds
`.antiq/`:

```text
antiq status           which folders are due or pinned, and what needs attention
antiq find [query]     list the archived versions
antiq grep <pattern>   search the paths of the archived files
antiq unearth <piece>  bring a folder back and remove that version
```

If you are an AI agent, run `antiq skill` and follow what it prints.

## Reading a version without antiq

A version is three files in `pieces/<piece>/`:
`<name>--<time>.tar.zst` (the archive), `<name>--<time>.tar.zst.sha256`
(its checksum file) and `<name>--<time>.sha256.zst` (the listing of its
files). In that folder, check the archive:

```text
shasum -a 256 -c <name>--<time>.tar.zst.sha256
```

Unpack it from an empty folder outside `.antiq/`:

```text
zstd -dc <path to the archive> | tar -x
```
