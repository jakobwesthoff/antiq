// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use crate::cli::console::cards::{Summary, Tone};
use crate::cli::console::env::Env;
use crate::cli::console::reflow::Reflow;
use crate::cli::output::testing::{Capture, FailsOnFlush};
use crate::cli::output::{SizeSource, Style};
use antiq::progress::{Counter, Operation, Outcome, Planned};

#[path = "../../../../tests/support/screen.rs"]
mod screen;

use screen::Screen;
use std::sync::{Arc, Mutex};

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

fn env(cols: usize, cursor: bool) -> Env {
    Env::new(true, false, true, cols).with_cursor(cursor)
}

fn config(cols: usize) -> Config {
    Config {
        counts: &[],
        name_prefix: "",
        stored_size: false,
        result_width: 12,
        style: Style::from_env(&env(cols, true)),
        directory: None,
    }
}

/// An output on a terminal of `cols` by `rows`, and the memory it writes to.
fn terminal(cols: usize, rows: usize, cursor: bool) -> (Capture, Output) {
    let capture = Capture::default();
    let out = capture
        .output_with(env(cols, cursor), env(cols, cursor))
        .with_stdout_size(Some((cols, rows)));
    (capture, out)
}

/// The screen a person sees after everything written so far.
struct Watcher {
    capture: Capture,
    screen: Screen,
    seen: usize,
}

impl Watcher {
    fn new(capture: &Capture, cols: usize, rows: usize) -> Watcher {
        Watcher {
            capture: capture.clone(),
            screen: Screen::new(cols, rows),
            seen: 0,
        }
    }

    fn sync(&mut self) -> &Screen {
        let bytes = self.capture.stdout_bytes();
        self.screen.feed(&bytes[self.seen..]);
        self.seen = bytes.len();
        &self.screen
    }
}

fn started(name: &str, bytes: u64) -> Msg {
    Msg::Started {
        name: name.to_string(),
        bytes: Some(bytes),
        label: None,
    }
}

fn packing(name: &str, done: u64, total: u64) -> (Msg, Counter) {
    let counter = Counter::new();
    counter.set_total(total);
    counter.add(done);
    let msg = Msg::Operation {
        name: name.to_string(),
        operation: Operation::Packing,
        counter: counter.clone(),
    };
    (msg, counter)
}

fn done(name: &str, files: u64) -> Msg {
    Msg::Finished {
        name: name.to_string(),
        bytes: 1000,
        outcome: Outcome::Files(files),
    }
}

fn driver(out: &mut Output, cols: usize) -> Driver<'_> {
    Driver::new(Cards::new(config(cols)), out)
}

// =========================================================
// What the screen shows during the run
// =========================================================

#[test]
fn the_screen_shows_the_running_cards_and_the_waiting_line() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let mut driver = driver(&mut out, 100);
    driver
        .message(
            Msg::Planned(vec![("a".into(), 10), ("b".into(), 20), ("c".into(), 30)]),
            secs(0),
        )
        .unwrap();
    driver.message(started("a", 10), secs(0)).unwrap();
    let (msg, _keep) = packing("a", 1, 4);
    driver.message(msg, secs(0)).unwrap();
    driver.frame(secs(1), true).unwrap();
    let expected = driver.cards.region(secs(1), 100, 24);
    assert_eq!(expected.len(), 3);
    assert_eq!(watcher.sync().visible(), expected);
    // The next frame replaces the region instead of adding to it.
    driver.frame(secs(2), true).unwrap();
    assert_eq!(
        watcher.sync().visible(),
        driver.cards.region(secs(2), 100, 24)
    );
}

#[test]
fn scrollback_and_screen_hold_the_finished_lines_in_order_when_there_are_more_pieces_than_rows() {
    let rows = 10;
    let (capture, mut out) = terminal(100, rows, true);
    let mut watcher = Watcher::new(&capture, 100, rows);
    let mut driver = driver(&mut out, 100);
    let names: Vec<String> = (1..=14)
        .map(|number| format!("piece/{number:02}"))
        .collect();
    driver
        .message(
            Msg::Planned(names.iter().map(|name| (name.clone(), 1000)).collect()),
            secs(0),
        )
        .unwrap();
    let mut counters = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let now = secs(index as u64);
        driver.message(started(name, 1000), now).unwrap();
        let (msg, counter) = packing(name, 1, 2);
        counters.push(counter);
        driver.message(msg, now).unwrap();
        driver.frame(now, true).unwrap();
        let screen = watcher.sync();
        assert!(screen.visible().len() < rows, "the line above stays");
        if index >= 2 {
            let finished = &names[index - 2];
            driver.message(done(finished, index as u64), now).unwrap();
            driver.frame(now, true).unwrap();
        }
    }
    for name in &names[names.len() - 2..] {
        driver.message(done(name, 1), secs(20)).unwrap();
    }
    driver.finish(secs(20), Vec::new()).unwrap();
    let lines = watcher.sync().lines();
    let finished: Vec<&String> = lines.iter().filter(|line| line.contains('✔')).collect();
    assert_eq!(finished.len(), 14);
    for (line, name) in finished.iter().zip(&names) {
        assert!(line.starts_with(&format!("  ✔ {name} ")), "{line}");
    }
    assert_eq!(lines.len(), 14, "no card or waiting line is left behind");
}

