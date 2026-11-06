// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

/// The width the three approved final mockups are laid out for.
const MOCKUP: usize = 96;

fn style(color: bool, utf8: bool) -> Style {
    Style {
        width: MOCKUP,
        color,
        terminal: true,
        utf8,
    }
}

type Counts = &'static [(Operation, Count)];

const ENTRIES: Count = Count {
    text: |done, total| items(done, total, "entries"),
    percent: true,
};
const CREATED: Count = Count {
    text: |done, total| items(done, total, "created"),
    percent: true,
};
const DELETED: Count = Count {
    text: |done, total| items(done, total, "deleted"),
    percent: true,
};

/// Counts that read like those of the three commands, so that the layout
/// tests run with the words the screen shows.
const ARCHIVE: Counts = &[(Operation::Removing, ENTRIES)];
const PUSH: Counts = &[
    (Operation::Storing, CREATED),
    (Operation::Removing, DELETED),
];
const UNEARTH: Counts = &[];

fn cards(counts: Counts) -> Cards {
    with_style(counts, style(false, true))
}

fn with_style(counts: Counts, style: Style) -> Cards {
    Cards::new(Config {
        counts,
        name_prefix: "",
        style,
        directory: None,
    })
}

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

fn started(name: &str, bytes: u64) -> Msg {
    Msg::Started {
        name: name.to_string(),
        bytes,
        label: None,
    }
}

fn operation(name: &str, operation: Operation, done: u64, total: u64) -> (Msg, Counter) {
    let counter = Counter::new();
    counter.set_total(total);
    counter.add(done);
    let msg = Msg::Operation {
        name: name.to_string(),
        operation,
        counter: counter.clone(),
    };
    (msg, counter)
}

fn finished(name: &str, bytes: u64, outcome: Outcome) -> Msg {
    Msg::Finished {
        name: name.to_string(),
        bytes,
        outcome,
    }
}

/// Runs one piece from its start to its end, `took` seconds apart.
fn run_piece(cards: &mut Cards, name: &str, bytes: u64, outcome: Outcome, start: u64, took: u64) {
    cards.apply(started(name, bytes), secs(start));
    cards.apply(finished(name, bytes, outcome), secs(start + took));
}

/// The lines every finished piece has left, in order of completion.
fn lines_of(cards: &mut Cards, cols: usize) -> Vec<String> {
    cards
        .take_ready()
        .iter()
        .flat_map(|ready| cards.finished_lines(ready, cols))
        .collect()
}

// =========================================================
// The approved final mockups
// =========================================================

#[test]
fn an_archive_run_leaves_the_lines_of_the_approved_mockup() {
    let mut cards = cards(ARCHIVE);
    run_piece(
        &mut cards,
        "experiments/ray-marcher",
        222,
        Outcome::Files(3),
        0,
        1,
    );
    cards.apply(
        finished(
            "projects/antiq",
            220,
            Outcome::Refused(vec!["pinned: projects/antiq/.antiq-pin".to_string()]),
        ),
        secs(1),
    );
    run_piece(
        &mut cards,
        "projects/recipe-api",
        48_300_000,
        Outcome::Files(15),
        1,
        5,
    );
    run_piece(
        &mut cards,
        "archive/2019/client-work/acme-warehouse-dashboard",
        1_800_000_000,
        Outcome::Files(518),
        6,
        11,
    );
    run_piece(
        &mut cards,
        "tools/pdf-merge",
        3_100_000,
        Outcome::Files(9),
        17,
        2,
    );
    assert_eq!(
        lines_of(&mut cards, MOCKUP),
        [
            "  ✔ experiments/ray-marcher                                    222 B               3 files  0:01",
            "  ✖ projects/antiq                                             220 B               refused  0:00",
            "    pinned, so it stays as it is. antiq unpin projects/antiq releases it",
            "  ✔ projects/recipe-api                                        48 MB              15 files  0:05",
            "  ✔ archive/2019/client-work/acme-warehouse-dashboard         1.8 GB             518 files  0:11",
            "  ✔ tools/pdf-merge                                           3.1 MB               9 files  0:02",
        ]
    );
}

#[test]
fn a_push_run_leaves_the_lines_of_the_approved_mockup() {
    let mut cards = cards(PUSH);
    let changes = |created, updated, deleted| Outcome::Changes {
        created,
        updated,
        deleted,
    };
    run_piece(
        &mut cards,
        "projects/antiq-site",
        12_000,
        changes(1, 0, 0),
        0,
        2,
    );
    run_piece(
        &mut cards,
        "tools/markdown-lint",
        86_000,
        changes(3, 0, 1),
        2,
        6,
    );
    run_piece(
        &mut cards,
        "experiments/wasm-fun",
        410_000,
        changes(0, 0, 1),
        8,
        3,
    );
    run_piece(
        &mut cards,
        "archive/2019/client-work/acme-warehouse-dashboard",
        1_800_000_000,
        changes(1, 1, 0),
        11,
        91,
    );
    assert_eq!(
        lines_of(&mut cards, MOCKUP),
        [
            "  ✔ projects/antiq-site                                        12 kB             1 created  0:02",
            "  ✔ tools/markdown-lint                                        86 kB  3 created, 1 deleted  0:06",
            "  ✔ experiments/wasm-fun                                      410 kB             1 deleted  0:03",
            "  ✔ archive/2019/client-work/acme-warehouse-dashboard         1.8 GB  1 created, 1 updated  1:31",
        ]
    );
}

