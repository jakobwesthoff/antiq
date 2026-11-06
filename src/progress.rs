// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a long run tells an observer while it works. The module knows no
//! terminal: the console turns these events into cards, a test records
//! them.
//!
//! State changes are events. Progress inside one operation is a
//! [`Counter`] that the step bumps once per file and the observer reads
//! whenever it likes, so the loops that do the work send no messages and an
//! observer cannot slow them down.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a piece is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Packing,
    Verifying,
    Storing,
    Removing,
    Extracting,
}

/// Shared by a step and its observer: the step adds, the observer reads.
/// Clones count into the same numbers.
#[derive(Debug, Clone, Default)]
pub struct Counter {
    shared: Arc<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    done: AtomicU64,
    total: AtomicU64,
}

impl Counter {
    pub fn new() -> Counter {
        Counter::default()
    }

    /// Counts `amount` more units as done.
    pub fn add(&self, amount: u64) {
        self.shared.done.fetch_add(amount, Ordering::Relaxed);
    }

    /// Sets how many units the operation has in all, once the step knows.
    pub fn set_total(&self, total: u64) {
        self.shared.total.store(total, Ordering::Relaxed);
    }

    pub fn done(&self) -> u64 {
        self.shared.done.load(Ordering::Relaxed)
    }

    /// The units in all, or 0 while the step does not know yet.
    pub fn total(&self) -> u64 {
        self.shared.total.load(Ordering::Relaxed)
    }
}

/// An item the run expects to work on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub name: String,
    pub bytes: u64,
}

/// How an item ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Archive and unearth: the files of the version.
    Files(u64),
    /// Push: what the mirror changed for the piece.
    Changes {
        created: u64,
        updated: u64,
        deleted: u64,
    },
    /// The item is left as it was, with the reasons.
    Refused(Vec<String>),
    /// The item was not processed; the text says why.
    Skipped(String),
    /// The item was not processed for lack of room: it needs `needed` bytes
    /// and `available` are free.
    NoRoom { needed: u64, available: u64 },
    /// The item is partly done and the repairs of the next run finish it.
    Unfinished(String),
}

#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// Before the work starts: the items that will probably be processed.
    Planned { items: &'a [Planned] },
    /// The item starts. `stamp` names the version of an unearth.
    Started {
        name: &'a str,
        bytes: u64,
        stamp: Option<&'a str>,
    },
    /// The item moves to an operation, whose progress `counter` shows.
    Operation {
        name: &'a str,
        operation: Operation,
        counter: &'a Counter,
    },
    Finished {
        name: &'a str,
        bytes: u64,
        outcome: &'a Outcome,
    },
    /// A planned item turned out not to be due when it came to its turn.
    Dropped { name: &'a str },
    /// The item has started and leaves its card until its next operation:
    /// the next [`Event::Operation`] of that name reopens the same item.
    /// It has not finished and gets its `Finished` only when it does.
    Paused { name: &'a str },
}

/// Receives the events of a run, from any thread.
pub trait Observer: Sync {
    fn event(&self, event: Event<'_>);
}

/// The observer of a run nobody watches.
pub struct Silent;

impl Observer for Silent {
    fn event(&self, _event: Event<'_>) {}
}

/// What a step needs to report on one item: the observer and the name the
/// events carry.
#[derive(Clone, Copy)]
pub(crate) struct Tracker<'a> {
    observer: &'a dyn Observer,
    name: &'a str,
}

impl<'a> Tracker<'a> {
    pub(crate) fn new(observer: &'a dyn Observer, name: &'a str) -> Tracker<'a> {
        Tracker { observer, name }
    }

    /// For the callers that nobody watches.
    pub(crate) const fn silent() -> Tracker<'static> {
        Tracker {
            observer: &Silent,
            name: "",
        }
    }

    pub(crate) fn started(&self, bytes: u64, stamp: Option<&str>) {
        self.observer.event(Event::Started {
            name: self.name,
            bytes,
            stamp,
        });
    }

    /// Announces the operation and hands back the counter the step adds to.
    pub(crate) fn operation(&self, operation: Operation) -> Counter {
        let counter = Counter::new();
        self.observer.event(Event::Operation {
            name: self.name,
            operation,
            counter: &counter,
        });
        counter
    }

    pub(crate) fn finished(&self, bytes: u64, outcome: &Outcome) {
        self.observer.event(Event::Finished {
            name: self.name,
            bytes,
            outcome,
        });
    }

    pub(crate) fn dropped(&self) {
        self.observer.event(Event::Dropped { name: self.name });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_clone_counts_into_the_same_numbers() {
        let counter = Counter::new();
        let reader = counter.clone();
        counter.add(2);
        counter.add(3);
        assert_eq!(reader.done(), 5);
    }

    #[test]
    fn the_total_is_unknown_until_it_is_set() {
        let counter = Counter::new();
        assert_eq!(counter.total(), 0);
        counter.set_total(518);
        assert_eq!(counter.total(), 518);
    }

    #[test]
    fn threads_adding_at_once_lose_nothing() {
        let counter = Counter::new();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..1000 {
                        counter.add(1);
                    }
                });
            }
        });
        assert_eq!(counter.done(), 4000);
    }

    /// An observer that keeps what it was told, as text.
    struct Recording(Mutex<Vec<String>>);

    impl Observer for Recording {
        fn event(&self, event: Event<'_>) {
            let text = match event {
                Event::Planned { items } => format!("planned {}", items.len()),
                Event::Started { name, bytes, .. } => format!("started {name} {bytes}"),
                Event::Operation {
                    name,
                    operation,
                    counter,
                } => format!(
                    "{name} {operation:?} {}/{}",
                    counter.done(),
                    counter.total()
                ),
                Event::Finished { name, outcome, .. } => format!("finished {name} {outcome:?}"),
                Event::Dropped { name } => format!("dropped {name}"),
                Event::Paused { name } => format!("paused {name}"),
            };
            self.0.lock().expect("no test thread panics").push(text);
        }
    }

    #[test]
    fn an_observer_sees_the_counter_as_the_step_left_it() {
        let recording = Recording(Mutex::new(Vec::new()));
        let observer: &dyn Observer = &recording;
        let counter = Counter::new();
        counter.set_total(3);
        observer.event(Event::Started {
            name: "projects/scanner",
            bytes: 4211,
            stamp: None,
        });
        observer.event(Event::Operation {
            name: "projects/scanner",
            operation: Operation::Packing,
            counter: &counter,
        });
        counter.add(2);
        observer.event(Event::Operation {
            name: "projects/scanner",
            operation: Operation::Verifying,
            counter: &counter,
        });
        assert_eq!(
            *recording.0.lock().expect("no test thread panics"),
            [
                "started projects/scanner 4211",
                "projects/scanner Packing 0/3",
                "projects/scanner Verifying 2/3",
            ]
        );
    }

    #[test]
    fn the_silent_observer_takes_every_event() {
        let observer: &dyn Observer = &Silent;
        observer.event(Event::Dropped { name: "a" });
        observer.event(Event::Planned { items: &[] });
    }
}
