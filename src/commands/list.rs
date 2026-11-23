// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `list` operation: one row per live piece and one per version of the store.

use crate::collection::Collection;
use crate::commands::live::{LiveError, LivePieces, Scope};
use crate::error::HasKind;
use crate::query::Query;
use crate::relpath::RelPath;
use crate::report::{self, CommandReport, Warning};
use crate::versions::VersionsError;
use crate::versions::damage::{self, Check, Condition, Inspection, Places};
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StorePiece, StoreVersion};
use jiff::Timestamp;
use serde::{Serialize, Serializer};
use std::collections::BTreeMap;
use std::num::NonZeroUsize;

/// Why list failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ListError {
    /// A file of the store could not be read for another reason than being
    /// gone. The source says which.
    #[error("cannot read the versions in the store")]
    Versions(
        #[from]
        #[diagnostic_source]
        VersionsError,
    ),
    /// The live pieces could not be selected.
    #[error("cannot select the live pieces")]
    Live(
        #[from]
        #[diagnostic_source]
        LiveError,
    ),
}

impl HasKind for ListError {}

/// What `list` could tell about a version from its head, its sidecars and
/// the size of its archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Ok,
    Damaged,
    /// Sidecars without an archive.
    Incomplete,
    /// The listing is of a newer format than antiq reads.
    NewerFormat,
    /// A file of the version could not be read, so nothing is known of it.
    Unreadable,
    /// A live piece: a folder of the collection, with no version.
    Live,
}

/// One version, or one live piece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub piece: RelPath,
    /// `None` for a live piece.
    #[serde(serialize_with = "as_text_option")]
    pub stamp: Option<Stamp>,
    /// The time of the stamp in UTC. The stamp has the counter this lacks.
    pub version: Option<Timestamp>,
    pub state: State,
    /// What the listing records. Each is `None` when the head lacks it.
    pub archive_bytes: Option<u64>,
    pub bytes: Option<u64>,
    pub files: Option<u64>,
    /// What is wrong with a damaged version, one text for each fault.
    pub damage: Vec<String>,
}

fn as_text_option<S: Serializer>(stamp: &Option<Stamp>, serializer: S) -> Result<S::Ok, S::Error> {
    match stamp {
        Some(stamp) => serializer.collect_str(stamp),
        None => serializer.serialize_none(),
    }
}

/// The rows of a listing, and what it learned about the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rows {
    pub versions: Vec<Row>,
    /// Whether the store holds any version at all: false for a store
    /// without a version, a version being stored apart. It tells "nothing
    /// to list" from "nothing matches". `None` when the store was not read.
    pub store_has_versions: Option<bool>,
    /// The files that could not be read.
    pub warnings: Vec<Warning>,
}

/// The piece at one path: whether it is live, and its folder of the store
/// if it has one.
struct Candidate<'a> {
    path: RelPath,
    live: bool,
    folder: Option<&'a StorePiece>,
}

/// Where the next row of the listing comes from.
enum Slot {
    Live(RelPath),
    /// The next inspected version.
    Stored,
}

