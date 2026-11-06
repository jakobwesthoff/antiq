// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a terminal shows when a piece reference or a version names more
//! than one thing, or nothing: the error, the table of what it may have
//! meant, and the info that nothing was done. The harness runs every
//! command under `TZ=Pacific/Auckland`, +13:00 in November 2026.

mod support;

use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};

const NEW: &str = "2026-11-04T090211";
const OLD: &str = "2026-09-03T121500";

/// Content that does not compress, so that every archive is some 200 bytes
/// and its size has three digits however the headers vary.
const PAYLOAD: &str = "5feceb66ffc86f38d952786c6d696c79c2dbc239dd4e91b46729d73a27fb57e96b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4bd4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab354e07408562bedb8b60ce05c1decfe3ad16b72230967de01f";

fn world(build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*", "tools/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), Mode::Text)
}

/// Two pieces in the store whose paths both contain `scan`, the first with
/// two versions.
fn stored_pieces(builder: &mut CollectionBuilder) {
    builder
        .version("projects/scanner", OLD, &[("main.rs", PAYLOAD)])
        .version("projects/scanner", NEW, &[("main.rs", PAYLOAD)])
        .version("tools/scan-old", NEW, &[("run.sh", PAYLOAD)]);
}

/// Two live pieces whose paths both contain `scan`, the first with a stored
/// version.
fn live_pieces(builder: &mut CollectionBuilder) {
    for piece in ["projects/scanner", "tools/scan-old"] {
        builder.piece(piece, |piece| {
            piece.age_days(40).file("data", "x");
        });
    }
    builder.version("projects/scanner", NEW, &[("main.rs", PAYLOAD)]);
}

/// What the command wrote to a stderr terminal without colour. The size of
/// an archive depends on the bytes of its headers, so each digit of a size
/// is `N`: the width of the column stays checked.
fn plain(world: &World, args: &[&str]) -> String {
    let run = world.run_tty_stderr_from_root(args, &[("NO_COLOR", "1")]);
    run.expect_code(1);
    mask_sizes(&run.stderr)
}

fn mask_sizes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut masked = String::new();
    let mut at = 0;
    while at < chars.len() {
        if !chars[at].is_ascii_digit() {
            masked.push(chars[at]);
            at += 1;
            continue;
        }
        let end = (at..chars.len())
            .find(|index| !chars[*index].is_ascii_digit())
            .unwrap_or(chars.len());
        let is_size = chars[end..].starts_with(&[' ', 'B'])
            && !chars
                .get(end + 2)
                .is_some_and(|next| next.is_alphanumeric());
        for digit in &chars[at..end] {
            masked.push(if is_size { 'N' } else { *digit });
        }
        at = end;
    }
    masked
}

#[test]
fn an_ambiguous_word_lists_the_list_table_of_the_store_pieces() {
    let world = world(stored_pieces);
    assert_eq!(
        plain(&world, &["unearth", "scan"]),
        "\x20✖ ERROR\n\
         ▌ No unique piece matches `scan`: it matches 2 pieces\n\
         ▌\n\
         ▌ Why it happened\n\
         ▌   The search word `scan` fits the paths of two pieces, listed below.\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   Name the piece by more of its path, for example projects/scanner.\n\
         ▌\n\
         ▌ What antiq was doing\n\
         ▌   Attempting to find the piece to unearth.\n\
         \n\
         The 2 pieces that match `scan`\n\
         PIECE              VERSION               SIZE    FILES\n\
         projects/scanner   2026-11-04 22:02:11   NNN B       1\n\
         projects/scanner   2026-09-04 00:15:00   NNN B       1\n\
         tools/scan-old     2026-11-04 22:02:11   NNN B       1\n\
         \n\
         \x20ℹ INFO  Nothing was unearthed.\n"
    );
}

#[test]
fn an_ambiguous_word_for_a_live_piece_counts_the_versions() {
    let world = world(live_pieces);
    assert_eq!(
        plain(&world, &["pin", "scan"]),
        "\x20✖ ERROR\n\
         ▌ No unique piece matches `scan`: it matches 2 pieces\n\
         ▌\n\
         ▌ Why it happened\n\
         ▌   The search word `scan` fits the paths of two pieces, listed below.\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   Name the piece by more of its path, for example projects/scanner.\n\
         ▌\n\
         ▌ What antiq was doing\n\
         ▌   Attempting to find the piece to pin.\n\
         \n\
         The 2 pieces that match `scan`\n\
         PIECE              VERSIONS\n\
         projects/scanner          1\n\
         tools/scan-old            0\n\
         \n\
         \x20ℹ INFO  Nothing was pinned.\n"
    );
}

