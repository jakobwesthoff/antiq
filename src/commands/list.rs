// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `list` operation: one row per version of the store.

use crate::collection::Collection;
use crate::error::HasKind;
use crate::lock;
use crate::query::Query;
use crate::relpath::RelPath;
use crate::report::{self, CommandReport, Warning, lock_note};
use crate::versions::VersionsError;
use crate::versions::damage::{self, Check, Condition, Inspection, Places};
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StorePiece, StoreVersion};
use jiff::Timestamp;
use serde::{Serialize, Serializer};
use std::num::NonZeroUsize;

/// Why list failed.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    /// A file of the store could not be read for another reason than being
    /// gone. The source says which.
    #[error("cannot read the versions in the store")]
    Versions(#[from] VersionsError),
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
}

/// One version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub piece: RelPath,
    #[serde(serialize_with = "as_text")]
    pub stamp: Stamp,
    /// The time of the stamp in UTC. The stamp has the counter this lacks.
    pub version: Timestamp,
    pub state: State,
    /// What the listing records. Each is `None` when the head lacks it.
    pub archive_bytes: Option<u64>,
    pub bytes: Option<u64>,
    pub files: Option<u64>,
    /// What is wrong with a damaged version, one text for each fault.
    pub damage: Vec<String>,
}

fn as_text<S: Serializer>(stamp: &Stamp, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(stamp)
}

/// The rows of a store, and whether the store holds any version at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rows {
    pub versions: Vec<Row>,
    /// False for a store without a version, a version being stored apart.
    /// It tells "nothing to list" from "nothing matches".
    pub store_has_versions: bool,
}

/// The rows of the pieces whose path matches `query`, pieces by path and
/// versions newest first.
///
/// The folders that hold sidecars only are searched too: their incomplete
/// versions are listed. A version whose archive is staged in `tmp/pack/` is
/// being moved in by a run and has no row.
pub fn gather(places: &Places, query: Option<&str>) -> Result<Rows, ListError> {
    let scan = store::scan(places.pieces_dir)?;
    let mut matcher = Query::parse(query.unwrap_or(""));

    let mut folders: Vec<&StorePiece> = scan.pieces.iter().chain(&scan.sidecar_only).collect();
    folders.sort_by(|left, right| left.path.cmp(&right.path));
    let items: Vec<(RelPath, StoreVersion)> = folders
        .iter()
        .filter(|folder| matcher.score(&folder.path.to_string()).is_some())
        .flat_map(|folder| {
            let newest_first = folder.versions.iter().rev();
            newest_first.map(|version| (folder.path.clone(), version.clone()))
        })
        .collect();

    let workers = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let inspections = damage::inspect_all(places, &items, Check::Head, workers);
    let mut versions = Vec::new();
    for ((piece, version), inspection) in items.iter().zip(inspections) {
        if let Some(row) = row_of(piece, version, inspection?) {
            versions.push(row);
        }
    }

    let store_has_versions = !versions.is_empty() || {
        // A query that matches nothing says nothing about the rest of the
        // store, so the folders it left out are asked.
        let everything: Vec<(RelPath, StoreVersion)> = folders
            .iter()
            .flat_map(|folder| {
                folder
                    .versions
                    .iter()
                    .map(|v| (folder.path.clone(), v.clone()))
            })
            .collect();
        damage::inspect_all(places, &everything, Check::Head, workers)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|inspection| inspection.condition != Condition::BeingStored)
    };
    Ok(Rows {
        versions,
        store_has_versions,
    })
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
        stamp: version.stamp,
        version: version.stamp.time(),
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
    #[serde(skip)]
    pub store_has_versions: bool,
}