/// The rows of the pieces whose path matches `query`, pieces by the score
/// of the match (best first, by path among equal scores), a live piece
/// before its versions and versions newest first. `live` holds the paths of
/// the live pieces and is read only when `scope` includes them.
///
/// The folders that hold sidecars only are searched too: their incomplete
/// versions are listed. A version whose archive is staged in `tmp/pack/` is
/// being moved in by a run and has no row.
pub fn gather(
    places: &Places,
    query: Option<&str>,
    scope: Scope,
    live: &[RelPath],
) -> Result<Rows, ListError> {
    let scan = if scope.includes_store() {
        Some(store::scan(places.pieces_dir)?)
    } else {
        None
    };
    let mut matcher = Query::parse(query.unwrap_or(""));

    let folders: Vec<&StorePiece> = scan
        .iter()
        .flat_map(|scan| scan.pieces.iter().chain(&scan.sidecar_only))
        .collect();
    // A store folder is in `pieces` or in `sidecar_only`, never both, so a
    // path has at most one folder.
    let mut candidates: BTreeMap<RelPath, Candidate> = BTreeMap::new();
    for path in live.iter().filter(|_| scope.includes_live()) {
        let candidate = Candidate {
            path: path.clone(),
            live: true,
            folder: None,
        };
        candidates.insert(path.clone(), candidate);
    }
    for folder in &folders {
        candidates
            .entry(folder.path.clone())
            .or_insert_with(|| Candidate {
                path: folder.path.clone(),
                live: false,
                folder: None,
            })
            .folder = Some(folder);
    }

    let mut slots = Vec::new();
    let mut items: Vec<(RelPath, StoreVersion)> = Vec::new();
    let ranked = matcher.rank(candidates.into_values().collect(), |found| &found.path);
    for candidate in ranked {
        if candidate.live {
            slots.push(Slot::Live(candidate.path.clone()));
        }
        let versions = candidate
            .folder
            .map_or(&[][..], |folder| &folder.versions[..]);
        for version in versions.iter().rev() {
            slots.push(Slot::Stored);
            items.push((candidate.path.clone(), version.clone()));
        }
    }

    let workers = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let inspections = damage::inspect_all(places, &items, Check::Head, workers);
    let mut versions = Vec::new();
    let mut stored_rows = 0;
    let mut warnings = Vec::new();
    let mut inspected = items.iter().zip(inspections);
    for slot in slots {
        match slot {
            Slot::Live(piece) => versions.push(live_row(piece)),
            Slot::Stored => {
                let ((piece, version), inspection) = inspected
                    .next()
                    .expect("every stored slot has an inspection");
                match inspection {
                    Ok(inspection) => {
                        let row = row_of(piece, version, inspection);
                        stored_rows += usize::from(row.is_some());
                        versions.extend(row);
                    }
                    // A file that cannot be read says nothing about the
                    // version, but it is there and listed.
                    Err(error) => {
                        warnings.push(Warning::new(error.to_string()));
                        stored_rows += 1;
                        versions.push(unreadable_row(piece, version));
                    }
                }
            }
        }
    }

    let store_has_versions = scope.includes_store().then(|| {
        stored_rows > 0 || {
            // A query that matches nothing says nothing about the rest of
            // the store, so the folders it left out are asked.
            let everything: Vec<(RelPath, StoreVersion)> = folders
                .iter()
                .flat_map(|folder| {
                    folder
                        .versions
                        .iter()
                        .map(|version| (folder.path.clone(), version.clone()))
                })
                .collect();
            damage::inspect_all(places, &everything, Check::Head, workers)
                .into_iter()
                .any(|inspection| {
                    inspection.map_or(true, |found| found.condition != Condition::BeingStored)
                })
        }
    });
    Ok(Rows {
        versions,
        store_has_versions,
        warnings,
    })
}

/// The row of a live piece. It has no version, so nothing else is known.
fn live_row(piece: RelPath) -> Row {
    Row {
        piece,
        stamp: None,
        version: None,
        state: State::Live,
        archive_bytes: None,
        bytes: None,
        files: None,
        damage: Vec::new(),
    }
}

fn unreadable_row(piece: &RelPath, version: &StoreVersion) -> Row {
    Row {
        piece: piece.clone(),
        stamp: Some(version.stamp),
        version: Some(version.stamp.time()),
        state: State::Unreadable,
        archive_bytes: None,
        bytes: None,
        files: None,
        damage: Vec::new(),
    }
}

