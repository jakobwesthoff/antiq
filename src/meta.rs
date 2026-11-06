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
pub(crate) fn is_payload_file_name(name: &str) -> bool {
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
pub(crate) fn is_listing_file_name(name: &str) -> bool {
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
            check_file_names(&raw.payloads)?;
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

/// The names of payloads and listings in a format 1 document are the ones
/// the remote layout gives them.
fn check_file_names(payloads: &[Payload]) -> Result<(), MetaError> {
    for payload in payloads {
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
    Ok(())
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

// =========================================================
// Reading and writing
// =========================================================

/// The action a history entry records, named after the commands that
/// change metadata (spec "Metadata changes").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAction {
    Archive,
    Reclaim,
    Unearth,
    Fork,
    Sync,
    Import,
    Resolve,
}

impl HistoryAction {
    pub const ALL: [HistoryAction; 7] = [
        HistoryAction::Archive,
        HistoryAction::Reclaim,
        HistoryAction::Unearth,
        HistoryAction::Fork,
        HistoryAction::Sync,
        HistoryAction::Import,
        HistoryAction::Resolve,
    ];

    /// The text stored in the `action` key.
    pub fn as_str(self) -> &'static str {
        match self {
            HistoryAction::Archive => "archive",
            HistoryAction::Reclaim => "reclaim",
            HistoryAction::Unearth => "unearth",
            HistoryAction::Fork => "fork",
            HistoryAction::Sync => "sync",
            HistoryAction::Import => "import",
            HistoryAction::Resolve => "resolve",
        }
    }
}

impl HistoryEntry {
    pub fn new(time: Timestamp, machine: impl Into<String>, action: HistoryAction) -> HistoryEntry {
        HistoryEntry {
            time: whole_seconds(time),
            machine: machine.into(),
            action: action.as_str().to_string(),
            extra: Map::new(),
        }
    }
}

/// The `format` of a metadata document, read without looking at anything
/// else in it, so that a document of a later format can still be told
/// apart from a broken one.
pub fn read_format(bytes: &[u8]) -> Result<u32, MetaError> {
    // Read into a map: a struct would also take a JSON array, which is no
    // document.
    let document: Map<String, Value> =
        serde_json::from_slice(bytes).map_err(|error| MetaError::Unreadable {
            format: None,
            reason: error.to_string(),
        })?;
    let format = document
        .get("format")
        .and_then(Value::as_u64)
        .and_then(|format| u32::try_from(format).ok())
        .ok_or_else(|| MetaError::Unreadable {
            format: None,
            reason: "`format` is missing or not a number".into(),
        })?;
    // Format 1 is the first one, so 0 names nothing a reader could rely on.
    if format == 0 {
        return Err(MetaError::Unreadable {
            format: None,
            reason: "format 0 does not exist".into(),
        });
    }
    Ok(format)
}

/// Reads a metadata document.
///
/// A document of a later format is returned as far as it fits the types
/// here; its additions sit in the `extra` maps. When it does not fit, the
/// error still carries the format, so callers can report "too new" rather
/// than "broken".
pub fn parse(bytes: &[u8]) -> Result<Metadata, MetaError> {
    let format = read_format(bytes)?;
    serde_json::from_slice(bytes).map_err(|error| MetaError::Unreadable {
        format: Some(format),
        reason: error.to_string(),
    })
}

/// The bytes of a document, in the form every reader accepts.
///
/// A document of a later format is never written, since its additions
/// might mean something this version does not know. The output is read
/// back before it is returned: a document that this reader would refuse
/// (a name outside the layout, a piece at the collection root, an `extra`
/// key that repeats a known one) must not reach the store or the remote,
/// where it would be an unreadable version for every machine.
pub fn to_bytes(metadata: &Metadata) -> Result<Vec<u8>, MetaError> {
    if metadata.format > FORMAT {
        return Err(MetaError::FormatTooNew {
            found: metadata.format,
        });
    }
    if metadata.format != FORMAT {
        return Err(MetaError::Unwritable {
            reason: format!("format {} does not exist", metadata.format),
        });
    }
    let bytes =
        serde_json::to_vec(metadata).expect("plain data with string keys always serialises");
    let read_back = parse(&bytes).map_err(|error| MetaError::Unwritable {
        reason: error.to_string(),
    })?;
    // A key in an `extra` map that has the name of a field is written once
    // when the field is left out (it is `None` or `false`) and reads back
    // as the field, which the reader cannot tell from the field itself.
    if extra_maps(&read_back) != extra_maps(metadata) {
        return Err(MetaError::Unwritable {
            reason: "an unknown key has the name of a known field".into(),
        });
    }
    Ok(bytes)
}

/// Every map of unknown keys in the document, in a fixed order.
fn extra_maps(metadata: &Metadata) -> Vec<&Map<String, Value>> {
    let mut maps = vec![&metadata.extra];
    for payload in &metadata.payloads {
        maps.push(&payload.extra);
        maps.push(&payload.checksum.extra);
    }
    maps.extend(metadata.history.iter().map(|entry| &entry.extra));
    maps
}

impl Metadata {
    /// Where the history entries that this document adds to `base` start.
    ///
    /// History only grows, so this document is based on `base` when `base`'s
    /// entries are its first ones. `None` means the histories diverged and
    /// the two documents conflict.
    pub fn history_added_over(&self, base: &Metadata) -> Option<usize> {
        let kept = base.history.len();
        (self.history.get(..kept) == Some(base.history.as_slice())).then_some(kept)
    }

    /// Gives the history entries from `first_new` on the time their version
    /// was committed. A change is recorded when a command runs, but its
    /// entry is read as the commit time (the remote-only tier measures from
    /// it), so the stamp is set before the document is uploaded.
    ///
    /// The order of a commit is: `history_added_over` gives `first_new`,
    /// this stamps the entries, `Store::write_pending` records the stamped
    /// document, the bytes of `meta.pending` are uploaded, and
    /// `Store::promote_pending` renames it, so that the local version and
    /// the remote's hold the same bytes.
    pub fn stamp_committed(&mut self, first_new: usize, commit_time: Timestamp) {
        let time = whole_seconds(commit_time);
        for entry in self.history.iter_mut().skip(first_new) {
            entry.time = time;
        }
    }
}

/// Errors of object metadata.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("the metadata has format {found}, which this version of antiq cannot write")]
    FormatTooNew { found: u32 },
    #[error("the metadata cannot be read: {reason}")]
    Unreadable {
        /// The `format` found by the first pass, when there was one.
        format: Option<u32>,
        reason: String,
    },
    #[error("the metadata cannot be written: {reason}")]
    Unwritable { reason: String },
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
            MetaError::Unreadable {
                format: Some(format),
                ..
            } if *format > FORMAT => ErrorKind::FormatTooNew,
            MetaError::Unreadable { .. }
            | MetaError::Unwritable { .. }
            | MetaError::InvalidFileName { .. }
            | MetaError::UnexpectedFileName { .. } => ErrorKind::Other,
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

    /// A document with unknown keys at every level, out of order.
    const UNKNOWN_KEYS: &str = concat!(
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

    /// `UNKNOWN_KEYS` as it is written: known keys in the spec order, unknown
    /// ones behind them in input order.
    const UNKNOWN_KEYS_WRITTEN: &str = concat!(
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

    #[test]
    fn unknown_keys_survive_behind_the_known_ones_in_input_order() {
        assert_eq!(
            serde_json::to_string(&parse(UNKNOWN_KEYS)).expect("serialises"),
            UNKNOWN_KEYS_WRITTEN
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

    // =====================================================
    // Reading and writing
    // =====================================================

    fn parse_result(text: &str) -> Result<Metadata, MetaError> {
        super::parse(text.as_bytes())
    }

    #[test]
    fn the_format_is_read_without_looking_at_anything_else() {
        assert_eq!(
            read_format(br#"{"format":1,"payloads":"junk"}"#).ok(),
            Some(1)
        );
        assert_eq!(read_format(br#"{"format":7}"#).ok(), Some(7));
    }

    #[test]
    fn a_missing_format_is_unreadable() {
        let error = read_format(br#"{"id":"x"}"#).expect_err("no format");
        assert!(
            matches!(error, MetaError::Unreadable { format: None, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_format_that_is_not_a_number_is_unreadable() {
        let error = read_format(br#"{"format":"1"}"#).expect_err("a string");
        assert!(
            matches!(error, MetaError::Unreadable { format: None, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn format_zero_does_not_exist() {
        let error = read_format(br#"{"format":0}"#).expect_err("format 0");
        assert!(
            matches!(error, MetaError::Unreadable { format: None, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_document_is_written_with_the_known_keys_in_the_specs_order() {
        assert_eq!(
            to_bytes(&parse(ARCHIVED)).expect("writes"),
            ARCHIVED.as_bytes()
        );
        assert_eq!(
            to_bytes(&parse(UNKNOWN_KEYS)).expect("writes"),
            UNKNOWN_KEYS_WRITTEN.as_bytes()
        );
    }

    #[test]
    fn unknown_keys_survive_a_read_and_a_write_at_every_level() {
        let written = to_bytes(&parse(UNKNOWN_KEYS)).expect("writes");
        let value: Value = serde_json::from_slice(&written).expect("json");
        assert_eq!(value["zeta"], json!(1));
        assert_eq!(value["alpha"], json!([1]));
        assert_eq!(
            value["payloads"][0]["payload_extra"],
            json!({"b": 1, "a": 2})
        );
        assert_eq!(value["payloads"][0]["checksum"]["sum_extra"], json!(true));
        assert_eq!(value["history"][0]["by"], json!("x"));
    }

    /// A document of format 2 that this version can read as far as it
    /// understands it: every key of format 1, two additions, and file names
    /// that format 1 would refuse.
    fn format_2_document() -> String {
        let text = with_names(2, "payload.bin", Some("payload.index"));
        text.replace(r#""history":[]}"#, r#""history":[],"future":true}"#)
            .replace(
                r#""machine":"imperius""#,
                r#""machine":"imperius","payload_future":1"#,
            )
    }

    #[test]
    fn a_later_format_is_read_with_its_additions_and_refused_on_write() {
        let metadata = parse(&format_2_document());
        assert_eq!(metadata.format, 2);
        assert_eq!(metadata.extra.get("future"), Some(&json!(true)));
        assert_eq!(
            metadata.payloads[0].extra.get("payload_future"),
            Some(&json!(1))
        );

        let error = to_bytes(&metadata).expect_err("format 2 is read-only");
        assert!(
            matches!(error, MetaError::FormatTooNew { found: 2 }),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
    }

    #[test]
    fn a_later_format_that_does_not_fit_is_unreadable_but_known_to_be_later() {
        let text = format_2_document().replace(r#""lifecycle":"live","#, "");
        let error = parse_result(&text).expect_err("lifecycle is missing");
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
        assert!(format!("{error:?}").contains("format: Some(2)"));
    }

    #[test]
    fn a_broken_document_of_format_1_is_unreadable_and_not_too_new() {
        let text = live_document("").replace(r#""lifecycle":"live","#, "");
        let error = parse_result(&text).expect_err("lifecycle is missing");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(format!("{error:?}").contains("format: Some(1)"));
    }

    fn assert_unwritable(metadata: &Metadata) {
        let result = to_bytes(metadata);
        assert!(
            matches!(result, Err(MetaError::Unwritable { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_payload_name_the_reader_would_refuse_is_not_written() {
        let mut metadata = parse(&live_document(""));
        metadata.payloads[0].file = FileName::new("payload.tar.zst").expect("a plain name");
        assert_unwritable(&metadata);
    }

    #[test]
    fn a_listing_name_the_reader_would_refuse_is_not_written() {
        let mut metadata = parse(&live_document(""));
        metadata.payloads[0].listing = Some(FileName::new("listing.zst").expect("a plain name"));
        assert_unwritable(&metadata);
    }

    #[test]
    fn a_piece_at_the_collection_root_is_not_written() {
        let mut metadata = parse(&live_document(""));
        metadata.path = RelPath::root();
        assert_unwritable(&metadata);
    }

    #[test]
    fn an_extra_key_that_repeats_a_known_key_is_not_written() {
        let mut metadata = parse(&live_document(""));
        metadata.extra.insert("format".into(), json!(7));
        assert_unwritable(&metadata);
    }

    #[test]
    fn format_zero_is_not_written() {
        let mut metadata = parse(&live_document(""));
        metadata.format = 0;
        assert_unwritable(&metadata);
    }

    #[test]
    fn an_extra_key_named_after_an_absent_payload_field_is_not_written() {
        let mut metadata = parse(&live_document(""));
        metadata.payloads[0]
            .extra
            .insert("superseded".into(), json!(true));
        assert_unwritable(&metadata);
    }

    #[test]
    fn an_extra_key_named_after_the_encoder_field_is_not_written() {
        let mut metadata = parse(&live_document(""));
        metadata.payloads[0]
            .extra
            .insert("encoder".into(), json!({}));
        assert_unwritable(&metadata);
    }

    #[test]
    fn a_json_array_is_not_a_document_whose_format_can_be_read() {
        let error = read_format(b"[7]").expect_err("an array has no keys");
        assert!(
            matches!(error, MetaError::Unreadable { format: None, .. }),
            "{error:?}"
        );
    }

    fn with_history(actions: &[&str]) -> Metadata {
        let mut metadata = parse(&live_document(""));
        metadata.history = actions
            .iter()
            .enumerate()
            .map(|(index, action)| HistoryEntry {
                time: Timestamp::from_second(1_600_000_000 + index as i64).expect("in range"),
                machine: "imperius".into(),
                action: (*action).into(),
                extra: Map::new(),
            })
            .collect();
        metadata
    }

    #[test]
    fn the_history_a_document_adds_starts_after_the_base() {
        let base = with_history(&["archive", "sync"]);
        let next = with_history(&["archive", "sync", "unearth", "sync"]);
        assert_eq!(next.history_added_over(&base), Some(2));
        assert_eq!(base.history_added_over(&base), Some(2));
    }

    #[test]
    fn a_base_that_is_not_a_prefix_of_the_history_is_a_conflict() {
        let base = with_history(&["archive", "sync"]);
        let diverged = with_history(&["archive", "unearth"]);
        assert_eq!(diverged.history_added_over(&base), None);
        let shorter = with_history(&["archive"]);
        assert_eq!(shorter.history_added_over(&base), None);
    }

    #[test]
    fn the_entries_a_version_adds_get_its_commit_time_in_whole_seconds() {
        let mut metadata = with_history(&["archive", "sync", "unearth"]);
        let commit: Timestamp = "2021-01-02T03:04:05.678Z".parse().expect("valid");
        metadata.stamp_committed(1, commit);
        let times: Vec<i64> = metadata
            .history
            .iter()
            .map(|entry| entry.time.as_second())
            .collect();
        let committed = commit.as_second();
        assert_eq!(times, vec![1_600_000_000, committed, committed]);
        assert_eq!(metadata.history[1].time.subsec_nanosecond(), 0);
    }

    #[test]
    fn stamping_past_the_end_changes_nothing() {
        let mut metadata = with_history(&["archive"]);
        let before = metadata.clone();
        metadata.stamp_committed(1, "2021-01-02T03:04:05Z".parse().expect("valid"));
        assert_eq!(metadata, before);
    }

    #[test]
    fn a_new_history_entry_carries_whole_seconds_and_the_action_name() {
        let entry = HistoryEntry::new(
            "2021-01-02T03:04:05.678Z".parse().expect("valid"),
            "imperius",
            HistoryAction::Reclaim,
        );
        assert_eq!(entry.action, "reclaim");
        assert_eq!(entry.time.subsec_nanosecond(), 0);
    }

    /// The names in the first column of the table that spec "Metadata"
    /// holds for the history actions.
    fn documented_history_actions() -> Vec<String> {
        let spec = include_str!("../docs/specs/spec-v1.md");
        let section = spec
            .split("\n### Metadata\n")
            .nth(1)
            .and_then(|rest| rest.split("\n### ").next())
            .expect("the spec has a Metadata section");
        section
            .lines()
            .skip_while(|line| !line.starts_with("| Action "))
            .skip(2)
            .take_while(|line| line.starts_with('|'))
            .filter_map(|line| line.split('`').nth(1))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_spec_lists_exactly_the_history_actions_the_code_writes() {
        let mut documented = documented_history_actions();
        documented.sort();
        let mut written: Vec<String> = HistoryAction::ALL
            .iter()
            .map(|action| action.as_str().to_string())
            .collect();
        written.sort();
        assert_eq!(documented, written);
    }
}