#[test]
fn an_unearth_run_leaves_the_line_of_the_approved_mockup() {
    let mut cards = cards(UNEARTH);
    cards.apply(
        Msg::Started {
            name: "projects/recipe-api".to_string(),
            bytes: 48_300_000,
            label: Some("2026-11-05 13:02:11".to_string()),
        },
        secs(0),
    );
    cards.apply(
        finished("projects/recipe-api", 48_300_000, Outcome::Files(15)),
        secs(4),
    );
    assert_eq!(
        lines_of(&mut cards, MOCKUP),
        [
            "  ✔ projects/recipe-api version 2026-11-05 13:02:11            48 MB              15 files  0:04"
        ]
    );
}

#[test]
fn the_colours_of_a_finished_line_are_those_of_the_mockup() {
    let mut cards = with_style(ARCHIVE, style(true, true));
    run_piece(
        &mut cards,
        "tools/pdf-merge",
        3_100_000,
        Outcome::Files(9),
        0,
        2,
    );
    cards.apply(
        finished(
            "projects/antiq",
            220,
            Outcome::Refused(vec!["pinned: projects/antiq/.antiq-pin".to_string()]),
        ),
        secs(2),
    );
    let pad = |count: usize| " ".repeat(count);
    assert_eq!(
        lines_of(&mut cards, MOCKUP),
        [
            format!(
                "  \x1b[32m✔\x1b[0m \x1b[1mtools/pdf-merge\x1b[0m{}  {}3.1 MB  {}9 files  \x1b[2m0:02\x1b[0m",
                pad(40),
                pad(1),
                pad(13)
            ),
            format!(
                "  \x1b[1m\x1b[31m✖\x1b[0m \x1b[1mprojects/antiq\x1b[0m{}  {}220 B  {}\x1b[1m\x1b[31mrefused\x1b[0m  \x1b[2m0:00\x1b[0m",
                pad(40),
                pad(3),
                pad(13)
            ),
            "    pinned, so it stays as it is. \x1b[1mantiq unpin projects/antiq\x1b[0m releases it"
                .to_string(),
        ]
    );
}

#[test]
fn the_summary_line_follows_the_mockup() {
    let cards = cards(ARCHIVE);
    let summary = Summary {
        tone: Tone::Ok,
        parts: vec![
            ("4 archived".to_string(), Tone::Ok),
            ("0 skipped".to_string(), Tone::Plain),
            ("1 refused".to_string(), Tone::Bad),
        ],
        took: secs(19),
    };
    assert_eq!(
        cards.summary_line(&summary),
        "✔ 4 archived · 0 skipped · 1 refused   took 0:19"
    );
    let colored = with_style(ARCHIVE, style(true, true));
    assert_eq!(
        colored.summary_line(&summary),
        "\x1b[32m✔\x1b[0m \x1b[32m4 archived\x1b[0m · 0 skipped · \x1b[1m\x1b[31m1 refused\x1b[0m   \x1b[2mtook 0:19\x1b[0m"
    );
}

// =========================================================
// The live region
// =========================================================

#[test]
fn the_region_holds_the_running_cards_and_the_waiting_line() {
    let mut cards = cards(ARCHIVE);
    cards.apply(
        Msg::Planned(vec![
            ("experiments/ray-marcher".to_string(), 222),
            ("projects/recipe-api".to_string(), 48_300_000),
            (
                "archive/2019/client-work/acme-warehouse-dashboard".to_string(),
                1_800_000_000,
            ),
            ("tools/pdf-merge".to_string(), 3_100_000),
        ]),
        secs(0),
    );
    run_piece(
        &mut cards,
        "experiments/ray-marcher",
        222,
        Outcome::Files(3),
        0,
        1,
    );
    cards.apply(started("projects/recipe-api", 48_300_000), secs(1));
    let (verifying, _keep) = operation("projects/recipe-api", Operation::Verifying, 12, 15);
    cards.apply(verifying, secs(1));
    cards.apply(
        started(
            "archive/2019/client-work/acme-warehouse-dashboard",
            1_800_000_000,
        ),
        secs(1),
    );
    let (packing, _keep_too) = operation(
        "archive/2019/client-work/acme-warehouse-dashboard",
        Operation::Packing,
        214,
        518,
    );
    cards.apply(packing, secs(1));

    let bar = |fill: usize, rest: usize| format!("{}╸{}", "━".repeat(fill), "─".repeat(rest));
    assert_eq!(
        cards.region(secs(7), MOCKUP, 24),
        [
            format!(
                "  ⠋ {:<54}  {:>8}  verifying",
                "projects/recipe-api", "48 MB"
            ),
            format!("    {}   80%  12/15 files", bar(32, 7)),
            format!(
                "  ⠋ {:<54}  {:>8}  packing",
                "archive/2019/client-work/acme-warehouse-dashboard", "1.8 GB"
            ),
            format!("    {}   41%  214/518 files", bar(16, 23)),
            "  ○ 1 waiting".to_string(),
        ]
    );
}

