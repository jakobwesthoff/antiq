// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Time source of a run.

use jiff::{SignedDuration, Timestamp};
use std::sync::Mutex;

/// Where a run gets the current time from, so tests can stop and move it.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// The wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock that stays where it is until it is advanced.
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<Timestamp>,
}

impl FixedClock {
    pub fn new(now: Timestamp) -> Self {
        FixedClock {
            now: Mutex::new(now),
        }
    }

    pub fn advance(&self, by: SignedDuration) {
        let mut now = self.lock();
        *now = now
            .checked_add(by)
            .expect("a test clock stays within the representable range");
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Timestamp> {
        self.now
            .lock()
            .expect("nothing panics while holding the clock's lock")
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        *self.lock()
    }
}

/// Drops the fraction of a second. Metadata, history and error details
/// carry whole seconds.
pub fn whole_seconds(time: Timestamp) -> Timestamp {
    Timestamp::from_second(time.as_second())
        .expect("the whole second of a valid timestamp is a valid timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::SignedDuration;

    fn assert_send_sync<T: ?Sized + Send + Sync>() {}

    #[test]
    fn clocks_can_be_shared_between_threads() {
        assert_send_sync::<dyn Clock>();
        assert_send_sync::<SystemClock>();
        assert_send_sync::<FixedClock>();
    }

    #[test]
    fn a_fixed_clock_stands_still_until_advanced() {
        let start: Timestamp = "2026-11-03T10:00:00Z".parse().expect("valid literal");
        let clock = FixedClock::new(start);
        assert_eq!(clock.now(), start);
        assert_eq!(clock.now(), start);
        clock.advance(SignedDuration::from_secs(90));
        assert_eq!(clock.now().as_second(), start.as_second() + 90);
    }

    #[test]
    fn the_system_clock_follows_the_wall_clock() {
        let before = Timestamp::now();
        let now = SystemClock.now();
        let after = Timestamp::now();
        assert!(before <= now && now <= after);
    }

    #[test]
    fn whole_seconds_drops_the_fraction() {
        let with_fraction: Timestamp = "2026-11-03T10:00:00.987654321Z"
            .parse()
            .expect("valid literal");
        assert_eq!(
            whole_seconds(with_fraction).to_string(),
            "2026-11-03T10:00:00Z"
        );
    }
}
