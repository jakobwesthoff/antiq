// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The scenario harness: a scenario builds a collection, runs antiq
//! commands against it once in text mode and once in JSON mode, and the
//! harness compares what the two runs left behind.
//!
//! # Writing a scenario
//!
//! A scenario is one call of [`world::both_modes`]. `build` makes the
//! starting [`world::Setup`] from a [`collection::Clock`], and `script`
//! runs the commands and checks the answers. The harness calls both once
//! per mode, each time with a fresh world built from the same clock.
//!
//! ```rust,ignore
//! both_modes(
//!     |clock| {
//!         let collection = Collection::build(clock)
//!             .idle_days(30)
//!             .piece("projects/scanner", |piece| {
//!                 piece.age_days(40).file("src/main.rs", "fn main() {}\n");
//!             })
//!             .finish();
//!         Setup::new(collection).with_target(Target::new())
//!     },
//!     |world| {
//!         let outcome = world.run(&["archive", "projects/scanner"]);
//!         outcome.expect_code(0);
//!         outcome.text_only(|out| assert!(!out.is_empty()));
//!         outcome.json_only(|json| assert_eq!(json["command"], "archive"));
//!         assert_eq!(world.collection().versions("projects/scanner").len(), 1);
//!     },
//! );
//! ```
//!
//! After the script ran in both modes, `both_modes` fails when the end
//! states of the two runs differ, with the paths that differ and both
//! contents. A check that holds in one mode only goes into `text_only` or
//! `json_only`, or into an `if world.mode() == Mode::Json` block.
//!
//! A scenario group is a file `tests/scenarios/<group>.rs` with one
//! `mod <group>;` line in `tests/scenarios/main.rs`. The tests of the
//! harness itself are in `tests/scenarios/harness.rs`.
//!
//! # The fixed clock
//!
//! [`collection::Clock::now`] reads the time once. Every age in a
//! collection is "days before that second", and both worlds of a scenario
//! are built from the same clock, so file times in the text world and the
//! JSON world are equal although the worlds are built seconds apart.
//! Directory times change with every file created in a directory, and tar
//! archives and listings record them, so each directory of a collection
//! carries the clock's `dir_time` (400 days before the base).
//! `Collection::settle` sets them again, and every run does so after the
//! command returned.
//!
//! # Building a collection
//!
//! - [`collection::Collection::build`] returns a builder. `id`, `patterns`,
//!   `exclude`, `ignore`, `timeless`, `idle_days` and `soon_days` are
//!   written to `.antiq/config.toml`; a key that is not set stays out of
//!   the file, so its default applies. The default ID is the same in every
//!   world.
//! - `piece(path, |piece| ..)`: `file`, `file_aged`, `dir`, `symlink`,
//!   `hard_link`, `age_days` (the age of every file that does not name its
//!   own), `rule_ignore`, `rule_timeless`, `pinned` and `restored`.
//!   `outside` writes a file in the collection root outside any piece.
//! - `version(piece, stamp, files)` writes a stored version: the archive,
//!   the checksum file and the listing, packed with the `tar`, `zstd` and
//!   `sha2` crates and a listing written as text, following the spec, so a
//!   scenario that reads it checks antiq against the spec and not against
//!   itself. The `damage_*` methods of `Collection` break one file of a
//!   version each (truncated archive, removed listing, corrupt checksum
//!   file, removed archive). `set_age(piece, days)` ages a piece's files
//!   and rewrites its `.antiq-restored` marker. `Collection::bare` is an
//!   empty directory for scenarios that start with `init`; the builder's
//!   `uninitialised` leaves out `.antiq/` and the config but writes the
//!   pieces, for an `init` in a directory that holds folders.
//! - [`versions::Source`] is a folder outside the collection, built like a
//!   piece, for `import`.
//!
//! # Target
//!
//! [`target::Target`] is a directory outside the collection. Passing it to
//! `Setup::with_target` writes its path as `[target] url`. `marker` writes
//! the `.antiq-target` file a push from this machine writes;
//! `marker_with(id, host, path)` writes one with any values, such as the
//! name of another machine. `remove` deletes the directory. `copy_back`
//! runs `rsync -aH` into a new directory and returns it as a collection,
//! for scenarios that restore from the target. `real_rsync` finds an rsync
//! of 3.2.4 or later and `Collection::use_real_rsync` writes it as
//! `[target] rsync`, so a push does not depend on the machine's `PATH`
//! order (macOS ships openrsync as `/usr/bin/rsync`).
//!
//! # Running commands
//!
//! - `world.run(args)` runs `antiq -C <root> [--json] <args>`. In JSON mode
//!   the harness checks every answer against the envelope of the spec
//!   ([`envelope::check`]) and fails the test when it is wrong; `--help`
//!   and `--version` are exempt.
//! - `run_in(dir, args)` runs with `dir` as the current directory and no
//!   `-C`, for a piece given as a path. `run_as(&collection, args)` runs
//!   against another collection, such as a copy back.
//! - `run_tty(args)` runs with stdout on a pseudo-terminal, in text mode
//!   whatever the world's mode is, and returns the output with `\r\n`
//!   turned into `\n`. It kills a command that runs longer than 20 seconds
//!   and fails; `run_tty_within` takes another limit.
//! - Every command runs with `TZ=Pacific/Auckland` and a home directory of
//!   its own, with a stand-in `ssh` in front of `PATH` that does not
//!   resolve any host.
//! - The [`world::Outcome`] has `code`, `stdout`, `stderr` and `mode`;
//!   `plain` (stdout without colour sequences, `world::strip_colour` for any
//!   other text); `expect_code`, `json_only`, `text_only`, `keys` (the ordered keys of
//!   the JSON object, checked in JSON mode only) and `error_kind` (the
//!   `kind` of the error object, `None` in text mode).
//! - `world.assert_unchanged(|| ..)` fails when the closure changed the
//!   collection or the target in any way, for dry runs.
//!
//! # End states
//!
//! [`state::State`] reads a directory tree: directories, links, and files
//! with mode and content. `both_modes` compares `State::normalised` of the
//! collection and of the target, where what is allowed to differ between
//! two worlds is replaced:
//!
//! - the collection and target paths become `<root>` and `<target>` in
//!   text content, config and link targets;
//! - version stamps become `<v1>`, `<v2>`, ... by their order within their
//!   piece folder (time, then counter as a number), in names and in text;
//! - the values of the listing header keys `archived`, `imported` and
//!   `rebuilt`, and the time in `.antiq-restored`, become `<time>`, and the
//!   `id` of `.antiq/config.toml`, which `init` mints anew in every world,
//!   becomes `<id>`;
//! - 64-digit hex digests in `*.sha256` files become `<sha256>`;
//! - a `.tar.zst` is compared by its members (path, type, mode, size, time,
//!   content digest) and a `.sha256.zst` by its decompressed text, so
//!   compression settings cannot fail a scenario (an archive that does not
//!   decode, such as a truncated one, is compared by its bytes);
//! - file times are not compared, the lock file is left out.
//!
//! `State::raw` reads exactly what is there, without any of this.
//! `State::without(paths)` drops paths and what is below them, to compare a
//! collection with its mirror.
//!
//! # Checks without antiq code
//!
//! [`outsider`] holds what a person would do by hand: `shasum_check` runs
//! `shasum -a 256 -c` in the store folder and fails, naming the tool, when
//! it is not installed; `tar_members` lists an archive; `store_files`
//! returns the files of each version found in a piece's store folder,
//! found by suffix, and `Collection::versions` the stamps of the versions
//! whose archive exists. `same_file` tells whether two paths are one file
//! under two names (hard links). `tar_extract` unpacks an archive with `tar --zstd
//! -xpf` into a new directory and `listing_check` runs `shasum -a 256 -c`
//! on the checksum lines of a listing in an unpacked tree; both fail,
//! naming the tool, when it is not installed.
//!
//! In JSON mode `world.run` accepts exit 1 without an `error` object for
//! `archive` when its `refused` list is not empty, as the spec's report of
//! `archive <piece>` on a refused piece.
//!
//! # A stand-in for the binary
//!
//! `Setup::with_program` runs another program in place of antiq.
//! `tests/fixtures/scenario-fake.sh` is one, used by the tests of the
//! harness; its header lists the commands it knows.

pub mod collection;
pub mod envelope;
pub mod outsider;
pub mod state;
pub mod target;
pub mod versions;
pub mod world;