#[test]
fn a_finished_piece_leaves_the_region_and_waits_for_its_line_once() {
    let mut cards = cards(ARCHIVE);
    run_piece(&mut cards, "a", 5, Outcome::Files(1), 0, 1);
    assert_eq!(cards.region(secs(1), MOCKUP, 24), Vec::<String>::new());
    assert_eq!(cards.take_ready().len(), 1);
    assert!(cards.take_ready().is_empty());
    assert!(cards.has_finished("a"));
}

#[test]
fn a_paused_piece_has_one_card_at_a_time_and_one_finished_line() {
    let mut cards = cards(PUSH);
    cards.apply(
        Msg::Planned(vec![("projects/site".to_string(), 12_000)]),
        secs(0),
    );
    cards.apply(started("projects/site", 12_000), secs(1));
    let (storing, _a) = operation("projects/site", Operation::Storing, 2, 2);
    cards.apply(storing, secs(1));
    assert_eq!(cards.region(secs(2), MOCKUP, 24).len(), 2);
    cards.apply(
        Msg::Paused {
            name: "projects/site".to_string(),
        },
        secs(4),
    );
    assert_eq!(cards.region(secs(5), MOCKUP, 24), ["  ○ 1 waiting"]);
    assert!(cards.take_ready().is_empty());
    let (removing, _b) = operation("projects/site", Operation::Removing, 0, 1);
    cards.apply(removing, secs(9));
    let region = cards.region(secs(10), MOCKUP, 24);
    assert_eq!(region.len(), 2);
    assert!(region[0].contains("projects/site") && region[0].ends_with("removing"));
    cards.apply(
        finished(
            "projects/site",
            12_000,
            Outcome::Changes {
                created: 2,
                updated: 0,
                deleted: 1,
            },
        ),
        secs(12),
    );
    let ready = cards.take_ready();
    assert_eq!(ready.len(), 1);
    // The segments are 1 to 4 and 9 to 12: six seconds, not eleven.
    assert_eq!(ready[0].elapsed, Some(secs(6)));
    assert!(cards.region(secs(12), MOCKUP, 24).is_empty());
}

#[test]
fn a_dropped_piece_leaves_the_waiting_count_without_a_line() {
    let mut cards = cards(ARCHIVE);
    cards.apply(
        Msg::Planned(vec![("a".to_string(), 1), ("b".to_string(), 2)]),
        secs(0),
    );
    assert_eq!(cards.region(secs(0), MOCKUP, 24), ["  ○ 2 waiting"]);
    cards.apply(
        Msg::Dropped {
            name: "a".to_string(),
        },
        secs(1),
    );
    assert_eq!(cards.region(secs(1), MOCKUP, 24), ["  ○ 1 waiting"]);
    assert!(cards.take_ready().is_empty());
}

#[test]
fn more_running_cards_than_rows_leave_a_count_in_the_waiting_line() {
    let mut cards = cards(ARCHIVE);
    cards.apply(Msg::Planned(vec![("late".to_string(), 1)]), secs(0));
    let mut counters = Vec::new();
    for name in ["a", "b", "c", "d", "e"] {
        cards.apply(started(name, 10), secs(0));
        let (msg, counter) = operation(name, Operation::Packing, 1, 2);
        counters.push(counter);
        cards.apply(msg, secs(0));
    }
    // Ten rows leave nine lines: one is the waiting line, the other eight
    // hold four cards.
    let region = cards.region(secs(0), MOCKUP, 10);
    assert_eq!(region.len(), 9);
    assert_eq!(region[8], "  ○ 1 waiting, 1 more running");
    assert!(region[0].contains("⠋ a ") && region[6].contains("⠋ d "));
}

#[test]
fn more_running_cards_than_rows_without_a_waiting_piece_still_name_the_rest() {
    let mut cards = cards(ARCHIVE);
    let mut counters = Vec::new();
    for name in ["a", "b", "c"] {
        cards.apply(started(name, 10), secs(0));
        let (msg, counter) = operation(name, Operation::Packing, 1, 2);
        counters.push(counter);
        cards.apply(msg, secs(0));
    }
    let region = cards.region(secs(0), MOCKUP, 5);
    assert_eq!(region.len(), 3);
    assert_eq!(region[2], "  ○ 2 more running");
}

