// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `status` operation.

use super::survey::{self, SurveyError, SurveyedPiece};
use crate::collection::Collection;
use crate::error::HasKind;
use crate::filesystem::tree::{DirEntry, TreeRoot};
use crate::lock::{self, Holder};
use crate::reference::{ReferenceError, stored_spelling};
use crate::relpath::RelPath;
use crate::repair::trees_of;
use crate::report::{self, Warning};
use crate::tree::EntryKind;
use crate::versions::VersionsError;
use crate::versions::damage::{Check, Condition, Damage, Places, inspect_all};
use crate::versions::stamp::Stamp;
use crate::versions::store::{StoreScan, StoreVersion, scan};
use jiff::Timestamp;
use std::collections::BTreeSet;
use std::io;

/// Why status failed. A store or a leftover directory that cannot be read
/// is a note of the report, not a failure.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("cannot judge the live pieces: {0}")]
    Survey(#[from] SurveyError),
}

impl HasKind for StatusError {}

/// A version that status names, with what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionNote {
    pub piece: RelPath,
    pub stamp: Stamp,
    /// Why the version is damaged; empty for the other kinds of notes.
    pub reasons: Vec<String>,
}

/// Why a leftover in `doomed/` cannot be deleted, as far as status can
/// read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoomedCause {
    /// The entry carries an immutable or append-only flag.
    Locked,
    /// The directory that holds the entry cannot be written by the user.
    DirectoryNotWritable,
}

/// A tree left in `doomed/` with the first entry the deletion stopped at.
/// Paths are relative to `.antiq/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doomed {
    pub path: RelPath,
    pub first_entry: RelPath,
    pub cause: Option<DoomedCause>,
}

/// A live piece whose spelling differs from the stored one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpellingDifference {
    pub live: RelPath,
    pub stored: RelPath,
}

/// Everything `status` tells. Paths of leftovers and of files in the store
/// are relative to `.antiq/`; the others are relative to the collection.
#[derive(Debug, Clone, Default)]
pub struct StatusReport {
    pub folders: usize,
    /// `None` when the store could not be read.
    pub versions: Option<usize>,
    /// The sum of the `archive-bytes` the listings record; `None` when the
    /// store could not be read.
    pub archive_bytes: Option<u64>,
    pub idle_days: u32,
    pub soon_days: u32,
    pub pieces: Vec<SurveyedPiece>,
    pub unreadable_directories: Vec<RelPath>,
    /// What could not be read of the store, as the text of the note.
    pub store_error: Option<String>,
    pub damaged: Vec<VersionNote>,
    pub incomplete: Vec<VersionNote>,
    pub newer_format: Vec<VersionNote>,
    pub recovered: Vec<RelPath>,
    pub trash: Vec<RelPath>,
    pub doomed: Vec<Doomed>,
    pub spelling: Vec<SpellingDifference>,
    pub unparsable: Vec<RelPath>,
    pub warnings: Vec<Warning>,
    /// The run that holds the lock.
    pub holder: Option<Holder>,
}

/// Reads the collection and reports its state at `now`. Nothing is written.
pub fn status(collection: &Collection, now: Timestamp) -> Result<StatusReport, StatusError> {
    let surveyed = survey::survey(collection, now)?;
    let config = &collection.config().collection;
    let live: Vec<RelPath> = surveyed.pieces.iter().map(|p| p.piece.clone()).collect();
    let mut report = StatusReport {
        folders: surveyed.pieces.len(),
        idle_days: config.idle_days,
        soon_days: config.soon_days,
        pieces: surveyed.pieces,
        unreadable_directories: surveyed.unreadable,
        warnings: report::setting_notes(collection),
        ..StatusReport::default()
    };

    // The lock is only read: taking it would make status a run of its own.
    match lock::holder(&collection.lock_path()) {
        Ok(holder) => report.holder = holder,
        Err(error) => report
            .warnings
            .push(Warning::new(format!("cannot read the lock file: {error}"))),
    }

    // A store that cannot be read costs the notes that need it, not the
    // table of live pieces: status is the command to ask what is wrong.
    let store = match scan(&collection.pieces_dir()) {
        Ok(store) => Some(store),
        Err(error) => {
            report.store_error = Some(store_note(&error));
            None
        }
    };
    if let Some(store) = &store {
        read_versions(collection, store, &mut report);
        report.unparsable = store
            .strays
            .iter()
            .map(|stray| path_of("pieces").concat(stray))
            .collect();
        read_spelling(collection, &live, &mut report);
    }
    read_leftovers(collection, store.as_ref(), &mut report);
    Ok(report)
}

fn path_of(text: &str) -> RelPath {
    RelPath::parse(text).expect("a fixed name is a valid path")
}

