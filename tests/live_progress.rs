// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What `archive`, `push`, `unearth` and `fork` show on a terminal while they work
//! and what stays after they end. The bytes of a run on a pseudo-terminal
//! are replayed on a screen, so the tests read what a person would see.

mod support;

use std::fs;
use support::cmd::hold_lock;
use support::fake_rsync::FakeRsync;
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};
use support::screen::Screen;

const NEW: &str = "2026-11-04T090211";

fn archive_world(count: usize) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*"]);
    for number in 1..=count {
        builder.piece(&format!("projects/p{number:02}"), |piece| {
            piece
                .age_days(40)
                .file("src/main.rs", "fn main() {}\n")
                .file("README", "read me\n");
        });
    }
    World::new(Setup::new(builder.finish()), Mode::Text)
}

fn replay(stdout: &str, columns: usize, rows: usize) -> Screen {
    let mut screen = Screen::new(columns, rows);
    screen.feed(stdout.as_bytes());
    assert!(screen.unsupported.is_empty(), "{:?}", screen.unsupported);
    screen
}

/// Whether the bytes hold a colour sequence.
fn has_colour(bytes: &str) -> bool {
    let mut rest = bytes;
    while let Some(start) = rest.find("\x1b[") {
        let tail = &rest[start + 2..];
        let end = tail
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(tail.len());
        if tail[end..].starts_with('m') {
            return true;
        }
        rest = &tail[end..];
    }
    false
}

/// Whether `time` is an elapsed time as `m:ss` or `h:mm:ss`, whatever the
/// machine made of it.
fn is_elapsed(time: &str) -> bool {
    let parts: Vec<&str> = time.split(':').collect();
    let digits = |part: &&str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    (parts.len() == 2 || parts.len() == 3)
        && parts.iter().all(digits)
        && parts[1..].iter().all(|part| part.len() == 2)
}

fn is_summary(line: &str, words: &str) -> bool {
    let Some(rest) = line.strip_prefix(words) else {
        return false;
    };
    let Some(time) = rest.strip_prefix("   took ") else {
        return false;
    };
    is_elapsed(time)
}

/// The stdout of a `--json` run on a terminal: one JSON object of the
/// command and nothing of the cards.
fn assert_only_json(stdout: &str, command: &str) {
    for card in ["\x1b", "Pushing to", "waiting", "╸", "⠋", "⠙", "⠹", "⠸"] {
        assert!(!stdout.contains(card), "{card:?} in {stdout:?}");
    }
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout:?}");
    let json: serde_json::Value = serde_json::from_str(lines[0]).expect("one JSON object");
    assert_eq!(json["command"], command);
}

// =========================================================
// archive
// =========================================================

#[test]
fn archive_leaves_one_finished_line_per_piece_and_the_summary() {
    let world = archive_world(3);
    let run = world.run_tty_sized(&["archive"], 100, 24, &[]);
    run.expect_code(0);
    let lines = replay(&run.stdout, 100, 24).lines();
    for name in ["projects/p01", "projects/p02", "projects/p03"] {
        let found: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with(&format!("  ✔ {name} ")))
            .collect();
        assert_eq!(found.len(), 1, "{name} in {lines:#?}");
        assert!(found[0].contains("2 files"), "{}", found[0]);
    }
    assert_eq!(lines.len(), 4 + 1, "{lines:#?}");
    assert_eq!(lines[3], "");
    assert!(
        is_summary(&lines[4], "✔ 3 archived · 0 skipped · 0 refused"),
        "{:?}",
        lines[4]
    );
}

#[test]
fn archive_with_more_pieces_than_rows_keeps_every_line_in_the_scrollback() {
    let world = archive_world(12);
    let run = world.run_tty_sized(&["archive"], 100, 8, &[]);
    run.expect_code(0);
    let screen = replay(&run.stdout, 100, 8);
    let lines = screen.lines();
    let finished = lines.iter().filter(|line| line.starts_with("  ✔ ")).count();
    assert_eq!(finished, 12, "{lines:#?}");
    assert!(
        is_summary(
            lines.last().expect("a last line"),
            "✔ 12 archived · 0 skipped · 0 refused"
        ),
        "{lines:#?}"
    );
    assert!(
        screen.visible().iter().all(|row| !row.contains("waiting")),
        "{:?}",
        screen.visible()
    );
}

