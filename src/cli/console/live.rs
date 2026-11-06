// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cards of a run on a terminal: the driver that draws them in place,
//! the observer that carries the events of the library to it, and the
//! session that runs both for the length of one command.
//!
//! The driver moves the cursor with ANSI sequences only. It switches no
//! terminal mode, so there is nothing to restore when a run ends by Ctrl-C.

#![allow(
    dead_code,
    reason = "the wiring of commit 7 is its caller and removes this"
)]

use crate::cli::console::cards::{Cards, Config, Msg, Ready};
use crate::cli::output::Output;
use antiq::progress::{Event, Observer};
use std::io;
use std::panic;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, Once, PoisonError};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

/// How often the region is redrawn at most, and the longest the loop waits
/// for a message.
const TICK: Duration = Duration::from_millis(100);
/// How long a panicking thread waits for the renderer to clear the region.
const HALT_WAIT: Duration = Duration::from_millis(500);
/// A terminal that reports no size gets this many rows.
const DEFAULT_ROWS: usize = 24;

// =========================================================
// The driver
// =========================================================

/// Draws the cards of [`Cards`] on stdout: the lines of finished pieces
/// once, the region of running pieces in place below them.
pub(crate) struct Driver<'o> {
    cards: Cards,
    out: &'o mut Output,
    /// The cursor moves, so the region can be redrawn in place.
    live: bool,
    /// The lines of the region on the screen. The cursor sits below them.
    shown: Vec<String>,
    last_size: (usize, usize),
    /// The first message prints this above the cards.
    header: Option<String>,
    /// A panic elsewhere cleared the region: finished lines go out plainly.
    halted: bool,
}