/// The note for a store that could not be read, naming the place the way
/// the user finds it below the collection.
fn store_note(error: &VersionsError) -> String {
    match error {
        VersionsError::Io { path, source, .. } => {
            let place = match path.as_str() {
                "." => "pieces".to_string(),
                absolute if absolute.starts_with('/') => absolute.to_string(),
                relative => format!("pieces/{relative}"),
            };
            format!("cannot read the store: {place}: {source}")
        }
        other => format!("cannot read the store: {other}"),
    }
}

// =========================================================
// The versions of the store
// =========================================================

fn read_versions(collection: &Collection, store: &StoreScan, report: &mut StatusReport) {
    report.versions = Some(
        store
            .pieces
            .iter()
            .flat_map(|piece| &piece.versions)
            .filter(|version| !version.is_incomplete())
            .count(),
    );

    // Folders with sidecars only are inspected too: their incomplete
    // versions are reported.
    let items: Vec<(RelPath, StoreVersion)> = store
        .pieces
        .iter()
        .chain(&store.sidecar_only)
        .flat_map(|piece| {
            piece
                .versions
                .iter()
                .map(|version| (piece.path.clone(), version.clone()))
        })
        .collect();
    let pieces_dir = collection.pieces_dir();
    let tmp_pack_dir = collection.tmp_dir().join("pack");
    let places = Places {
        pieces_dir: &pieces_dir,
        tmp_pack_dir: &tmp_pack_dir,
    };
    // The same width as `grep`: every core but two, at least one.
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .saturating_sub(2)
        .max(1);

    let mut bytes = 0u64;
    let results = inspect_all(&places, &items, Check::Full, workers);
    for ((piece, version), result) in items.into_iter().zip(results) {
        let note = |reasons: Vec<String>| VersionNote {
            piece: piece.clone(),
            stamp: version.stamp,
            reasons,
        };
        let inspection = match result {
            Ok(inspection) => inspection,
            Err(error) => {
                report.warnings.push(Warning::new(format!(
                    "cannot inspect {piece} {}: {error}",
                    version.stamp
                )));
                continue;
            }
        };
        if let Some(recorded) = inspection.head.as_ref().and_then(|head| head.archive_bytes) {
            bytes = bytes.saturating_add(recorded);
        }
        match inspection.condition {
            Condition::Sound => {}
            Condition::Incomplete => report.incomplete.push(note(Vec::new())),
            // A running `archive` or repair 1 owns the version.
            Condition::BeingStored => {}
            Condition::NewerFormat => report.newer_format.push(note(Vec::new())),
            Condition::Damaged(damages) => {
                report
                    .damaged
                    .push(note(damages.iter().map(reason).collect()));
            }
        }
    }
    report.archive_bytes = Some(bytes);
    for notes in [
        &mut report.damaged,
        &mut report.incomplete,
        &mut report.newer_format,
    ] {
        notes.sort_by(|a, b| a.piece.cmp(&b.piece).then(a.stamp.cmp(&b.stamp)));
    }
}

/// How the table of notes words a damage: the two missing files in a few
/// words, everything else as the damage describes itself.
fn reason(damage: &Damage) -> String {
    match damage {
        Damage::NoListing => "no listing".to_string(),
        Damage::NoChecksum => "no checksum file".to_string(),
        other => other.to_string(),
    }
}

// =========================================================
// Spelling
// =========================================================

type Lookup<'a> = &'a dyn Fn(&RelPath) -> Result<RelPath, ReferenceError>;

fn read_spelling(collection: &Collection, live: &[RelPath], report: &mut StatusReport) {
    // The scan opened the same directory, so a store that cannot be opened
    // now is one that is not there, and no piece has a stored spelling.
    let Ok(store) = TreeRoot::open(&collection.pieces_dir()) else {
        return;
    };
    let (differences, warnings) =
        spelling_differences(live, &|piece| stored_spelling(&store, piece));
    report.spelling = differences;
    report.warnings.extend(warnings);
}

/// The live pieces whose stored spelling differs, and a warning for each
/// piece whose spelling could not be looked up. A failed lookup is never
/// read as "no difference".
fn spelling_differences(
    pieces: &[RelPath],
    lookup: Lookup<'_>,
) -> (Vec<SpellingDifference>, Vec<Warning>) {
    let mut differences = Vec::new();
    let mut warnings = Vec::new();
    for piece in pieces {
        match lookup(piece) {
            Ok(stored) if &stored != piece => differences.push(SpellingDifference {
                live: piece.clone(),
                stored,
            }),
            Ok(_) => {}
            Err(error) => warnings.push(Warning::new(format!(
                "cannot compare the spelling of {piece}: {error}"
            ))),
        }
    }
    (differences, warnings)
}

