// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tracking entries: what this machine knows about each folder, kept in
//! `.antiq/tracking/<ulid>.json`.
//!
//! An entry never leaves the machine. Keys are written in the order of
//! the fields; optional keys, empty lists and `false` flags are left out.

use crate::clock::whole_seconds;
use crate::id::ObjectId;
use crate::meta::{FileName, PayloadKind};
use crate::relpath::RelPath;
use jiff::Timestamp;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Errors of tracking entries.
#[derive(Debug, thiserror::Error)]
pub enum TrackingError {}

impl crate::error::HasKind for TrackingError {}

/// What the version taken out reads while its change is not committed.
const PENDING: &str = "pending";

/// The metadata version a folder was taken out at: a committed number,
/// or `pending` while the change that created it is not committed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionTakenOut {
    Committed(u32),
    Pending,
}

impl Serialize for VersionTakenOut {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            VersionTakenOut::Committed(number) => serializer.serialize_u32(*number),
            VersionTakenOut::Pending => serializer.serialize_str(PENDING),
        }
    }
}

impl<'de> Deserialize<'de> for VersionTakenOut {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u32),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Number(number) => Ok(VersionTakenOut::Committed(number)),
            Raw::Text(text) if text == PENDING => Ok(VersionTakenOut::Pending),
            Raw::Text(text) => Err(D::Error::custom(format!(
                "the version taken out is a number or `{PENDING}`, not `{text}`"
            ))),
        }
    }
}

/// Where a folder is in its life on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    Unearthing,
    Live,
    Removing,
    Gone,
}

/// A payload this machine holds. The file name identifies it: two
/// machines can both make archive version 4 with different content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPayload {
    pub kind: PayloadKind,
    pub version: u32,
    pub file: FileName,
}

/// The decisions a piece can wait on (spec "Decisions").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    VersionConflict,
    ReplacingWithLoss,
    DivergedFolder,
    SupersededArchive,
}

/// The answers of `resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Answer {
    Mine,
    Theirs,
    Fork,
    Replace,
    Skip,
    DropSuperseded,
}

impl DecisionKind {
    /// The answers that fit this decision, in the order of the spec's
    /// decision table.
    pub fn options(self) -> &'static [Answer] {
        match self {
            DecisionKind::VersionConflict => &[Answer::Mine, Answer::Theirs, Answer::Fork],
            DecisionKind::ReplacingWithLoss => &[Answer::Replace, Answer::Fork, Answer::Skip],
            DecisionKind::DivergedFolder => &[Answer::Theirs, Answer::Mine, Answer::Fork],
            DecisionKind::SupersededArchive => &[Answer::DropSuperseded],
        }
    }
}

/// A decision a run could not ask for and left to `resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDecision {
    pub kind: DecisionKind,
}

impl PendingDecision {
    pub fn options(&self) -> &'static [Answer] {
        self.kind.options()
    }
}

/// What this machine keeps about one folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackingEntry {
    #[serde(deserialize_with = "piece_path")]
    pub path: RelPath,
    pub id: ObjectId,
    pub version_taken_out: VersionTakenOut,
    /// The archive payload version the folder was unearthed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unearthed_payload_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "seconds_opt")]
    pub unearthed: Option<Timestamp>,
    pub state: EntryState,
    /// The archive payload whose listing the folder is compared with
    /// while the state is `removing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removing_payload: Option<FileName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_payloads: Vec<LocalPayload>,
    /// Metadata versions this machine committed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_versions: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_decisions: Vec<PendingDecision>,
    /// When the entry was last written.
    #[serde(with = "seconds")]
    pub written: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "seconds_opt")]
    pub stale_backup_since: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub missing_on_remote: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub diverged: bool,
}

impl TrackingEntry {
    pub fn new(
        id: ObjectId,
        path: RelPath,
        state: EntryState,
        version_taken_out: VersionTakenOut,
        now: Timestamp,
    ) -> TrackingEntry {
        TrackingEntry {
            path,
            id,
            version_taken_out,
            unearthed_payload_version: None,
            unearthed: None,
            state,
            removing_payload: None,
            local_payloads: Vec::new(),
            committed_versions: Vec::new(),
            pending_decisions: Vec::new(),
            written: now,
            stale_backup_since: None,
            missing_on_remote: false,
            diverged: false,
        }
    }
}

impl TrackingEntry {
    /// Records a decision as pending. A decision kind is recorded once. A
    /// diverged folder is diverged, so that decision sets the flag.
    pub fn decide(&mut self, kind: DecisionKind) {
        if !self.pending_decisions.iter().any(|d| d.kind == kind) {
            self.pending_decisions.push(PendingDecision { kind });
        }
        if kind == DecisionKind::DivergedFolder {
            self.diverged = true;
        }
    }

