// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The marker file at the root of a target.

use crate::error::HasKind;
use crate::id::ObjectId;
use serde::{Deserialize, Serialize};

/// The name of the marker in the target's root.
pub const FILE_NAME: &str = ".antiq-target";

/// The `format` this version reads and writes.
const CURRENT: u64 = 1;

/// Why a marker cannot be used.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum MarkerError {
    #[error("the marker {FILE_NAME} cannot be read: {reason}")]
    Invalid { reason: String },
    #[diagnostic(
        code(antiq::target::marker_too_new),
        help("Use a version of antiq that reads the newer format.")
    )]
    #[error("the marker {FILE_NAME} has format {found}, written by a newer antiq than this one")]
    TooNew { found: u64 },
}

impl HasKind for MarkerError {}

/// The content of `.antiq-target`: the collection the target mirrors and
/// the machine and place it is pushed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub format: u64,
    pub id: ObjectId,
    /// The machine name `machine::detect` gives.
    pub host: String,
    /// The canonical root of the collection.
    pub path: String,
}

impl Marker {
    pub fn new(id: ObjectId, host: String, path: String) -> Marker {
        Marker {
            format: CURRENT,
            id,
            host,
            path,
        }
    }

    /// The bytes of the file: one line of JSON with the keys in the order
    /// of the fields.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes =
            serde_json::to_vec(self).expect("a marker of a number and strings always serialises");
        bytes.push(b'\n');
        bytes
    }

    /// Reads a marker. Keys this version does not know are ignored. A
    /// `format` above the one this version writes is refused before any
    /// other key is looked at, since a newer antiq may have changed them.
    pub fn parse(bytes: &[u8]) -> Result<Marker, MarkerError> {
        let invalid = |reason: String| MarkerError::Invalid { reason };
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|error| invalid(error.to_string()))?;
        // A newer antiq may have changed the other keys, so only `format`
        // decides before they are read.
        if let Some(format) = value.get("format").and_then(serde_json::Value::as_u64)
            && format > CURRENT
        {
            return Err(MarkerError::TooNew { found: format });
        }
        let marker: Marker =
            serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
        if marker.format == 0 {
            return Err(invalid("format is 0".to_string()));
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

    fn marker() -> Marker {
        Marker::new(id(), "Studio Mac".into(), "/Users/jakob/projects".into())
    }

    fn invalid(text: &str) -> bool {
        matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::Invalid { .. })
        )
    }

    #[test]
    fn a_marker_is_one_line_of_json_in_a_fixed_key_order() {
        let expected = format!(
            "{{\"format\":1,\"id\":\"{ID}\",\"host\":\"Studio Mac\",\"path\":\"/Users/jakob/projects\"}}\n"
        );
        assert_eq!(marker().to_bytes(), expected.as_bytes());
    }

    #[test]
    fn quotes_and_backslashes_are_escaped_and_non_ascii_stays_literal() {
        let marker = Marker::new(id(), "Jakob's \"Mac\"".into(), "/Büro\\neu".into());
        let expected = format!(
            "{{\"format\":1,\"id\":\"{ID}\",\"host\":\"Jakob's \\\"Mac\\\"\",\"path\":\"/Büro\\\\neu\"}}\n"
        );
        assert_eq!(marker.to_bytes(), expected.as_bytes());
        assert_eq!(Marker::parse(&marker.to_bytes()).expect("a marker"), marker);
    }

    #[test]
    fn a_marker_reads_back() {
        let parsed = Marker::parse(&marker().to_bytes()).expect("a marker");
        assert_eq!(parsed, marker());
    }

    #[test]
    fn the_final_newline_is_optional() {
        let bytes = marker().to_bytes();
        let parsed = Marker::parse(&bytes[..bytes.len() - 1]).expect("a marker");
        assert_eq!(parsed, marker());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let text = format!(
            "{{\"format\":1,\"id\":\"{ID}\",\"host\":\"h\",\"path\":\"/p\",\"layout\":1,\"note\":\"x\"}}"
        );
        let parsed = Marker::parse(text.as_bytes()).expect("a marker");
        assert_eq!(parsed, Marker::new(id(), "h".into(), "/p".into()));
    }

    #[test]
    fn a_higher_format_is_too_new() {
        let text = format!("{{\"format\":2,\"id\":\"{ID}\",\"host\":\"h\",\"path\":\"/p\"}}");
        assert!(matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::TooNew { found: 2 })
        ));
    }

    #[test]
    fn a_higher_format_is_too_new_whatever_else_the_marker_holds() {
        assert!(matches!(
            Marker::parse(br#"{"format":2,"id":7}"#),
            Err(MarkerError::TooNew { found: 2 })
        ));
    }

    #[test]
    fn a_format_beyond_u64_is_invalid() {
        assert!(invalid(r#"{"format":99999999999999999999}"#));
    }

    #[test]
    fn a_format_that_is_no_unsigned_integer_is_invalid() {
        assert!(invalid(r#"{"format":"1"}"#));
        assert!(invalid(r#"{"format":-1}"#));
        assert!(invalid(r#"{"format":1.5}"#));
    }

    #[test]
    fn a_format_of_zero_is_invalid() {
        let text = format!("{{\"format\":0,\"id\":\"{ID}\",\"host\":\"h\",\"path\":\"/p\"}}");
        assert!(invalid(&text));
    }

    #[test]
    fn a_missing_format_is_invalid() {
        let text = format!("{{\"id\":\"{ID}\",\"host\":\"h\",\"path\":\"/p\"}}");
        assert!(invalid(&text));
    }

    #[test]
    fn text_that_is_no_json_object_is_invalid() {
        assert!(invalid("not json"));
        assert!(invalid("[1]"));
    }

    #[test]
    fn each_missing_key_is_invalid() {
        for missing in ["id", "host", "path"] {
            let mut keys = vec![
                format!("\"id\":\"{ID}\""),
                "\"host\":\"h\"".to_string(),
                "\"path\":\"/p\"".to_string(),
            ];
            keys.retain(|key| !key.starts_with(&format!("\"{missing}\"")));
            let text = format!("{{\"format\":1,{}}}", keys.join(","));
            assert!(invalid(&text), "{missing}");
        }
    }

    #[test]
    fn a_key_of_the_wrong_type_is_invalid() {
        let text = format!("{{\"format\":1,\"id\":\"{ID}\",\"host\":7,\"path\":\"/p\"}}");
        assert!(invalid(&text));
    }
}
