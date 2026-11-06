// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Object IDs.

use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use ulid::Ulid;

/// The ID of an object, a ULID. It is written in upper case and read in
/// either case. The same type names a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(Ulid);

impl ObjectId {
    #[cfg(test)]
    pub fn from_ulid(ulid: Ulid) -> Self {
        ObjectId(ulid)
    }

    #[cfg(test)]
    pub fn ulid(self) -> Ulid {
        self.0
    }

    #[cfg(test)]
    /// The time the ID was minted for, in milliseconds since the epoch.
    pub fn timestamp_ms(self) -> u64 {
        self.0.timestamp_ms()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ObjectId {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, IdError> {
        // The decoder lets a first character above 7 overflow the 128 bits
        // silently, so only text that prints back as itself is an ID.
        Ulid::from_string(text)
            .ok()
            .filter(|ulid| ulid.to_string().eq_ignore_ascii_case(text))
            .map(ObjectId)
            .ok_or_else(|| IdError::Invalid(text.to_string()))
    }
}

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Mints the IDs of new objects. The ID carries the time it is minted for,
/// which the caller works out from the folder.
pub trait IdSource: Send + Sync {
    fn mint(&self, time: Timestamp) -> ObjectId;
}

/// Mints IDs with a random part, one per call.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomIds;

impl IdSource for RandomIds {
    fn mint(&self, time: Timestamp) -> ObjectId {
        ObjectId(Ulid::from_datetime(SystemTime::from(time)))
    }
}

/// The time in milliseconds since the epoch; a time before 1970 is 0, as
/// `Ulid::from_datetime` reads it.
#[cfg(test)]
fn milliseconds(time: Timestamp) -> u64 {
    u64::try_from(time.as_millisecond()).unwrap_or(0)
}

/// Mints IDs whose random part counts up, so tests get the same IDs on
/// every run.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct SeqIds {
    next: AtomicU64,
}

#[cfg(test)]
impl SeqIds {
    pub fn new(first: u64) -> Self {
        SeqIds {
            next: AtomicU64::new(first),
        }
    }
}

#[cfg(test)]
impl IdSource for SeqIds {
    fn mint(&self, time: Timestamp) -> ObjectId {
        let counter = self.next.fetch_add(1, Ordering::Relaxed);
        ObjectId(Ulid::from_parts(milliseconds(time), u128::from(counter)))
    }
}

/// Errors of object IDs.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum IdError {
    #[error("`{0}` is not an object ID")]
    Invalid(String),
}