#[test]
fn archive_on_a_narrow_terminal_keeps_its_lines_within_the_width() {
    let world = archive_world(3);
    let run = world.run_tty_sized(&["archive"], 60, 10, &[]);
    run.expect_code(0);
    let lines = replay(&run.stdout, 60, 10).lines();
    assert!(
        lines.iter().all(|line| line.chars().count() <= 60),
        "{lines:#?}"
    );
    assert_eq!(
        lines.iter().filter(|line| line.starts_with("  ✔ ")).count(),
        3
    );
}

#[test]
fn archive_without_colour_writes_no_colour_sequence() {
    let world = archive_world(2);
    let run = world.run_tty_sized(&["archive"], 100, 24, &[("NO_COLOR", "1")]);
    run.expect_code(0);
    assert!(!has_colour(&run.stdout), "{:?}", run.stdout);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert_eq!(
        lines.iter().filter(|line| line.starts_with("  ✔ ")).count(),
        2
    );
}

#[test]
fn archive_with_colour_paints_the_marks() {
    let world = archive_world(2);
    let run = world.run_tty_sized(&["archive"], 100, 24, &[]);
    assert!(run.stdout.contains("\x1b[32m✔\x1b[0m"), "{:?}", run.stdout);
}

#[test]
fn archive_in_a_non_utf8_locale_writes_ascii_only() {
    let world = archive_world(2);
    let run = world.run_tty_sized(&["archive"], 100, 24, &[("LC_ALL", "C")]);
    run.expect_code(0);
    assert!(run.stdout.is_ascii(), "{:?}", run.stdout);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("  ok projects/p01 ")),
        "{lines:#?}"
    );
}

#[test]
fn archive_on_a_dumb_terminal_prints_each_line_when_its_piece_ends_and_never_moves_the_cursor() {
    let world = archive_world(2);
    let run = world.run_tty_sized(&["archive"], 100, 24, &[("TERM", "dumb")]);
    run.expect_code(0);
    assert!(
        !run.stdout.contains('\x1b') && !run.stdout.contains('\r'),
        "{:?}",
        run.stdout
    );
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{lines:#?}");
    // The pieces run side by side, so either one may finish first.
    let mut names: Vec<&str> = lines[..2]
        .iter()
        .map(|line| line.split_whitespace().nth(1).expect("a name"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["projects/p01", "projects/p02"]);
    assert!(is_summary(lines[3], "✔ 2 archived · 0 skipped · 0 refused"));
}

#[test]
fn a_dry_run_on_a_terminal_shows_no_cards() {
    let world = archive_world(2);
    let run = world.run_tty_sized(&["archive", "-n"], 100, 24, &[]);
    run.expect_code(0);
    assert!(!run.stdout.contains('╸') && !run.stdout.contains("waiting"));
    assert!(!run.stdout.contains("\x1b[1A") && !run.stdout.contains("\x1b[J"));
}

#[test]
fn a_json_run_on_a_terminal_prints_the_json_and_no_cards() {
    let archive = archive_world(2);
    let run = archive.run_tty_sized(&["--json", "archive"], 100, 24, &[]);
    run.expect_code(0);
    assert_only_json(&run.stdout, "archive");
}

#[test]
fn a_run_with_nothing_due_prints_the_text_it_always_did() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["projects/*"])
        .piece("projects/fresh", |piece| {
            piece.age_days(1).file("a", "a");
        })
        .finish();
    let world = World::new(Setup::new(collection), Mode::Text);
    let run = world.run_tty_sized(&["archive"], 100, 24, &[]);
    run.expect_code(0);
    assert_eq!(run.stdout, "Archived 0, skipped 0, refused 0.\n");
}

#[test]
fn a_pipe_keeps_the_lines_of_old() {
    let world = archive_world(2);
    let run = world.run(&["archive"]);
    run.expect_code(0);
    let mut lines: Vec<&str> = run.stdout.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "Archived 2, skipped 0, refused 0.",
            "archived projects/p01",
            "archived projects/p02",
        ]
    );
}