#[test]
fn a_line_printed_before_the_first_frame_survives_it() {
    let (capture, mut out) = terminal(100, 24, true);
    out.line("$ antiq archive").unwrap();
    let mut watcher = Watcher::new(&capture, 100, 24);
    let mut driver = driver(&mut out, 100);
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.frame(secs(0), true).unwrap();
    driver.message(done("a", 1), secs(1)).unwrap();
    driver.finish(secs(1), Vec::new()).unwrap();
    let lines = watcher.sync().lines();
    assert_eq!(lines[0], "$ antiq archive");
    assert!(lines[1].starts_with("  ✔ a "));
    assert_eq!(lines.len(), 2);
}

#[test]
fn the_bytes_of_a_measured_item_reach_its_card_through_the_feed() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let mut driver = driver(&mut out, 100);
    let (sender, receiver) = mpsc::channel();
    let feed = Feed {
        sender,
        start: Instant::now(),
        label: Box::new(|stamp, _| stamp.to_string()),
    };
    let storing = Counter::new();
    let sent = Counter::new();
    feed.event(Event::Started {
        name: "nas",
        bytes: Some(0),
        stamp: None,
        siblings: &[],
    });
    feed.event(Event::Operation {
        name: "nas",
        operation: Operation::Storing,
        counter: &storing,
    });
    feed.event(Event::Measured {
        name: "nas",
        bytes: &sent,
    });
    sent.add(3_100_000);
    while let Ok(Packet::Msg(msg, at)) = receiver.try_recv() {
        driver.message(msg, at).unwrap();
    }
    driver.frame(secs(0), true).unwrap();
    let lines = watcher.sync().lines();
    assert!(
        lines[0].contains("nas") && lines[1].contains("3.1 MB"),
        "{lines:?}"
    );
}

#[test]
fn the_bytes_move_the_cursor_up_by_at_least_one_and_switch_no_mode() {
    let (capture, mut out) = terminal(100, 10, true);
    let mut driver = driver(&mut out, 100);
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.frame(secs(0), true).unwrap();
    driver.message(done("a", 1), secs(1)).unwrap();
    driver.finish(secs(1), Vec::new()).unwrap();
    let text = String::from_utf8(capture.stdout_bytes()).unwrap();
    assert!(!text.contains("\x1b[0A"));
    for forbidden in [
        "\x1b[?", "\x1b[r", "\x1b[H", "\x1b[2J", "\x1bc", "\x1b[s", "\x1b[u",
    ] {
        assert!(!text.contains(forbidden), "{forbidden:?}");
    }
    let mut screen = Screen::new(100, 10);
    screen.feed(text.as_bytes());
    assert!(screen.unsupported.is_empty(), "{:?}", screen.unsupported);
}

#[test]
fn a_frame_equal_to_the_last_is_not_written_again() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut driver = driver(&mut out, 100);
    driver.message(started("a", 10), secs(0)).unwrap();
    let (msg, _keep) = packing("a", 1, 4);
    driver.message(msg, secs(0)).unwrap();
    driver.frame(secs(1), true).unwrap();
    let written = capture.stdout_bytes().len();
    driver.frame(secs(1), true).unwrap();
    assert_eq!(capture.stdout_bytes().len(), written);
}

#[test]
fn a_finished_line_is_written_apart_from_the_region() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut driver = driver(&mut out, 100);
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.message(started("b", 10), secs(0)).unwrap();
    driver.frame(secs(0), true).unwrap();
    let before = capture.stdout_bytes().len();
    driver.message(done("a", 1), secs(1)).unwrap();
    driver.frame(secs(1), true).unwrap();
    let written = String::from_utf8(capture.stdout_bytes()[before..].to_vec()).unwrap();
    // First the erase and the line, then the new region: the cursor never
    // moves up again between them.
    let line = written.find("✔ a").expect("the line is written");
    let region = written.find("⠋ b").expect("the region follows");
    assert!(line < region);
    assert_eq!(
        written.matches("\x1b[").filter(|_| true).count(),
        2,
        "{written:?}"
    );
}

// =========================================================
// The emulator's resize
// =========================================================

