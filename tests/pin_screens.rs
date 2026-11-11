// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What `pin` and `unpin` print on a terminal: the result, and an info that
//! says what a pin is. In a pipe the one-line sentences stay as they are.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};

/// No colour, so the text can be compared as it reads.
const PLAIN: &[(&str, &str)] = &[("NO_COLOR", "1")];

fn world(build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), Mode::Text)
}

fn alpha(builder: &mut CollectionBuilder) {
    builder.piece("projects/alpha", |piece| {
        piece.age_days(40).file("data", "x");
    });
}

#[test]
fn unpinning_a_piece_that_is_not_pinned_is_an_info_and_exits_0() {
    let world = world(alpha);
    let run = world.run_tty_env(&["unpin", "alpha"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "▌ ℹ INFO\n\
         ▌ Nothing to unpin: projects/alpha is not pinned.\n\
         ▌\n\
         ▌   A pin is the file .antiq-pin at the top of a piece, and this piece has none.\n\
         ▌   Nothing was changed.\n\
         ▌\n\
         ▌ → antiq status shows which pieces are pinned.\n"
    );
}

#[test]
fn pinning_a_piece_that_is_pinned_is_an_info_and_exits_0() {
    let world = world(alpha);
    world.run(&["pin", "alpha"]).expect_code(0);
    let run = world.run_tty_env(&["pin", "alpha"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "▌ ℹ INFO\n\
         ▌ Nothing to pin: projects/alpha is already pinned.\n\
         ▌\n\
         ▌   The file .antiq-pin at the top of the piece pins it, and this piece has it.\n\
         ▌   Nothing was changed.\n\
         ▌\n\
         ▌ → antiq status shows which pieces are pinned.\n"
    );
}

#[test]
fn a_pin_says_what_it_does_under_the_result() {
    let world = world(alpha);
    let run = world.run_tty_env(&["pin", "alpha"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "Pinned projects/alpha\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ A pinned piece is never archived.\n\
         ▌\n\
         ▌   The file projects/alpha/.antiq-pin marks it. push still mirrors the piece,\n\
         ▌   and status shows it as pinned.\n\
         ▌\n\
         ▌ → antiq unpin projects/alpha removes the file again.\n"
    );
}

#[test]
fn an_unpin_says_what_it_does_under_the_result() {
    let world = world(alpha);
    world.run(&["pin", "alpha"]).expect_code(0);
    let run = world.run_tty_env(&["unpin", "alpha"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "Unpinned projects/alpha\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ The piece can be archived again.\n\
         ▌\n\
         ▌   The file projects/alpha/.antiq-pin is removed. The piece is archived once it\n\
         ▌   has been idle long enough.\n"
    );
}

#[test]
fn a_pin_dry_run_says_what_pinning_would_do_before_the_footer() {
    let world = world(alpha);
    let run = world.run_tty_env(&["pin", "-n", "alpha"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would pin  projects/alpha\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ A pinned piece is never archived.\n\
         ▌\n\
         ▌   Pinning creates the file projects/alpha/.antiq-pin. push still mirrors a\n\
         ▌   pinned piece, and status shows it as pinned.\n\
         \n\
         \x20○ DRY RUN  Nothing was pinned. Run again without -n to pin the piece.\n"
    );
}

#[test]
fn an_unpin_dry_run_says_what_unpinning_would_do_before_the_footer() {
    let world = world(alpha);
    world.run(&["pin", "alpha"]).expect_code(0);
    let run = world.run_tty_env(&["unpin", "-n", "alpha"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would unpin  projects/alpha\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ An unpinned piece can be archived again.\n\
         ▌\n\
         ▌   Unpinning removes the file projects/alpha/.antiq-pin. The piece is archived\n\
         ▌   once it has been idle long enough.\n\
         \n\
         \x20○ DRY RUN  Nothing was unpinned. Run again without -n to unpin the piece.\n"
    );
}

#[test]
fn a_pipe_gets_the_one_line_sentences_only() {
    let world = world(alpha);
    let run = world.run(&["unpin", "alpha"]);
    run.expect_code(0);
    assert_eq!(run.stdout, "projects/alpha is not pinned\n");
    let run = world.run(&["pin", "alpha"]);
    assert_eq!(run.stdout, "Pinned projects/alpha\n");
}