// =========================================================
// What trash/, recovered/ and doomed/ hold
// =========================================================

fn read_leftovers(collection: &Collection, store: Option<&StoreScan>, report: &mut StatusReport) {
    for kind in ["recovered", "trash", "doomed"] {
        let opened = TreeRoot::open(&collection.antiq_dir().join(kind)).and_then(|root| {
            let stamps = root.read_dir(&RelPath::root())?;
            Ok((root, stamps))
        });
        let (root, stamps) = match opened {
            Ok(opened) => opened,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                report
                    .warnings
                    .push(Warning::new(format!("cannot read {kind}/: {error}")));
                continue;
            }
        };
        let (trees, warnings) = trees_in(&root, kind, stamps, store);
        report.warnings.extend(warnings);
        match kind {
            "recovered" => report.recovered = trees,
            "trash" => report.trash = trees,
            _ => report.doomed.extend(
                trees
                    .into_iter()
                    .filter_map(|tree| doomed_leftover(&root, tree)),
            ),
        }
    }
}

/// The trees below `<kind>/<stamp>/`, each named with `kind/` in front.
/// A directory chain that holds no other entry is no tree: the repairs
/// remove those as they are.
fn trees_in(
    root: &TreeRoot,
    kind: &str,
    stamps: Vec<DirEntry>,
    store: Option<&StoreScan>,
) -> (Vec<RelPath>, Vec<Warning>) {
    let mut trees = BTreeSet::new();
    let mut warnings = Vec::new();
    for entry in stamps {
        let prefix = path_of(kind);
        if !is_directory(root, &entry.path, entry.kind) {
            trees.insert(prefix.concat(&entry.path));
            continue;
        }
        let stamp = std::str::from_utf8(entry.name.as_bytes())
            .ok()
            .and_then(|name| name.parse::<Stamp>().ok());
        // Repair 4 finds the trees of a stamp folder the same way. A folder
        // that cannot be read is reported by the walk below.
        let pieces = stamp
            .as_ref()
            .zip(store)
            .map(|(stamp, store)| trees_of(root, &entry.path, store, stamp).unwrap_or_default())
            .unwrap_or_default();
        let mut named: Vec<RelPath> = Vec::new();
        let mut pending = vec![entry.path.clone()];
        while let Some(directory) = pending.pop() {
            let listed = match root.read_dir(&directory) {
                Ok(listed) => listed,
                Err(error) => {
                    warnings.push(Warning::new(format!(
                        "cannot read {kind}/{directory}: {error}"
                    )));
                    continue;
                }
            };
            for found in listed {
                if named.iter().any(|tree| is_below(&found.path, tree)) {
                    continue;
                }
                if is_directory(root, &found.path, found.kind) {
                    pending.push(found.path);
                    continue;
                }
                let inner = below(&found.path, &entry.path);
                let tree_inner = pieces
                    .iter()
                    .find(|tree| is_below(&inner, tree))
                    .cloned()
                    .or_else(|| (inner.components().count() > 1).then(|| first_component(&inner)))
                    .unwrap_or(inner);
                named.push(entry.path.concat(&tree_inner));
            }
        }
        trees.extend(named.into_iter().map(|tree| prefix.concat(&tree)));
    }
    (trees.into_iter().collect(), warnings)
}

fn is_directory(root: &TreeRoot, path: &RelPath, reported: EntryKind) -> bool {
    match reported {
        EntryKind::Unknown => root
            .lstat(path)
            .is_ok_and(|entry| entry.kind == EntryKind::Directory),
        kind => kind == EntryKind::Directory,
    }
}

/// Whether `path` is `tree` or lies below it.
fn is_below(path: &RelPath, tree: &RelPath) -> bool {
    let (path, tree) = (path.as_bytes(), tree.as_bytes());
    path.starts_with(tree) && (path.len() == tree.len() || path[tree.len()] == b'/')
}

/// `path` without its leading `base`.
fn below(path: &RelPath, base: &RelPath) -> RelPath {
    let rest = &path.as_bytes()[base.as_bytes().len()..];
    RelPath::from_bytes(rest.strip_prefix(b"/").unwrap_or(rest).to_vec())
        .expect("what follows a path in a longer path is a path")
}

fn first_component(path: &RelPath) -> RelPath {
    let first = path.components().next().expect("the path has a name");
    RelPath::from_bytes(first.to_vec()).expect("a name is a path")
}

fn parent_of(path: &RelPath) -> RelPath {
    let names: Vec<&[u8]> = path.components().collect();
    RelPath::from_bytes(names[..names.len().saturating_sub(1)].join(&b'/'))
        .expect("leading names of a path are a path")
}