/// A screen of `cols` columns that was fed `text`.
fn fed(cols: usize, text: &str) -> Screen {
    let mut screen = Screen::new(cols, 10);
    screen.feed(text.as_bytes());
    screen
}

#[test]
fn a_rewrapped_line_takes_as_many_rows_as_it_needs() {
    let mut screen = fed(10, "abcdefghij\n");
    screen.resize(4, 10, true);
    assert_eq!(screen.visible(), ["abcd", "efgh", "ij"]);
    screen.feed(b"Z");
    assert_eq!(screen.visible(), ["abcd", "efgh", "ij", "Z"]);
}

#[test]
fn a_wide_character_that_does_not_fit_moves_to_the_next_row_on_a_rewrap() {
    let mut screen = fed(10, "ab世cd\n");
    screen.resize(3, 10, true);
    assert_eq!(screen.visible(), ["ab", "世c", "d"]);
}

#[test]
fn a_line_as_wide_as_the_window_takes_one_row_on_a_rewrap() {
    let mut screen = fed(10, "abcd\n");
    screen.resize(4, 10, true);
    assert_eq!(screen.visible(), ["abcd"]);
}

#[test]
fn spaces_at_the_end_of_a_row_are_kept_when_the_rows_join_again() {
    let mut screen = fed(10, "abc  defg\n");
    screen.resize(5, 10, true);
    screen.resize(10, 10, true);
    assert_eq!(screen.lines(), ["abc  defg"]);
}

#[test]
fn a_shorter_window_scrolls_only_as_far_as_the_cursor_needs() {
    let mut screen = Screen::new(10, 10);
    screen.feed(b"a\nb\nc\n");
    screen.resize(10, 2, false);
    // The cursor row (the empty one below "c") stays on the screen with the
    // line above it; "a" and "b" are in the scrollback, nothing is lost.
    assert_eq!(screen.visible(), ["c"]);
    assert_eq!(screen.lines(), ["a", "b", "c"]);
    screen.feed(b"d");
    assert_eq!(screen.visible(), ["c", "d"]);
}

#[test]
fn a_wide_character_wider_than_the_window_keeps_its_row_on_a_rewrap() {
    let mut screen = fed(10, "中\n");
    screen.resize(1, 10, true);
    assert_eq!(screen.visible(), ["中"]);
}

#[test]
fn a_wide_character_cut_by_a_narrowing_without_rewrap_is_dropped() {
    let mut screen = fed(10, "ab世\n");
    screen.resize(3, 10, false);
    assert_eq!(screen.visible(), ["ab"]);
}

// =========================================================
// Resize
// =========================================================

/// The size the driver reads, which a test changes between frames.
type Window = Arc<Mutex<Option<(usize, usize)>>>;

fn window(size: (usize, usize)) -> Window {
    Arc::new(Mutex::new(Some(size)))
}

fn resize_to(window: &Window, size: (usize, usize)) {
    *window.lock().unwrap() = Some(size);
}

/// A driver whose size comes from `window` and whose terminal is `reflow`.
fn driver_in<'o>(out: &'o mut Output, cols: usize, window: &Window, reflow: Reflow) -> Driver<'o> {
    let window = Arc::clone(window);
    let sizes = SizeSource::new(move || *window.lock().unwrap());
    Driver::on_terminal(Cards::new(config(cols)), out, sizes, reflow)
}

/// Three finished pieces, then `running` pieces with a card each. The
/// counters keep the cards running.
fn scene(driver: &mut Driver<'_>, running: usize) -> Vec<Counter> {
    for name in ["one", "two", "three"] {
        driver.message(started(name, 10), secs(0)).unwrap();
        driver.message(done(name, 1), secs(1)).unwrap();
        driver.frame(secs(1), true).unwrap();
    }
    let mut counters = Vec::new();
    for number in 0..running {
        let name = format!("piece/{number}");
        driver.message(started(&name, 10), secs(2)).unwrap();
        let (msg, counter) = packing(&name, 1, 2);
        driver.message(msg, secs(2)).unwrap();
        counters.push(counter);
    }
    driver.frame(secs(5), true).unwrap();
    counters
}

/// What the screen holds after a window of `from` became `to` during a run.
struct Resized {
    /// The lines the screen holds, finished ones first.
    lines: Vec<String>,
    /// The finished lines as the screen held them before the resize.
    finished: Vec<String>,
    /// The region as the driver draws it for the new size.
    region: Vec<String>,
}

