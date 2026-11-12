// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the console prints when a command refuses a piece reference: each
//! cause once, and the candidates of an ambiguous word. Every command that
//! takes a piece reference runs through the binary in text mode.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

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
fn the_one_candidate_of_an_inexact_word_is_a_line_on_a_pipe() {
    let world = world(Mode::Text, stored_pieces);
    let run = world.run(&["unearth", "scaner"]);
    run.expect_code(1);
    let lines: Vec<&str> = run.stderr.lines().collect();
    assert_eq!(
        lines,
        [
            "error: `scaner` is not the path of a piece, and this command acts only on a path",
            "  fix: Run the command again with the path of the piece: projects/scanner.",
            "    candidate: projects/scanner",
            "  while attempting to unearth the version",
        ]
    );
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
            "  fix: Name the piece by more of its path.",
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
        "error: `2026-11-04` names no single version of `projects/scanner`, as 2 versions match it: 2026-11-04T100000, 2026-11-04T090211\n  \
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
        "{\"format\":1,\"command\":\"unearth\",\"error\":{\"kind\":\"unknown_reference\",\"code\":\"antiq::reference::unknown\",\"message\":\"cannot find the piece: no piece matches `scnaer`, as none of the pieces has a path that fits it\",\"fix\":\"Check the spelling, or name the piece by its path.\"}}\n"
    );
}

#[test]
fn the_json_of_an_ambiguous_version_is_pinned_byte_for_byte() {
    let world = world(Mode::Json, two_versions_one_day);
    let run = world.run(&["unearth", "projects/scanner", "--version", "2026-11-04"]);
    run.expect_code(1);
    assert_eq!(
        run.stdout,
        "{\"format\":1,\"command\":\"unearth\",\"error\":{\"kind\":\"other\",\"code\":\"antiq::version::ambiguous\",\"message\":\"`2026-11-04` names no single version of `projects/scanner`, as 2 versions match it: 2026-11-04T100000, 2026-11-04T090211\",\"fix\":\"Name one version by more of its stamp, for example `2026-11-04T100000`.\"}}\n"
    );
}

/// A stored piece whose name holds two backticks.
fn ticked_piece(builder: &mut CollectionBuilder) {
    builder.version("projects/a`b`c", STAMP, &[("main.rs", "fn main() {}\n")]);
}

#[test]
fn a_piece_with_backticks_is_fenced_in_the_pipe_text_of_a_version_lookup() {
    let world = world(Mode::Text, ticked_piece);
    let run = world.run(&["unearth", "projects/a`b`c", "--version", "2020"]);
    run.expect_code(1);
    assert_eq!(
        run.stderr.lines().next().expect("a title"),
        "error: `2020` matches no version of ``projects/a`b`c``; there are 2026-11-04T090211"
    );
}

#[test]
fn a_piece_with_backticks_is_fenced_in_the_json_message_of_a_version_lookup() {
    let world = world(Mode::Json, ticked_piece);
    let run = world.run(&["unearth", "projects/a`b`c", "--version", "2020"]);
    run.expect_code(1);
    let json: Value = serde_json::from_str(&run.stdout).expect("a JSON envelope");
    assert_eq!(
        json["error"]["message"],
        "`2020` matches no version of ``projects/a`b`c``; there are 2026-11-04T090211"
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

/// A collection whose config is no TOML, which the parser explains over
/// several lines.
fn broken_config() -> World {
    let world = world(Mode::Text, |_| {});
    let path = world.collection().path(".antiq/config.toml");
    fs::write(path, "[collection\nid = 1\n").expect("the config is written");
    world
}

#[test]
fn a_message_of_several_lines_is_a_title_and_indented_lines_in_a_pipe() {
    let world = broken_config();
    let run = world.run(&["status"]);
    run.expect_code(1);
    let lines: Vec<&str> = run.stderr.lines().collect();
    assert!(
        lines[0].starts_with("error: invalid config in ") && lines[0].ends_with("config.toml:"),
        "{lines:?}"
    );
    assert_eq!(
        lines[1], "  TOML parse error at line 1, column 12",
        "{lines:?}"
    );
    assert_eq!(lines[3], "  1 | [collection", "{lines:?}");
    assert!(!run.stderr.contains("%0A"), "{:?}", run.stderr);
}

#[test]
fn a_message_of_several_lines_is_a_title_and_plain_lines_on_a_terminal() {
    let world = broken_config();
    let run = world.run_tty_stderr(&["status"], &[("NO_COLOR", "1")]);
    run.expect_code(1);
    let lines: Vec<&str> = run.stderr.lines().collect();
    assert_eq!(lines[0], "▌ ✖ ERROR", "{lines:?}");
    assert!(
        lines.contains(&"▌   TOML parse error at line 1, column 12"),
        "{lines:?}"
    );
    assert!(lines.contains(&"▌   1 | [collection"), "{lines:?}");
    assert!(!run.stderr.contains("%0A"), "{:?}", run.stderr);
}

#[test]
fn the_json_message_of_several_lines_keeps_its_line_breaks() {
    let world = broken_config();
    let run = world.run(&["--json", "status"]);
    run.expect_code(1);
    let json: Value = serde_json::from_str(&run.stdout).expect("a JSON object");
    let message = json["error"]["message"].as_str().expect("a message");
    assert!(
        message.contains(":\nTOML parse error at line 1, column 12\n"),
        "{message:?}"
    );
    assert!(!message.contains("%0A"), "{message:?}");
}