/// The row of an inspected version, or `None` for one that is being moved in.
fn row_of(piece: &RelPath, version: &StoreVersion, inspection: Inspection) -> Option<Row> {
    let (state, damage) = match inspection.condition {
        Condition::BeingStored => return None,
        Condition::Sound => (State::Ok, Vec::new()),
        Condition::Incomplete => (State::Incomplete, Vec::new()),
        Condition::NewerFormat => (State::NewerFormat, Vec::new()),
        Condition::Damaged(faults) => (
            State::Damaged,
            faults.iter().map(ToString::to_string).collect(),
        ),
    };
    let head = inspection.head.unwrap_or_default();
    Some(Row {
        piece: piece.clone(),
        stamp: Some(version.stamp),
        version: Some(version.stamp.time()),
        state,
        archive_bytes: head.archive_bytes,
        bytes: head.bytes,
        files: head.files,
        damage,
    })
}

/// What `list` found.
#[derive(Debug, Serialize)]
pub struct ListReport {
    pub versions: Vec<Row>,
    pub warnings: Vec<Warning>,
    /// The run that holds the lock, while there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Which sides the listing covers.
    #[serde(skip)]
    pub scope: Scope,
    /// Whether the store holds a version; `None` when it was not read.
    #[serde(skip)]
    pub store_has_versions: Option<bool>,
    /// Whether the collection has a live piece; `None` when the live pieces
    /// were not selected, or when none was found and a directory that may
    /// hold some could not be read.
    #[serde(skip)]
    pub live_pieces: Option<bool>,
}

impl CommandReport for ListReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// Lists the live pieces and the versions of the collection that `scope`
/// covers. Nothing is locked or written, so a running `archive` is only
/// noted.
///
/// The live pieces are selected before the store is read.
pub fn list(
    collection: &Collection,
    query: Option<&str>,
    scope: Scope,
) -> Result<ListReport, ListError> {
    let places = Places {
        pieces_dir: &collection.pieces_dir(),
        tmp_pack_dir: &collection.tmp_dir().join("pack"),
    };
    let live = scope
        .includes_live()
        .then(|| LivePieces::select(collection))
        .transpose()?;
    let live_paths = live.as_ref().map_or(&[][..], LivePieces::pieces);
    let rows = gather(&places, query, scope, live_paths)?;

    let mut warnings = report::setting_notes(collection);
    warnings.extend(rows.warnings);
    warnings.extend(live.iter().flat_map(LivePieces::unreadable_warnings));
    let (note, unreadable_lock) = report::lock_status(collection);
    warnings.extend(unreadable_lock);
    Ok(ListReport {
        versions: rows.versions,
        warnings,
        note,
        scope,
        store_has_versions: rows.store_has_versions,
        live_pieces: live.as_ref().and_then(live_pieces_known),
    })
}