    /// Removes the pending decision of this kind. The answer's own step
    /// settles what the decision was about, so the `diverged` flag stays.
    pub fn resolve_decision(&mut self, kind: DecisionKind) {
        self.pending_decisions.retain(|d| d.kind != kind);
    }

    /// Sets or clears the `diverged` flag. Clearing it also drops a
    /// pending decision about the divergence, which has nothing left to
    /// decide.
    pub fn set_diverged(&mut self, diverged: bool) {
        self.diverged = diverged;
        if !diverged {
            self.resolve_decision(DecisionKind::DivergedFolder);
        }
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

/// `Option<Timestamp>` in the same form; a struct field skips `None`.
mod seconds_opt {
    use super::*;

    pub fn serialize<S: Serializer>(
        time: &Option<Timestamp>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match time {
            Some(time) => super::seconds::serialize(time, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Timestamp>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| text.parse().map_err(D::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const NOW: &str = "2020-09-13T12:26:40Z";

    fn entry(state: EntryState, version: VersionTakenOut) -> TrackingEntry {
        TrackingEntry::new(
            ID.parse().expect("a valid ID"),
            RelPath::parse("projects/a").expect("a valid path"),
            state,
            version,
            NOW.parse().expect("a valid time"),
        )
    }

    fn archive(version: u32, file: &str) -> LocalPayload {
        LocalPayload {
            kind: PayloadKind::Archive,
            version,
            file: FileName::new(file).expect("a plain file name"),
        }
    }

    fn text(entry: &TrackingEntry) -> String {
        serde_json::to_string(entry).expect("an entry serialises")
    }

    fn parse(text: &str) -> Result<TrackingEntry, serde_json::Error> {
        serde_json::from_str(text)
    }

    #[test]
    fn a_new_entry_is_written_in_the_field_order() {
        let entry = entry(EntryState::Live, VersionTakenOut::Pending);
        assert_eq!(
            text(&entry),
            concat!(
                r#"{"path":"projects/a","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#,
                r#""version_taken_out":"pending","state":"live","#,
                r#""written":"2020-09-13T12:26:40Z"}"#
            )
        );
    }

    #[test]
    fn pending_round_trips() {
        let entry = entry(EntryState::Live, VersionTakenOut::Pending);
        assert_eq!(parse(&text(&entry)).expect("parses"), entry);
    }

    #[test]
    fn a_committed_number_round_trips() {
        let entry = entry(EntryState::Gone, VersionTakenOut::Committed(7));
        assert!(text(&entry).contains(r#""version_taken_out":7"#));
        assert_eq!(parse(&text(&entry)).expect("parses"), entry);
    }

    #[test]
    fn other_text_as_version_is_refused() {
        let mut value = json!(entry(EntryState::Live, VersionTakenOut::Pending));
        value["version_taken_out"] = json!("soon");
        assert!(parse(&value.to_string()).is_err());
    }

    #[test]
    fn a_negative_version_is_refused() {
        let mut value = json!(entry(EntryState::Live, VersionTakenOut::Pending));
        value["version_taken_out"] = json!(-1);
        assert!(parse(&value.to_string()).is_err());
    }

    #[test]
    fn every_field_round_trips() {
        let mut entry = entry(EntryState::Removing, VersionTakenOut::Committed(3));
        entry.unearthed_payload_version = Some(2);
        entry.unearthed = Some("2020-09-10T00:00:00Z".parse().expect("a time"));
        entry.removing_payload = Some(FileName::new("0123456789abcdef-a.tar.zst").expect("a name"));
        entry.local_payloads = vec![archive(2, "0123456789abcdef-a.tar.zst")];
        entry.committed_versions = vec![1, 3];
        entry.pending_decisions = vec![PendingDecision {
            kind: DecisionKind::ReplacingWithLoss,
        }];
        entry.stale_backup_since = Some("2020-09-11T00:00:00Z".parse().expect("a time"));
        entry.missing_on_remote = true;
        entry.diverged = true;
        assert_eq!(parse(&text(&entry)).expect("parses"), entry);
    }

    #[test]
    fn two_payloads_of_one_version_differ_by_file() {
        let mut entry = entry(EntryState::Gone, VersionTakenOut::Committed(2));
        entry.local_payloads = vec![
            archive(4, "0123456789abcdef-a.tar.zst"),
            archive(4, "fedcba9876543210-a.tar.zst"),
        ];
        let back = parse(&text(&entry)).expect("parses");
        assert_eq!(back.local_payloads, entry.local_payloads);
    }

    #[test]
    fn a_removing_payload_round_trips() {
        let mut entry = entry(EntryState::Removing, VersionTakenOut::Pending);
        entry.removing_payload = Some(FileName::new("0123456789abcdef-a.tar.zst").expect("a name"));
        assert!(text(&entry).contains(r#""removing_payload":"0123456789abcdef-a.tar.zst""#));
        assert_eq!(parse(&text(&entry)).expect("parses"), entry);
    }

    #[test]
    fn times_are_written_in_whole_seconds() {
        let mut entry = entry(EntryState::Live, VersionTakenOut::Pending);
        entry.written = "2020-09-13T12:26:40.987654Z".parse().expect("a time");
        assert!(text(&entry).contains(r#""written":"2020-09-13T12:26:40Z""#));
    }

    #[test]
    fn the_root_is_not_a_piece_path() {
        let mut value = json!(entry(EntryState::Live, VersionTakenOut::Pending));
        value["path"] = json!("");
        assert!(parse(&value.to_string()).is_err());
    }

    fn answers(kind: DecisionKind) -> Vec<Answer> {
        kind.options().to_vec()
    }

    #[test]
    fn a_version_conflict_lists_its_options_from_the_spec_table() {
        assert_eq!(
            answers(DecisionKind::VersionConflict),
            [Answer::Mine, Answer::Theirs, Answer::Fork]
        );
    }

    #[test]
    fn replacing_with_loss_lists_its_options_from_the_spec_table() {
        assert_eq!(
            answers(DecisionKind::ReplacingWithLoss),
            [Answer::Replace, Answer::Fork, Answer::Skip]
        );
    }

    #[test]
    fn a_diverged_folder_lists_its_options_from_the_spec_table() {
        assert_eq!(
            answers(DecisionKind::DivergedFolder),
            [Answer::Theirs, Answer::Mine, Answer::Fork]
        );
    }

    #[test]
    fn a_superseded_archive_lists_its_options_from_the_spec_table() {
        assert_eq!(
            answers(DecisionKind::SupersededArchive),
            [Answer::DropSuperseded]
        );
    }

    #[test]
    fn a_pending_decision_lists_the_options_of_its_kind() {
        let decision = PendingDecision {
            kind: DecisionKind::SupersededArchive,
        };
        assert_eq!(decision.options(), [Answer::DropSuperseded]);
    }

    #[test]
    fn a_missing_time_serialises_as_null_where_the_key_is_not_skipped() {
        let value = seconds_opt::serialize(&None, serde_json::value::Serializer)
            .expect("a missing time serialises");
        assert_eq!(value, serde_json::Value::Null);
    }

    fn has(entry: &TrackingEntry, kind: DecisionKind) -> bool {
        entry.pending_decisions.iter().any(|d| d.kind == kind)
    }

    #[test]
    fn a_decision_kind_appears_once() {
        let mut entry = entry(EntryState::Live, VersionTakenOut::Pending);
        entry.decide(DecisionKind::VersionConflict);
        entry.decide(DecisionKind::VersionConflict);
        assert_eq!(entry.pending_decisions.len(), 1);
    }

    #[test]
    fn deciding_on_divergence_sets_the_flag() {
        let mut entry = entry(EntryState::Live, VersionTakenOut::Pending);
        entry.decide(DecisionKind::DivergedFolder);
        assert!(entry.diverged);
    }

    #[test]
    fn clearing_divergence_clears_its_decision() {
        let mut entry = entry(EntryState::Live, VersionTakenOut::Pending);
        entry.decide(DecisionKind::DivergedFolder);
        entry.decide(DecisionKind::VersionConflict);
        entry.set_diverged(false);
        assert!(!entry.diverged);
        assert!(!has(&entry, DecisionKind::DivergedFolder));
        assert!(has(&entry, DecisionKind::VersionConflict));
    }

    #[test]
    fn resolving_the_decision_keeps_the_flag() {
        let mut entry = entry(EntryState::Live, VersionTakenOut::Pending);
        entry.decide(DecisionKind::DivergedFolder);
        entry.resolve_decision(DecisionKind::DivergedFolder);
        assert!(entry.diverged);
        assert!(entry.pending_decisions.is_empty());
    }

    #[test]
    fn the_flag_can_be_set_without_a_decision() {
        let mut entry = entry(EntryState::Live, VersionTakenOut::Pending);
        entry.set_diverged(true);
        assert!(entry.diverged);
        assert!(entry.pending_decisions.is_empty());
    }
}
