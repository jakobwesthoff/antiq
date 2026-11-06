<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/emblem/archie-ledge-rim-light.svg">
    <img src="assets/emblem/archie-ledge.svg" width="256" height="256" alt="antiq's logo: Archie, a pangolin with obsidian scales and a gold face, peeking over a ledge with its paws on the edge">
  </picture>
</p>

# antiq

Archive what you no longer touch, find it when you need it.

antiq is a command-line tool written in Rust for directories where
working folders pile up. It archives the folders of a collection that
nobody has touched for a while, finds them again by name or by the files
they hold, brings them back, and mirrors the whole collection to one backup
target. The code is being reworked to this design, which
`docs/specs/spec-v1.md` specifies. Of the commands only `antiq init` works
so far; it makes a directory a collection.

The pangolin in the logo is [Archie](assets/archie-512.webp), antiq's
mascot: a small idol of obsidian and gold. Archie shares its name with
the first internet search engine, released in 1990, which indexed the
files on FTP archives so people could find them again. The search
engine's name, too, was a shortening of "archive".

Version 1:

- All work happens on one machine. The target is a plain copy of the
  collection: copying it back restores the live folders and the versions.
- Folders that have not changed for a set number of days become versions:
  compressed `tar.zst` archives with a listing that `shasum -c` reads once
  `zstd` has decompressed it.
- `antiq list` and `antiq grep` find versions by piece name and by the
  paths of the files they hold.
- `antiq push` mirrors the collection to the target with rsync.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE).