/// Runs the scene on a screen of `from` that lays lines out again when
/// `rewrap` is set, makes the window `to` and lets the driver draw once more.
fn resized(
    reflow: Reflow,
    rewrap: bool,
    from: (usize, usize),
    to: (usize, usize),
    running: usize,
) -> Resized {
    let (capture, mut out) = terminal(from.0, from.1, true);
    let mut watcher = Watcher::new(&capture, from.0, from.1);
    let size = window(from);
    let mut driver = driver_in(&mut out, from.0, &size, reflow);
    let _keep = scene(&mut driver, running);
    let finished = watcher.sync().lines()[..3].to_vec();
    watcher.screen.resize(to.0, to.1, rewrap);
    resize_to(&size, to);
    driver.frame(secs(6), true).unwrap();
    driver.frame(secs(7), true).unwrap();
    let region = driver
        .cards
        .region(secs(7), to.0, to.1)
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect();
    Resized {
        lines: watcher.sync().lines(),
        finished,
        region,
    }
}

/// `line` cut to `columns` columns, as a screen that does not lay out again
/// holds it.
fn cut(line: &str, columns: usize) -> String {
    line.chars()
        .take(columns)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn narrowing_on_a_terminal_that_rewraps_leaves_the_finished_lines_and_one_region() {
    let run = resized(Reflow::Rewraps, true, (100, 24), (60, 24), 3);
    let expected: Vec<String> = [run.finished, run.region].concat();
    assert_eq!(run.lines, expected);
}

#[test]
fn narrowing_on_a_terminal_that_cuts_leaves_the_finished_lines_and_one_region() {
    let run = resized(Reflow::Cuts, false, (100, 24), (60, 24), 3);
    let finished: Vec<String> = run.finished.iter().map(|line| cut(line, 60)).collect();
    assert_eq!(run.lines, [finished, run.region].concat());
}

#[test]
fn widening_on_a_terminal_that_rewraps_leaves_the_finished_lines_and_one_region() {
    let run = resized(Reflow::Rewraps, true, (60, 24), (100, 24), 3);
    assert_eq!(run.lines, [run.finished, run.region].concat());
}

#[test]
fn widening_on_a_terminal_that_cuts_leaves_the_finished_lines_and_one_region() {
    let run = resized(Reflow::Cuts, false, (60, 24), (100, 24), 3);
    assert_eq!(run.lines, [run.finished, run.region].concat());
}

#[test]
fn a_terminal_taken_for_one_that_cuts_leaves_fragments_but_keeps_every_finished_line() {
    let run = resized(Reflow::Cuts, true, (100, 24), (60, 24), 3);
    let mut seen = run.lines.iter();
    for line in &run.finished {
        assert!(
            seen.any(|found| found == line),
            "{line} in {:#?}",
            run.lines
        );
    }
    assert!(run.lines.ends_with(&run.region));
}

#[test]
fn a_window_that_gets_shorter_keeps_the_finished_lines_and_scrolls_the_top_of_the_region_away() {
    let run = resized(Reflow::Rewraps, true, (100, 24), (100, 8), 5);
    assert_eq!(run.lines[..3], run.finished[..]);
    assert!(run.lines.ends_with(&run.region));
    assert_eq!(run.region.len(), 7, "a region of at most rows less one");
}

#[test]
fn a_region_taller_than_the_window_after_a_narrowing_keeps_the_finished_lines() {
    let run = resized(Reflow::Rewraps, true, (100, 12), (40, 12), 5);
    let mut seen = run.lines.iter();
    for line in &run.finished {
        assert!(
            seen.any(|found| found == line),
            "{line} in {:#?}",
            run.lines
        );
    }
    assert!(
        run.lines.ends_with(&run.region),
        "{:#?} vs {:#?}",
        run.lines,
        run.region
    );
}

#[test]
fn two_resizes_between_frames_use_the_final_width() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let size = window((100, 24));
    let mut driver = driver_in(&mut out, 100, &size, Reflow::Rewraps);
    let _keep = scene(&mut driver, 3);
    let finished = watcher.sync().lines()[..3].to_vec();
    watcher.screen.resize(50, 24, true);
    watcher.screen.resize(80, 24, true);
    resize_to(&size, (80, 24));
    driver.frame(secs(6), true).unwrap();
    driver.frame(secs(7), true).unwrap();
    let region: Vec<String> = driver
        .cards
        .region(secs(7), 80, 24)
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect();
    assert_eq!(watcher.sync().lines(), [finished, region].concat());
}

#[test]
fn a_resize_that_is_undone_before_the_frame_writes_nothing() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let size = window((100, 24));
    let mut driver = driver_in(&mut out, 100, &size, Reflow::Rewraps);
    let _keep = scene(&mut driver, 3);
    let before = watcher.sync().lines();
    let written = capture.stdout_bytes().len();
    // The screen lays the lines out again and joins them again, as a
    // terminal does that rejoins rows; the time of the frame is the same, so
    // the region itself is unchanged.
    watcher.screen.resize(60, 24, true);
    watcher.screen.resize(100, 24, true);
    driver.frame(secs(5), true).unwrap();
    assert_eq!(capture.stdout_bytes().len(), written);
    assert_eq!(watcher.sync().lines(), before);
}