impl<'o> Driver<'o> {
    pub(crate) fn new(cards: Cards, out: &'o mut Output, header: Option<String>) -> Driver<'o> {
        let env = out.stdout_env();
        Driver {
            cards,
            live: env.rich && env.cursor,
            out,
            shown: Vec::new(),
            last_size: (0, 0),
            header,
            halted: false,
        }
    }

    fn size(&self) -> (usize, usize) {
        let columns = self.out.stdout_env().width;
        self.out.stdout_size().unwrap_or((columns, DEFAULT_ROWS))
    }

    pub(crate) fn message(&mut self, msg: Msg, now: Duration) -> io::Result<()> {
        if let Some(header) = self.header.take() {
            self.out.line(&header)?;
            self.out.line("")?;
        }
        self.cards.apply(msg, now);
        Ok(())
    }

    /// Whether a finished piece waits for its line.
    pub(crate) fn has_ready(&self) -> bool {
        self.cards.has_ready()
    }

    /// The sequence that takes the cursor to the start of the region and
    /// erases everything from there on.
    fn erase(&self) -> String {
        let mut sequence = String::from("\r");
        if !self.shown.is_empty() {
            sequence.push_str(&format!("\x1b[{}A", self.shown.len()));
        }
        sequence.push_str("\x1b[J");
        sequence
    }

    fn text(lines: &[String]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    /// Writes the lines of the pieces that finished and the region as it
    /// stands at `now`. The lines go out in their own write, so that an
    /// interrupted run has them whole; a frame that equals the last one is
    /// not written again.
    /// With `draw_region` off the region is taken off the screen instead.
    pub(crate) fn frame(&mut self, now: Duration, draw_region: bool) -> io::Result<()> {
        let size = self.size();
        let (columns, rows) = size;
        let ready = self.cards.take_ready();
        let lines: Vec<String> = ready
            .iter()
            .flat_map(|ready| self.cards.finished_lines(ready, columns))
            .collect();
        if !self.live || self.halted {
            for line in &lines {
                self.out.line(line)?;
            }
            return Ok(());
        }
        let region = if draw_region {
            self.cards.region(now, columns, rows)
        } else {
            Vec::new()
        };
        if lines.is_empty() && region == self.shown && size == self.last_size {
            return Ok(());
        }
        self.last_size = size;
        let mut prefix = self.erase();
        if !lines.is_empty() {
            self.out
                .stdout_raw(format!("{prefix}{}", Self::text(&lines)).as_bytes())?;
            self.shown.clear();
            prefix.clear();
        }
        self.out
            .stdout_raw(format!("{prefix}{}", Self::text(&region)).as_bytes())?;
        self.shown = region;
        Ok(())
    }

    /// Takes the region off the screen.
    pub(crate) fn clear(&mut self) -> io::Result<()> {
        if self.live && !self.shown.is_empty() {
            let erase = self.erase();
            self.out.stdout_raw(erase.as_bytes())?;
            self.shown.clear();
        }
        Ok(())
    }

    /// A panic on another thread: the region goes, and finished lines are
    /// printed plainly from now on.
    pub(crate) fn halt(&mut self) -> io::Result<()> {
        self.clear()?;
        self.halted = true;
        Ok(())
    }

    /// The end of the run: the lines still owed, the region gone, and a
    /// line for each of `missing` that never sent an event.
    pub(crate) fn finish(&mut self, now: Duration, missing: Vec<Ready>) -> io::Result<()> {
        self.frame(now, false)?;
        let columns = self.size().0;
        for ready in missing {
            if self.cards.has_finished(&ready.name) {
                continue;
            }
            for line in self.cards.finished_lines(&ready, columns) {
                self.out.line(&line)?;
            }
        }
        Ok(())
    }

    pub(crate) fn into_cards(self) -> Cards {
        self.cards
    }
}

// =========================================================
// The observer
// =========================================================

enum Packet {
    Msg(Msg, Duration),
    Stop,
    /// Clear the region and say so on the channel.
    Halt(Sender<()>),
}

/// The observer a command gives the library: it turns events into
/// messages for the renderer. The sender is `Sync`, so the worker threads
/// of a run share one.
pub(crate) struct Feed {
    sender: Sender<Packet>,
    start: Instant,
    /// How the version of an unearth is shown.
    label: Box<dyn Fn(&str) -> String + Send + Sync>,
}

impl Observer for Feed {
    fn event(&self, event: Event<'_>) {
        let msg = match event {
            Event::Planned { items } => Msg::Planned(
                items
                    .iter()
                    .map(|item| (item.name.clone(), item.bytes))
                    .collect(),
            ),
            Event::Started { name, bytes, stamp } => Msg::Started {
                name: name.to_string(),
                bytes,
                label: stamp.map(|stamp| (self.label)(stamp)),
            },
            Event::Operation {
                name,
                operation,
                counter,
            } => Msg::Operation {
                name: name.to_string(),
                operation,
                counter: counter.clone(),
            },
            Event::Finished {
                name,
                bytes,
                outcome,
            } => Msg::Finished {
                name: name.to_string(),
                bytes,
                outcome: outcome.clone(),
            },
            Event::Dropped { name } => Msg::Dropped {
                name: name.to_string(),
            },
            Event::Paused { name } => Msg::Paused {
                name: name.to_string(),
            },
        };
        // The renderer is gone when its output failed; the run goes on.
        let _ = self.sender.send(Packet::Msg(msg, self.start.elapsed()));
    }
}

// =========================================================
// The loop and the session
// =========================================================

/// Applies messages and redraws until told to stop or until every sender is
/// gone, then writes what is still owed and clears the region.
fn render<'o>(
    mut driver: Driver<'o>,
    receiver: mpsc::Receiver<Packet>,
    start: Instant,
) -> io::Result<Driver<'o>> {
    let mut last_frame = Duration::ZERO;
    loop {
        match receiver.recv_timeout(TICK) {
            Ok(Packet::Msg(msg, at)) => driver.message(msg, at)?,
            Ok(Packet::Halt(acknowledge)) => {
                driver.halt()?;
                let _ = acknowledge.send(());
            }
            Ok(Packet::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = start.elapsed();
        if now.saturating_sub(last_frame) >= TICK || driver.has_ready() {
            driver.frame(now, true)?;
            last_frame = now;
        }
    }
    for packet in receiver.try_iter() {
        if let Packet::Msg(msg, at) = packet {
            driver.message(msg, at)?;
        }
    }
    driver.frame(start.elapsed(), false)?;
    Ok(driver)
}

/// What a command gives to [`observed`].
pub(crate) struct Setup {
    pub config: Config,
    /// Printed above the cards when the first event arrives.
    pub header: Option<String>,
    pub label: Box<dyn Fn(&str) -> String + Send + Sync>,
    /// Install the panic hook that clears the region before the panic text
    /// is written. Tests that run in parallel leave it off, since a hook
    /// is global to the process.
    pub panic_hook: bool,
}

/// What a command gets back from [`observed`].
pub(crate) struct Observed<T> {
    pub value: T,
    pub cards: Cards,
    /// From the start of the run to its end.
    pub took: Duration,
}

/// Where a panicking thread finds the renderer to halt.
static RENDERER: Mutex<Option<(Sender<Packet>, ThreadId)>> = Mutex::new(None);
static HOOK: Once = Once::new();

fn renderer_slot() -> std::sync::MutexGuard<'static, Option<(Sender<Packet>, ThreadId)>> {
    RENDERER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ends the renderer when the run ends, however it ends: a run that
/// panics unwinds through this, so the scope that joins the renderer does
/// not wait for a message that never comes.
struct Release {
    sender: Sender<Packet>,
    hook: bool,
}

impl Drop for Release {
    fn drop(&mut self) {
        if self.hook {
            *renderer_slot() = None;
        }
        let _ = self.sender.send(Packet::Stop);
    }
}

fn install_hook() {
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let target = renderer_slot()
                .clone()
                .filter(|(_, thread)| *thread != thread::current().id());
            if let Some((sender, _)) = target {
                let (acknowledge, acknowledged) = mpsc::channel();
                if sender.send(Packet::Halt(acknowledge)).is_ok() {
                    let _ = acknowledged.recv_timeout(HALT_WAIT);
                }
            }
            previous(info);
        }));
    });
}

/// Runs `run` with an observer whose events become cards on stdout.
/// `missing` names the pieces of the result that never sent an event; they
/// get their line after the others. An error of the output ends the cards
/// but not `run`, whose value is then lost with the error.
pub(crate) fn observed<T>(
    out: &mut Output,
    setup: Setup,
    run: impl FnOnce(&dyn Observer) -> T,
    missing: impl FnOnce(&T) -> Vec<Ready>,
) -> io::Result<Observed<T>> {
    let start = Instant::now();
    let (sender, receiver) = mpsc::channel();
    let feed = Feed {
        sender: sender.clone(),
        start,
        label: setup.label,
    };
    let driver = Driver::new(Cards::new(setup.config), out, setup.header);
    thread::scope(|scope| {
        let renderer = scope.spawn(move || render(driver, receiver, start));
        if setup.panic_hook {
            install_hook();
            *renderer_slot() = Some((sender.clone(), renderer.thread().id()));
        }
        let release = Release {
            sender,
            hook: setup.panic_hook,
        };
        let value = run(&feed);
        drop(release);
        let mut driver = renderer
            .join()
            .map_err(|panic| io::Error::other(format!("the renderer panicked: {panic:?}")))??;
        let took = start.elapsed();
        driver.finish(took, missing(&value))?;
        Ok(Observed {
            value,
            cards: driver.into_cards(),
            took,
        })
    })
}

#[cfg(test)]
mod tests;