#[test]
fn a_named_pinned_piece_is_refused_with_the_command_that_releases_it() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["projects/*"])
        .piece("projects/antiq", |piece| {
            piece.age_days(40).file("a", "a").pinned();
        })
        .finish();
    let root = collection.root();
    let world = World::new(Setup::new(collection), Mode::Text);
    let run = world.run_tty_sized(&["archive", "projects/antiq"], 100, 24, &[]);
    run.expect_code(1);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(lines[0].starts_with("  ✖ projects/antiq "), "{lines:#?}");
    assert!(lines[0].contains("refused"));
    let blank = lines
        .iter()
        .position(String::is_empty)
        .expect("a blank line");
    // The hint wraps wherever the width ends, also inside a long path at a
    // hyphen, so the words are compared without their blanks.
    let reason: String = lines[1..blank]
        .iter()
        .flat_map(|line| line.split_whitespace())
        .collect();
    let expected = format!(
        "pinned, so it stays as it is. antiq -C {} unpin projects/antiq releases it",
        root.display()
    );
    assert_eq!(
        reason,
        expected.split_whitespace().collect::<String>(),
        "{lines:#?}"
    );
    assert!(
        lines
            .last()
            .expect("a summary")
            .starts_with("✖ 0 archived · 0 skipped · 1 refused")
    );
}

// =========================================================
// unearth
// =========================================================

fn unearth_world() -> World {
    let clock = Clock::now();
    let mut builder: CollectionBuilder = Collection::build(&clock);
    builder.patterns(&["projects/*"]).version(
        "projects/scanner",
        NEW,
        &[("src/main.rs", "fn main() {}\n"), ("README", "scan\n")],
    );
    World::new(Setup::new(builder.finish()), Mode::Text)
}

#[test]
fn unearth_leaves_the_line_of_the_piece_and_a_summary_with_the_destination() {
    let world = unearth_world();
    let run = world.run_tty_sized(&["unearth", "projects/scanner"], 100, 24, &[]);
    run.expect_code(0);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        lines[0].starts_with("  ✔ projects/scanner version 2026-11-04 "),
        "{lines:#?}"
    );
    assert!(lines[0].contains("2 files"));
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert!(
        is_summary(&lines[2], "✔ Unearthed to ./projects/scanner"),
        "{:?}",
        lines[2]
    );
}

#[test]
fn a_json_unearth_on_a_terminal_prints_the_json_and_no_cards() {
    let world = unearth_world();
    let run = world.run_tty_sized(&["--json", "unearth", "projects/scanner"], 100, 24, &[]);
    run.expect_code(0);
    assert_only_json(&run.stdout, "unearth");
}

#[test]
fn a_skipped_unearth_leaves_its_line_its_reason_and_a_warning_summary() {
    let clock = Clock::now();
    let mut builder: CollectionBuilder = Collection::build(&clock);
    builder
        .patterns(&["projects/*"])
        .version("projects/scanner", NEW, &[("a.txt", "hello\n")]);
    let world = World::new(Setup::new(builder.finish()), Mode::Text);
    world.oversize_listing("projects/scanner", NEW);
    let run = world.run_tty_sized(&["unearth", "projects/scanner"], 100, 24, &[]);
    run.expect_code(3);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        lines[0].starts_with("  ⚠ projects/scanner version "),
        "{lines:#?}"
    );
    assert!(lines[0].contains("skipped"), "{lines:#?}");
    assert!(
        lines[1].contains("needed,") && lines[1].trim_end().ends_with("free"),
        "{lines:#?}"
    );
    assert!(
        lines
            .last()
            .expect("a summary")
            .starts_with("⚠ Skipped projects/scanner"),
        "{lines:#?}"
    );
    assert!(
        !run.stderr.contains("Skipped projects/scanner:"),
        "{:?}",
        run.stderr
    );
}

#[test]
fn unearth_on_a_dumb_terminal_prints_the_line_once() {
    let world = unearth_world();
    let run = world.run_tty_sized(
        &["unearth", "projects/scanner"],
        100,
        24,
        &[("TERM", "dumb")],
    );
    run.expect_code(0);
    assert!(!run.stdout.contains('\x1b'));
    assert_eq!(run.stdout.lines().count(), 3, "{:?}", run.stdout);
}

// =========================================================
// fork
// =========================================================

#[test]
fn fork_leaves_the_line_of_the_piece_and_a_summary_with_the_copy() {
    let world = unearth_world();
    let run = world.run_tty_sized(&["fork", "projects/scanner"], 100, 24, &[]);
    run.expect_code(0);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        lines[0].starts_with("  ✔ projects/scanner version 2026-11-04 "),
        "{lines:#?}"
    );
    assert!(lines[0].contains("2 files"));
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert!(
        is_summary(&lines[2], "✔ Forked to ./projects/scanner"),
        "{:?}",
        lines[2]
    );
    assert!(!run.stdout.contains("Forked projects/scanner (version"));
}