impl CommandReport for ListReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// Lists the versions of the collection. Nothing is locked or written, so a
/// running `archive` is only noted.
pub fn list(collection: &Collection, query: Option<&str>) -> Result<ListReport, ListError> {
    let places = Places {
        pieces_dir: &collection.pieces_dir(),
        tmp_pack_dir: &collection.tmp_dir().join("pack"),
    };
    let rows = gather(&places, query)?;

    let mut warnings = report::setting_notes(collection);
    let mut note = None;
    match lock::holder(&collection.lock_path()) {
        Ok(Some(holder)) => note = Some(lock_note(&holder).message),
        Ok(None) => {}
        Err(error) => warnings.push(Warning::new(format!("cannot read the lock file: {error}"))),
    }
    Ok(ListReport {
        versions: rows.versions,
        warnings,
        note,
        store_has_versions: rows.store_has_versions,
    })
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
        tree
    }

    fn rows(store: &Store, query: Option<&str>) -> Rows {
        gather(&store.places(), query).expect("the store lists")
    }

    fn found(rows: &Rows) -> Vec<String> {
        rows.versions
            .iter()
            .map(|row| format!("{} {}", row.piece, row.stamp))
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
        assert_eq!(row.version.to_string(), "2026-11-04T09:02:11Z");
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
    fn a_query_that_matches_no_piece_leaves_no_row_in_a_store_with_versions() {
        let (store, _) = one_version("notes");
        let rows = rows(&store, Some("zzz"));
        assert!(rows.versions.is_empty(), "{:?}", found(&rows));
        assert!(rows.store_has_versions);
    }

    #[test]
    fn an_empty_store_has_no_versions() {
        let rows = rows(&Store::new(), None);
        assert!(rows.versions.is_empty(), "{:?}", found(&rows));
        assert!(!rows.store_has_versions);
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
        assert!(!rows(&store, None).store_has_versions);
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

    /// Takes the mode away from a file and gives it back when dropped, also
    /// on a panic.
    struct Unreadable(std::path::PathBuf);

    impl Unreadable {
        fn new(path: std::path::PathBuf) -> Unreadable {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
                .expect("the mode changes");
            Unreadable(path)
        }
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_fails_the_listing() {
        let (store, version) = one_version("notes");
        let _guard = Unreadable::new(version.path(VersionFile::Listing));
        assert!(
            fs::File::open(version.path(VersionFile::Listing)).is_err(),
            "the test needs a non-root user"
        );
        let error = gather(&store.places(), None).expect_err("the listing cannot be opened");
        assert!(matches!(error, ListError::Versions(_)), "{error}");
        assert_eq!(error.kind(), crate::error::ErrorKind::Other);
    }

    #[test]
    fn a_file_that_cannot_be_read_fails_a_listing_that_matched_nothing_too() {
        let (store, version) = one_version("notes");
        let _guard = Unreadable::new(version.path(VersionFile::Listing));
        assert!(
            fs::File::open(version.path(VersionFile::Listing)).is_err(),
            "the test needs a non-root user"
        );
        let error = gather(&store.places(), Some("zzz")).expect_err("the store is asked");
        assert!(matches!(error, ListError::Versions(_)), "{error}");
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
        let report = list(&collection(&temp, ""), None).expect("lists");
        assert!(report.versions.is_empty());
        assert!(!report.store_has_versions);
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
        let report = list(&collection, None).expect("lists");
        let note = report.note.expect("the lock is noted");
        assert!(
            note.contains(&format!("pid {}", std::process::id())),
            "{note}"
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_lock_file_that_cannot_be_read_is_a_warning() {
        let mut temp = TempCollection::minimal();
        temp.tree().dir(".antiq/lock");
        let report = list(&collection(&temp, ""), None).expect("lists");
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
        let report = list(&collection(&temp, "colour = 1"), None).expect("lists");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].message.contains("colour"));
    }

    #[test]
    fn the_report_serialises_versions_warnings_and_the_note_in_that_order() {
        let report = ListReport {
            versions: Vec::new(),
            warnings: vec![Warning::new("w")],
            note: Some("n".to_string()),
            store_has_versions: true,
        };
        assert_eq!(
            serde_json::to_string(&report).expect("serialises"),
            r#"{"versions":[],"warnings":[{"message":"w"}],"note":"n"}"#
        );
    }
}