#[test]
fn cards_stay_in_the_order_the_pieces_started() {
    let mut cards = cards(ARCHIVE);
    cards.apply(
        Msg::Planned(vec![("a".to_string(), 1), ("b".to_string(), 1)]),
        secs(0),
    );
    let (b, _kb) = operation("b", Operation::Packing, 0, 4);
    cards.apply(started("b", 1), secs(0));
    cards.apply(b, secs(0));
    let (a, _ka) = operation("a", Operation::Packing, 0, 4);
    cards.apply(started("a", 1), secs(1));
    cards.apply(a, secs(1));
    let region = cards.region(secs(1), MOCKUP, 24);
    assert!(region[0].contains("⠋ b ") && region[2].contains("⠋ a "));
}

#[test]
fn an_operation_without_a_total_draws_a_moving_bar_without_figures() {
    let mut cards = cards(ARCHIVE);
    cards.apply(started("a", 10), secs(0));
    let (storing, _k) = operation("a", Operation::Storing, 0, 0);
    cards.apply(storing, secs(0));
    let first = cards.region(Duration::from_millis(0), MOCKUP, 24);
    let later = cards.region(Duration::from_millis(500), MOCKUP, 24);
    assert_eq!(first[1], format!("    {}{}", "━━━━", "─".repeat(36)));
    assert_ne!(first[1], later[1]);
    assert!(!later[1].contains('%'));
    assert_eq!(later[1].chars().count(), first[1].chars().count());
}

#[test]
fn a_counter_at_or_above_its_total_shows_99_percent_until_the_card_is_replaced() {
    let mut cards = cards(ARCHIVE);
    cards.apply(started("a", 10), secs(0));
    let (verifying, counter) = operation("a", Operation::Verifying, 4, 4);
    cards.apply(verifying, secs(0));
    let at_total = format!("    {}╸   99%  4/4 files", "━".repeat(39));
    assert_eq!(cards.region(secs(0), MOCKUP, 24)[1], at_total);

    counter.add(5);
    assert_eq!(cards.region(secs(0), MOCKUP, 24)[1], at_total);
}

#[test]
fn the_unit_of_a_count_follows_the_command_and_the_operation() {
    let mut push = cards(PUSH);
    push.apply(started("a", 10), secs(0));
    let (storing, _a) = operation("a", Operation::Storing, 2, 3);
    push.apply(storing, secs(0));
    assert!(push.region(secs(0), MOCKUP, 24)[1].ends_with("67%  2/3 created"));
    let (removing, _b) = operation("a", Operation::Removing, 1, 2);
    push.apply(removing, secs(0));
    assert!(push.region(secs(0), MOCKUP, 24)[1].ends_with("50%  1/2 deleted"));

    let mut archive = cards(ARCHIVE);
    archive.apply(started("a", 10), secs(0));
    let (removing, _c) = operation("a", Operation::Removing, 1, 4);
    archive.apply(removing, secs(0));
    assert!(archive.region(secs(0), MOCKUP, 24)[1].ends_with("25%  1/4 entries"));

    let mut unearth = cards(UNEARTH);
    unearth.apply(started("a", 10), secs(0));
    let (extracting, _d) = operation("a", Operation::Extracting, 1, 4);
    unearth.apply(extracting, secs(0));
    assert!(unearth.region(secs(0), MOCKUP, 24)[1].ends_with("25%  1/4 files"));
}

/// A count whose text is the sizes of a byte count, and that shows no
/// percentage.
const SIZES: Count = Count {
    text: |done, total| match total {
        Some(total) => format!("{done} of {total} bytes"),
        None => format!("{done} bytes"),
    },
    percent: false,
};
const OF: Counts = &[(Operation::Packing, SIZES)];

#[test]
fn a_count_without_percentage_draws_a_bar_and_its_own_text() {
    let mut cards = cards(OF);
    cards.apply(started("a", 10), secs(0));
    let (msg, _k) = operation("a", Operation::Packing, 1, 4);
    cards.apply(msg, secs(0));
    assert_eq!(
        cards.region(secs(0), MOCKUP, 24)[1],
        format!("    {}╸{}  1 of 4 bytes", "━".repeat(10), "─".repeat(29))
    );
}

#[test]
fn a_count_without_percentage_past_its_total_moves_and_says_the_count_alone() {
    let mut cards = cards(OF);
    cards.apply(started("a", 10), secs(0));
    let (msg, _k) = operation("a", Operation::Packing, 5, 4);
    cards.apply(msg, secs(0));
    let line = cards.region(secs(0), MOCKUP, 24)[1].clone();
    assert!(line.ends_with("  5 bytes"), "{line:?}");
    assert!(!line.contains('%'), "{line:?}");
}

#[test]
fn a_count_whose_total_is_unknown_says_its_text_under_the_moving_bar() {
    let mut cards = cards(OF);
    cards.apply(started("a", 10), secs(0));
    let (msg, _k) = operation("a", Operation::Packing, 3, 0);
    cards.apply(msg, secs(0));
    let line = cards.region(secs(0), MOCKUP, 24)[1].clone();
    assert!(line.ends_with("  3 bytes"), "{line:?}");
    assert!(!line.contains('%'), "{line:?}");
}