/// The first entry of a tree in `doomed/` in the walk's order, which is the
/// byte order of the whole path, and what keeps it from being deleted.
/// `tree` is named with `doomed/` in front. A tree with no entry other than
/// directories has nothing left to delete. A directory that cannot be read
/// was reported when the tree was named.
fn doomed_leftover(root: &TreeRoot, tree: RelPath) -> Option<Doomed> {
    let mut entries = Vec::new();
    let mut pending = vec![below(&tree, &path_of("doomed"))];
    while let Some(directory) = pending.pop() {
        let Ok(listed) = root.read_dir(&directory) else {
            continue;
        };
        for found in listed {
            if is_directory(root, &found.path, found.kind) {
                pending.push(found.path);
            } else {
                entries.push(found.path);
            }
        }
    }
    let first = entries.into_iter().min()?;
    let locked = root.lstat(&first).is_ok_and(|entry| entry.flag_locked);
    let cause = if locked {
        Some(DoomedCause::Locked)
    } else if root
        .user_can_write(&parent_of(&first))
        .is_ok_and(|can| !can)
    {
        Some(DoomedCause::DirectoryNotWritable)
    } else {
        None
    };
    Some(Doomed {
        path: tree,
        first_entry: path_of("doomed").concat(&first),
        cause,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::{COLLECTION_ID, TempCollection, Tree};
    use crate::testing::versions::Store;
    use crate::versions::names::VersionFile;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    const DAY: i64 = 86_400;
    const NOW: i64 = 2_000_000_000;
    const STAMP: &str = "2026-11-04T090211";
    const OTHER_STAMP: &str = "2026-11-05T101500";

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn stamp(text: &str) -> Stamp {
        text.parse().expect("a valid stamp")
    }

    fn now() -> Timestamp {
        Timestamp::from_second(NOW).expect("in range")
    }

    fn world(extra: &str) -> TempCollection {
        let temp = TempCollection::minimal();
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{extra}\n"),
        )
        .expect("the config is written");
        temp
    }

    fn reported(temp: &TempCollection) -> StatusReport {
        let collection = Collection::find(temp.root()).expect("a collection");
        status(&collection, now()).expect("status succeeds")
    }

    /// A source tree holding the piece `name` with two files.
    fn source(name: &str) -> Tree {
        let mut tree = Tree::new();
        tree.file(&format!("{name}/a.txt"), "alpha");
        tree.file(&format!("{name}/sub/b.txt"), "beta");
        tree
    }

    /// Moves every folder of the fixture store into the collection's store.
    fn put(temp: &TempCollection, store: &Store) {
        let target = temp.root().join(".antiq/pieces");
        for entry in fs::read_dir(store.pieces_dir()).expect("the store lists") {
            let entry = entry.expect("an entry");
            fs::rename(entry.path(), target.join(entry.file_name())).expect("the folder moves");
        }
    }

    fn antiq(temp: &TempCollection, relative: &str) -> std::path::PathBuf {
        temp.root().join(".antiq").join(relative)
    }

    fn assert_not_root(directory: &Path) {
        assert!(
            fs::read_dir(directory).is_err(),
            "the test needs a user that mode 000 locks out"
        );
    }

    // =========================================================
    // The header
    // =========================================================

    #[test]
    fn the_header_counts_folders_and_versions_and_sums_the_recorded_archive_sizes() {
        let mut temp = world("");
        temp.tree().file_with("live/f", "x", 0o644, NOW - DAY);
        temp.tree().file_with("other/f", "x", 0o644, NOW - DAY);
        let mut store = Store::new();
        let first = store.add_version(&source("notes"), "notes", STAMP);
        let second = store.add_version(&source("notes"), "notes", OTHER_STAMP);
        let sizes: u64 = [&first, &second]
            .iter()
            .map(|version| {
                fs::metadata(version.path(VersionFile::Archive))
                    .expect("the archive exists")
                    .len()
            })
            .sum();
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(found.folders, 2);
        assert_eq!(found.versions, Some(2));
        assert_eq!(found.archive_bytes, Some(sizes));
        assert_eq!((found.idle_days, found.soon_days), (30, 3));
        assert_eq!(found.store_error, None);
    }

    #[test]
    fn an_empty_store_has_no_versions_and_no_bytes() {
        let mut temp = world("");
        temp.tree().file_with("live/f", "x", 0o644, NOW - DAY);

        let found = reported(&temp);

        assert_eq!(found.versions, Some(0));
        assert_eq!(found.archive_bytes, Some(0));
    }

    // =========================================================
    // Damaged, incomplete and newer versions
    // =========================================================

    #[test]
    fn a_version_without_a_listing_or_checksum_file_is_damaged_in_short_words() {
        let temp = world("");
        let mut store = Store::new();
        let no_listing = store.add_version(&source("notes"), "notes", STAMP);
        no_listing.remove(VersionFile::Listing);
        let no_checksum = store.add_version(&source("notes"), "notes", OTHER_STAMP);
        no_checksum.remove(VersionFile::Checksum);
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(
            found.damaged,
            [
                VersionNote {
                    piece: path("notes"),
                    stamp: stamp(STAMP),
                    reasons: vec!["no listing".to_string()],
                },
                VersionNote {
                    piece: path("notes"),
                    stamp: stamp(OTHER_STAMP),
                    reasons: vec!["no checksum file".to_string()],
                },
            ]
        );
    }

    #[test]
    fn a_truncated_archive_is_damaged_and_the_header_sums_the_recorded_size() {
        let temp = world("");
        let mut store = Store::new();
        let version = store.add_version(&source("notes"), "notes", STAMP);
        let recorded = fs::metadata(version.path(VersionFile::Archive))
            .expect("the archive exists")
            .len();
        version.truncate(VersionFile::Archive, recorded - 4);
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(found.damaged.len(), 1);
        assert_eq!(
            found.damaged[0].reasons,
            [format!(
                "the archive has {} bytes, its listing records {recorded}",
                recorded - 4
            )]
        );
        assert_eq!(found.archive_bytes, Some(recorded));
        assert_eq!(found.versions, Some(1));
    }

    #[test]
    fn a_listing_that_does_not_decode_to_its_end_is_damaged() {
        let temp = world("");
        let mut store = Store::new();
        let version = store.add_version(&source("notes"), "notes", STAMP);
        version.cut_listing();
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(found.damaged.len(), 1);
        assert!(
            found.damaged[0].reasons[0].starts_with("the listing is damaged"),
            "{:?}",
            found.damaged[0].reasons
        );
    }

    #[test]
    fn two_files_in_one_slot_make_a_damaged_version_naming_both() {
        let temp = world("");
        let mut store = Store::new();
        let version = store.add_version(&source("notes"), "notes", STAMP);
        version.add_twin(VersionFile::Checksum, "twin");
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(found.damaged.len(), 1);
        assert!(
            found.damaged[0].reasons[0].starts_with("2 files for the checksum file"),
            "{:?}",
            found.damaged[0].reasons
        );
    }

    #[test]
    fn sidecars_without_an_archive_are_an_incomplete_version_and_no_version() {
        let temp = world("");
        let mut store = Store::new();
        let gone = store.add_version(&source("notes"), "notes", STAMP);
        gone.remove(VersionFile::Archive);
        store.add_version(&source("notes"), "notes", OTHER_STAMP);
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(
            found.incomplete,
            [VersionNote {
                piece: path("notes"),
                stamp: stamp(STAMP),
                reasons: Vec::new(),
            }]
        );
        assert_eq!(found.versions, Some(1));
        assert!(found.damaged.is_empty());
    }

    #[test]
    fn sidecars_whose_archive_is_staged_are_a_running_archive_and_not_reported() {
        let temp = world("");
        let mut store = Store::new();
        let moving = store.add_version(&source("notes"), "notes", STAMP);
        moving.stage_archive(&store);
        moving.remove(VersionFile::Archive);
        fs::create_dir_all(antiq(&temp, "tmp/pack")).expect("the staging folder is made");
        fs::rename(
            store.tmp_pack_dir().join(STAMP),
            antiq(&temp, "tmp/pack").join(STAMP),
        )
        .expect("the staging moves");
        put(&temp, &store);

        let found = reported(&temp);

        assert!(found.incomplete.is_empty());
        assert!(found.damaged.is_empty());
    }

    #[test]
    fn a_sidecar_only_folder_reports_its_incomplete_versions() {
        let temp = world("");
        let mut store = Store::new();
        let version = store.add_version(&source("notes"), "notes", STAMP);
        version.remove(VersionFile::Archive);
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(found.incomplete.len(), 1);
        assert_eq!(found.versions, Some(0));
    }

    #[test]
    fn a_listing_of_a_newer_format_is_named_and_neither_damaged_nor_incomplete() {
        let temp = world("");
        let mut store = Store::new();
        let version = store.add_version(&source("notes"), "notes", STAMP);
        version.edit_listing(|text| text.replacen("antiq listing 1", "antiq listing 2", 1));
        put(&temp, &store);

        let found = reported(&temp);

        assert_eq!(found.newer_format.len(), 1);
        assert_eq!(found.newer_format[0].stamp, stamp(STAMP));
        assert!(found.damaged.is_empty());
        assert_eq!(found.versions, Some(1));
    }

    #[test]
    fn a_version_that_cannot_be_inspected_is_a_warning_and_stays_counted() {
        let temp = world("");
        let mut store = Store::new();
        let version = store.add_version(&source("notes"), "notes", STAMP);
        let listing = version.path(VersionFile::Listing);
        fs::set_permissions(&listing, fs::Permissions::from_mode(0o000)).expect("the mode is set");
        assert!(
            fs::File::open(&listing).is_err(),
            "the test needs a user mode 000 locks out"
        );
        put(&temp, &store);

        let found = reported(&temp);
        let moved = antiq(&temp, "pieces/notes").join(version.names.of(VersionFile::Listing));
        fs::set_permissions(moved, fs::Permissions::from_mode(0o644)).expect("restores");

        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0]
                .message
                .starts_with("cannot inspect notes 2026-11-04T090211"),
            "{}",
            found.warnings[0].message
        );
        assert_eq!(found.versions, Some(1));
    }

    // =========================================================
    // The store itself
    // =========================================================

    #[test]
    fn a_file_in_a_store_folder_that_is_no_version_file_is_listed_below_antiq() {
        let temp = world("");
        let mut store = Store::new();
        store.add_version(&source("notes"), "notes", STAMP);
        put(&temp, &store);
        fs::write(antiq(&temp, "pieces/notes/readme.txt"), "x").expect("a stray file");

        let found = reported(&temp);

        assert_eq!(found.unparsable, [path("pieces/notes/readme.txt")]);
    }

    #[test]
    fn a_store_note_names_the_place_below_the_collection_and_any_other_error_as_it_is() {
        let io = |path: &str| VersionsError::Io {
            action: "list",
            path: path.to_string(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };

        assert_eq!(
            store_note(&io("notes")),
            "cannot read the store: pieces/notes: permission denied"
        );
        assert_eq!(
            store_note(&io(".")),
            "cannot read the store: pieces: permission denied"
        );
        assert_eq!(
            store_note(&io("/work/.antiq/pieces")),
            "cannot read the store: /work/.antiq/pieces: permission denied"
        );
        assert_eq!(
            store_note(&VersionsError::InvalidStamp("x".to_string())),
            "cannot read the store: `x` is not a version stamp"
        );
    }

    #[test]
    fn a_collection_without_a_store_folder_has_no_versions_and_no_spelling() {
        let mut temp = world("");
        temp.tree().file_with("live/f", "x", 0o644, NOW - DAY);
        fs::remove_dir(antiq(&temp, "pieces")).expect("the store folder is removed");

        let found = reported(&temp);

        assert_eq!(found.versions, Some(0));
        assert!(found.spelling.is_empty());
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    #[test]
    fn a_store_that_cannot_be_read_is_a_note_and_the_live_pieces_stay() {
        let mut temp = world("");
        temp.tree().file_with("live/f", "x", 0o644, NOW - 40 * DAY);
        let mut store = Store::new();
        store.add_version(&source("notes"), "notes", STAMP);
        put(&temp, &store);
        let folder = antiq(&temp, "pieces/notes");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).expect("the mode is set");
        let locked_out = fs::read_dir(&folder).is_err();

        let found = reported(&temp);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).expect("restores");

        assert!(locked_out, "the test needs a user that mode 000 locks out");
        let note = found.store_error.expect("the store note");
        assert!(
            note.starts_with("cannot read the store: ") && note.contains("notes"),
            "{note}"
        );
        assert_eq!((found.versions, found.archive_bytes), (None, None));
        assert_eq!(found.folders, 1);
        assert!(found.damaged.is_empty() && found.spelling.is_empty());
    }

    // =========================================================
    // Leftovers in trash/, recovered/ and doomed/
    // =========================================================

    fn leftover(temp: &TempCollection, relative: &str) {
        let file = antiq(temp, relative);
        fs::create_dir_all(file.parent().expect("a parent")).expect("the chain is made");
        fs::write(file, "x").expect("the file is written");
    }

    #[test]
    fn the_leftovers_stay_when_the_store_cannot_be_read_each_named_by_its_first_component() {
        let temp = world("");
        let mut store = Store::new();
        store.add_version(&source("projects/scanner"), "projects/scanner", STAMP);
        put(&temp, &store);
        for kind in ["recovered", "trash", "doomed"] {
            leftover(
                &temp,
                &format!("{kind}/2026-11-04T090211/projects/scanner/a.txt"),
            );
        }
        let folder = antiq(&temp, "pieces/projects/scanner");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).expect("the mode is set");
        let locked_out = fs::read_dir(&folder).is_err();

        let found = reported(&temp);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).expect("restores");

        assert!(locked_out, "the test needs a user that mode 000 locks out");
        assert!(found.store_error.is_some());
        assert_eq!(
            found.recovered,
            [path("recovered/2026-11-04T090211/projects")]
        );
        assert_eq!(found.trash, [path("trash/2026-11-04T090211/projects")]);
        assert_eq!(found.doomed.len(), 1);
        assert_eq!(
            found.doomed[0].path,
            path("doomed/2026-11-04T090211/projects")
        );
    }

    #[test]
    fn a_tree_in_trash_is_named_by_the_piece_whose_version_has_its_stamp() {
        let temp = world("");
        let mut store = Store::new();
        store.add_version(&source("projects/scanner"), "projects/scanner", STAMP);
        put(&temp, &store);
        leftover(&temp, "trash/2026-11-04T090211/projects/scanner/a.txt");
        leftover(&temp, "trash/2026-11-04T090211/projects/scanner/sub/b.txt");

        let found = reported(&temp);

        assert_eq!(
            found.trash,
            [path("trash/2026-11-04T090211/projects/scanner")]
        );
    }

    #[test]
    fn a_tree_whose_stamp_names_no_version_is_named_by_its_first_component() {
        let temp = world("");
        leftover(&temp, "recovered/2026-11-04T090211/projects/scanner/a.txt");

        let found = reported(&temp);

        assert_eq!(
            found.recovered,
            [path("recovered/2026-11-04T090211/projects")]
        );
    }

    #[test]
    fn two_trees_of_one_stamp_are_named_apart() {
        let temp = world("");
        let mut store = Store::new();
        store.add_version(&source("a"), "a", STAMP);
        store.add_version(&source("b"), "b", STAMP);
        put(&temp, &store);
        leftover(&temp, "trash/2026-11-04T090211/b/x");
        leftover(&temp, "trash/2026-11-04T090211/a/x");

        let found = reported(&temp);

        assert_eq!(
            found.trash,
            [
                path("trash/2026-11-04T090211/a"),
                path("trash/2026-11-04T090211/b")
            ]
        );
    }

    #[test]
    fn a_chain_of_empty_directories_is_no_leftover_and_a_missing_directory_is_none() {
        let temp = world("");
        fs::create_dir_all(antiq(&temp, "trash/2026-11-04T090211/projects/scanner"))
            .expect("the chain is made");
        fs::remove_dir(antiq(&temp, "doomed")).expect("doomed is removed");

        let found = reported(&temp);

        assert!(found.trash.is_empty());
        assert!(found.doomed.is_empty());
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    #[test]
    fn a_directory_that_cannot_be_read_is_a_warning_naming_it() {
        let temp = world("");
        let trash = antiq(&temp, "trash");
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o000)).expect("the mode is set");
        let locked_out = fs::read_dir(&trash).is_err();

        let found = reported(&temp);
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).expect("restores");

        assert!(locked_out, "the test needs a user that mode 000 locks out");
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0].message.starts_with("cannot read trash/"),
            "{}",
            found.warnings[0].message
        );
        assert!(found.trash.is_empty());
    }

    #[test]
    fn a_file_directly_in_a_leftover_directory_is_its_own_tree() {
        let temp = world("");
        leftover(&temp, "trash/2026-11-04T090211/loose.txt");
        leftover(&temp, "trash/stray.txt");

        let found = reported(&temp);

        assert_eq!(
            found.trash,
            [
                path("trash/2026-11-04T090211/loose.txt"),
                path("trash/stray.txt")
            ]
        );
    }

    #[test]
    fn a_directory_inside_a_leftover_that_cannot_be_read_is_a_warning_and_the_rest_is_named() {
        let temp = world("");
        leftover(&temp, "doomed/2026-11-04T090211/notes/f");
        let locked = antiq(&temp, "doomed/2026-11-04T090211/notes/locked");
        fs::create_dir(&locked).expect("the directory is made");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode is set");
        let locked_out = fs::read_dir(&locked).is_err();

        let found = reported(&temp);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restores");

        assert!(locked_out, "the test needs a user that mode 000 locks out");
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0]
                .message
                .starts_with("cannot read doomed/2026-11-04T090211/notes/locked: "),
            "{}",
            found.warnings[0].message
        );
        assert_eq!(found.doomed.len(), 1);
        assert_eq!(
            found.doomed[0].first_entry,
            path("doomed/2026-11-04T090211/notes/f")
        );
    }

    #[test]
    fn a_doomed_tree_names_the_first_entry_in_path_byte_order() {
        let temp = world("");
        // `dir-y` sorts before `dir/x`: `-` is 0x2D and `/` is 0x2F.
        leftover(&temp, "doomed/2026-11-04T090211/notes/dir/x");
        leftover(&temp, "doomed/2026-11-04T090211/notes/dir-y");

        let found = reported(&temp);

        assert_eq!(
            found.doomed,
            [Doomed {
                path: path("doomed/2026-11-04T090211/notes"),
                first_entry: path("doomed/2026-11-04T090211/notes/dir-y"),
                cause: None,
            }]
        );
    }

    #[test]
    fn a_doomed_entry_in_a_directory_the_user_cannot_write_names_that_cause() {
        let temp = world("");
        leftover(&temp, "doomed/2026-11-04T090211/notes/ro/f");
        let directory = antiq(&temp, "doomed/2026-11-04T090211/notes/ro");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).expect("the mode");
        let cannot_write = fs::File::create(directory.join("probe")).is_err();

        let found = reported(&temp);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).expect("restores");

        assert!(
            cannot_write,
            "the test needs a user that mode 500 keeps from writing"
        );
        assert_eq!(found.doomed.len(), 1);
        assert_eq!(
            found.doomed[0].cause,
            Some(DoomedCause::DirectoryNotWritable)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_doomed_entry_with_an_immutable_flag_names_the_flag() {
        let mut temp = world("");
        leftover(&temp, "doomed/2026-11-04T090211/notes/f");
        temp.tree()
            .immutable(".antiq/doomed/2026-11-04T090211/notes/f");

        let found = reported(&temp);

        assert_eq!(found.doomed.len(), 1);
        assert_eq!(found.doomed[0].cause, Some(DoomedCause::Locked));
    }

    // =========================================================
    // Spelling
    // =========================================================

    fn folds_case(directory: &Path) -> bool {
        let probe = directory.join("CaseProbe");
        fs::create_dir(&probe).expect("the probe is made");
        let folds = directory.join("caseprobe").exists();
        fs::remove_dir(&probe).expect("the probe is removed");
        folds
    }

    #[test]
    fn a_live_folder_spelled_differently_from_the_store_is_listed_on_a_folding_volume() {
        let mut temp = world("");
        temp.tree().file_with("projects/f", "x", 0o644, NOW - DAY);
        let mut store = Store::new();
        store.add_version(&source("Projects"), "Projects", STAMP);
        put(&temp, &store);
        let folds = folds_case(&antiq(&temp, "pieces"));

        let found = reported(&temp);

        if folds {
            assert_eq!(
                found.spelling,
                [SpellingDifference {
                    live: path("projects"),
                    stored: path("Projects"),
                }]
            );
        } else {
            assert!(found.spelling.is_empty());
        }
    }

    #[test]
    fn a_failing_lookup_of_a_spelling_is_a_warning_and_no_difference() {
        use crate::reference::ReferenceError;
        let pieces = [path("a"), path("b"), path("c")];
        let lookup = |piece: &RelPath| match piece.to_string().as_str() {
            "a" => Ok(path("a")),
            "b" => Ok(path("B")),
            _ => Err(ReferenceError::Unknown {
                reference: "c".to_string(),
            }),
        };

        let (differences, warnings) = spelling_differences(&pieces, &lookup);

        assert_eq!(
            differences,
            [SpellingDifference {
                live: path("b"),
                stored: path("B"),
            }]
        );
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .message
                .starts_with("cannot compare the spelling of c: "),
            "{}",
            warnings[0].message
        );
    }

    // =========================================================
    // The lock and the settings
    // =========================================================

    #[test]
    fn a_held_lock_names_the_holder_and_status_leaves_the_lock_alone() {
        let temp = world("");
        let collection = Collection::find(temp.root()).expect("a collection");
        let _held = crate::lock::acquire(
            &collection.lock_path(),
            "imperius",
            &crate::clock::SystemClock,
        )
        .expect("the lock is free");

        let found = status(&collection, now()).expect("status succeeds");

        let holder = found.holder.expect("the holder is named");
        assert_eq!(holder.pid, std::process::id());
        assert_eq!(holder.machine, "imperius");
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    #[test]
    fn a_lock_file_that_cannot_be_read_is_a_warning() {
        let temp = world("");
        fs::create_dir(antiq(&temp, "lock")).expect("a directory in the lock's place");

        let found = reported(&temp);

        assert_eq!(found.holder.map(|holder| holder.pid), None);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert!(
            found.warnings[0]
                .message
                .starts_with("cannot read the lock file: "),
            "{}",
            found.warnings[0].message
        );
    }

    #[test]
    fn the_notes_about_the_config_are_warnings() {
        let temp = world("idle_dyas = 3");

        let found = reported(&temp);

        assert_eq!(
            found.warnings,
            [Warning::new(
                "the key `idle_dyas` under [collection] is unknown and ignored"
            )]
        );
    }

    #[test]
    fn a_directory_selection_cannot_read_is_named() {
        let mut temp = world("patterns = [\"*/*\"]");
        temp.tree().file_with("ok/piece/f", "x", 0o644, NOW - DAY);
        assert_not_root(&temp.tree().unreadable_dir("area"));

        let found = reported(&temp);

        assert_eq!(found.unreadable_directories, [path("area")]);
    }
}
