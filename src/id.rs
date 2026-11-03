// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Object IDs.

use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use ulid::Ulid;

/// The ID of an object, a ULID. It is written in upper case and read in
/// either case. The same type names a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(Ulid);

impl ObjectId {
    pub fn from_ulid(ulid: Ulid) -> Self {
        ObjectId(ulid)
    }

    pub fn ulid(self) -> Ulid {
        self.0
    }

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

/// Errors of object IDs.
#[derive(Debug, thiserror::Error)]
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
}