/// Whether there is a live piece, unless a directory that could not be read
/// leaves that open.
fn live_pieces_known(live: &LivePieces) -> Option<bool> {
    let found = !live.pieces().is_empty();
    (found || live.unreadable().is_empty()).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock;
    use crate::testing::tree::{COLLECTION_ID, TempCollection, Tree};
    use crate::testing::versions::{Store, StoredVersion};
    use crate::versions::names::VersionFile;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn source() -> Tree {
        let mut tree = Tree::new();
        tree.file("notes/a.txt", "alpha");
        tree.file("notes/sub/b.txt", "beta");
        tree.file("projects/scanner/main.rs", "fn main() {}");
        tree.file("tools/scan-old/run.sh", "echo");
        tree.file("clients/scala-notes/todo.txt", "call");
        tree
    }

    fn rows(store: &Store, query: Option<&str>) -> Rows {
        gather(&store.places(), query, Scope::Archived, &[]).expect("the store lists")
    }

    fn found(rows: &Rows) -> Vec<String> {
        rows.versions
            .iter()
            .map(|row| {
                format!(
                    "{} {}",
                    row.piece,
                    row.stamp
                        .map_or("live".to_string(), |stamp| stamp.to_string())
                )
            })
            .collect()
    }

    fn only(rows: &Rows) -> &Row {
        assert_eq!(rows.versions.len(), 1, "{:?}", found(rows));
        &rows.versions[0]
    }

    fn one_version(piece: &str) -> (Store, StoredVersion) {
        let mut store = Store::new();
        let version = store.add_version(&source(), piece, "2026-11-04T090211");
        (store, version)
    }

    #[test]
    fn pieces_come_by_path_and_versions_newest_first() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        store.add_version(&tree, "notes", "2026-09-03T121500");
        store.add_version(&tree, "notes", "2026-11-04T090211");
        store.add_version(&tree, "notes", "2026-11-04T090211-2");
        assert_eq!(
            found(&rows(&store, None)),
            [
                "notes 2026-11-04T090211-2",
                "notes 2026-11-04T090211",
                "notes 2026-09-03T121500",
                "tools/scan-old 2025-02-11T184007",
            ]
        );
    }

    #[test]
    fn a_sound_version_carries_what_its_listing_records() {
        let (store, version) = one_version("notes");
        let rows = rows(&store, None);
        let row = only(&rows);
        let archive_size = fs::metadata(version.path(VersionFile::Archive))
            .expect("the archive exists")
            .len();
        assert_eq!(row.state, State::Ok);
        assert_eq!(row.archive_bytes, Some(archive_size));
        assert_eq!(row.files, Some(2));
        assert_eq!(row.bytes, Some(9));
        assert_eq!(
            row.version.expect("a version").to_string(),
            "2026-11-04T09:02:11Z"
        );
        assert!(row.damage.is_empty());
    }

    #[test]
    fn the_query_keeps_the_pieces_whose_path_matches() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "projects/scanner", "2026-11-04T090211");
        store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        store.add_version(&tree, "notes", "2026-11-04T090211");
        assert_eq!(
            found(&rows(&store, Some("scan"))),
            [
                "projects/scanner 2026-11-04T090211",
                "tools/scan-old 2025-02-11T184007"
            ]
        );
        assert_eq!(rows(&store, Some("   ")).versions.len(), 3);
    }

    #[test]
    fn a_word_at_a_word_start_is_listed_before_scattered_letters() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "clients/scala-notes", "2026-11-04T090211");
        store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        assert_eq!(
            found(&rows(&store, Some("scan"))),
            [
                "tools/scan-old 2025-02-11T184007",
                "clients/scala-notes 2026-11-04T090211"
            ]
        );
    }

    #[test]
    fn the_versions_of_a_ranked_piece_stay_together_newest_first() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "clients/scala-notes", "2026-11-04T090211");
        store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        store.add_version(&tree, "tools/scan-old", "2026-01-05T101010");
        assert_eq!(
            found(&rows(&store, Some("scan"))),
            [
                "tools/scan-old 2026-01-05T101010",
                "tools/scan-old 2025-02-11T184007",
                "clients/scala-notes 2026-11-04T090211"
            ]
        );
    }

    #[test]
    fn pieces_with_equal_scores_come_by_path() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        store.add_version(&tree, "projects/scanner", "2026-11-04T090211");
        assert_eq!(
            found(&rows(&store, Some("scan"))),
            [
                "projects/scanner 2026-11-04T090211",
                "tools/scan-old 2025-02-11T184007"
            ]
        );
    }

    #[test]
    fn a_query_that_matches_no_piece_leaves_no_row_in_a_store_with_versions() {
        let (store, _) = one_version("notes");
        let rows = rows(&store, Some("zzz"));
        assert!(rows.versions.is_empty(), "{:?}", found(&rows));
        assert_eq!(rows.store_has_versions, Some(true));
    }

    #[test]
    fn an_empty_store_has_no_versions() {
        let rows = rows(&Store::new(), None);
        assert!(rows.versions.is_empty(), "{:?}", found(&rows));
        assert_eq!(rows.store_has_versions, Some(false));
    }

    #[test]
    fn a_version_without_its_archive_is_incomplete_also_in_a_folder_of_sidecars_only() {
        let mut store = Store::new();
        let tree = source();
        let lone = store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        lone.remove(VersionFile::Archive);
        store.add_version(&tree, "notes", "2026-11-04T090211");
        let rows = rows(&store, Some("scan"));
        let row = only(&rows);
        assert_eq!(row.piece.to_string(), "tools/scan-old");
        assert_eq!(row.state, State::Incomplete);
        assert_eq!(
            (row.archive_bytes, row.bytes, row.files),
            (None, None, None)
        );
    }

    #[test]
    fn a_version_whose_archive_is_staged_has_no_row_and_does_not_count() {
        let mut store = Store::new();
        let tree = source();
        let moving = store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        moving.stage_archive(&store);
        moving.remove(VersionFile::Archive);
        assert_eq!(rows(&store, None).store_has_versions, Some(false));
        store.add_version(&tree, "notes", "2026-11-04T090211");
        assert_eq!(found(&rows(&store, None)), ["notes 2026-11-04T090211"]);
    }

    #[test]
    fn a_truncated_archive_is_damaged_and_shows_the_recorded_size() {
        let (store, version) = one_version("notes");
        let recorded = fs::metadata(version.path(VersionFile::Archive))
            .expect("the archive exists")
            .len();
        version.truncate(VersionFile::Archive, 10);
        let rows = rows(&store, None);
        let row = only(&rows);
        assert_eq!(row.state, State::Damaged);
        assert_eq!(row.archive_bytes, Some(recorded));
        assert_eq!(row.damage.len(), 1, "{:?}", row.damage);
        assert!(
            row.damage[0].contains("the archive has 10 bytes"),
            "{:?}",
            row.damage
        );
    }

    #[test]
    fn a_head_that_lacks_a_line_leaves_that_number_out() {
        let (store, version) = one_version("notes");
        version.listing_without("archive-bytes");
        let rows = rows(&store, None);
        let row = only(&rows);
        assert_eq!(row.state, State::Damaged);
        assert_eq!(row.archive_bytes, None);
        assert_eq!(row.files, Some(2));
        assert!(row.damage[0].contains("archive-bytes"), "{:?}", row.damage);
    }

    #[test]
    fn a_missing_listing_or_checksum_file_is_damage() {
        for kind in [VersionFile::Listing, VersionFile::Checksum] {
            let (store, version) = one_version("notes");
            version.remove(kind);
            let rows = rows(&store, None);
            assert_eq!(only(&rows).state, State::Damaged, "{kind:?}");
        }
    }

    #[test]
    fn a_listing_of_a_newer_format_is_marked_as_such() {
        let (store, version) = one_version("notes");
        version.listing_from_text(b"# antiq listing 99\n#\n");
        let rows = rows(&store, None);
        let row = only(&rows);
        assert_eq!(row.state, State::NewerFormat);
        assert_eq!(
            (row.archive_bytes, row.bytes, row.files),
            (None, None, None)
        );
    }

    #[test]
    fn a_doubled_slot_is_damaged_and_names_every_file() {
        let (store, version) = one_version("notes");
        let twin = version.add_twin(VersionFile::Archive, "other");
        let rows = rows(&store, None);
        let row = only(&rows);
        assert_eq!(row.state, State::Damaged);
        assert_eq!(row.archive_bytes, None);
        let twin = twin
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        assert!(row.damage[0].contains(&twin), "{:?}", row.damage);
        assert!(
            row.damage[0].contains(&version.names.archive),
            "{:?}",
            row.damage
        );
    }

    /// Takes the mode away from a file or folder and gives it back when
    /// dropped, also on a panic.
    struct Unreadable(std::path::PathBuf, fs::Permissions);

    impl Unreadable {
        fn new(path: std::path::PathBuf) -> Unreadable {
            let before = fs::metadata(&path).expect("the path exists").permissions();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
                .expect("the mode changes");
            Unreadable(path, before)
        }
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, self.1.clone());
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_leaves_its_row_unreadable_and_is_a_warning() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "projects/scanner", "2026-11-04T090211");
        let version = store.add_version(&tree, "notes", "2026-11-04T090211");
        let _guard = Unreadable::new(version.path(VersionFile::Listing));
        assert!(
            fs::File::open(version.path(VersionFile::Listing)).is_err(),
            "the test needs a non-root user"
        );
        let rows = rows(&store, None);
        assert_eq!(
            found(&rows),
            [
                "notes 2026-11-04T090211",
                "projects/scanner 2026-11-04T090211"
            ]
        );
        let row = &rows.versions[0];
        assert_eq!(row.state, State::Unreadable);
        assert_eq!(
            (row.archive_bytes, row.bytes, row.files),
            (None, None, None)
        );
        assert_eq!(rows.versions[1].state, State::Ok);
        assert_eq!(rows.warnings.len(), 1, "{:?}", rows.warnings);
        let message = &rows.warnings[0].message;
        assert!(message.starts_with("cannot open notes/"), "{message}");
        assert!(message.contains(&version.names.listing), "{message}");
    }

    #[test]
    fn a_file_that_cannot_be_read_still_counts_as_a_version_when_a_query_matches_nothing() {
        let (store, version) = one_version("notes");
        let _guard = Unreadable::new(version.path(VersionFile::Listing));
        assert!(
            fs::File::open(version.path(VersionFile::Listing)).is_err(),
            "the test needs a non-root user"
        );
        let rows = rows(&store, Some("zzz"));
        assert!(rows.versions.is_empty());
        assert_eq!(rows.store_has_versions, Some(true));
        assert!(rows.warnings.is_empty(), "{:?}", rows.warnings);
    }

    #[test]
    fn a_store_folder_that_cannot_be_read_fails_the_listing() {
        let (store, _) = one_version("notes");
        let _guard = Unreadable::new(store.pieces_dir().join("notes"));
        assert!(
            fs::read_dir(store.pieces_dir().join("notes")).is_err(),
            "the test needs a non-root user"
        );
        let error = gather(&store.places(), None, Scope::Archived, &[])
            .expect_err("the folder cannot be listed");
        assert!(matches!(error, ListError::Versions(_)), "{error}");
        assert_eq!(error.kind(), crate::error::ErrorKind::Other);
    }

    #[test]
    fn a_row_serialises_with_the_stamp_beside_the_utc_time() {
        let (store, _) = one_version("notes");
        let rows = rows(&store, None);
        let row = only(&rows);
        let archive_bytes = row.archive_bytes.expect("recorded");
        assert_eq!(
            serde_json::to_string(row).expect("serialises"),
            json!({
                "piece": "notes",
                "stamp": "2026-11-04T090211",
                "version": "2026-11-04T09:02:11Z",
                "state": "ok",
                "archive_bytes": archive_bytes,
                "bytes": 9,
                "files": 2,
                "damage": []
            })
            .to_string()
        );
    }

    fn collection(temp: &TempCollection, config_lines: &str) -> Collection {
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{config_lines}\n"),
        )
        .expect("a config can be written");
        Collection::find(temp.root()).expect("a collection")
    }

    #[test]
    fn a_free_lock_leaves_no_note() {
        let temp = TempCollection::minimal();
        let report = list(&collection(&temp, ""), None, Scope::Both).expect("lists");
        assert!(report.versions.is_empty());
        assert_eq!(report.store_has_versions, Some(false));
        assert_eq!(report.note, None);
        assert!(report.warnings.is_empty());
        assert!(!report.partial());
    }

    #[test]
    fn a_held_lock_is_a_note_and_the_listing_goes_on() {
        let temp = TempCollection::minimal();
        let collection = collection(&temp, "");
        let _lock = lock::acquire(
            &collection.lock_path(),
            "imperius",
            &crate::clock::SystemClock,
        )
        .expect("the lock is free");
        let report = list(&collection, None, Scope::Both).expect("lists");
        let note = report.note.expect("the lock is noted");
        assert!(
            note.contains(&format!("pid is {}", std::process::id())),
            "{note}"
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_lock_file_that_cannot_be_read_is_a_warning() {
        let mut temp = TempCollection::minimal();
        temp.tree().dir(".antiq/lock");
        let report = list(&collection(&temp, ""), None, Scope::Both).expect("lists");
        assert_eq!(report.note, None);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0]
                .message
                .starts_with("cannot read the lock file"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn the_settings_notes_are_warnings() {
        let temp = TempCollection::minimal();
        let report = list(&collection(&temp, "colour = 1"), None, Scope::Both).expect("lists");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].message.contains("colour"));
        assert_eq!(CommandReport::warnings(&report), report.warnings.as_slice());
    }

    #[test]
    fn the_report_serialises_versions_warnings_and_the_note_in_that_order() {
        let report = ListReport {
            versions: Vec::new(),
            warnings: vec![Warning::new("w")],
            note: Some("n".to_string()),
            scope: Scope::Both,
            store_has_versions: Some(true),
            live_pieces: Some(false),
        };
        assert_eq!(
            serde_json::to_string(&report).expect("serialises"),
            r#"{"versions":[],"warnings":[{"message":"w"}],"note":"n"}"#
        );
    }

    // =========================================================
    // Live pieces
    // =========================================================

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    fn with_live(store: &Store, query: Option<&str>, scope: Scope, live: &[&str]) -> Rows {
        let live: Vec<RelPath> = live.iter().map(|text| path(text)).collect();
        gather(&store.places(), query, scope, &live).expect("the store lists")
    }

    #[test]
    fn a_live_piece_is_a_row_of_its_own() {
        let rows = with_live(&Store::new(), None, Scope::Both, &["projects/scanner"]);
        let row = only(&rows);
        assert_eq!(row.piece.to_string(), "projects/scanner");
        assert_eq!(row.state, State::Live);
        assert_eq!(
            (
                row.stamp,
                row.version,
                row.archive_bytes,
                row.bytes,
                row.files
            ),
            (None, None, None, None, None)
        );
        assert!(row.damage.is_empty());
    }

    #[test]
    fn a_live_row_comes_directly_before_the_versions_of_its_piece() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "notes", "2026-09-03T121500");
        store.add_version(&tree, "notes", "2026-11-04T090211");
        store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        let rows = with_live(&store, None, Scope::Both, &["notes"]);
        assert_eq!(
            found(&rows),
            [
                "notes live",
                "notes 2026-11-04T090211",
                "notes 2026-09-03T121500",
                "tools/scan-old 2025-02-11T184007",
            ]
        );
    }

    #[test]
    fn live_and_archived_pieces_share_one_ranking() {
        let mut store = Store::new();
        let tree = source();
        store.add_version(&tree, "clients/scala-notes", "2026-11-04T090211");
        store.add_version(&tree, "tools/scan-old", "2025-02-11T184007");
        let live_better = with_live(&store, Some("scan"), Scope::Both, &["projects/scanner"]);
        assert_eq!(
            found(&live_better),
            [
                "projects/scanner live",
                "tools/scan-old 2025-02-11T184007",
                "clients/scala-notes 2026-11-04T090211"
            ]
        );
        let live_worse = with_live(&store, Some("scan"), Scope::Both, &["archive/rescan-logs"]);
        assert_eq!(
            found(&live_worse),
            [
                "tools/scan-old 2025-02-11T184007",
                "clients/scala-notes 2026-11-04T090211",
                "archive/rescan-logs live"
            ]
        );
    }

    #[test]
    fn without_a_query_the_live_pieces_are_listed_by_path_with_the_archived_ones() {
        let (store, _) = one_version("notes");
        let rows = with_live(&store, None, Scope::Both, &["a-live", "z-live"]);
        assert_eq!(
            found(&rows),
            ["a-live live", "notes 2026-11-04T090211", "z-live live"]
        );
    }

    #[test]
    fn a_query_keeps_only_the_live_pieces_that_match() {
        let rows = with_live(
            &Store::new(),
            Some("scan"),
            Scope::Both,
            &["notes", "scanner"],
        );
        assert_eq!(found(&rows), ["scanner live"]);
    }

    #[test]
    fn the_archived_scope_lists_no_live_row() {
        let (store, _) = one_version("notes");
        let rows = with_live(&store, None, Scope::Archived, &["notes", "other"]);
        assert_eq!(found(&rows), ["notes 2026-11-04T090211"]);
    }

    #[test]
    fn the_live_scope_lists_no_version_and_reads_no_store() {
        let (store, version) = one_version("notes");
        version.truncate(VersionFile::Archive, 10);
        version.remove(VersionFile::Checksum);
        let rows = with_live(&store, None, Scope::Live, &["notes"]);
        assert_eq!(found(&rows), ["notes live"]);
        assert_eq!(rows.store_has_versions, None);
        assert!(rows.warnings.is_empty());
    }

    #[test]
    fn an_empty_store_with_a_live_piece_has_no_versions() {
        let rows = with_live(&Store::new(), None, Scope::Both, &["notes"]);
        assert_eq!(rows.store_has_versions, Some(false));
    }

    #[test]
    fn the_report_of_a_live_row_serialises_nulls() {
        let rows = with_live(&Store::new(), None, Scope::Both, &["notes"]);
        assert_eq!(
            serde_json::to_value(only(&rows)).expect("serialises"),
            json!({
                "piece": "notes",
                "stamp": null,
                "version": null,
                "state": "live",
                "archive_bytes": null,
                "bytes": null,
                "files": null,
                "damage": []
            })
        );
    }

    #[test]
    fn list_selects_the_live_pieces_of_the_collection() {
        let mut temp = TempCollection::minimal();
        temp.tree().file("notes/a.txt", "a");
        temp.tree().file("pinned/b.txt", "b");
        temp.tree().file("pinned/.antiq-pin", "");
        temp.tree().file("nested/inner/.antiq/config.toml", "x");
        let report = list(&collection(&temp, ""), None, Scope::Both).expect("lists");
        assert_eq!(
            report
                .versions
                .iter()
                .map(|row| row.piece.to_string())
                .collect::<Vec<_>>(),
            ["nested", "notes", "pinned"],
            "a pinned piece and one that archive refuses are live too"
        );
        assert_eq!(report.live_pieces, Some(true));
        assert_eq!(report.store_has_versions, Some(false));
    }

    #[test]
    fn a_collection_without_a_live_piece_says_so() {
        let temp = TempCollection::minimal();
        let report = list(&collection(&temp, ""), None, Scope::Both).expect("lists");
        assert_eq!(report.live_pieces, Some(false));
    }

    #[test]
    fn the_archived_scope_selects_nothing() {
        let mut temp = TempCollection::minimal();
        temp.tree().file("notes/a.txt", "a");
        let report = list(
            &collection(&temp, "patterns = [\"[\"]"),
            None,
            Scope::Archived,
        )
        .expect("lists");
        assert!(report.versions.is_empty());
        assert_eq!(report.live_pieces, None);
    }

    #[test]
    fn patterns_that_cannot_be_used_fail_the_listing() {
        let temp = TempCollection::minimal();
        let error = list(&collection(&temp, "patterns = [\"[\"]"), None, Scope::Both)
            .expect_err("the pattern is invalid");
        assert!(matches!(error, ListError::Live(_)), "{error:?}");
    }

    #[test]
    fn a_directory_that_cannot_be_read_is_a_warning_and_leaves_the_live_pieces_unknown() {
        let mut temp = TempCollection::minimal();
        temp.tree().unreadable_dir("projects");
        let report = list(
            &collection(&temp, "patterns = [\"*/*\"]"),
            None,
            Scope::Both,
        )
        .expect("lists");
        let messages: Vec<&str> = report
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert_eq!(messages, ["unreadable: projects"]);
        assert_eq!(report.live_pieces, None);
    }
}
