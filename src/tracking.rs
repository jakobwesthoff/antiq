// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tracking entries: what this machine knows about each folder, kept in
//! `.antiq/tracking/<ulid>.json`.
//!
//! An entry never leaves the machine. Keys are written in the order of
//! the fields; optional keys, empty lists and `false` flags are left out.

use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::atomic::{flush_dir, write_atomic};
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::meta::{FileName, PayloadKind};
use crate::relpath::RelPath;
use jiff::Timestamp;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Errors of tracking entries.
#[derive(Debug, thiserror::Error)]
pub enum TrackingError {
    #[error(transparent)]
    Fsx(#[from] FsxError),
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error("the tracking entry {path} cannot be read: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("the tracking entry {path} is for {found}, not for the ID in its name")]
    IdMismatch { path: String, found: ObjectId },
}

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

/// The entries of a collection that could be read, and the files that
/// could not.
#[derive(Debug)]
pub struct TrackingSet {
    /// Sorted by ID.
    pub entries: Vec<TrackingEntry>,
    pub damaged: Vec<Damaged>,
}

/// A tracking file that is named like an entry and cannot be used. Its
/// folder is unknown, so callers keep from minting a new ID at the path of
/// this ID's metadata.
#[derive(Debug)]
pub struct Damaged {
    pub id: ObjectId,
    pub path: PathBuf,
    pub error: TrackingError,
}

/// The entry of `id`, `None` when this machine has none.
pub fn read(collection: &Collection, id: ObjectId) -> Result<Option<TrackingEntry>, TrackingError> {
    read_file(&collection.tracking_path(id), id)
}

/// Every entry, reading without the run lock.
///
/// A file named like an entry that cannot be used lands in
/// [`TrackingSet::damaged`] instead of failing the call: one bad file
/// would otherwise stop every command, `reconcile` included. Only an
/// unreadable directory fails.
pub fn read_all(collection: &Collection) -> Result<TrackingSet, TrackingError> {
    let dir = collection.tracking_dir();
    let listing = match fs::read_dir(&dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TrackingSet {
                entries: Vec::new(),
                damaged: Vec::new(),
            });
        }
        Err(error) => return Err(io_error("list", &dir)(error)),
    };
    let mut ids = Vec::new();
    for item in listing {
        let item = item.map_err(io_error("list", &dir))?;
        if let Some(id) = entry_id(&item.file_name()) {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(load(&dir, &ids))
}

/// The ID of a directory item that is an entry. Only the name that
/// `Collection::tracking_path` gives counts: manifests, the temp files of
/// an interrupted write and lower-case spellings (which `read` never
/// opens) are not entries.
fn entry_id(name: &OsStr) -> Option<ObjectId> {
    let name = name.to_str()?;
    let id: ObjectId = name.strip_suffix(".json")?.parse().ok()?;
    (name == format!("{id}.json")).then_some(id)
}

/// Reads the entries of `ids` below `dir`. An entry that vanishes before
/// it is opened counts as absent: a run may remove it while a read-only
/// command lists the directory.
fn load(dir: &Path, ids: &[ObjectId]) -> TrackingSet {
    let mut set = TrackingSet {
        entries: Vec::new(),
        damaged: Vec::new(),
    };
    for &id in ids {
        let path = dir.join(format!("{id}.json"));
        match read_file(&path, id) {
            Ok(Some(entry)) => set.entries.push(entry),
            Ok(None) => {}
            Err(error) => set.damaged.push(Damaged { id, path, error }),
        }
    }
    set
}

fn read_file(path: &Path, id: ObjectId) -> Result<Option<TrackingEntry>, TrackingError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read", path)(error)),
    };
    let entry: TrackingEntry =
        serde_json::from_slice(&bytes).map_err(|source| TrackingError::Parse {
            path: path.display().to_string(),
            source,
        })?;
    if entry.id != id {
        return Err(TrackingError::IdMismatch {
            path: path.display().to_string(),
            found: entry.id,
        });
    }
    Ok(Some(entry))
}

