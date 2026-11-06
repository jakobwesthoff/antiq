// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An observer that keeps the events of a run, for the tests of the
//! commands that report progress.

use crate::progress::{Counter, Event, Observer, Operation, Outcome, Planned};
use std::sync::Mutex;

/// One event as the run sent it. The counter of an operation is kept, not
/// read, so a test sees where the step left it.
#[derive(Debug, Clone)]
pub enum Seen {
    Planned(Vec<Planned>),
    Started {
        name: String,
        bytes: u64,
        stamp: Option<String>,
    },
    Operation {
        name: String,
        operation: Operation,
        counter: Counter,
    },
    Finished {
        name: String,
        bytes: u64,
        outcome: Outcome,
    },
    Dropped {
        name: String,
    },
    Paused {
        name: String,
    },
}

impl Seen {
    /// The event as one line: the counters read as they are now.
    pub fn line(&self) -> String {
        match self {
            Seen::Planned(items) => {
                let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
                format!("planned {}", names.join(" "))
            }
            Seen::Started { name, bytes, stamp } => match stamp {
                Some(stamp) => format!("started {name} {bytes} {stamp}"),
                None => format!("started {name} {bytes}"),
            },
            Seen::Operation {
                name,
                operation,
                counter,
            } => format!(
                "{name} {operation:?} {}/{}",
                counter.done(),
                counter.total()
            ),
            Seen::Finished {
                name,
                bytes,
                outcome,
            } => format!("finished {name} {bytes} {outcome:?}"),
            Seen::Dropped { name } => format!("dropped {name}"),
            Seen::Paused { name } => format!("paused {name}"),
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            Seen::Planned(_) => None,
            Seen::Started { name, .. }
            | Seen::Operation { name, .. }
            | Seen::Finished { name, .. }
            | Seen::Dropped { name }
            | Seen::Paused { name } => Some(name),
        }
    }
}

#[derive(Default)]
pub struct Recorder(Mutex<Vec<Seen>>);

impl Recorder {
    pub fn new() -> Recorder {
        Recorder::default()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.0
            .lock()
            .expect("no test thread panics holding the lock")
            .clone()
    }

    /// The lines of all events, in the order they arrived.
    pub fn lines(&self) -> Vec<String> {
        self.seen().iter().map(Seen::line).collect()
    }

    /// The lines of the events of one item, in the order they arrived.
    pub fn lines_of(&self, name: &str) -> Vec<String> {
        self.seen()
            .iter()
            .filter(|seen| seen.name() == Some(name))
            .map(Seen::line)
            .collect()
    }
}

impl Observer for Recorder {
    fn event(&self, event: Event<'_>) {
        let seen = match event {
            Event::Planned { items } => Seen::Planned(items.to_vec()),
            Event::Started {
                name, bytes, stamp, ..
            } => Seen::Started {
                name: name.to_string(),
                bytes,
                stamp: stamp.map(str::to_string),
            },
            Event::Operation {
                name,
                operation,
                counter,
            } => Seen::Operation {
                name: name.to_string(),
                operation,
                counter: counter.clone(),
            },
            Event::Finished {
                name,
                bytes,
                outcome,
            } => Seen::Finished {
                name: name.to_string(),
                bytes,
                outcome: outcome.clone(),
            },
            Event::Dropped { name } => Seen::Dropped {
                name: name.to_string(),
            },
            Event::Paused { name } => Seen::Paused {
                name: name.to_string(),
            },
        };
        self.0
            .lock()
            .expect("no test thread panics holding the lock")
            .push(seen);
    }
}
