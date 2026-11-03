// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Object metadata: the data types of the JSON object that is shared
//! through the remote.
//!
//! Keys are written in the order of the fields. A key the struct does not
//! know is kept in `extra` and written behind the known ones, in the order
//! it was read, so a newer writer's additions survive a rewrite.

use crate::clock::whole_seconds;
use crate::error::{ErrorKind, HasKind};
use crate::id::ObjectId;
use crate::relpath::{Name, RelPath};
use jiff::Timestamp;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// The metadata format this version reads and writes.
pub const FORMAT: u32 = 1;

/// How a piece currently exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Live,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PayloadKind {
    Archive,
    Live,
}

/// Where the time in `created` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CreatedSource {
    Birth,
    OldestMtime,
    Import,
    Fork,
    Minted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checksum {
    pub algorithm: String,
    pub value: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Checksum {
    pub fn sha256(value: impl Into<String>) -> Checksum {
        Checksum {
            algorithm: "sha256".into(),
            value: value.into(),
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(with = "seconds")]
    pub time: Timestamp,
    pub machine: String,
    pub action: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One payload of an object: an archive or a live package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payload {
    pub kind: PayloadKind,
    pub version: u32,
    pub file: String,
    pub package_format: String,
    pub size: u64,
    pub checksum: Checksum,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<String>,
    pub machine: String,
    #[serde(with = "seconds")]
    pub created: Timestamp,
    /// When the remote copy was last checked. Absent where it does not
    /// apply, `null` where it applies and was never checked.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "verified")]
    pub verified: Option<Option<Timestamp>>,
    /// The archive lost a conflict and is kept. Absent means `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub superseded: bool,
    /// zstd version and parameters of a live package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub format: u32,
    pub id: ObjectId,
    /// The collection that owns the object.
    pub collection: ObjectId,
    /// A piece is never the collection root, so the path is not empty.
    #[serde(deserialize_with = "piece_path")]
    pub path: RelPath,
    pub name: Name,
    #[serde(with = "seconds")]
    pub created: Timestamp,
    pub created_source: CreatedSource,
    pub lifecycle: Lifecycle,
    pub payloads: Vec<Payload>,
    pub history: Vec<HistoryEntry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn piece_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RelPath, D::Error> {
    let path = RelPath::deserialize(deserializer)?;
    if path.is_root() {
        return Err(D::Error::custom("the path of a piece is not empty"));
    }
    Ok(path)
}

/// Times as RFC 3339 UTC text in whole seconds.
mod seconds {
    use super::*;

    pub fn serialize<S: Serializer>(time: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&whole_seconds(*time))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// `Option<Option<Timestamp>>` as a key that is absent, `null` or a time.
/// The outer option is the key's presence; a struct field skips it when it
/// is `None`.
mod verified {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<Option<Timestamp>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(Some(time)) => serializer.collect_str(&whole_seconds(*time)),
            _ => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<Timestamp>>, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?;
        let time = text
            .map(|text| text.parse::<Timestamp>().map_err(D::Error::custom))
            .transpose()?;
        Ok(Some(time))
    }
}

/// Errors of object metadata.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("the metadata has format {found}, which this version of antiq cannot write")]
    FormatTooNew { found: u32 },
}

impl HasKind for MetaError {
    fn kind(&self) -> ErrorKind {
        match self {
            MetaError::FormatTooNew { .. } => ErrorKind::FormatTooNew,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ObjectId;
    use crate::relpath::{Name, RelPath};
    use serde_json::{Value, json};

    const ARCHIVED: &str = concat!(
        r#"{"format":1,"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#,
        r#""collection":"01BX5ZZKBKACTAV9WEVGEMMVRZ","path":"projects/a%FF","#,
        r#""name":"a%FF","created":"2020-09-13T12:26:40Z","#,
        r#""created_source":"oldest-mtime","lifecycle":"archived","payloads":["#,
        r#"{"kind":"archive","version":1,"file":"0123456789abcdef-a.tar.zst","#,
        r#""package_format":"tar+zstd/1","size":1234,"#,
        r#""checksum":{"algorithm":"sha256","value":"abcdef"},"#,
        r#""listing":"0123456789abcdef.listing.zst","machine":"imperius","#,
        r#""created":"2020-09-13T12:26:41Z","verified":"2020-09-14T00:00:00Z","#,
        r#""superseded":true,"encoder":{"zstd":"1.5.7"}}],"#,
        r#""history":[{"time":"2020-09-13T12:26:41Z","machine":"imperius","#,
        r#""action":"archive"}]}"#
    );

    fn parse(text: &str) -> Metadata {
        serde_json::from_str(text).expect("the document is valid metadata")
    }

    fn live_document(payload_tail: &str) -> String {
        format!(
            concat!(
                r#"{{"format":1,"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#,
                r#""collection":"01BX5ZZKBKACTAV9WEVGEMMVRZ","path":"a","name":"a","#,
                r#""created":"2020-09-13T12:26:40Z","created_source":"birth","#,
                r#""lifecycle":"live","payloads":[{{"kind":"live","version":2,"#,
                r#""file":"f.tar.zst","package_format":"tar+zstd/1","size":5,"#,
                r#""checksum":{{"algorithm":"sha256","value":"00"}},"#,
                r#""machine":"imperius","created":"2020-09-13T12:26:40Z"{}}}],"#,
                r#""history":[]}}"#
            ),
            payload_tail
        )
    }

    #[test]
    fn a_document_is_written_back_byte_for_byte_in_the_specs_key_order() {
        assert_eq!(
            serde_json::to_string(&parse(ARCHIVED)).expect("serialises"),
            ARCHIVED
        );
    }

    #[test]
    fn the_fields_hold_what_the_document_says() {
        let metadata = parse(ARCHIVED);
        assert_eq!(metadata.format, 1);
        assert_eq!(metadata.id.to_string(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(
            metadata.collection.to_string(),
            "01BX5ZZKBKACTAV9WEVGEMMVRZ"
        );
        assert_eq!(metadata.path.as_bytes(), b"projects/a\xff");
        assert_eq!(metadata.name.as_bytes(), b"a\xff");
        assert_eq!(metadata.created.as_second(), 1_600_000_000);
        assert_eq!(metadata.created_source, CreatedSource::OldestMtime);
        assert_eq!(metadata.lifecycle, Lifecycle::Archived);
        let payload = &metadata.payloads[0];
        assert_eq!(payload.kind, PayloadKind::Archive);
        assert_eq!(payload.version, 1);
        assert_eq!(payload.size, 1234);
        assert_eq!(payload.checksum.algorithm, "sha256");
        assert_eq!(
            payload.listing.as_deref(),
            Some("0123456789abcdef.listing.zst")
        );
        assert!(payload.superseded);
        assert_eq!(metadata.history[0].action, "archive");
    }

    #[test]
    fn a_live_payload_leaves_out_the_keys_that_do_not_apply() {
        let text = live_document("");
        let metadata = parse(&text);
        let payload = &metadata.payloads[0];
        assert_eq!(payload.listing, None);
        assert_eq!(payload.verified, None);
        assert!(!payload.superseded);
        assert_eq!(payload.encoder, None);
        assert_eq!(serde_json::to_string(&metadata).expect("serialises"), text);
    }

    #[test]
    fn verified_is_absent_null_or_a_time() {
        let absent = parse(&live_document(""));
        assert_eq!(absent.payloads[0].verified, None);

        let never = parse(&live_document(r#","verified":null"#));
        assert_eq!(never.payloads[0].verified, Some(None));

        let at = parse(&live_document(r#","verified":"2020-09-14T00:00:00Z""#));
        assert_eq!(
            at.payloads[0]
                .verified
                .map(|time| time.map(|t| t.as_second())),
            Some(Some(1_600_041_600))
        );
    }

    #[test]
    fn each_verified_state_is_written_back_as_it_was_read() {
        for tail in [
            "",
            r#","verified":null"#,
            r#","verified":"2020-09-14T00:00:00Z""#,
        ] {
            let text = live_document(tail);
            assert_eq!(
                serde_json::to_string(&parse(&text)).expect("serialises"),
                text,
                "tail {tail:?}"
            );
        }
    }

    #[test]
    fn unknown_keys_survive_behind_the_known_ones_in_input_order() {
        let text = concat!(
            r#"{"zeta":1,"format":1,"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#,
            r#""collection":"01BX5ZZKBKACTAV9WEVGEMMVRZ","path":"a","name":"a","#,
            r#""created":"2020-09-13T12:26:40Z","created_source":"birth","#,
            r#""lifecycle":"live","payloads":[{"kind":"live","payload_extra":{"b":1,"a":2},"#,
            r#""version":2,"file":"f","package_format":"tar+zstd/1","size":5,"#,
            r#""checksum":{"value":"00","algorithm":"sha256","sum_extra":true},"#,
            r#""machine":"m","created":"2020-09-13T12:26:40Z"}],"#,
            r#""history":[{"time":"2020-09-13T12:26:40Z","by":"x","machine":"m","action":"a"}],"#,
            r#""alpha":[1]}"#
        );
        let expected = concat!(
            r#"{"format":1,"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#,
            r#""collection":"01BX5ZZKBKACTAV9WEVGEMMVRZ","path":"a","name":"a","#,
            r#""created":"2020-09-13T12:26:40Z","created_source":"birth","#,
            r#""lifecycle":"live","payloads":[{"kind":"live","version":2,"file":"f","#,
            r#""package_format":"tar+zstd/1","size":5,"#,
            r#""checksum":{"algorithm":"sha256","value":"00","sum_extra":true},"#,
            r#""machine":"m","created":"2020-09-13T12:26:40Z","payload_extra":{"b":1,"a":2}}],"#,
            r#""history":[{"time":"2020-09-13T12:26:40Z","machine":"m","action":"a","by":"x"}],"#,
            r#""zeta":1,"alpha":[1]}"#
        );
        assert_eq!(
            serde_json::to_string(&parse(text)).expect("serialises"),
            expected
        );
    }

    #[test]
    fn times_are_written_in_whole_seconds() {
        let text =
            live_document("").replace("2020-09-13T12:26:40Z", "2020-09-13T12:26:40.987654321Z");
        let written = serde_json::to_string(&parse(&text)).expect("serialises");
        assert!(!written.contains(".987"), "{written}");
        assert!(
            written.contains(r#""created":"2020-09-13T12:26:40Z""#),
            "{written}"
        );
    }

    #[test]
    fn all_five_sources_of_the_created_time_are_known() {
        let sources = [
            ("birth", CreatedSource::Birth),
            ("oldest-mtime", CreatedSource::OldestMtime),
            ("import", CreatedSource::Import),
            ("fork", CreatedSource::Fork),
            ("minted", CreatedSource::Minted),
        ];
        for (name, source) in sources {
            let value: CreatedSource = serde_json::from_value(json!(name)).expect("known source");
            assert_eq!(value, source);
            assert_eq!(
                serde_json::to_value(source).expect("serialises"),
                json!(name)
            );
        }
    }

    #[test]
    fn a_document_built_in_code_has_the_same_text() {
        let metadata = Metadata {
            format: FORMAT,
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV"
                .parse::<ObjectId>()
                .expect("valid"),
            collection: "01BX5ZZKBKACTAV9WEVGEMMVRZ"
                .parse::<ObjectId>()
                .expect("valid"),
            path: RelPath::parse("a").expect("valid"),
            name: Name::from_bytes(b"a".to_vec()).expect("valid"),
            created: "2020-09-13T12:26:40Z".parse().expect("valid"),
            created_source: CreatedSource::Birth,
            lifecycle: Lifecycle::Live,
            payloads: vec![Payload {
                kind: PayloadKind::Live,
                version: 2,
                file: "f.tar.zst".into(),
                package_format: "tar+zstd/1".into(),
                size: 5,
                checksum: Checksum::sha256("00"),
                listing: None,
                machine: "imperius".into(),
                created: "2020-09-13T12:26:40Z".parse().expect("valid"),
                verified: None,
                superseded: false,
                encoder: None,
                extra: serde_json::Map::new(),
            }],
            history: vec![],
            extra: serde_json::Map::new(),
        };
        assert_eq!(
            serde_json::to_string(&metadata).expect("serialises"),
            live_document("")
        );
    }

    #[test]
    fn a_piece_is_never_the_collection_root() {
        let text = live_document("").replace(r#""path":"a""#, r#""path":"""#);
        assert!(serde_json::from_str::<Metadata>(&text).is_err());
    }

    #[test]
    fn a_path_that_leaves_the_collection_is_refused() {
        let text = live_document("").replace(r#""path":"a""#, r#""path":"../x""#);
        assert!(serde_json::from_str::<Metadata>(&text).is_err());
    }

    #[test]
    fn extra_keys_are_plain_json_values() {
        let metadata = parse(ARCHIVED);
        assert_eq!(
            metadata.payloads[0]
                .encoder
                .as_ref()
                .map(|e| Value::Object(e.clone())),
            Some(json!({"zstd": "1.5.7"}))
        );
    }
}