/// Stamps `entry.written` with the time of this write and stores the
/// entry. The stamp reaches `entry` only when the write succeeded.
///
/// The file is replaced atomically: a reader, or a run that starts after
/// a crash, sees the old content or the new. An error from the final
/// directory flush can mean the new bytes are in place with their
/// durability uncertain, so the caller treats any error as a failed write.
///
/// The spec fixes no order among the files of one step, so the commands
/// that write entries keep this one, which the start-of-run repairs rely
/// on: a new ID's metadata before its entry; archive: payload, listing,
/// metadata, entry `pending`, then `removing` with its payload, the
/// rename, then `gone`; unearth: `unearthing`, the rename, `live`, and
/// only then the metadata change; fork: the old ID's archived metadata,
/// the old entry `gone` with its local payloads, then the new ID's
/// metadata and entry.
///
/// The `&RunLock` is the proof that the run lock is held. `RunLock` cannot
/// be built outside `lock::acquire` (see its own doctest), so a call that
/// puts anything else in that place does not compile:
///
/// ```compile_fail,E0308
/// use antiq::clock::SystemClock;
/// use antiq::collection::Collection;
/// use antiq::tracking::{TrackingEntry, write};
///
/// fn store(collection: &Collection, entry: &mut TrackingEntry) {
///     // A unit value is not a `RunLock`: the run lock is not held.
///     let _ = write(collection, &(), &SystemClock, entry);
/// }
/// ```
pub fn write(
    collection: &Collection,
    _lock: &RunLock,
    clock: &dyn Clock,
    entry: &mut TrackingEntry,
) -> Result<(), TrackingError> {
    let mut stored = entry.clone();
    stored.written = whole_seconds(clock.now());
    let mut bytes = serde_json::to_vec(&stored).expect("an entry is plain data and serialises");
    bytes.push(b'\n');
    write_atomic(&collection.tracking_path(entry.id), &bytes)?;
    entry.written = stored.written;
    Ok(())
}