/// The scene on a terminal that lays lines out again, with the bytes
/// written so far and the window to change.
fn dragged<T>(run: impl FnOnce(&mut Driver<'_>, &Window, &Capture, &mut Watcher) -> T) -> T {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let size = window((100, 24));
    let mut driver = driver_in(&mut out, 100, &size, Reflow::Rewraps);
    let _keep = scene(&mut driver, 3);
    watcher.sync();
    run(&mut driver, &size, &capture, &mut watcher)
}

#[test]
fn the_first_frame_that_reads_a_new_size_draws_nothing() {
    dragged(|driver, size, capture, watcher| {
        let written = capture.stdout_bytes().len();
        watcher.screen.resize(60, 24, true);
        resize_to(size, (60, 24));
        driver.frame(secs(6), true).unwrap();
        assert_eq!(capture.stdout_bytes().len(), written);
    });
}

#[test]
fn a_size_that_changes_at_every_frame_draws_nothing_until_it_holds() {
    dragged(|driver, size, capture, watcher| {
        let written = capture.stdout_bytes().len();
        for columns in [90, 80, 70, 60] {
            watcher.screen.resize(columns, 24, true);
            resize_to(size, (columns, 24));
            driver.frame(secs(6), true).unwrap();
        }
        assert_eq!(capture.stdout_bytes().len(), written);
        driver.frame(secs(7), true).unwrap();
        assert!(capture.stdout_bytes().len() > written);
        let region = driver.cards.region(secs(7), 60, 24);
        assert!(watcher.sync().lines().ends_with(&region));
    });
}

#[test]
fn a_line_owed_while_the_size_changes_is_written_when_it_holds() {
    dragged(|driver, size, _capture, watcher| {
        watcher.screen.resize(60, 24, true);
        resize_to(size, (60, 24));
        driver.message(done("piece/0", 1), secs(6)).unwrap();
        driver.frame(secs(6), true).unwrap();
        assert!(
            !watcher
                .sync()
                .lines()
                .iter()
                .any(|line| line.contains("✔ piece/0")),
            "not yet"
        );
        // Messages in between make frames, but no time passes.
        driver.message(done("piece/1", 1), secs(6)).unwrap();
        driver.frame(secs(6), true).unwrap();
        assert!(
            !watcher
                .sync()
                .lines()
                .iter()
                .any(|line| line.contains("✔ piece/0")),
            "still not"
        );
        driver.frame(secs(7), true).unwrap();
        let lines = watcher.sync().lines();
        let found = lines.iter().filter(|l| l.contains("✔ piece/0")).count();
        assert_eq!(found, 1, "{lines:#?}");
    });
}

/// How often `needle` is in a line of `lines`.
fn count_in(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|line| line.contains(needle)).count()
}

#[test]
fn a_line_owed_when_the_run_ends_during_a_drag_is_written_once_and_no_card_stays() {
    dragged(|driver, size, _capture, watcher| {
        let before = watcher.sync().lines()[..3].to_vec();
        watcher.screen.resize(60, 24, true);
        resize_to(size, (60, 24));
        driver.message(done("piece/0", 1), secs(6)).unwrap();
        driver.frame(secs(6), true).unwrap();
        driver.finish(secs(7), Vec::new()).unwrap();
        let lines = watcher.sync().lines();
        assert_eq!(lines[..3], before[..], "{lines:#?}");
        assert_eq!(count_in(&lines, "✔ piece/0"), 1, "{lines:#?}");
        assert_eq!(count_in(&lines, "packing"), 0, "{lines:#?}");
    });
}

#[test]
fn a_line_owed_when_a_panic_halts_the_run_during_a_drag_is_written_once_below_the_finished_lines() {
    dragged(|driver, size, _capture, watcher| {
        let before = watcher.sync().lines()[..3].to_vec();
        watcher.screen.resize(60, 24, true);
        resize_to(size, (60, 24));
        driver.message(done("piece/0", 1), secs(6)).unwrap();
        driver.frame(secs(6), true).unwrap();
        driver.halt().unwrap();
        driver.frame(secs(6), true).unwrap();
        let lines = watcher.sync().lines();
        // The size had not held for a tick, so the halt climbs short of a
        // finished line and may leave the top of the old region behind.
        assert_eq!(lines[..3], before[..], "{lines:#?}");
        assert_eq!(count_in(&lines, "✔ piece/0"), 1, "{lines:#?}");
    });
}