#[test]
fn the_text_of_a_count_drops_below_sixty_columns() {
    let mut cards = cards(OF);
    cards.apply(started("a", 10), secs(0));
    let (msg, _k) = operation("a", Operation::Packing, 1, 4);
    cards.apply(msg, secs(0));
    assert!(!cards.region(secs(0), 59, 24)[1].contains("bytes"));
}

// =========================================================
// What an event for an unknown piece does
// =========================================================

#[test]
fn an_operation_for_a_piece_that_never_started_opens_a_card() {
    let mut cards = cards(ARCHIVE);
    let (packing, _k) = operation("stray", Operation::Packing, 1, 2);
    cards.apply(packing, secs(0));
    cards.apply(
        Msg::Paused {
            name: "other".to_string(),
        },
        secs(0),
    );
    cards.apply(
        Msg::Dropped {
            name: "other".to_string(),
        },
        secs(0),
    );
    let region = cards.region(secs(0), MOCKUP, 24);
    assert_eq!(region.len(), 2);
    assert!(region[0].contains("stray"));
}

#[test]
fn a_piece_that_finishes_without_a_start_is_a_line_with_no_time() {
    let mut cards = cards(ARCHIVE);
    cards.apply(
        finished("a", 7, Outcome::Skipped("it changed".to_string())),
        secs(3),
    );
    let ready = cards.take_ready();
    assert_eq!(ready[0].elapsed, Some(Duration::ZERO));
}

// =========================================================
// Errors under a finished piece
// =========================================================

#[test]
fn a_pinned_piece_names_the_command_with_the_directory_of_the_run() {
    let mut cards = Cards::new(Config {
        counts: ARCHIVE,
        name_prefix: "",
        style: style(false, true),
        directory: Some(PathBuf::from("/data/my notes")),
    });
    cards.apply(
        finished(
            "projects/antiq",
            220,
            Outcome::Refused(vec!["pinned: projects/antiq/.antiq-pin".to_string()]),
        ),
        secs(0),
    );
    let lines = lines_of(&mut cards, MOCKUP);
    assert_eq!(
        lines[1],
        "    pinned, so it stays as it is. antiq -C '/data/my notes' unpin projects/antiq releases it"
    );
}

#[test]
fn other_reasons_are_wrapped_one_block_each_and_never_cut() {
    let mut cards = cards(ARCHIVE);
    cards.apply(
        finished(
            "a",
            1,
            Outcome::Refused(vec![
                "nested collection: a/inner has its own .antiq folder and so it cannot be archived as a piece".to_string(),
                "empty: a".to_string(),
            ]),
        ),
        secs(0),
    );
    let lines = lines_of(&mut cards, 60);
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines[1],
        "    nested collection: a/inner has its own .antiq folder and"
    );
    assert_eq!(lines[2], "    so it cannot be archived as a piece");
    assert_eq!(lines[3], "    empty: a");
}

#[test]
fn a_skipped_piece_shows_its_message_in_yellow_and_no_room_shows_sizes() {
    let mut cards = with_style(UNEARTH, style(true, true));
    cards.apply(
        finished(
            "p",
            25,
            Outcome::NoRoom {
                needed: 25,
                available: 3,
            },
        ),
        secs(0),
    );
    let lines = lines_of(&mut cards, MOCKUP);
    assert!(lines[0].starts_with("  \x1b[33m⚠\x1b[0m "));
    assert!(lines[0].contains("\x1b[33mskipped\x1b[0m"));
    assert_eq!(lines[1], "    25 B needed, 3 B free");
}

#[test]
fn an_unfinished_piece_is_red_and_keeps_its_message() {
    let mut cards = cards(UNEARTH);
    cards.apply(
        finished(
            "p",
            25,
            Outcome::Unfinished("the version could not be removed".to_string()),
        ),
        secs(0),
    );
    let lines = lines_of(&mut cards, MOCKUP);
    assert!(lines[0].starts_with("  ✖ p"));
    assert!(lines[0].contains("unfinished"));
    assert_eq!(lines[1], "    the version could not be removed");
}

#[test]
fn a_went_to_message_is_wrapped_at_width_sixty_not_cut() {
    let mut cards = cards(ARCHIVE);
    cards.apply(
        finished(
            "p",
            1,
            Outcome::Skipped(
                "the folder changed while it was archived and went to recovered/2026-11-05T13-02-11Z/p".to_string(),
            ),
        ),
        secs(0),
    );
    let lines = lines_of(&mut cards, 60);
    assert!(lines.len() >= 3);
    assert!(lines[1..].iter().all(|line| line.chars().count() <= 60));
    assert_eq!(
        lines[1..]
            .iter()
            .map(|line| line.trim())
            .collect::<Vec<_>>()
            .join(" "),
        "the folder changed while it was archived and went to recovered/2026-11-05T13-02-11Z/p"
    );
}