/// Removes the entry of `id`. An entry that is not there is fine, and the
/// directory is flushed either way, so a repeat after a crash that lost
/// the flush still makes the removal durable.
///
/// The live manifest of the same ID is not touched; it is a file of
/// `pack::manifest`.
pub fn remove(collection: &Collection, _lock: &RunLock, id: ObjectId) -> Result<(), TrackingError> {
    let path = collection.tracking_path(id);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("remove", &path)(error)),
    }
    flush_dir(&collection.tracking_dir())?;
    Ok(())
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> TrackingError {
    let path = path.display().to_string();
    move |source| TrackingError::Io {
        action,
        path,
        source,
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
    use crate::clock::FixedClock;
    use crate::lock;
    use crate::testing::tree::TempCollection;
    use jiff::SignedDuration;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

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

    // =====================================================================
    // Reading and writing
    // =====================================================================

    const SECOND_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
    const THIRD_ID: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    const WRITE_TIME: &str = "2021-01-01T00:00:00Z";

    /// A collection with the run lock held and a clock standing at
    /// [`WRITE_TIME`].
    struct Fixture {
        _temp: TempCollection,
        collection: Collection,
        clock: FixedClock,
        lock: RunLock,
    }

    fn fixture() -> Fixture {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        let clock = FixedClock::new(WRITE_TIME.parse().expect("a time"));
        let lock = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("the lock");
        Fixture {
            _temp: temp,
            collection,
            clock,
            lock,
        }
    }

    fn entry_of(id: &str) -> TrackingEntry {
        let mut entry = entry(EntryState::Live, VersionTakenOut::Pending);
        entry.id = id.parse().expect("a valid ID");
        entry
    }

    fn store(fixture: &Fixture, entry: &mut TrackingEntry) {
        write(&fixture.collection, &fixture.lock, &fixture.clock, entry).expect("the write works");
    }

    fn file_of(fixture: &Fixture, id: &str) -> PathBuf {
        fixture.collection.tracking_dir().join(format!("{id}.json"))
    }

    #[test]
    fn write_stamps_the_time_of_each_write() {
        let fixture = fixture();
        let mut entry = entry_of(ID);
        store(&fixture, &mut entry);
        let first = read(&fixture.collection, entry.id)
            .expect("reads")
            .expect("is there");
        fixture.clock.advance(SignedDuration::from_secs(60));
        store(&fixture, &mut entry);
        let second = read(&fixture.collection, entry.id)
            .expect("reads")
            .expect("is there");
        assert_eq!(
            first.written,
            WRITE_TIME.parse::<Timestamp>().expect("a time")
        );
        assert_eq!(
            second.written,
            "2021-01-01T00:01:00Z".parse::<Timestamp>().expect("a time")
        );
        assert_eq!(entry.written, second.written);
    }

    #[test]
    fn a_failed_write_keeps_the_entry_in_memory_unstamped() {
        let fixture = fixture();
        fs::remove_dir(fixture.collection.tracking_dir()).expect("the directory is empty");
        let mut entry = entry_of(ID);
        let before = entry.written;
        let result = write(
            &fixture.collection,
            &fixture.lock,
            &fixture.clock,
            &mut entry,
        );
        assert!(matches!(result, Err(TrackingError::Fsx(_))), "{result:?}");
        assert_eq!(entry.written, before);
    }

    #[test]
    fn a_failed_write_leaves_the_file_as_it_was() {
        let fixture = fixture();
        let mut entry = entry_of(ID);
        store(&fixture, &mut entry);
        let stored = fs::read(file_of(&fixture, ID)).expect("the file");

        let dir = fixture.collection.tracking_dir();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("chmod");
        assert!(
            fs::File::create(dir.join("probe")).is_err(),
            "this test needs a user that cannot write into a 0555 directory"
        );
        fixture.clock.advance(SignedDuration::from_secs(60));
        entry.missing_on_remote = true;
        let result = write(
            &fixture.collection,
            &fixture.lock,
            &fixture.clock,
            &mut entry,
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert!(result.is_err());
        assert_eq!(fs::read(file_of(&fixture, ID)).expect("the file"), stored);
    }

    #[test]
    fn read_returns_what_write_stored() {
        let fixture = fixture();
        let mut entry = entry_of(ID);
        entry.local_payloads = vec![archive(1, "0123456789abcdef-a.tar.zst")];
        store(&fixture, &mut entry);
        assert_eq!(
            read(&fixture.collection, entry.id).expect("reads"),
            Some(entry)
        );
    }

    #[test]
    fn read_of_an_absent_entry_is_none() {
        let fixture = fixture();
        let id = ID.parse().expect("an ID");
        assert_eq!(read(&fixture.collection, id).expect("reads"), None);
    }

    #[test]
    fn read_of_a_damaged_file_names_it() {
        let fixture = fixture();
        fs::write(file_of(&fixture, ID), "{not json").expect("write");
        let error = read(&fixture.collection, ID.parse().expect("an ID")).expect_err("damaged");
        assert!(matches!(error, TrackingError::Parse { .. }), "{error:?}");
        assert!(error.to_string().contains(&format!("{ID}.json")), "{error}");
    }

    #[test]
    fn read_of_a_file_under_the_wrong_id_is_refused() {
        let fixture = fixture();
        let other = entry_of(SECOND_ID);
        fs::write(file_of(&fixture, ID), text(&other)).expect("write");
        let error = read(&fixture.collection, ID.parse().expect("an ID")).expect_err("wrong ID");
        assert!(
            matches!(error, TrackingError::IdMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn read_all_takes_entries_and_skips_everything_else() {
        let fixture = fixture();
        let (mut first, mut second) = (entry_of(SECOND_ID), entry_of(ID));
        store(&fixture, &mut first);
        store(&fixture, &mut second);
        let dir = fixture.collection.tracking_dir();
        fs::write(dir.join(format!("{ID}.manifest.json")), "{}").expect("a manifest");
        fs::write(dir.join(format!(".{ID}.json.1.0.tmp")), "{").expect("a leftover");
        fs::write(dir.join("notes.txt"), "hello").expect("a stray file");
        // Entries are addressed by their upper-case name.
        fs::write(
            dir.join(format!("{}.json", THIRD_ID.to_lowercase())),
            text(&entry_of(THIRD_ID)),
        )
        .expect("a lower-case name");

        let set = read_all(&fixture.collection).expect("lists");
        let ids: Vec<String> = set.entries.iter().map(|e| e.id.to_string()).collect();
        assert_eq!(ids, [ID, SECOND_ID]);
        assert!(set.damaged.is_empty(), "{:?}", set.damaged);
    }

    #[test]
    fn read_all_of_a_missing_directory_is_empty() {
        let fixture = fixture();
        fs::remove_dir(fixture.collection.tracking_dir()).expect("the directory is empty");
        let set = read_all(&fixture.collection).expect("lists");
        assert!(set.entries.is_empty() && set.damaged.is_empty());
    }

    #[test]
    fn a_damaged_entry_is_listed_and_the_others_still_read() {
        let fixture = fixture();
        let mut good = entry_of(SECOND_ID);
        store(&fixture, &mut good);
        fs::write(file_of(&fixture, ID), "{not json").expect("write");

        let set = read_all(&fixture.collection).expect("lists");
        assert_eq!(set.entries, [good]);
        assert_eq!(set.damaged.len(), 1);
        assert_eq!(set.damaged[0].id.to_string(), ID);
        assert!(matches!(set.damaged[0].error, TrackingError::Parse { .. }));
    }

    #[test]
    fn a_file_under_the_wrong_id_is_damaged() {
        let fixture = fixture();
        fs::write(file_of(&fixture, ID), text(&entry_of(SECOND_ID))).expect("write");
        let set = read_all(&fixture.collection).expect("lists");
        assert!(set.entries.is_empty());
        assert!(matches!(
            set.damaged[0].error,
            TrackingError::IdMismatch { .. }
        ));
    }

    #[test]
    fn an_entry_that_vanishes_before_it_is_opened_counts_as_absent() {
        let fixture = fixture();
        let mut entry = entry_of(ID);
        store(&fixture, &mut entry);
        let ghost: ObjectId = SECOND_ID.parse().expect("an ID");
        let set = load(&fixture.collection.tracking_dir(), &[entry.id, ghost]);
        assert_eq!(set.entries, [entry]);
        assert!(set.damaged.is_empty());
    }

    #[test]
    fn remove_deletes_the_entry_and_is_idempotent() {
        let fixture = fixture();
        let mut entry = entry_of(ID);
        store(&fixture, &mut entry);
        remove(&fixture.collection, &fixture.lock, entry.id).expect("removes");
        remove(&fixture.collection, &fixture.lock, entry.id).expect("removes again");
        assert_eq!(read(&fixture.collection, entry.id).expect("reads"), None);
    }

    #[test]
    fn an_unreadable_directory_fails_read_all() {
        let fixture = fixture();
        let dir = fixture.collection.tracking_dir();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod");
        let precondition = fs::read_dir(&dir).is_err();
        let result = read_all(&fixture.collection);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert!(
            precondition,
            "this test needs a user that cannot list a 0000 directory"
        );
        let error = result.expect_err("the directory cannot be listed");
        assert!(
            matches!(error, TrackingError::Io { action: "list", .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("cannot list"), "{error}");
    }

    #[test]
    fn an_unreadable_entry_is_damaged() {
        let fixture = fixture();
        let mut entry = entry_of(ID);
        store(&fixture, &mut entry);
        let file = file_of(&fixture, ID);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).expect("chmod");
        let precondition = fs::read(&file).is_err();
        let set = read_all(&fixture.collection);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod back");

        assert!(
            precondition,
            "this test needs a user that cannot read a 0000 file"
        );
        let set = set.expect("lists");
        assert!(set.entries.is_empty());
        assert!(matches!(
            set.damaged[0].error,
            TrackingError::Io { action: "read", .. }
        ));
    }

    #[test]
    fn remove_reports_an_entry_that_cannot_be_removed() {
        let fixture = fixture();
        fs::create_dir(file_of(&fixture, ID)).expect("a directory in the entry's place");
        let error = remove(
            &fixture.collection,
            &fixture.lock,
            ID.parse().expect("an ID"),
        )
        .expect_err("a directory is not removed as a file");
        assert!(
            matches!(
                error,
                TrackingError::Io {
                    action: "remove",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn remove_leaves_the_manifest() {
        let fixture = fixture();
        let mut entry = entry_of(ID);
        store(&fixture, &mut entry);
        let manifest = fixture
            .collection
            .tracking_dir()
            .join(format!("{ID}.manifest.json"));
        fs::write(&manifest, "{}").expect("a manifest");
        remove(&fixture.collection, &fixture.lock, entry.id).expect("removes");
        assert!(manifest.exists());
    }
}