#[test]
fn fork_with_a_name_names_the_copy_in_the_summary_and_the_piece_on_the_card() {
    let world = unearth_world();
    let run = world.run_tty_sized(
        &["fork", "projects/scanner", "--name", "projects/copy"],
        100,
        24,
        &[],
    );
    run.expect_code(0);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        lines[0].starts_with("  ✔ projects/scanner version "),
        "{lines:#?}"
    );
    assert!(
        is_summary(&lines[2], "✔ Forked to ./projects/copy"),
        "{:?}",
        lines[2]
    );
}

#[test]
fn a_json_fork_on_a_terminal_prints_the_json_and_no_cards() {
    let world = unearth_world();
    let run = world.run_tty_sized(&["--json", "fork", "projects/scanner"], 100, 24, &[]);
    run.expect_code(0);
    assert_only_json(&run.stdout, "fork");
}

#[test]
fn a_fork_dry_run_on_a_terminal_shows_no_cards() {
    let world = unearth_world();
    let run = world.run_tty_sized(&["fork", "projects/scanner", "-n"], 100, 24, &[]);
    run.expect_code(0);
    assert!(!run.stdout.contains('╸') && !run.stdout.contains("waiting"));
    assert!(!run.stdout.contains("\x1b[1A") && !run.stdout.contains("\x1b[J"));
    assert!(run.stdout.contains("would fork"), "{:?}", run.stdout);
}

#[test]
fn fork_on_a_dumb_terminal_prints_the_line_once() {
    let world = unearth_world();
    let run = world.run_tty_sized(&["fork", "projects/scanner"], 100, 24, &[("TERM", "dumb")]);
    run.expect_code(0);
    assert!(!run.stdout.contains('\x1b'));
    assert_eq!(run.stdout.lines().count(), 3, "{:?}", run.stdout);
}

#[test]
fn a_skipped_fork_leaves_its_line_its_reason_and_a_warning_summary() {
    let clock = Clock::now();
    let mut builder: CollectionBuilder = Collection::build(&clock);
    builder
        .patterns(&["projects/*"])
        .version("projects/scanner", NEW, &[("a.txt", "hello\n")]);
    let world = World::new(Setup::new(builder.finish()), Mode::Text);
    world.oversize_listing("projects/scanner", NEW);
    let run = world.run_tty_sized(&["fork", "projects/scanner"], 100, 24, &[]);
    run.expect_code(3);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        lines[0].starts_with("  ⚠ projects/scanner version "),
        "{lines:#?}"
    );
    assert!(lines[0].contains("skipped"), "{lines:#?}");
    assert!(
        lines[1].contains("needed,") && lines[1].trim_end().ends_with("free"),
        "{lines:#?}"
    );
    assert!(
        lines
            .last()
            .expect("a summary")
            .starts_with("⚠ Skipped projects/scanner"),
        "{lines:#?}"
    );
    assert!(
        !run.stderr.contains("Skipped projects/scanner:"),
        "{:?}",
        run.stderr
    );
}

#[test]
fn a_fork_of_a_damaged_version_shows_the_warning_after_the_summary() {
    let world = unearth_world();
    let store = world.collection().path(".antiq/pieces/projects/scanner");
    for entry in fs::read_dir(&store).expect("the store folder") {
        let path = entry.expect("an entry").path();
        if path.to_string_lossy().ends_with(".tar.zst.sha256") {
            fs::remove_file(path).expect("removes the checksum file");
        }
    }
    let run = world.run_tty_sized(&["fork", "projects/scanner"], 100, 24, &[]);
    run.expect_code(0);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        is_summary(&lines[2], "✔ Forked to ./projects/scanner"),
        "{lines:#?}"
    );
    assert!(run.stderr.contains("is damaged"), "{:?}", run.stderr);
}

#[test]
fn a_fork_to_a_taken_path_leaves_no_card_and_names_the_name_option() {
    let world = unearth_world();
    fs::create_dir_all(world.collection().path("projects/scanner")).expect("takes the place");
    let run = world.run_tty_sized(&["fork", "projects/scanner"], 100, 24, &[]);
    run.expect_code(1);
    let screen = replay(&run.stdout, 100, 24).lines().join("\n");
    assert!(!screen.contains('✔') && !screen.contains('╸'), "{screen:?}");
    assert!(run.stderr.contains("`--name`"), "{:?}", run.stderr);
}