// =========================================================
// Names, widths and symbols
// =========================================================

#[test]
fn a_long_name_is_cut_in_the_middle() {
    let mut cards = cards(ARCHIVE);
    let name = "archive/2019/client-work/with-a-very-long-folder-name/acme-warehouse-dashboard-v2";
    run_piece(&mut cards, name, 5, Outcome::Files(1), 0, 1);
    let lines = lines_of(&mut cards, MOCKUP);
    assert!(lines[0].contains('…'));
    assert!(lines[0].starts_with("  ✔ archive/2019/client"));
    assert!(lines[0].contains("dashboard-v2  "));
    assert_eq!(lines[0].chars().count(), MOCKUP);
}

#[test]
fn a_name_with_control_characters_is_escaped() {
    let mut cards = cards(ARCHIVE);
    run_piece(&mut cards, "evil\x1b[2J\nname", 5, Outcome::Files(1), 0, 1);
    let lines = lines_of(&mut cards, MOCKUP);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("evil%1B[2J%0Aname"));
    assert!(!lines[0].contains('\x1b'));
}

#[test]
fn no_line_of_the_region_is_wider_than_the_terminal() {
    for cols in [100, 80, 60, 40] {
        let mut cards = cards(ARCHIVE);
        cards.apply(
            Msg::Planned(vec![("waiting/piece".to_string(), 3)]),
            secs(0),
        );
        cards.apply(
            started(
                "archive/2019/client-work/acme-warehouse-dashboard",
                1_800_000_000,
            ),
            secs(0),
        );
        let (msg, _k) = operation(
            "archive/2019/client-work/acme-warehouse-dashboard",
            Operation::Packing,
            214,
            518,
        );
        cards.apply(msg, secs(0));
        for line in cards.region(secs(0), cols, 24) {
            assert!(line.chars().count() <= cols, "{cols}: {line:?}");
        }
        run_piece(
            &mut cards,
            "tools/pdf-merge",
            3_100_000,
            Outcome::Files(9),
            0,
            2,
        );
        let wide = cols.min(100);
        for line in lines_of(&mut cards, cols) {
            assert!(line.chars().count() <= wide, "{cols}: {line:?}");
        }
    }
}

#[test]
fn the_count_of_files_groups_its_thousands_with_a_space() {
    let mut cards = cards(UNEARTH);
    cards.apply(started("a", 10), secs(0));
    let (msg, _k) = operation("a", Operation::Extracting, 1_203, 2_931);
    cards.apply(msg, secs(0));
    assert!(
        cards.region(secs(0), MOCKUP, 24)[1].ends_with("1 203/2 931 files"),
        "{:?}",
        cards.region(secs(0), MOCKUP, 24)[1]
    );
}

#[test]
fn a_card_with_big_counts_keeps_both_lines_within_the_terminal_by_dropping_the_detail() {
    for color in [false, true] {
        for cols in [70, 72, 75] {
            // The line takes 52 columns before the detail.
            for (kind, operation_kind, unit) in [
                (ARCHIVE, Operation::Removing, "entries"),
                (PUSH, Operation::Storing, "created"),
                (ARCHIVE, Operation::Packing, "files"),
            ] {
                let detail_fits = 52 + "123 456/654 321 ".len() + unit.len() <= cols;
                let mut cards = with_style(kind, style(color, true));
                cards.apply(started("tools/pdf-merge", 3_100_000), secs(0));
                let (msg, _k) = operation("tools/pdf-merge", operation_kind, 123_456, 654_321);
                cards.apply(msg, secs(0));
                let region = cards.region(secs(0), cols, 24);
                for line in &region {
                    assert!(visible_width(line) <= cols, "{cols} {unit}: {line:?}");
                }
                assert!(region[1].contains("19%"), "{cols} {unit}: {:?}", region[1]);
                if detail_fits {
                    assert!(region[1].contains(unit), "{cols} {unit}: {:?}", region[1]);
                } else {
                    assert!(!region[1].contains(unit), "{cols} {unit}: {:?}", region[1]);
                }
            }
        }
    }
}

#[test]
fn a_line_of_the_region_that_is_still_too_wide_is_cut_without_breaking_its_colour() {
    let mut cards = with_style(UNEARTH, style(true, true));
    cards.apply(
        Msg::Started {
            name: "projects/recipe-api".to_string(),
            bytes: 5,
            label: Some("2026-11-05 13:02:11".to_string()),
        },
        secs(0),
    );
    let (msg, _k) = operation("projects/recipe-api", Operation::Extracting, 1, 4);
    cards.apply(msg, secs(0));
    let region = cards.region(secs(0), 20, 24);
    assert!(visible_width(&region[0]) <= 20, "{:?}", region[0]);
    assert!(region[0].ends_with("\x1b[0m"), "{:?}", region[0]);
}

