// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the console prints when a command refuses a piece reference: each
//! cause once, and the candidates of an ambiguous word. Every command that
//! takes a piece reference runs through the binary in text mode.

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Outcome, Setup, World};

const STAMP: &str = "2026-11-04T090211";

fn world(mode: Mode, build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*", "tools/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), mode)
}

/// Two live pieces whose paths both contain `scan`.
fn live_pieces(builder: &mut CollectionBuilder) {
    for piece in ["projects/scanner", "tools/scan-old"] {
        builder.piece(piece, |piece| {
            piece.age_days(40).file("data", "x");
        });
    }
}

/// Two pieces in the store whose paths both contain `scan`.
fn stored_pieces(builder: &mut CollectionBuilder) {
    builder
        .version("projects/scanner", STAMP, &[("main.rs", "fn main() {}\n")])
        .version("tools/scan-old", STAMP, &[("run.sh", "echo\n")]);
}

/// How often `needle` occurs in `text`.
fn times(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

/// Checks the console form of an ambiguous `scan`: each candidate path and
/// the sentence about the match once, no line twice.
fn assert_listed_once(run: &Outcome) {
    run.expect_code(1);
    let stderr = &run.stderr;
    assert_eq!(times(stderr, "fits the paths of 2 pieces"), 1, "{stderr}");
    assert_eq!(times(stderr, "candidate: projects/scanner"), 1, "{stderr}");
    assert_eq!(times(stderr, "candidate: tools/scan-old"), 1, "{stderr}");
    let mut lines: Vec<&str> = stderr.lines().collect();
    lines.sort_unstable();
    let count = lines.len();
    lines.dedup();
    assert_eq!(lines.len(), count, "a line appears twice:\n{stderr}");
}

#[test]
fn archive_lists_the_candidates_of_an_ambiguous_word() {
    let world = world(Mode::Text, live_pieces);
    assert_listed_once(&world.run(&["archive", "scan"]));
}

#[test]
fn unearth_lists_the_candidates_of_an_ambiguous_word_and_says_it_once() {
    let world = world(Mode::Text, stored_pieces);
    assert_listed_once(&world.run(&["unearth", "scan"]));
}

#[test]
fn forget_lists_the_candidates_of_an_ambiguous_word() {
    let world = world(Mode::Text, stored_pieces);
    assert_listed_once(&world.run(&["forget", "scan", STAMP]));
}

#[test]
fn pin_lists_the_candidates_of_an_ambiguous_word() {
    let world = world(Mode::Text, live_pieces);
    assert_listed_once(&world.run(&["pin", "scan"]));
}

#[test]
fn unpin_lists_the_candidates_of_an_ambiguous_word() {
    let world = world(Mode::Text, live_pieces);
    assert_listed_once(&world.run(&["unpin", "scan"]));
}

#[test]
fn rebuild_lists_the_candidates_of_an_ambiguous_word() {
    let world = world(Mode::Text, stored_pieces);
    assert_listed_once(&world.run(&["rebuild", "scan", STAMP]));
}

#[test]
fn the_candidates_come_before_the_lines_naming_what_the_run_was_doing() {
    let world = world(Mode::Text, stored_pieces);
    let run = world.run(&["unearth", "scan"]);
    run.expect_code(1);
    let lines: Vec<&str> = run.stderr.lines().collect();
    assert_eq!(
        lines,
        [
            "error: no unique piece matches `scan`, as it fits the paths of 2 pieces",
            "  fix: Name the piece by more of its path, for example projects/scanner.",
            "    candidate: projects/scanner",
            "    candidate: tools/scan-old",
            "  while attempting to unearth the version",
        ]
    );
}

#[test]
fn the_json_message_of_an_ambiguous_unearth_says_it_once() {
    let world = world(Mode::Json, stored_pieces);
    let run = world.run(&["unearth", "scan"]);
    run.expect_code(1);
    let json: Value = serde_json::from_str(&run.stdout).expect("a JSON envelope");
    assert_eq!(
        json["error"]["message"],
        "cannot find the piece: no unique piece matches `scan`, as it fits the paths of 2 pieces"
    );
    assert_eq!(
        json["error"]["candidates"],
        serde_json::json!([{"path": "projects/scanner"}, {"path": "tools/scan-old"}])
    );
}

/// Two versions of one piece whose local times both start with the same
/// date, so the date names neither.
fn two_versions_one_day(builder: &mut CollectionBuilder) {
    builder
        .version("projects/scanner", STAMP, &[("main.rs", "fn main() {}\n")])
        .version(
            "projects/scanner",
            "2026-11-04T100000",
            &[("main.rs", "fn main() {}\n")],
        );
}

#[test]
fn an_ambiguous_version_lists_one_candidate_line_per_stamp_on_a_pipe() {
    let world = world(Mode::Text, two_versions_one_day);
    let run = world.run(&["unearth", "projects/scanner", "--version", "2026-11-04"]);
    run.expect_code(1);
    assert_eq!(
        run.stderr,
        "error: `2026-11-04` names no single version of projects/scanner, as 2 versions match it: 2026-11-04T100000, 2026-11-04T090211\n  \
         fix: Name one version by more of its stamp, for example 2026-11-04T100000.\n    \
         candidate: 2026-11-04T100000\n    \
         candidate: 2026-11-04T090211\n  \
         while attempting to unearth the version\n"
    );
}

#[test]
fn the_json_of_an_unknown_piece_is_pinned_byte_for_byte() {
    let world = world(Mode::Json, stored_pieces);
    let run = world.run(&["unearth", "scnaer"]);
    run.expect_code(1);
    assert_eq!(
        run.stdout,
        "{\"format\":1,\"command\":\"unearth\",\"error\":{\"kind\":\"unknown_reference\",\"code\":\"antiq::reference::unknown\",\"message\":\"cannot find the piece: no piece matches `scnaer`, as none of the pieces has a path that fits it\",\"fix\":\"Check the spelling, or look for the piece with `antiq list`.\"}}\n"
    );
}

#[test]
fn the_json_of_an_ambiguous_version_is_pinned_byte_for_byte() {
    let world = world(Mode::Json, two_versions_one_day);
    let run = world.run(&["unearth", "projects/scanner", "--version", "2026-11-04"]);
    run.expect_code(1);
    assert_eq!(
        run.stdout,
        "{\"format\":1,\"command\":\"unearth\",\"error\":{\"kind\":\"other\",\"code\":\"antiq::version::ambiguous\",\"message\":\"`2026-11-04` names no single version of projects/scanner, as 2 versions match it: 2026-11-04T100000, 2026-11-04T090211\",\"fix\":\"Name one version by more of its stamp, for example `2026-11-04T100000`.\"}}\n"
    );
}

/// Makes `path` read-only, and writable again when dropped, also after a
/// panic, so the temporary directory can always be removed.
struct ReadOnly(PathBuf);

impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_pin_that_fails_below_a_marker_error_says_each_cause_once() {
    let world = world(Mode::Text, live_pieces);
    let tmp = world.collection().path(".antiq/tmp");
    fs::create_dir_all(&tmp).expect("the staging folder");
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).expect("read-only");
    let _restore = ReadOnly(tmp.clone());
    assert!(
        fs::write(tmp.join("probe"), "").is_err(),
        "the test needs a user that a read-only directory stops"
    );

    let run = world.run(&["pin", "projects/scanner"]);

    run.expect_code(1);
    assert!(
        run.stderr.contains("cannot create the pin file"),
        "{}",
        run.stderr
    );
    let mut lines: Vec<&str> = run.stderr.lines().collect();
    let count = lines.len();
    lines.sort_unstable();
    lines.dedup();
    assert_eq!(lines.len(), count, "a line appears twice:\n{}", run.stderr);
    assert_eq!(
        times(&run.stderr, "cannot create the pin file"),
        1,
        "{}",
        run.stderr
    );
    let first = run.stderr.lines().next().expect("an error line");
    let causes = first.matches("Permission denied").count();
    assert_eq!(causes, 1, "{first}");
}