// =========================================================
// push
// =========================================================

fn push_world() -> World {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["*/*"])
        .piece("notes/daily", |piece| {
            piece.age_days(5).file("today.md", "fresh\n");
        })
        .piece("projects/scanner", |piece| {
            piece
                .age_days(10)
                .file("src/main.rs", "fn main() {}\n")
                .file("notes.txt", "to do\n");
        })
        .finish();
    World::new(
        Setup::new(collection)
            .with_target(support::scenario::target::Target::new())
            .with_real_rsync(),
        Mode::Text,
    )
}

/// The table of what a push sent, as its lines on the screen: the header
/// row and one row per piece.
fn table_rows(lines: &[String]) -> Vec<String> {
    let start = lines
        .iter()
        .position(|line| line.starts_with("PIECE"))
        .expect("the table has its header");
    lines[start..]
        .iter()
        .take_while(|line| !line.is_empty())
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

#[test]
fn push_init_leaves_the_table_the_claim_and_no_card() {
    let world = push_world();
    let run = world.run_tty_sized(&["push", "--init"], 100, 24, &[("NO_COLOR", "1")]);
    run.expect_code(0);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert_eq!(
        table_rows(&lines),
        [
            "PIECE CREATED UPDATED DELETED",
            "notes/daily 1 0 0",
            "projects/scanner 2 0 0",
            "other files 1 0 0",
        ]
    );
    assert!(
        lines.iter().all(|line| !line.contains("Pushing to ")),
        "the card is gone: {lines:#?}"
    );
    assert!(
        lines.iter().all(|line| !line.starts_with("  ✔ ")),
        "no line per piece: {lines:#?}"
    );
    let summary = lines
        .iter()
        .find(|line| line.starts_with("✔ Claimed "))
        .expect("the claim summary");
    assert!(
        summary.contains("2 pieces pushed · 4 files changed   took "),
        "{summary}"
    );
    assert!(lines.iter().any(|line| line.contains(".antiq-target")));
}

#[test]
fn push_that_sends_nothing_prints_the_text_it_always_did() {
    let world = push_world();
    world
        .run_tty_sized(&["push", "--init"], 100, 24, &[])
        .expect_code(0);
    let run = world.run_tty_sized(&["push"], 100, 24, &[("NO_COLOR", "1")]);
    run.expect_code(0);
    assert!(run.stdout.contains("Nothing changed."), "{:?}", run.stdout);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert!(
        lines
            .iter()
            .all(|line| !line.contains("Pushing to ") && !line.contains("storing")),
        "the card is gone: {lines:#?}"
    );
}

#[test]
fn a_second_push_shows_the_pieces_that_changed_in_the_table() {
    let world = push_world();
    world
        .run_tty_sized(&["push", "--init"], 100, 24, &[])
        .expect_code(0);
    std::fs::write(world.collection().path("notes/daily/today.md"), "changed\n")
        .expect("the file changes");
    let run = world.run_tty_sized(&["push"], 100, 24, &[("NO_COLOR", "1")]);
    run.expect_code(0);
    let lines = replay(&run.stdout, 100, 24).lines();
    assert_eq!(
        table_rows(&lines),
        ["PIECE CREATED UPDATED DELETED", "notes/daily 0 1 0"]
    );
    assert!(
        lines
            .last()
            .expect("a last line")
            .starts_with("✔ 1 piece pushed · 1 file changed")
    );
}

/// A world whose rsync is a stand-in that prints `listing` slowly, so that
/// the cards have time to be drawn.
fn slow_push(listing: &str) -> (World, FakeRsync, support::scenario::target::Target) {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["*/*"])
        .piece("notes/daily", |piece| {
            piece.age_days(5).file("today.md", "fresh\n");
        })
        .finish();
    let target = support::scenario::target::Target::new();
    target.marker(&collection);
    let fake = FakeRsync::new("rsync  version 3.2.4  protocol version 31\n");
    fake.answer_all(listing, "", 0);
    fake.pace(1, "0.3");
    collection.use_target(&target.path());
    let config = collection.path(".antiq/config.toml");
    let text = std::fs::read_to_string(&config).expect("the config exists");
    let rsync = serde_json::to_string(fake.path().to_str().expect("a UTF-8 path")).expect("quotes");
    std::fs::write(
        &config,
        text.replacen("[target]\n", &format!("[target]\nrsync = {rsync}\n"), 1),
    )
    .expect("the config can be written");
    (World::new(Setup::new(collection), Mode::Text), fake, target)
}

