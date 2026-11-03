<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/emblem/emblem-ledge-dark.svg">
    <img src="assets/emblem/emblem-ledge.svg" width="256" height="256" alt="antiq's logo: Archie, a pangolin with obsidian scales and a gold face, peeking over a ledge with its paws on the edge">
  </picture>
</p>

# antiq

Archive what you no longer touch, find it when you need it.

antiq is a command-line tool written in Rust for directories where
working folders pile up. It is in early design, and nothing works yet.

The pangolin in the logo is [Archie](assets/archie-512.webp), antiq's
mascot: a small idol of obsidian and gold. Archie shares its name with
the first internet search engine, released in 1990, which indexed the
files on FTP archives so people could find them again. The search
engine's name, too, was a shortening of "archive".

The plan for version 1:

- Folders that have not changed for a set number of days get packed into
  compressed archives.
- An index of each archive's contents keeps it searchable, and a picker
  restores it.
- Archives move to a backup store, such as a NAS, in tiers. In the later
  tier only the index stays local, and restoring fetches the archive from
  the store.
- Folders still in use are backed up to the same store as packages, and
  several machines can share one store.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE).
