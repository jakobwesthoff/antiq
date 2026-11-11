// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a dry run (`-n`) prints on a terminal and in a pipe: a tag in front
//! of each action and a footer on a terminal, the `Would ...` lines alone
//! in a pipe. The harness runs every command under `TZ=Pacific/Auckland`,
//! +13:00 in November 2026.

mod support;

use support::cmd::Antiq;
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World, run_on_terminal};
use support::tree::Tree;

const NEW: &str = "2026-11-04T090211";

/// No colour, so the text can be compared as it reads.
const PLAIN: &[(&str, &str)] = &[("NO_COLOR", "1")];

fn world(build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*", "tools/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), Mode::Text)
}

fn due_pieces(builder: &mut CollectionBuilder) {
    for piece in ["projects/alpha", "projects/scanner", "tools/scan-old"] {
        builder.piece(piece, |piece| {
            piece.age_days(40).file("data", "x");
        });
    }
}

#[test]
fn an_archive_dry_run_on_a_terminal_tags_each_action_and_ends_with_a_footer() {
    let world = world(due_pieces);
    let run = world.run_tty_env(&["archive", "-n"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would archive  projects/alpha\n\
         ○ would archive  projects/scanner\n\
         ○ would archive  tools/scan-old\n\
         \n\
         \x20○ DRY RUN  Nothing was archived. Run again without -n to archive these pieces.\n"
    );
}

#[test]
fn a_dry_run_on_a_terminal_shows_what_a_piece_leaves_out_under_it_and_widens_every_tag() {
    let world = world(|builder| {
        builder.ignore(&["node_modules/"]);
        builder.piece("projects/alpha", |piece| {
            piece
                .age_days(40)
                .file("data", "x")
                .file("node_modules/dep/index.js", "x");
        });
        builder.piece("tools/scan-old", |piece| {
            piece.age_days(40).file("data", "x");
        });
    });
    let run = world.run_tty_env(&["archive", "-n"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would archive    projects/alpha\n\
         ○ would leave out  node_modules/\n\
         ○ would archive    tools/scan-old\n\
         \n\
         \x20○ DRY RUN  Nothing was archived. Run again without -n to archive these pieces.\n"
    );
}

#[test]
fn a_dry_run_on_a_terminal_colours_the_tag_and_the_footer_badge_magenta() {
    let world = world(due_pieces);
    let run = world.run_tty_env(&["archive", "-n", "alpha"], &[]);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "\x1b[1m\x1b[35m○ would archive\x1b[0m  projects/alpha\n\
         \n\
         \x1b[30m\x1b[45m ○ DRY RUN \x1b[0m Nothing was archived. Run again without \x1b[1m-n\x1b[0m to archive the piece.\n"
    );
}

#[test]
fn a_dry_run_in_a_pipe_has_the_would_lines_and_no_footer() {
    let world = world(due_pieces);
    let run = world.run(&["archive", "-n"]);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "Would archive projects/alpha\n\
         Would archive projects/scanner\n\
         Would archive tools/scan-old\n"
    );
}

#[test]
fn a_dry_run_with_nothing_to_do_has_no_footer() {
    let world = world(|_| {});
    let run = world.run_tty_env(&["archive", "-n"], PLAIN);
    run.expect_code(0);
    assert_eq!(run.stdout, "Nothing to do.\n");
}