#[test]
fn push_shows_one_card_for_the_whole_run_that_counts_entries_without_a_percentage() {
    let (world, fake, _target) = slow_push(
        ".d          96 ./\n\
         >f+++++++++ 5 notes/daily/today.md\n\
         \r              5 100%    0.00kB/s    0:00:00 (xfr#1, to-chk=1/3)\n\
         .f          3 notes/daily/other.md\n",
    );

    let run = world.run_tty_sized(&["push"], 100, 24, &[("NO_COLOR", "1")]);

    run.expect_code(0);
    let calls = fake.calls();
    let transfers: Vec<&Vec<String>> = calls.iter().filter(|call| call[0] == "transfer").collect();
    assert_eq!(transfers.len(), 1, "no dry run: {transfers:?}");
    assert!(!transfers[0].contains(&"-n".to_string()));
    // The screen as it was when the second line of the card said `2 of 3`.
    let at = run
        .stdout
        .find("2 of 3 entries")
        .expect("the card counted entries against rsync's total");
    let mid = replay(&run.stdout[..at + "2 of 3 entries".len()], 100, 24).visible();
    assert_eq!(mid.len(), 2, "{mid:#?}");
    assert!(mid[0].contains("Pushing to "), "{mid:#?}");
    assert!(
        mid[0].contains("5 B") && mid[0].ends_with("storing"),
        "{mid:#?}"
    );
    assert!(!mid[1].contains('%'), "{mid:#?}");
    let end = replay(&run.stdout, 100, 24).lines();
    assert!(
        end.iter().all(|line| !line.contains("Pushing to ")),
        "{end:#?}"
    );
}

#[test]
fn push_on_a_dumb_terminal_makes_no_dry_run_to_forecast_and_lists_its_pieces_at_the_end() {
    let clock = Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["*/*"])
        .piece("notes/daily", |piece| {
            piece.age_days(5).file("today.md", "fresh\n");
        })
        .finish();
    let target = support::scenario::target::Target::new();
    target.marker(&collection);
    let fake = FakeRsync::new("rsync  version 3.2.4  protocol version 31\n");
    fake.answer_all(">f+++++++++ notes/daily/today.md\n", "", 0);
    collection.use_target(&target.path());
    let config = collection.path(".antiq/config.toml");
    let text = std::fs::read_to_string(&config).expect("the config exists");
    let rsync = serde_json::to_string(fake.path().to_str().expect("a UTF-8 path")).expect("quotes");
    std::fs::write(
        &config,
        text.replacen("[target]\n", &format!("[target]\nrsync = {rsync}\n"), 1),
    )
    .expect("the config can be written");
    let world = World::new(Setup::new(collection), Mode::Text);

    let run = world.run_tty_sized(&["push"], 100, 24, &[("TERM", "dumb")]);

    run.expect_code(0);
    let listings = fake
        .calls()
        .into_iter()
        .filter(|call| call.iter().any(|arg| arg == "--out-format=%i %l %n%L"))
        .count();
    assert_eq!(listings, 0, "{:?}", fake.calls());
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("  ✔ notes/daily ")),
        "{lines:#?}"
    );
    assert!(!run.stdout.contains('\x1b') && !run.stdout.contains('\r'));
}

// =========================================================
// A run that cannot start
// =========================================================

#[test]
fn a_run_that_cannot_take_the_lock_prints_no_cards_and_fails_as_before() {
    let archive = archive_world(1);
    let push = push_world();
    let unearth = unearth_world();
    for (world, args) in [
        (&archive, vec!["archive"]),
        (&push, vec!["push"]),
        (&unearth, vec!["unearth", "projects/scanner"]),
    ] {
        let _lock = hold_lock(&world.collection().root());
        let run = world.run_tty_sized(&args, 100, 24, &[]);
        assert_ne!(run.code(), 0, "{args:?}");
        assert_eq!(run.stdout, "", "{args:?}: {:?}", run.stdout);
        assert!(run.stderr.contains("lock"), "{args:?}: {:?}", run.stderr);
    }
}