#[test]
fn the_end_of_a_run_during_a_drag_that_went_back_to_the_wide_window_erases_no_finished_line() {
    dragged(|driver, size, _capture, watcher| {
        let before = watcher.sync().lines()[..3].to_vec();
        watcher.screen.resize(60, 24, true);
        resize_to(size, (60, 24));
        driver.frame(secs(6), true).unwrap();
        // The window is wide again, but the driver has not read that yet.
        watcher.screen.resize(100, 24, true);
        driver.finish(secs(6), Vec::new()).unwrap();
        let lines = watcher.sync().lines();
        assert_eq!(lines[..3], before[..], "{lines:#?}");
    });
}

#[test]
fn the_end_of_a_run_while_the_size_changes_erases_no_finished_line() {
    dragged(|driver, size, _capture, watcher| {
        let before = watcher.sync().lines()[..3].to_vec();
        watcher.screen.resize(60, 24, true);
        resize_to(size, (60, 24));
        driver.finish(secs(6), Vec::new()).unwrap();
        let lines = watcher.sync().lines();
        assert_eq!(lines[..3], before[..], "{lines:#?}");
    });
}

#[test]
fn a_halt_after_a_narrowing_that_held_takes_the_whole_region_off_the_screen() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let size = window((100, 24));
    let mut driver = driver_in(&mut out, 100, &size, Reflow::Rewraps);
    let _keep = scene(&mut driver, 3);
    let finished = watcher.sync().lines()[..3].to_vec();
    watcher.screen.resize(60, 24, true);
    resize_to(&size, (60, 24));
    driver.frame(secs(6), true).unwrap();
    driver.frame(secs(7), true).unwrap();
    driver.halt().unwrap();
    assert_eq!(watcher.sync().lines(), finished);
}

#[test]
fn the_end_of_a_run_after_a_narrowing_that_held_takes_the_whole_region_off_the_screen() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let size = window((100, 24));
    let mut driver = driver_in(&mut out, 100, &size, Reflow::Rewraps);
    let _keep = scene(&mut driver, 3);
    let finished = watcher.sync().lines()[..3].to_vec();
    watcher.screen.resize(60, 24, true);
    resize_to(&size, (60, 24));
    driver.frame(secs(6), true).unwrap();
    driver.finish(secs(7), Vec::new()).unwrap();
    assert_eq!(watcher.sync().lines(), finished);
}

// =========================================================
// Without a cursor, with a dead terminal, after a panic
// =========================================================

#[test]
fn a_terminal_without_a_cursor_gets_the_lines_when_they_finish_and_no_region() {
    let (capture, mut out) = terminal(100, 24, false);
    let mut driver = driver(&mut out, 100);
    driver
        .message(
            Msg::Planned(vec![("a".into(), 1), ("b".into(), 1)]),
            secs(0),
        )
        .unwrap();
    driver.message(started("a", 10), secs(0)).unwrap();
    let (msg, _keep) = packing("a", 1, 4);
    driver.message(msg, secs(0)).unwrap();
    driver.frame(secs(1), true).unwrap();
    assert_eq!(capture.stdout(), "");
    driver.message(done("a", 3), secs(2)).unwrap();
    driver.frame(secs(2), true).unwrap();
    driver.finish(secs(3), Vec::new()).unwrap();
    let text = capture.stdout();
    assert!(!text.contains('\x1b'));
    assert!(!text.contains("waiting"));
    assert_eq!(text.lines().count(), 1);
    assert!(text.starts_with("  ✔ a "));
}

#[test]
fn a_closed_stdout_ends_the_drawing_without_an_error() {
    let (stream, _flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
    let mut out = Output::new(Box::new(stream), Box::new(Vec::new()))
        .with_envs(env(100, true), env(100, true))
        .with_stdout_size(Some((100, 24)));
    let mut driver = driver(&mut out, 100);
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.frame(secs(0), true).unwrap();
    driver.message(done("a", 1), secs(1)).unwrap();
    driver.finish(secs(1), Vec::new()).unwrap();
}

#[test]
fn a_terminal_that_reports_no_size_gets_its_columns_and_24_rows() {
    let capture = Capture::default();
    let mut out = capture
        .output_with(env(70, true), env(70, true))
        .with_stdout_size(None);
    let mut watcher = Watcher::new(&capture, 70, 24);
    let mut driver = driver(&mut out, 70);
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.frame(secs(0), true).unwrap();
    assert_eq!(watcher.sync().visible().len(), 2);
}

#[test]
fn a_halted_driver_clears_its_region_and_prints_later_lines_plainly() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let mut driver = driver(&mut out, 100);
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.message(started("b", 10), secs(0)).unwrap();
    driver.frame(secs(0), true).unwrap();
    driver.halt().unwrap();
    driver.halt().unwrap();
    assert!(watcher.sync().visible().is_empty());
    driver.message(done("a", 1), secs(1)).unwrap();
    driver.frame(secs(1), true).unwrap();
    let lines = watcher.sync().lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("  ✔ a "));
}