#[test]
fn a_narrow_terminal_drops_the_detail_then_the_size() {
    let mut cards = cards(ARCHIVE);
    cards.apply(started("tools/pdf-merge", 3_100_000), secs(0));
    let (msg, _k) = operation("tools/pdf-merge", Operation::Packing, 1, 4);
    cards.apply(msg, secs(0));
    let sixty = cards.region(secs(0), 59, 24);
    assert!(sixty[0].contains("3.1 MB"));
    assert_eq!(sixty[1], format!("    ━━━╸{}   25%", "─".repeat(8)));
    let forty = cards.region(secs(0), 40, 24);
    assert!(!forty[0].contains("3.1 MB"));
    assert_eq!(forty[1], format!("    ━━━╸{}   25%", "─".repeat(8)));
}

#[test]
fn without_utf8_the_symbols_are_ascii() {
    let mut cards = with_style(ARCHIVE, style(false, false));
    cards.apply(Msg::Planned(vec![("w".to_string(), 1)]), secs(0));
    cards.apply(started("a", 10), secs(0));
    let (msg, _k) = operation("a", Operation::Packing, 1, 4);
    cards.apply(msg, secs(0));
    let region = cards.region(secs(0), MOCKUP, 24);
    assert!(region[0].starts_with("  | a "));
    assert_eq!(
        region[1],
        format!("    {}>{}   25%  1/4 files", "=".repeat(10), "-".repeat(29))
    );
    assert_eq!(region[2], "  o 1 waiting");
    run_piece(&mut cards, "b", 5, Outcome::Files(1), 0, 1);
    assert!(lines_of(&mut cards, MOCKUP)[0].starts_with("  ok b"));
}

#[test]
fn without_colour_no_line_has_an_escape() {
    let mut cards = cards(ARCHIVE);
    cards.apply(started("a", 10), secs(0));
    let (msg, _k) = operation("a", Operation::Packing, 1, 4);
    cards.apply(msg, secs(0));
    assert!(
        cards
            .region(secs(0), MOCKUP, 24)
            .iter()
            .all(|line| !line.contains('\x1b'))
    );
}

// =========================================================
// Lines for pieces that sent nothing
// =========================================================

#[test]
fn a_line_without_size_and_time_keeps_its_columns() {
    let cards = cards(ARCHIVE);
    let ready = Ready {
        name: "late/piece".to_string(),
        bytes: None,
        label: None,
        outcome: Outcome::Files(4),
        elapsed: None,
    };
    assert_eq!(
        cards.finished_lines(&ready, MOCKUP),
        [
            format!("  ✔ {:<54}  {:>8}  {:>20}", "late/piece", "", "4 files")
                .trim_end()
                .to_string()
        ]
    );
}

#[test]
fn the_ascii_symbol_set_reaches_every_glyph_of_a_finished_line_and_the_summary() {
    let mut cards = with_style(ARCHIVE, style(false, false));
    let name = "archive/2019/client-work/with-a-very-long-folder-name/acme-warehouse-dashboard-v2";
    run_piece(&mut cards, name, 5, Outcome::Files(1), 0, 1);
    cards.apply(
        finished("p", 1, Outcome::Refused(vec!["empty: p".to_string()])),
        secs(1),
    );
    cards.apply(
        finished("q", 1, Outcome::Skipped("it changed".to_string())),
        secs(1),
    );
    let lines = lines_of(&mut cards, MOCKUP);
    assert!(lines[0].starts_with("  ok archive/2019/"));
    assert!(lines[0].contains("..."));
    assert!(lines.iter().all(|line| line.is_ascii()));
    assert!(lines[1].starts_with("  x p"));
    assert!(lines[3].starts_with("  ! q"));
    let summary = Summary {
        tone: Tone::Ok,
        parts: vec![
            ("1 archived".to_string(), Tone::Ok),
            ("1 refused".to_string(), Tone::Bad),
        ],
        took: secs(3),
    };
    assert_eq!(
        cards.summary_line(&summary),
        "ok 1 archived - 1 refused   took 0:03"
    );
}

#[test]
fn the_version_of_an_unearth_keeps_its_place_by_cutting_the_name_and_goes_last() {
    let mut cards = cards(UNEARTH);
    cards.apply(
        Msg::Started {
            name: "projects/recipe-api".to_string(),
            bytes: 5,
            label: Some("2026-11-05 13:02:11".to_string()),
        },
        secs(0),
    );
    cards.apply(
        finished("projects/recipe-api", 5, Outcome::Files(1)),
        secs(1),
    );
    let ready = cards.take_ready();
    let eighty = cards.finished_lines(&ready[0], 80);
    assert!(eighty[0].contains("…") && eighty[0].contains("version 2026-11-05 13:02:11"));
    assert!(eighty[0].chars().count() <= 80);
    let forty = cards.finished_lines(&ready[0], 40);
    assert!(!forty[0].contains("version"));
}

