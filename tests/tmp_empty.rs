// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A run that ends leaves `.antiq/tmp/` empty, so that the next run plans
//! no repair 2. Every case runs one command, checks that the folder holds
//! nothing, and then probes with a dry run of `archive` that lists no
//! repair of the temporary files.

mod support;

use std::fs;
use support::scenario::collection::{Clock, Collection};
use support::scenario::world::{Mode, Setup, World};

const PIECE: &str = "projects/scanner";
const STAMP: &str = "2026-11-04T090211";

fn world(
    mode: Mode,
    build: impl FnOnce(&mut support::scenario::collection::CollectionBuilder),
) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), mode)
}

fn stored(builder: &mut support::scenario::collection::CollectionBuilder) {
    builder.version(PIECE, STAMP, &[("src/main.rs", "fn main() {}\n")]);
}

fn live_pieces(builder: &mut support::scenario::collection::CollectionBuilder) {
    builder.idle_days(30);
    builder.piece("projects/alpha", |piece| {
        piece.age_days(40).file("a.txt", "a");
    });
    builder.piece("projects/beta", |piece| {
        piece.age_days(50).file("b.txt", "b");
    });
}

/// Asserts that `.antiq/tmp/` holds nothing and that a dry run of `archive`
/// lists no deletion of a temporary entry.
fn assert_tmp_empty_and_no_repair(world: &World) {
    let tmp = world.collection().path(".antiq/tmp");
    let left: Vec<String> = fs::read_dir(&tmp)
        .expect("tmp exists")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into()
        })
        .collect();
    assert!(left.is_empty(), "tmp holds {left:?}");

    let probe = world.run(&["archive", "-n"]);
    assert!(
        !probe.stdout.contains("delete_temporary") && !probe.stdout.contains(".antiq/tmp"),
        "the dry run plans a repair: {}",
        probe.stdout
    );
}

#[test]
fn archive_of_one_piece_leaves_tmp_empty() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, live_pieces);
        world.run(&["archive", "projects/alpha"]).expect_code(0);
        assert_tmp_empty_and_no_repair(&world);
    }
}

#[test]
fn archive_of_two_pieces_leaves_tmp_empty() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, live_pieces);
        world.run(&["archive"]).expect_code(0);
        assert_tmp_empty_and_no_repair(&world);
    }
}

#[test]
fn unearth_leaves_tmp_empty() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, stored);
        world.run(&["unearth", PIECE]).expect_code(0);
        assert_tmp_empty_and_no_repair(&world);
    }
}

#[test]
fn rebuild_leaves_tmp_empty() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, stored);
        let version = world.collection().version_files(PIECE, STAMP);
        fs::remove_file(&version.listing).expect("the listing exists");
        world.run(&["rebuild", PIECE, STAMP]).expect_code(0);
        assert_tmp_empty_and_no_repair(&world);
    }
}

#[test]
fn pin_and_unpin_leave_tmp_empty() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, live_pieces);
        world.run(&["pin", "projects/alpha"]).expect_code(0);
        assert_tmp_empty_and_no_repair(&world);
        world.run(&["unpin", "projects/alpha"]).expect_code(0);
        assert_tmp_empty_and_no_repair(&world);
    }
}