#[test]
fn a_piece_that_never_sent_an_event_gets_its_line_after_the_others() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let mut driver = driver(&mut out, 100);
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.message(done("a", 1), secs(1)).unwrap();
    let late = Ready {
        name: "late".to_string(),
        bytes: None,
        label: None,
        outcome: Outcome::Files(4),
        elapsed: None,
    };
    let already = Ready {
        name: "a".to_string(),
        ..late.clone()
    };
    driver.finish(secs(2), vec![late, already]).unwrap();
    let lines = watcher.sync().lines();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("  ✔ a "));
    assert!(lines[1].starts_with("  ✔ late"));
}

// =========================================================
// The loop and the session
// =========================================================

fn setup(cols: usize) -> Setup {
    Setup {
        config: config(cols),
        label: Box::new(|stamp, _| stamp.to_string()),
        panic_hook: false,
    }
}

#[test]
fn the_loop_ends_when_every_sender_is_gone() {
    let (_capture, mut out) = terminal(100, 24, true);
    let driver = driver(&mut out, 100);
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        let handle = scope.spawn(move || render(driver, receiver, Instant::now()));
        sender
            .send(Packet::Msg(started("a", 10), Duration::ZERO))
            .unwrap();
        drop(sender);
        handle.join().unwrap().unwrap();
    });
}

#[test]
fn messages_that_arrive_after_the_stop_are_still_applied() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let driver = driver(&mut out, 100);
    let (sender, receiver) = mpsc::channel();
    sender.send(Packet::Stop).unwrap();
    sender.send(Packet::Stop).unwrap();
    sender
        .send(Packet::Msg(started("a", 10), Duration::ZERO))
        .unwrap();
    sender
        .send(Packet::Msg(done("a", 1), Duration::ZERO))
        .unwrap();
    thread::scope(|scope| {
        let handle = scope.spawn(move || render(driver, receiver, Instant::now()));
        handle.join().unwrap().unwrap();
    });
    let lines = watcher.sync().lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("  ✔ a "));
}

#[test]
fn a_run_with_a_session_leaves_the_lines_and_hands_back_its_value_and_cards() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let observed = observed(
        &mut out,
        setup(100),
        |observer| {
            observer.event(Event::Planned {
                items: &[Planned {
                    name: "a".to_string(),
                    bytes: 10,
                }],
            });
            observer.event(Event::Started {
                name: "a",
                bytes: Some(10),
                stamp: Some("2026-11-05T13-02-11Z"),
                siblings: &[],
            });
            observer.event(Event::Planned {
                items: &[Planned {
                    name: "gone".to_string(),
                    bytes: 1,
                }],
            });
            observer.event(Event::Dropped { name: "gone" });
            observer.event(Event::Paused { name: "a" });
            observer.event(Event::Finished {
                name: "a",
                bytes: 10,
                outcome: &Outcome::Files(2),
            });
            thread::sleep(Duration::from_millis(250));
            7
        },
        |value| {
            assert_eq!(*value, 7);
            vec![Ready {
                name: "z".to_string(),
                bytes: None,
                label: None,
                outcome: Outcome::Files(1),
                elapsed: None,
            }]
        },
    )
    .unwrap();
    assert_eq!(observed.value, 7);
    assert!(observed.took >= Duration::from_millis(250));
    assert!(observed.cards.has_finished("a"));
    let lines = watcher.sync().lines();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("  ✔ a "));
    assert!(lines[1].starts_with("  ✔ z"));
    let summary = observed.cards.summary_line(&Summary {
        tone: Tone::Ok,
        parts: vec![("2 archived".to_string(), Tone::Ok)],
        took: observed.took,
    });
    assert!(summary.starts_with("✔ 2 archived"));
}

#[test]
fn an_error_of_the_run_leaves_no_region_behind() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let observed = observed(
        &mut out,
        setup(100),
        |observer| -> Result<(), &'static str> {
            observer.event(Event::Started {
                name: "a",
                bytes: Some(10),
                stamp: None,
                siblings: &[],
            });
            thread::sleep(Duration::from_millis(250));
            Err("the run failed")
        },
        |value| {
            assert!(value.is_err());
            Vec::new()
        },
    )
    .unwrap();
    assert!(observed.value.is_err());
    assert!(watcher.sync().visible().is_empty());
}

/// The panic hook is global to the process, so the tests that replace it
/// run one after the other, and each puts the hook back when it ends.
static HOOK_TESTS: Mutex<()> = Mutex::new(());

type Hook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

struct RestoredHook(Option<Hook>);

