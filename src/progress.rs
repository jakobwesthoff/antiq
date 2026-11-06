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

    /// Raises the units done to `value`, unless more are done already. For
    /// a step that reads the total so far from somewhere else, such as the
    /// compressor, whose polls must never move the bar back.
    pub fn raise_to(&self, value: u64) {
        self.shared.done.fetch_max(value, Ordering::Relaxed);
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

/// A reader that counts the bytes passing through it into a [`Counter`]
/// as they are read, so that a bar follows a long read instead of
/// jumping when it ends.
pub struct Metered<'a, R> {
    inner: R,
    counter: Option<&'a Counter>,
}

impl<'a, R> Metered<'a, R> {
    /// Without a counter the reader just reads.
    pub fn new(inner: R, counter: Option<&'a Counter>) -> Metered<'a, R> {
        Metered { inner, counter }
    }
}

impl<R: std::io::Read> std::io::Read for Metered<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buf)?;
        if let Some(counter) = self.counter {
            counter.add(count as u64);
        }
        Ok(count)
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
    /// and `available` are free. `needed` is `None` where the volume ran full
    /// during an extraction whose size was not known.
    NoRoom { needed: Option<u64>, available: u64 },
    /// The item is partly done and the repairs of the next run finish it.
    Unfinished(String),
    /// Rebuild: the files it wrote for a damaged version.
    Rebuilt { listing: bool, checksum: bool },
}

#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// Before the work starts: the items that will probably be processed.
    Planned { items: &'a [Planned] },
    /// The item starts. `stamp` names the version of an unearth.
    Started {
        name: &'a str,
        /// The size of the item, or `None` where it is not known.
        bytes: Option<u64>,
        stamp: Option<&'a str>,
        /// The stamps of the complete versions of the piece, so that the
        /// observer can tell apart two that read alike.
        siblings: &'a [String],
    },
    /// The item moves to an operation, whose progress `counter` shows.
    Operation {
        name: &'a str,
        operation: Operation,
        counter: &'a Counter,
    },
    /// The bytes the item has sent so far grow in `bytes`, which an observer
    /// reads whenever it likes. An item whose size is not known beforehand
    /// sends this in place of the size of its `Started`.
    Measured { name: &'a str, bytes: &'a Counter },
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

    /// For the tests of the steps that nobody watches.
    #[cfg(test)]
    pub(crate) const fn silent() -> Tracker<'static> {
        Tracker {
            observer: &Silent,
            name: "",
        }
    }

    pub(crate) fn started(&self, bytes: Option<u64>, stamp: Option<&str>, siblings: &[String]) {
        self.observer.event(Event::Started {
            name: self.name,
            bytes,
            stamp,
            siblings,
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
    fn raising_a_counter_keeps_the_larger_value() {
        let counter = Counter::new();
        counter.raise_to(5);
        counter.raise_to(3);
        assert_eq!(counter.done(), 5);
        counter.raise_to(9);
        assert_eq!(counter.done(), 9);
    }

    #[test]
    fn a_metered_reader_adds_the_bytes_of_each_read_as_they_pass() {
        use std::io::Read;
        let counter = Counter::new();
        let mut reader = Metered::new(&b"0123456789"[..], Some(&counter));
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).expect("ten bytes are there");
        assert_eq!(counter.done(), 4);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).expect("the rest reads");
        assert_eq!(counter.done(), 10);
    }

    #[test]
    fn a_metered_reader_without_a_counter_just_reads() {
        use std::io::Read;
        let mut reader = Metered::new(&b"abc"[..], None);
        let mut text = String::new();
        reader.read_to_string(&mut text).expect("reads");
        assert_eq!(text, "abc");
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
                Event::Started { name, bytes, .. } => {
                    format!(
                        "started {name} {}",
                        bytes.map_or("-".to_string(), |bytes| bytes.to_string())
                    )
                }
                Event::Operation {
                    name,
                    operation,
                    counter,
                } => format!(
                    "{name} {operation:?} {}/{}",
                    counter.done(),
                    counter.total()
                ),
                Event::Measured { name, bytes } => format!("{name} sent {}", bytes.done()),
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
            bytes: Some(4211),
            stamp: None,
            siblings: &[],
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
        observer.event(Event::Planned {
            items: &[Planned {
                name: "tools/ray".to_string(),
                bytes: 9,
            }],
        });
        let sent = Counter::new();
        sent.add(512);
        observer.event(Event::Measured {
            name: "projects/scanner",
            bytes: &sent,
        });
        observer.event(Event::Paused {
            name: "projects/scanner",
        });
        observer.event(Event::Finished {
            name: "projects/scanner",
            bytes: 4211,
            outcome: &Outcome::Files(3),
        });
        observer.event(Event::Dropped { name: "tools/ray" });
        assert_eq!(
            *recording.0.lock().expect("no test thread panics"),
            [
                "started projects/scanner 4211",
                "projects/scanner Packing 0/3",
                "projects/scanner Verifying 2/3",
                "planned 1",
                "projects/scanner sent 512",
                "paused projects/scanner",
                "finished projects/scanner Files(3)",
                "dropped tools/ray",
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