#[test]
fn a_piece_that_finishes_twice_leaves_one_line() {
    let mut cards = cards(ARCHIVE);
    run_piece(&mut cards, "a", 5, Outcome::Files(1), 0, 1);
    cards.apply(finished("a", 5, Outcome::Files(2)), secs(2));
    assert_eq!(cards.take_ready().len(), 1);
}

#[test]
fn a_push_without_a_change_is_told_as_no_changes() {
    let mut cards = cards(PUSH);
    run_piece(
        &mut cards,
        "a",
        5,
        Outcome::Changes {
            created: 0,
            updated: 0,
            deleted: 0,
        },
        0,
        1,
    );
    assert!(lines_of(&mut cards, MOCKUP)[0].contains("no changes"));
}

#[test]
fn a_very_narrow_terminal_has_a_bar_of_ten_cells() {
    let mut cards = cards(ARCHIVE);
    cards.apply(started("a", 10), secs(0));
    let (msg, _keep) = operation("a", Operation::Packing, 1, 2);
    cards.apply(msg, secs(0));
    assert_eq!(
        cards.region(secs(0), 30, 24)[1],
        format!("    {}╸{}   50%", "━".repeat(5), "─".repeat(4))
    );
}

#[test]
fn a_command_in_a_reason_that_is_cut_by_wrapping_is_not_bold() {
    let mut cards = with_style(ARCHIVE, style(true, true));
    cards.apply(
        finished(
            "projects/antiq",
            1,
            Outcome::Refused(vec!["pinned: projects/antiq/.antiq-pin".to_string()]),
        ),
        secs(0),
    );
    let lines = lines_of(&mut cards, 40);
    assert!(lines.len() > 2);
    assert!(
        lines[1..].iter().all(|line| !line.contains("\x1b[1m")),
        "{lines:?}"
    );
}

// =========================================================
// A card named by a prefix, with a size that grows
// =========================================================

fn prefixed(counts: Counts, prefix: &'static str) -> Cards {
    Cards::new(Config {
        counts,
        name_prefix: prefix,
        style: style(false, true),
        directory: None,
    })
}

fn measured(name: &str, bytes: u64) -> (Msg, Counter) {
    let counter = Counter::new();
    counter.add(bytes);
    let msg = Msg::Measured {
        name: name.to_string(),
        bytes: counter.clone(),
    };
    (msg, counter)
}

#[test]
fn the_prefix_goes_before_the_name_of_a_running_card_and_not_before_a_finished_line() {
    let mut cards = prefixed(UNEARTH, "Pushing to ");
    cards.apply(started("/Volumes/nas", 0), secs(0));
    let (storing, _k) = operation("/Volumes/nas", Operation::Storing, 1, 4);
    cards.apply(storing, secs(0));
    let region = cards.region(secs(0), MOCKUP, 24);
    assert!(
        region[0].starts_with("  ⠋ Pushing to /Volumes/nas "),
        "{:?}",
        region[0]
    );
    assert!(region[0].ends_with("storing"), "{:?}", region[0]);

    cards.apply(finished("/Volumes/nas", 5, Outcome::Files(1)), secs(1));
    let lines = lines_of(&mut cards, MOCKUP);
    assert!(lines[0].starts_with("  ✔ /Volumes/nas "), "{lines:?}");
}

#[test]
fn a_long_name_is_cut_behind_the_prefix() {
    let mut cards = prefixed(UNEARTH, "Pushing to ");
    let name = "backup@nas.local:/volume1/antiq/projects/collection";
    cards.apply(started(name, 0), secs(0));
    let (storing, _k) = operation(name, Operation::Storing, 1, 4);
    cards.apply(storing, secs(0));
    let region = cards.region(secs(0), MOCKUP, 24);
    assert!(
        region[0].starts_with("  ⠋ Pushing to backup@"),
        "{:?}",
        region[0]
    );
    assert!(region[0].contains('…'), "{:?}", region[0]);
    assert!(region[0].ends_with("storing"), "{:?}", region[0]);
    assert!(visible_width(&region[0]) <= MOCKUP, "{:?}", region[0]);
}

#[test]
fn the_size_of_a_running_card_is_the_bytes_measured_and_grows() {
    let mut cards = cards(UNEARTH);
    cards.apply(started("a", 0), secs(0));
    let (storing, _k) = operation("a", Operation::Storing, 1, 4);
    cards.apply(storing, secs(0));
    let (msg, bytes) = measured("a", 3_100_000);
    cards.apply(msg, secs(0));
    assert!(cards.region(secs(0), MOCKUP, 24)[0].contains("3.1 MB"));

    bytes.add(45_200_000);
    assert!(cards.region(secs(1), MOCKUP, 24)[0].contains("48 MB"));
}

#[test]
fn a_size_measured_for_an_unknown_piece_opens_no_card() {
    let mut cards = cards(UNEARTH);
    let (msg, _bytes) = measured("ghost", 5);
    cards.apply(msg, secs(0));
    assert_eq!(cards.region(secs(0), MOCKUP, 24), Vec::<String>::new());
}