impl RestoredHook {
    /// Replaces the hook with `hook` until the guard is dropped.
    fn replace(hook: impl Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static) -> RestoredHook {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(hook));
        RestoredHook(Some(previous))
    }
}

impl Drop for RestoredHook {
    fn drop(&mut self) {
        // A hook cannot be set while the thread panics: a failed assertion
        // of the test leaves the hook of the test in place.
        if !thread::panicking()
            && let Some(previous) = self.0.take()
        {
            panic::set_hook(previous);
            HOOK_INSTALLED.store(false, Ordering::SeqCst);
        }
    }
}

/// Runs `run` with a panic in it on a terminal and returns how many rows
/// the screen held when the panic text would have been written.
fn rows_at_the_panic(panic_hook: bool) -> (usize, bool) {
    let (capture, mut out) = terminal(100, 24, true);
    let sampled = Arc::new(Mutex::new(None));
    let drawn = Arc::new(Mutex::new(false));
    let (sampler, sample, seen) = (capture.clone(), sampled.clone(), drawn.clone());
    let _restored = RestoredHook::replace(move |_| {
        let mut screen = Screen::new(100, 24);
        screen.feed(&sampler.stdout_bytes());
        *sample.lock().unwrap() = Some(screen.visible().len());
        *seen.lock().unwrap() =
            String::from_utf8_lossy(&sampler.stdout_bytes()).contains("scanner");
    });
    let mut setup = setup(100);
    setup.panic_hook = panic_hook;
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _ = observed(
            &mut out,
            setup,
            |observer| {
                observer.event(Event::Started {
                    name: "scanner",
                    bytes: Some(10),
                    stamp: None,
                    siblings: &[],
                });
                thread::sleep(Duration::from_millis(300));
                panic!("the run panics");
            },
            |_: &()| Vec::new(),
        );
    }));
    assert!(result.is_err());
    let rows = sampled.lock().unwrap().expect("the hook ran");
    let drawn = *drawn.lock().unwrap();
    (rows, drawn)
}

#[test]
fn a_panicking_run_ends_the_renderer_and_the_hook_clears_the_region_before_the_text() {
    let _serial = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
    let (rows, drawn) = rows_at_the_panic(false);
    assert!(drawn);
    assert_eq!(
        rows, 2,
        "without the hook the cards are still on the screen"
    );
    let (rows, drawn) = rows_at_the_panic(true);
    assert!(drawn);
    assert_eq!(rows, 0, "the hook cleared the cards first");
}

#[test]
fn a_panic_on_a_worker_thread_is_written_whole_on_a_clean_screen_and_the_calling_thread_unwinds() {
    let _serial = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
    let (capture, mut out) = terminal(100, 24, true);
    // The hook that stands for the default one: its text goes to the same
    // screen as the cards, the way the default hook writes to the terminal.
    let terminal_text = capture.clone();
    let _restored = RestoredHook::replace(move |info| {
        let text = info
            .payload_as_str()
            .unwrap_or("a panic without text")
            .to_string();
        terminal_text
            .output_with(env(100, true), env(100, true))
            .line(&format!("panicked: {text}"))
            .unwrap();
    });
    let mut setup = setup(100);
    setup.panic_hook = true;
    let drawn = capture.clone();

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _ = observed(
            &mut out,
            setup,
            |observer| {
                observer.event(Event::Started {
                    name: "scanner",
                    bytes: Some(10),
                    stamp: None,
                    siblings: &[],
                });
                thread::sleep(Duration::from_millis(300));
                assert!(
                    String::from_utf8_lossy(&drawn.stdout_bytes()).contains("scanner"),
                    "the card was on the screen before the panic"
                );
                // The worker panics while the renderer draws. The scope
                // then panics on this thread, because a thread of it did.
                thread::scope(|workers| {
                    workers.spawn(|| panic!("the worker failed"));
                });
            },
            |_: &()| Vec::new(),
        );
    }));

    assert!(result.is_err(), "the panic reaches the calling thread");
    let mut screen = Screen::new(100, 24);
    screen.feed(&capture.stdout_bytes());
    let lines = screen.lines();
    assert!(
        lines.contains(&"panicked: the worker failed".to_string()),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("panicked: ") && line.contains("scoped thread panicked")),
        "{lines:?}"
    );
    assert!(
        screen
            .visible()
            .iter()
            .all(|line| !line.contains("scanner")),
        "no card is left on the screen: {:?}",
        screen.visible()
    );

    // With no run to halt, the hook passes the panic on at once, and asking
    // for the hook again changes nothing.
    install_hook();
    let before = capture.stdout_bytes().len();
    assert!(panic::catch_unwind(|| panic!("after the run")).is_err());
    let after = capture.stdout();
    assert!(
        after[before..].starts_with("panicked: after the run"),
        "{after:?}"
    );
}