#[test]
fn a_forget_dry_run_lists_the_files_of_the_version_and_says_the_deletion_is_final() {
    let world = world(|builder| {
        builder.version("projects/scanner", NEW, &[("main.rs", "fn main() {}\n")]);
    });
    let run = world.run_tty_env(&["forget", "-n", "projects/scanner", "2026-11-04"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would delete  version 2026-11-04 22:02:11 of projects/scanner, three files:\n\
         \x20 archive    projects-scanner--2026-11-04T090211.tar.zst\n\
         \x20 listing    projects-scanner--2026-11-04T090211.sha256.zst\n\
         \x20 checksum   projects-scanner--2026-11-04T090211.tar.zst.sha256\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ forget deletes a version for good.\n\
         ▌\n\
         ▌   unearth and fork cannot bring it back afterwards.\n\
         \n\
         \x20○ DRY RUN  Nothing was deleted. Run again without -n to delete the version.\n"
    );
}

#[test]
fn an_init_dry_run_does_not_print_the_id_a_real_run_would_mint() {
    let mut tree = Tree::new();
    tree.dir("a");
    let antiq = Antiq::new()
        .arg("-C")
        .arg(tree.root())
        .args(["init", "-n", "--pattern", "a*"])
        .env("NO_COLOR", "1");
    let (run, stdout) = run_on_terminal(antiq);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    // The temporary directory is a link on macOS, which the run resolves.
    let root = std::fs::canonicalize(tree.root())
        .expect("the tree exists")
        .display()
        .to_string();
    let shown = stdout.replace(&root, "<root>");
    assert!(
        shown.starts_with(
            "○ would create  the antiq collection at <root>\n\
             ○ would write   .antiq/config.toml, the settings of the collection:\n\
             │ # antiq collection configuration.\n\
             │\n\
             │ [collection]\n\
             │ # ID of this collection. The marker `push` writes to the target records it.\n\
             │ # id is minted by the real run\n\
             │\n\
             │ # Glob patterns"
        ),
        "{shown}"
    );
    assert!(
        shown.ends_with(
            "\n\x20○ DRY RUN  Nothing was written. Run again without -n to create the antiq\n            collection.\n"
        ),
        "{shown}"
    );
    assert!(!shown.contains("id = "), "{shown}");
}

#[test]
fn an_init_dry_run_in_a_pipe_prints_the_config_with_the_comment_in_place_of_the_id() {
    let mut tree = Tree::new();
    tree.dir("a");
    let run = Antiq::new()
        .arg("-C")
        .arg(tree.root())
        .args(["init", "-n", "--pattern", "a*"])
        .run();
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(
        run.stdout
            .contains("[collection]\n# ID of this collection. The marker `push` writes to the target records it.\n# id is minted by the real run\n"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("id = "), "{}", run.stdout);
    assert!(
        run.stderr.starts_with("Would create a collection at "),
        "{}",
        run.stderr
    );
}

fn one_scanner_version(builder: &mut CollectionBuilder) {
    builder.version("projects/scanner", NEW, &[("main.rs", "fn main() {}\n")]);
}

#[test]
fn a_rebuild_dry_run_names_what_is_checked_and_written_and_explains_rebuild() {
    let world = world(one_scanner_version);
    world
        .collection()
        .damage_remove_listing("projects/scanner", NEW);
    let run = world.run_tty_env(&["rebuild", "-n", "projects/scanner", NEW], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would rebuild  version 2026-11-04 22:02:11 of projects/scanner\n\
         \x20 check    projects-scanner--2026-11-04T090211.tar.zst\n\
         \x20 against  projects-scanner--2026-11-04T090211.tar.zst.sha256\n\
         \x20 write    projects-scanner--2026-11-04T090211.sha256.zst\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ rebuild repairs a damaged version by writing only what is missing.\n\
         ▌\n\
         ▌   It checks the archive against what survives first and runs only when asked,\n\
         ▌   never as a repair.\n\
         \n\
         \x20○ DRY RUN  Nothing was rebuilt. Run again without -n to rebuild the version.\n"
    );
}

#[test]
fn a_rebuild_dry_run_in_a_pipe_keeps_its_one_sentence() {
    let world = world(one_scanner_version);
    world
        .collection()
        .damage_remove_listing("projects/scanner", NEW);
    let run = world.run(&["rebuild", "-n", "projects/scanner", NEW]);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "Would check projects-scanner--2026-11-04T090211.tar.zst against projects-scanner--2026-11-04T090211.tar.zst.sha256 and write projects-scanner--2026-11-04T090211.sha256.zst\n"
    );
}

#[test]
fn an_unearth_dry_run_lists_the_checks_and_explains_unearth() {
    let world = world(one_scanner_version);
    let run = world.run_tty_env(&["unearth", "-n", "projects/scanner"], PLAIN);
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would unearth  projects/scanner (version 2026-11-04 22:02:11)\n\
         Checks the real run makes first:\n\
         \x20 the version 2026-11-04 22:02:11 is sound\n\
         \x20 the destination projects/scanner is free\n\
         \x20 there is room for 13 B\n\
         \x20 the member names fit the volume (not run by -n)\n\
         \x20 the extracted tree equals the listing (not run by -n)\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ unearth brings a stored version back as a live folder.\n\
         ▌\n\
         ▌   It removes the version from the store once the folder is complete and\n\
         ▌   verified.\n\
         \n\
         \x20○ DRY RUN  Nothing was unearthed. Run again without -n to unearth the version.\n"
    );
}

#[test]
fn a_fork_dry_run_lists_the_checks_and_explains_fork() {
    let world = world(one_scanner_version);
    let run = world.run_tty_env(
        &["fork", "-n", "projects/scanner", "--name", "tools/copy"],
        PLAIN,
    );
    run.expect_code(0);
    assert_eq!(
        run.stdout,
        "○ would fork  projects/scanner (version 2026-11-04 22:02:11) to tools/copy\n\
         Checks the real run makes first:\n\
         \x20 the version 2026-11-04 22:02:11 is sound\n\
         \x20 the destination tools/copy is free\n\
         \x20 there is room for 13 B\n\
         \x20 the member names fit the volume (not run by -n)\n\
         \x20 the extracted tree equals the listing (not run by -n)\n\
         \n\
         ▌ ℹ INFO\n\
         ▌ fork extracts a working copy of a stored version.\n\
         ▌\n\
         ▌   The version stays in the store unchanged.\n\
         \n\
         \x20○ DRY RUN  Nothing was forked. Run again without -n to fork the version.\n"
    );
}

#[test]
fn the_dry_runs_of_unearth_and_fork_in_a_pipe_keep_their_check_lines() {
    let world = world(one_scanner_version);
    let run = world.run(&["unearth", "-n", "projects/scanner"]);
    run.expect_code(0);
    assert!(
        run.stdout.starts_with(
            "Would unearth projects/scanner (version 2026-11-04T090211)\n  check the version"
        ),
        "{}",
        run.stdout
    );
}