#[test]
fn an_ambiguous_version_marks_its_matches_among_all_versions() {
    let world = world(|builder| {
        builder
            .version("projects/scanner", "2025-06-01T103000", &[("a", PAYLOAD)])
            .version("projects/scanner", "2025-06-18T154509", &[("a", PAYLOAD)])
            .version("projects/scanner", "2025-11-20T071530", &[("a", PAYLOAD)])
            .version("projects/scanner", NEW, &[("a", PAYLOAD)]);
    });
    assert_eq!(
        plain(
            &world,
            &["unearth", "projects/scanner", "--version", "2025-06"]
        ),
        "\x20✖ ERROR\n\
         ▌ `2025-06` matches 2 of the 4 versions of projects/scanner\n\
         ▌\n\
         ▌ Why it happened\n\
         ▌   Two versions start with `2025-06`, marked below.\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   Name one version by more of its stamp, for example antiq unearth\n\
         ▌   projects/scanner --version 2025-06-18T154509.\n\
         ▌\n\
         ▌ What antiq was doing\n\
         ▌   Attempting to find the version to unearth.\n\
         \n\
         The versions of projects/scanner, the 2 matches marked\n\
         \x20\x20PIECE              VERSION               SIZE    FILES\n\
         \x20\x20projects/scanner   2026-11-04 22:02:11   NNN B       1\n\
         \x20\x20projects/scanner   2025-11-20 20:15:30   NNN B       1\n\
         ▸ projects/scanner   2025-06-19 03:45:09   NNN B       1\n\
         ▸ projects/scanner   2025-06-01 22:30:00   NNN B       1\n\
         \n\
         \x20ℹ INFO  Nothing was unearthed.\n"
    );
}

#[test]
fn an_unknown_word_offers_the_closest_pieces_and_a_command_to_copy() {
    let world = world(stored_pieces);
    assert_eq!(
        plain(&world, &["unearth", "scnaer"]),
        "\x20✖ ERROR\n\
         ▌ No piece matches `scnaer`\n\
         ▌\n\
         ▌ Why it happened\n\
         ▌   antiq compared the word with the path of every piece in the store, and none\n\
         ▌   fits.\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   Check the spelling, or look for the piece with antiq list.\n\
         ▌\n\
         ▌ What antiq was doing\n\
         ▌   Attempting to find the piece to unearth.\n\
         \n\
         The closest pieces\n\
         PIECE              VERSION               SIZE    FILES\n\
         projects/scanner   2026-11-04 22:02:11   NNN B       1\n\
         projects/scanner   2026-09-04 00:15:00   NNN B       1\n\
         \n\
         \x20ℹ INFO  Nothing was unearthed.\n\
         \n\
         \x20→ HINT  Did you mean projects/scanner? Run antiq unearth projects/scanner.\n"
    );
}

#[test]
fn an_unknown_word_with_nothing_near_offers_no_piece() {
    let world = world(stored_pieces);
    assert_eq!(
        plain(&world, &["unearth", "zzzzzzzz"]),
        "\x20✖ ERROR\n\
         ▌ No piece matches `zzzzzzzz`\n\
         ▌\n\
         ▌ Why it happened\n\
         ▌   antiq compared the word with the path of every piece in the store, and none\n\
         ▌   fits.\n\
         ▌\n\
         ▌ How to fix it\n\
         ▌   Check the spelling, or look for the piece with antiq list.\n\
         ▌\n\
         ▌ What antiq was doing\n\
         ▌   Attempting to find the piece to unearth.\n\
         \n\
         \x20ℹ INFO  Nothing was unearthed.\n"
    );
}

#[test]
fn colour_marks_the_matches_and_dims_the_rest() {
    let world = world(|builder| {
        builder
            .version("projects/scanner", "2025-06-01T103000", &[("a", PAYLOAD)])
            .version("projects/scanner", "2025-06-18T154509", &[("a", PAYLOAD)])
            .version("projects/scanner", NEW, &[("a", PAYLOAD)]);
    });
    let run = world.run_tty_stderr(
        &["unearth", "projects/scanner", "--version", "2025-06"],
        &[],
    );
    run.expect_code(1);
    let table: Vec<&str> = run
        .stderr
        .lines()
        .skip_while(|line| !line.contains("PIECE"))
        .take(4)
        .collect();
    let masked = mask_sizes(&table.join("\n"));
    // The rows that are not matches are dim in every column, the matches
    // carry a bold red marker and a bold red start of the time.
    assert_eq!(
        masked,
        "\x20\x20\x1b[2mPIECE              VERSION               SIZE    FILES\x1b[0m\n\
         \x20\x20\x1b[2mprojects/scanner\x1b[0m   \x1b[2m2026-11-04 22:02:11\x1b[0m   \x1b[2mNNN B\x1b[0m       \x1b[2m1\x1b[0m\n\
         \x1b[1m\x1b[31m▸\x1b[0m projects/scanner   \x1b[1m\x1b[31m2025-06\x1b[0m-19 03:45:09   NNN B       1\n\
         \x1b[1m\x1b[31m▸\x1b[0m projects/scanner   \x1b[1m\x1b[31m2025-06\x1b[0m-01 22:30:00   NNN B       1"
    );
}

#[test]
fn a_version_that_does_not_exist_is_an_error_without_a_table_or_an_info() {
    let world = world(stored_pieces);
    let text = plain(
        &world,
        &["unearth", "projects/scanner", "--version", "1999"],
    );
    assert!(text.starts_with("\x20✖ ERROR\n"), "{text}");
    assert!(!text.contains("Nothing was"), "{text}");
    assert!(!text.contains("VERSION  "), "{text}");
}
