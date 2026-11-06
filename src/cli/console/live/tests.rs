// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use crate::cli::console::cards::{Kind, Summary, Tone};
use crate::cli::console::env::Env;
use crate::cli::output::Style;
use crate::cli::output::testing::{Capture, FailsOnFlush};
use antiq::progress::{Counter, Operation, Outcome, Planned};

#[path = "../../../../tests/support/screen.rs"]
mod screen;

use screen::Screen;
use std::sync::Arc;

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

fn env(cols: usize, cursor: bool) -> Env {
    Env::new(true, false, true, cols).with_cursor(cursor)
}

fn config(cols: usize) -> Config {
    Config {
        kind: Kind::Archive,
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
        bytes,
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
    Driver::new(Cards::new(config(cols)), out, None)
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
fn a_header_goes_above_the_cards_once() {
    let (capture, mut out) = terminal(100, 24, true);
    let mut watcher = Watcher::new(&capture, 100, 24);
    let mut driver = Driver::new(
        Cards::new(config(100)),
        &mut out,
        Some("Pushing to /Volumes/nas".to_string()),
    );
    driver.message(started("a", 10), secs(0)).unwrap();
    driver.message(started("b", 10), secs(0)).unwrap();
    driver.frame(secs(0), true).unwrap();
    let lines = watcher.sync().lines();
    assert_eq!(lines[0], "Pushing to /Volumes/nas");
    assert_eq!(lines[1], "");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("Pushing"))
            .count(),
        1
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
    let region = written.find("b ").expect("the region follows");
    assert!(line < region);
    assert_eq!(
        written.matches("\x1b[").filter(|_| true).count(),
        2,
        "{written:?}"
    );
}

// =========================================================
// Resize
// =========================================================

fn resize_run(rewrap: bool) {
    let (capture, mut out) = terminal(100, 12, true);
    let mut watcher = Watcher::new(&capture, 100, 12);
    let mut driver = driver(&mut out, 100);
    for (index, name) in ["one", "two", "three"].iter().enumerate() {
        driver.message(started(name, 10), secs(0)).unwrap();
        driver.message(done(name, 1), secs(1)).unwrap();
        driver.frame(secs(index as u64), true).unwrap();
    }
    driver.message(started("four", 10), secs(2)).unwrap();
    let (msg, _keep) = packing("four", 1, 2);
    driver.message(msg, secs(2)).unwrap();
    driver.frame(secs(2), true).unwrap();
    watcher.sync();
    // The window shrinks. The driver has no signal for it and goes on with
    // the size it was given.
    watcher.screen.resize(60, 12, rewrap);
    driver.message(started("five", 10), secs(3)).unwrap();
    driver.message(done("four", 1), secs(3)).unwrap();
    driver.message(done("five", 1), secs(3)).unwrap();
    driver.finish(secs(3), Vec::new()).unwrap();
    let lines = watcher.sync().lines();
    for name in ["one", "two", "three", "four", "five"] {
        let found = lines
            .iter()
            .filter(|line| line.starts_with(&format!("  ✔ {name}")))
            .count();
        assert_eq!(found, 1, "{name} in {lines:#?}");
    }
}

#[test]
fn no_finished_line_is_erased_when_the_window_shrinks_and_the_terminal_rewraps() {
    resize_run(true);
}

#[test]
fn no_finished_line_is_erased_when_the_window_shrinks_and_the_terminal_does_not_rewrap() {
    resize_run(false);
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
        header: None,
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
                bytes: 10,
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
                bytes: 10,
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
        *seen.lock().unwrap() = String::from_utf8_lossy(&sampler.stdout_bytes()).contains("10 B");
    });
    let mut setup = setup(100);
    setup.panic_hook = panic_hook;
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _ = observed(
            &mut out,
            setup,
            |observer| {
                observer.event(Event::Started {
                    name: "a",
                    bytes: 10,
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
                    name: "a",
                    bytes: 10,
                    stamp: None,
                    siblings: &[],
                });
                thread::sleep(Duration::from_millis(300));
                assert!(
                    String::from_utf8_lossy(&drawn.stdout_bytes()).contains("10 B"),
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
        screen.visible().iter().all(|line| !line.contains("10 B")),
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
