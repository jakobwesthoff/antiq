# antiq

Archive what you no longer touch, find it when you need it.

antiq is a command-line tool written in Rust for directories where
working folders pile up. It is in early design, and nothing works yet.

The plan for version 1:

- Folders that have not changed for a set number of days get packed into
  compressed archives.
- An index of each archive's contents keeps it searchable, and a picker
  restores it.
- Archives move to a backup store, such as a NAS, in tiers. In the later
  tier only the index stays local, and restoring fetches the archive from
  the store.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE).
