// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The marker file at the root of a remote.

use crate::error::HasKind;
use crate::id::ObjectId;
use serde::{Deserialize, Serialize};

/// The name of the marker in the remote's root.
pub const FILE_NAME: &str = "antiq-remote.json";

/// The `format` and `layout` this version reads and writes.
const CURRENT: u32 = 1;

/// Why a marker cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum MarkerError {
    #[error("the marker {FILE_NAME} cannot be read: {reason}")]
    Invalid { reason: String },
    #[error("the marker {FILE_NAME} has {field} {found}, written by a newer antiq than this one")]
    TooNew { field: &'static str, found: u32 },
}

impl HasKind for MarkerError {}

/// The content of `antiq-remote.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub format: u32,
    pub id: ObjectId,
    pub layout: u32,
}

impl Marker {
    pub fn new(id: ObjectId) -> Marker {
        Marker {
            format: CURRENT,
            id,
            layout: CURRENT,
        }
    }

    /// The bytes of the file: one line of JSON.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes =
            serde_json::to_vec(self).expect("a marker of numbers and a string always serialises");
        bytes.push(b'\n');
        bytes
    }

    /// Reads a marker. Keys this version does not know are ignored, and a
    /// `format` or `layout` above the one this version writes is refused,
    /// since a newer antiq may have changed what the root holds.
    pub fn parse(bytes: &[u8]) -> Result<Marker, MarkerError> {
        let marker: Marker =
            serde_json::from_slice(bytes).map_err(|error| MarkerError::Invalid {
                reason: error.to_string(),
            })?;
        for (field, found) in [("format", marker.format), ("layout", marker.layout)] {
            if found > CURRENT {
                return Err(MarkerError::TooNew { field, found });
            }
            if found == 0 {
                return Err(MarkerError::Invalid {
                    reason: format!("{field} is 0"),
                });
            }
        }
        Ok(marker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

    fn id() -> ObjectId {
        ID.parse().expect("a ULID")
    }

    fn expected_bytes() -> Vec<u8> {
        format!("{{\"format\":1,\"id\":\"{ID}\",\"layout\":1}}\n").into_bytes()
    }

    #[test]
    fn a_marker_is_one_line_of_json_in_a_fixed_key_order() {
        assert_eq!(Marker::new(id()).to_bytes(), expected_bytes());
    }

    #[test]
    fn a_marker_reads_back() {
        let parsed = Marker::parse(&Marker::new(id()).to_bytes()).expect("a marker");
        assert_eq!(parsed, Marker::new(id()));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let text = format!("{{\"format\":1,\"id\":\"{ID}\",\"layout\":1,\"note\":\"x\"}}");
        assert_eq!(
            Marker::parse(text.as_bytes()).expect("a marker"),
            Marker::new(id())
        );
    }

    #[test]
    fn text_that_is_no_json_is_invalid() {
        assert!(matches!(
            Marker::parse(b"not json"),
            Err(MarkerError::Invalid { .. })
        ));
    }

    #[test]
    fn a_marker_without_an_id_is_invalid() {
        assert!(matches!(
            Marker::parse(br#"{"format":1,"layout":1}"#),
            Err(MarkerError::Invalid { .. })
        ));
    }

    #[test]
    fn a_format_of_zero_is_invalid() {
        let text = format!("{{\"format\":0,\"id\":\"{ID}\",\"layout\":1}}");
        assert!(matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::Invalid { .. })
        ));
    }

    #[test]
    fn a_higher_format_is_too_new() {
        let text = format!("{{\"format\":2,\"id\":\"{ID}\",\"layout\":1}}");
        assert!(matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::TooNew {
                field: "format",
                found: 2
            })
        ));
    }

    #[test]
    fn a_higher_layout_is_too_new() {
        let text = format!("{{\"format\":1,\"id\":\"{ID}\",\"layout\":2}}");
        assert!(matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::TooNew {
                field: "layout",
                found: 2
            })
        ));
    }
}
