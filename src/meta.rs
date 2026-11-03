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

/// The name of a file in an object's directory on the remote and in the
/// local store. The metadata comes from the remote, and these names are
/// joined to a directory path to read and delete files, so a name must be
/// one plain component: not empty, without `/` or NUL, not `.` or `..`,
/// and not starting with `.`, which temporary names do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct FileName(String);

impl FileName {
    pub fn new(name: impl Into<String>) -> Result<FileName, MetaError> {
        let name = name.into();
        let reason = if name.is_empty() {
            Some("it is empty")
        } else if name.contains(['/', '\0']) {
            Some("it contains `/` or a NUL byte")
        } else if name.starts_with('.') {
            Some("it starts with a dot")
        } else {
            None
        };
        match reason {
            None => Ok(FileName(name)),
            Some(reason) => Err(MetaError::InvalidFileName { name, reason }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for FileName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl<'de> Deserialize<'de> for FileName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        FileName::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Whether `name` is the name of an archive or a live package in the
/// remote layout: 16 hex digits of the SHA-256, a dash, the piece's name
/// part (at most 64 bytes of `[A-Za-z0-9._-]` and `%XX` escapes) and
/// `.tar.zst`.
fn is_payload_file_name(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".tar.zst") else {
        return false;
    };
    let Some((digest, name_part)) = rest.split_once('-') else {
        return false;
    };
    is_digest(digest) && (1..=64).contains(&name_part.len()) && is_name_part(name_part.as_bytes())
}

/// Whether `name` is the name of a listing: 16 hex digits and
/// `.listing.zst`.
fn is_listing_file_name(name: &str) -> bool {
    name.strip_suffix(".listing.zst").is_some_and(is_digest)
}

fn is_digest(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_name_part(mut bytes: &[u8]) -> bool {
    while let Some((&first, rest)) = bytes.split_first() {
        bytes = match first {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => rest,
            b'%' => match rest {
                [high, low, tail @ ..] if is_upper_hex(*high) && is_upper_hex(*low) => tail,
                _ => return false,
            },
            _ => return false,
        };
    }
    true
}

fn is_upper_hex(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'A'..=b'F')
}

/// One payload of an object: an archive or a live package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payload {
    pub kind: PayloadKind,
    pub version: u32,
    pub file: FileName,
    pub package_format: String,
    pub size: u64,
    pub checksum: Checksum,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<FileName>,
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

/// Metadata is read through [`RawMetadata`], so the checks that need the
/// whole document cannot be skipped by constructing it differently: format
/// 1 documents name their files the way the remote layout does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawMetadata")]
pub struct Metadata {
    pub format: u32,
    pub id: ObjectId,
    /// The collection that owns the object.
    pub collection: ObjectId,
    /// A piece is never the collection root, so the path is not empty.
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

/// The fields of [`Metadata`] as they are read, before the whole document
/// is checked.
#[derive(Deserialize)]
struct RawMetadata {
    format: u32,
    id: ObjectId,
    collection: ObjectId,
    #[serde(deserialize_with = "piece_path")]
    path: RelPath,
    name: Name,
    #[serde(with = "seconds")]
    created: Timestamp,
    created_source: CreatedSource,
    lifecycle: Lifecycle,
    payloads: Vec<Payload>,
    history: Vec<HistoryEntry>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl TryFrom<RawMetadata> for Metadata {
    type Error = MetaError;

    fn try_from(raw: RawMetadata) -> Result<Metadata, MetaError> {
        if raw.format == FORMAT {
            for payload in &raw.payloads {
                let file = payload.file.as_str();
                if !is_payload_file_name(file) {
                    return Err(MetaError::UnexpectedFileName {
                        name: file.to_string(),
                        expected: "<16 hex digits>-<name part>.tar.zst",
                    });
                }
                let listing = payload.listing.as_ref().map(FileName::as_str);
                if let Some(listing) = listing.filter(|name| !is_listing_file_name(name)) {
                    return Err(MetaError::UnexpectedFileName {
                        name: listing.to_string(),
                        expected: "<16 hex digits>.listing.zst",
                    });
                }
            }
        }
        Ok(Metadata {
            format: raw.format,
            id: raw.id,
            collection: raw.collection,
            path: raw.path,
            name: raw.name,
            created: raw.created,
            created_source: raw.created_source,
            lifecycle: raw.lifecycle,
            payloads: raw.payloads,
            history: raw.history,
            extra: raw.extra,
        })
    }
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
    #[error("`{name}` is not a plain file name: {reason}")]
    InvalidFileName { name: String, reason: &'static str },
    #[error("`{name}` is not a file name of metadata format 1, which is {expected}")]
    UnexpectedFileName {
        name: String,
        expected: &'static str,
    },
}

impl HasKind for MetaError {
    fn kind(&self) -> ErrorKind {
        match self {
            MetaError::FormatTooNew { .. } => ErrorKind::FormatTooNew,
            MetaError::InvalidFileName { .. } | MetaError::UnexpectedFileName { .. } => {
                ErrorKind::Other
            }
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
                r#""file":"0123456789abcdef-f.tar.zst","package_format":"tar+zstd/1","size":5,"#,
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
            payload.listing.as_ref().map(FileName::as_str),
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
            r#""version":2,"file":"0123456789abcdef-f.tar.zst","package_format":"tar+zstd/1","size":5,"#,
            r#""checksum":{"value":"00","algorithm":"sha256","sum_extra":true},"#,
            r#""machine":"m","created":"2020-09-13T12:26:40Z"}],"#,
            r#""history":[{"time":"2020-09-13T12:26:40Z","by":"x","machine":"m","action":"a"}],"#,
            r#""alpha":[1]}"#
        );
        let expected = concat!(
            r#"{"format":1,"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#,
            r#""collection":"01BX5ZZKBKACTAV9WEVGEMMVRZ","path":"a","name":"a","#,
            r#""created":"2020-09-13T12:26:40Z","created_source":"birth","#,
            r#""lifecycle":"live","payloads":[{"kind":"live","version":2,"file":"0123456789abcdef-f.tar.zst","#,
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
                file: FileName::new("0123456789abcdef-f.tar.zst").expect("valid"),
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

    /// The live document with other values in `file` and `listing`.
    fn with_names(format: u32, file: &str, listing: Option<&str>) -> String {
        let mut text = live_document("");
        text = text.replace(
            r#""file":"0123456789abcdef-f.tar.zst""#,
            &format!(r#""file":{}"#, json!(file)),
        );
        text = text.replace(r#""format":1"#, &format!(r#""format":{format}"#));
        if let Some(listing) = listing {
            text = text.replace(
                r#""machine":"imperius""#,
                &format!(r#""listing":{},"machine":"imperius""#, json!(listing)),
            );
        }
        text
    }

    #[test]
    fn a_payload_file_that_is_not_one_plain_name_is_refused() {
        let bad = ["../x", "/x", "a/b", "", ".hidden", ".", "..", "a\0b"];
        for name in bad {
            for format in [1, 2] {
                let text = with_names(format, name, None);
                let result = serde_json::from_str::<Metadata>(&text);
                assert!(
                    result.is_err(),
                    "format {format}, file {name:?} was accepted"
                );
            }
        }
    }

    #[test]
    fn a_listing_that_is_not_one_plain_name_is_refused() {
        let file = "0123456789abcdef-f.tar.zst";
        for name in ["/x", "../x", "x/y", "", ".x.zst", "a\0b"] {
            let text = with_names(1, file, Some(name));
            assert!(
                serde_json::from_str::<Metadata>(&text).is_err(),
                "listing {name:?} was accepted"
            );
        }
    }

    #[test]
    fn format_1_accepts_exactly_the_names_of_the_storage_layout() {
        let name_part_64 = "n".repeat(64);
        let good_files = [
            "0123456789abcdef-a.tar.zst",
            "0123456789abcdef-photos%FF%25.tar.zst",
            "0123456789abcdef-a_b.c-d.tar.zst",
        ];
        for file in good_files {
            let text = with_names(1, file, Some("0123456789abcdef.listing.zst"));
            assert!(serde_json::from_str::<Metadata>(&text).is_ok(), "{file:?}");
        }
        let long_enough = format!("0123456789abcdef-{name_part_64}.tar.zst");
        assert!(serde_json::from_str::<Metadata>(&with_names(1, &long_enough, None)).is_ok());

        let name_part_65 = "n".repeat(65);
        let bad_files = [
            "f.tar.zst",
            "0123456789ABCDEF-a.tar.zst",
            "0123456789abcdef-.tar.zst",
            "0123456789abcdef-a.tar.xz",
            "0123456789abcdef-a b.tar.zst",
            "0123456789abcdef-a%ff.tar.zst",
            "0123456789abcdef-a%F.tar.zst",
            "0123456789abcdef-ä.tar.zst",
            "0123456789abcdef.tar.zst",
        ];
        for file in bad_files {
            let text = with_names(1, file, None);
            assert!(serde_json::from_str::<Metadata>(&text).is_err(), "{file:?}");
        }
        let too_long = format!("0123456789abcdef-{name_part_65}.tar.zst");
        assert!(serde_json::from_str::<Metadata>(&with_names(1, &too_long, None)).is_err());

        let file = "0123456789abcdef-a.tar.zst";
        for listing in [
            "f.listing.zst",
            "0123456789abcdef.zst",
            "0123456789abcdef.listing.xz",
        ] {
            let text = with_names(1, file, Some(listing));
            assert!(
                serde_json::from_str::<Metadata>(&text).is_err(),
                "{listing:?}"
            );
        }
    }

    #[test]
    fn a_refused_name_is_an_error_without_a_kind_of_its_own() {
        let plain: crate::error::Error = FileName::new("../x").expect_err("refused").into();
        assert_eq!(plain.kind(), crate::error::ErrorKind::Other);
        assert!(plain.to_string().contains("`../x`"), "{plain}");
        let layout = MetaError::UnexpectedFileName {
            name: "f".into(),
            expected: "the layout",
        };
        assert_eq!(
            crate::error::Error::from(layout).kind(),
            crate::error::ErrorKind::Other
        );
    }

    #[test]
    fn a_later_format_may_name_files_differently_but_still_plainly() {
        let text = with_names(2, "payload.bin", Some("payload.index"));
        let metadata = parse(&text);
        assert_eq!(metadata.format, 2);
        assert_eq!(metadata.payloads[0].file, "payload.bin");
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