impl crate::error::HasKind for IdError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, ErrorKind};
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Barrier;
    use ulid::Ulid;

    const TEXT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn an_id_is_written_in_upper_case() {
        let id: ObjectId = TEXT.parse().expect("a valid ULID");
        assert_eq!(id.to_string(), TEXT);
    }

    #[test]
    fn an_id_is_read_in_either_case() {
        let lower: ObjectId = TEXT.to_lowercase().parse().expect("a valid ULID");
        let upper: ObjectId = TEXT.parse().expect("a valid ULID");
        assert_eq!(lower, upper);
    }

    #[test]
    fn text_that_is_not_an_id_is_refused() {
        let bad = [
            "",
            "01ARZ3NDEKTSV4RRFFQ69G5FA",
            "01ARZ3NDEKTSV4RRFFQ69G5FAVX",
            "01ARZ3NDEKTSV4RRFFQ69G5FAI",
            "81ARZ3NDEKTSV4RRFFQ69G5FAV",
        ];
        for text in bad {
            assert!(text.parse::<ObjectId>().is_err(), "{text:?} was accepted");
        }
    }

    #[test]
    fn ids_sort_by_their_time() {
        let early = ObjectId::from_ulid(Ulid::from_parts(1_000, 7));
        let late = ObjectId::from_ulid(Ulid::from_parts(2_000, 1));
        assert!(early < late);
        assert_eq!(early.timestamp_ms(), 1_000);
        assert_eq!(early.ulid(), Ulid::from_parts(1_000, 7));
    }

    #[test]
    fn an_id_serialises_as_its_text() {
        let id: ObjectId = TEXT.parse().expect("a valid ULID");
        assert_eq!(serde_json::to_value(id).expect("serialises"), json!(TEXT));
        let back: ObjectId = serde_json::from_value(json!(TEXT.to_lowercase())).expect("parses");
        assert_eq!(back, id);
        assert!(serde_json::from_value::<ObjectId>(json!("nope")).is_err());
    }

    #[test]
    fn an_invalid_id_is_a_failure_without_a_kind_of_its_own() {
        let error: Error = "nope".parse::<ObjectId>().expect_err("invalid").into();
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(error.to_string().contains("nope"));
    }

    #[test]
    fn id_sources_can_be_shared_between_threads() {
        fn assert_send_sync<T: ?Sized + Send + Sync>() {}
        assert_send_sync::<dyn IdSource>();
    }

    #[test]
    fn an_id_source_mints_for_a_given_time() {
        struct Fixed;
        impl IdSource for Fixed {
            fn mint(&self, time: jiff::Timestamp) -> ObjectId {
                let ms = u64::try_from(time.as_millisecond()).expect("after 1970");
                ObjectId::from_ulid(Ulid::from_parts(ms, 0))
            }
        }
        let time: jiff::Timestamp = "2026-11-03T10:00:00Z".parse().expect("valid");
        assert_eq!(
            Fixed.mint(time).timestamp_ms(),
            u64::try_from(time.as_millisecond()).expect("after 1970")
        );
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("a valid timestamp")
    }

    fn milliseconds(time: Timestamp) -> u64 {
        u64::try_from(time.as_millisecond()).expect("after 1970")
    }

    #[test]
    fn random_ids_carry_the_given_time() {
        let time = at("2026-11-03T10:00:00.123Z");
        assert_eq!(RandomIds.mint(time).timestamp_ms(), milliseconds(time));
    }

    #[test]
    fn random_ids_of_one_time_differ() {
        let time = at("2026-11-03T10:00:00Z");
        let ids: HashSet<ObjectId> = (0..1000).map(|_| RandomIds.mint(time)).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn random_ids_sort_by_their_time() {
        let early = RandomIds.mint(at("2020-01-01T00:00:00Z"));
        let late = RandomIds.mint(at("2026-01-01T00:00:00Z"));
        assert!(early < late);
    }

    #[test]
    fn a_time_before_1970_mints_with_time_zero_randomly() {
        assert_eq!(RandomIds.mint(at("1969-12-31T23:59:59Z")).timestamp_ms(), 0);
    }

    #[test]
    fn sequential_ids_count_up_within_one_time() {
        let time = at("2026-11-03T10:00:00Z");
        let ids = SeqIds::default();
        let ulids: Vec<Ulid> = (0..3).map(|_| ids.mint(time).ulid()).collect();
        let ms = milliseconds(time);
        assert_eq!(
            ulids,
            [
                Ulid::from_parts(ms, 0),
                Ulid::from_parts(ms, 1),
                Ulid::from_parts(ms, 2)
            ]
        );
    }

    #[test]
    fn sequential_ids_start_where_told() {
        let time = at("2026-11-03T10:00:00Z");
        let ids = SeqIds::new(7);
        assert_eq!(
            ids.mint(time).ulid(),
            Ulid::from_parts(milliseconds(time), 7)
        );
    }

    #[test]
    fn a_default_source_starts_at_zero() {
        let time = at("2026-11-03T10:00:00Z");
        let ms = milliseconds(time);
        assert_eq!(SeqIds::default().mint(time).ulid(), Ulid::from_parts(ms, 0));
        let random: RandomIds = Default::default();
        assert_eq!(random.mint(time).timestamp_ms(), ms);
    }

    #[test]
    fn a_time_before_1970_mints_with_time_zero_sequentially() {
        let id = SeqIds::new(5).mint(at("1969-12-31T23:59:59Z"));
        assert_eq!(id.ulid(), Ulid::from_parts(0, 5));
    }

    #[test]
    fn sequential_ids_sort_by_time_not_by_call_order() {
        let ids = SeqIds::default();
        let later = ids.mint(at("2026-01-01T00:00:00Z"));
        let earlier = ids.mint(at("2020-01-01T00:00:00Z"));
        assert!(earlier < later);
    }

    #[test]
    fn sequential_ids_stay_unique_across_threads() {
        const THREADS: usize = 8;
        const MINTS: usize = 250;
        let time = at("2026-11-03T10:00:00Z");
        let ids = SeqIds::default();
        let start = Barrier::new(THREADS);
        let minted: Vec<Vec<ObjectId>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        (0..MINTS).map(|_| ids.mint(time)).collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("a minting thread does not panic"))
                .collect()
        });
        let unique: HashSet<ObjectId> = minted.into_iter().flatten().collect();
        assert_eq!(unique.len(), THREADS * MINTS);
    }
}
