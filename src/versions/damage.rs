// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The classification of incomplete and damaged versions.

use super::VersionsError;
use super::names::VersionFile;
use super::store::{StoreVersion, is_being_stored};
use crate::archive::PackError;
use crate::archive::check::ChecksumFile;
use crate::filesystem::tree::TreeRoot;
use crate::listing::{self, Head, ListingError};
use crate::relpath::{Name, RelPath};
use crate::shown::Shown;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// How much of a version `inspect` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The head of the listing, the checksum file and the size of the
    /// archive.
    Head,
    /// Everything `Head` reads, and the whole listing: decoded to its end,
    /// parsed to its end, with the counts of its head checked.
    Full,
}

/// The two folders `inspect` looks in.
#[derive(Debug, Clone, Copy)]
pub struct Places<'a> {
    /// `pieces/`, the store.
    pub pieces_dir: &'a Path,
    /// `tmp/pack/`, where an archive is staged on its way in.
    pub tmp_pack_dir: &'a Path,
}

/// What `inspect` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Sound,
    /// Sidecars without a `.tar.zst`, and no archive staged for it.
    Incomplete,
    /// Sidecars without a `.tar.zst`, whose archive is staged: the move
    /// of a running `archive` or of repair 1.
    BeingStored,
    /// The listing names a higher format than antiq reads.
    NewerFormat,
    Damaged(Vec<Damage>),
}

/// One way a version is damaged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    /// Two or more files in one slot.
    Doubled {
        kind: VersionFile,
        files: Vec<OsString>,
    },
    NoListing,
    NoChecksum,
    /// The head of the listing cannot be read or lacks a line.
    Head {
        reason: String,
    },
    /// The listing does not decode or parse to its end, or its counts
    /// differ from its entries.
    Listing {
        reason: String,
    },
    /// The archive has another size than its listing records.
    ArchiveSize {
        found: u64,
        recorded: u64,
    },
    /// The checksum file is not exactly one line for the archive.
    Checksum {
        reason: String,
    },
}

impl fmt::Display for Damage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Damage::Doubled { kind, files } => {
                let names: Vec<String> = files
                    .iter()
                    .map(|file| format!("`{}`", Shown(file)))
                    .collect();
                write!(
                    f,
                    "{} files for the {}: {}",
                    files.len(),
                    slot(*kind),
                    names.join(", ")
                )
            }
            Damage::NoListing => write!(f, "the listing is missing"),
            Damage::NoChecksum => write!(f, "the checksum file is missing"),
            Damage::Head { reason } => write!(f, "the head of the listing is damaged: {reason}"),
            Damage::ArchiveSize { found, recorded } => write!(
                f,
                "the archive has {found} bytes, its listing records {recorded}"
            ),
            Damage::Listing { reason } => write!(f, "the listing is damaged: {reason}"),
            Damage::Checksum { reason } => write!(f, "the checksum file is damaged: {reason}"),
        }
    }
}

fn slot(kind: VersionFile) -> &'static str {
    match kind {
        VersionFile::Archive => "archive",
        VersionFile::Listing => "listing",
        VersionFile::Checksum => "checksum file",
    }
}

/// The condition of a version and, when its listing was readable, the
/// head, so a caller that lists the version does not read the listing
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    pub condition: Condition,
    pub head: Option<Head>,
}

/// Classifies one version of `piece`, as a scan of the store found it.
///
/// The files are read without taking the run lock, so any of them may
/// vanish between the scan and the read. A sidecar that does is missing; a
/// version whose archive does is classified as one found without it.
///
/// Presence damage and a damaged checksum file are decided before the
/// format of the listing: they do not depend on it, so a newer format
/// hides neither. A listing, an archive or a checksum file that cannot be
/// opened for another reason than being absent is an error, not damage of
/// the version.
pub fn inspect(
    places: &Places,
    piece: &RelPath,
    version: &StoreVersion,
    check: Check,
) -> Result<Inspection, VersionsError> {
    if version.archives.is_empty() {
        return without_archive(places, piece, version);
    }

    // With two files in a slot no file of the version can be picked, so
    // nothing else is read.
    let doubled: Vec<Damage> = VersionFile::ALL
        .into_iter()
        .filter(|kind| version.files(*kind).len() > 1)
        .map(|kind| Damage::Doubled {
            kind,
            files: version.files(kind).to_vec(),
        })
        .collect();
    if !doubled.is_empty() {
        return Ok(Inspection {
            condition: Condition::Damaged(doubled),
            head: None,
        });
    }

    let store = TreeRoot::open(places.pieces_dir)
        .map_err(|source| io_error("open", Shown(&places.pieces_dir).to_string(), source))?;
    match examine(&store, piece, version, check)? {
        Some(inspection) => Ok(inspection),
        None => without_archive(places, piece, version),
    }
}

/// Inspects every item with `workers` threads (at least one, at most one
/// per item). The results come in the order of `items`.
pub fn inspect_all(
    places: &Places,
    items: &[(RelPath, StoreVersion)],
    check: Check,
    workers: usize,
) -> Vec<Result<Inspection, VersionsError>> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Result<Inspection, VersionsError>>>> =
        Mutex::new(items.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, items.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some((piece, version)) = items.get(index) else {
                        break;
                    };
                    let found = inspect(places, piece, version, check);
                    let mut results = results.lock().unwrap_or_else(PoisonError::into_inner);
                    results[index] = Some(found);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .map(|found| found.expect("every item was inspected"))
        .collect()
}

/// A version found without a `.tar.zst`: being stored when the archive is
/// staged, incomplete otherwise.
fn without_archive(
    places: &Places,
    piece: &RelPath,
    version: &StoreVersion,
) -> Result<Inspection, VersionsError> {
    let condition = if is_being_stored(places.tmp_pack_dir, piece, &version.stamp)? {
        Condition::BeingStored
    } else {
        Condition::Incomplete
    };
    Ok(Inspection {
        condition,
        head: None,
    })
}

/// Reads the files of a version that has exactly one archive. `None` when
/// the archive is gone by the time it is looked at.
///
/// The archive is looked at last. `forget` removes it first, so a sidecar
/// that is missing because the version is being forgotten shows up as an
/// archive that is gone too.
fn examine(
    store: &TreeRoot,
    piece: &RelPath,
    version: &StoreVersion,
    check: Check,
) -> Result<Option<Inspection>, VersionsError> {
    let mut of_listing = Vec::new();
    let mut of_checksum = Vec::new();
    let mut newer_format = false;
    let mut head = None;
    // The listing, when its head is sound and the whole of it is to be read.
    let mut to_decode = None;

    match open_sidecar(store, piece, version, VersionFile::Listing)? {
        None => of_listing.push(Damage::NoListing),
        Some((file, path)) => match listing::read_head(&file) {
            Err(ListingError::FormatTooNew { .. }) => newer_format = true,
            Err(error) => of_listing.push(Damage::Head {
                reason: error.to_string(),
            }),
            Ok(read) => {
                if let Some(reason) = read.damage() {
                    of_listing.push(Damage::Head { reason });
                } else {
                    to_decode = Some((file, path));
                }
                head = Some(read);
            }
        },
    }

    match open_sidecar(store, piece, version, VersionFile::Checksum)? {
        None => of_checksum.push(Damage::NoChecksum),
        Some((mut file, path)) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|source| io_error("read", path.to_string(), source))?;
            let archive_name = version.archives[0].to_string_lossy();
            if let Err(error) =
                ChecksumFile::parse(&bytes).and_then(|line| line.check_for(&archive_name))
            {
                let reason = match error {
                    PackError::ChecksumFile(reason) => reason,
                    other => other.to_string(),
                };
                of_checksum.push(Damage::Checksum { reason });
            }
        }
    }

    let archive = piece.join(&name_of(&version.archives[0]));
    let archive_size = match store.lstat(&archive) {
        Ok(entry) => entry.size,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("look at", archive.to_string(), source)),
    };
    // A damaged head is not read again: the whole listing would only
    // repeat it.
    if check == Check::Full
        && let Some((mut file, path)) = to_decode
    {
        file.rewind()
            .map_err(|source| io_error("read", path.to_string(), source))?;
        if let Err(error) = listing::read_full(&file) {
            of_listing.push(Damage::Listing {
                reason: error.to_string(),
            });
        }
    }
    let mut of_size = Vec::new();
    if let Some(recorded) = head.as_ref().and_then(|head| head.archive_bytes)
        && recorded != archive_size
    {
        of_size.push(Damage::ArchiveSize {
            found: archive_size,
            recorded,
        });
    }

    let damages: Vec<Damage> = [of_listing, of_size, of_checksum].concat();
    let condition = if !damages.is_empty() {
        Condition::Damaged(damages)
    } else if newer_format {
        Condition::NewerFormat
    } else {
        Condition::Sound
    };
    Ok(Some(Inspection { condition, head }))
}

/// The sidecar of `kind` opened for reading with its path, or `None` when
/// the scan found none or it is gone.
fn open_sidecar(
    store: &TreeRoot,
    piece: &RelPath,
    version: &StoreVersion,
    kind: VersionFile,
) -> Result<Option<(File, RelPath)>, VersionsError> {
    let Some(name) = version.files(kind).first() else {
        return Ok(None);
    };
    let path = piece.join(&name_of(name));
    match store.open_read(&path) {
        Ok(file) => Ok(Some((file, path))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("open", path.to_string(), source)),
    }
}

fn name_of(file: &OsString) -> Name {
    Name::from_bytes(file.as_bytes().to_vec()).expect("a scanned file name is a valid name")
}

fn io_error(action: &'static str, path: String, source: io::Error) -> VersionsError {
    VersionsError::Io {
        action,
        path,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::check::ChecksumFile;
    use crate::testing::tree::Tree;
    use crate::testing::versions::{Store, StoredVersion};
    use crate::versions::names::VersionFile::{Archive, Checksum, Listing};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const STAMP: &str = "2026-11-04T090211";

    fn sound() -> (Store, StoredVersion) {
        let mut source = Tree::new();
        source.file("notes/a.txt", "alpha");
        source.file("notes/sub/b.txt", "beta");
        let mut store = Store::new();
        let version = store.add_version(&source, "notes", STAMP);
        (store, version)
    }

    /// The version as a scan finds it now.
    fn scanned(store: &Store, version: &StoredVersion) -> StoreVersion {
        let scan = store.scan();
        scan.pieces
            .iter()
            .chain(&scan.sidecar_only)
            .flat_map(|piece| &piece.versions)
            .find(|found| found.stamp == version.stamp)
            .expect("the scan finds the version")
            .clone()
    }

    fn run(
        store: &Store,
        version: &StoredVersion,
        stale: &StoreVersion,
    ) -> Result<Inspection, VersionsError> {
        let _ = version;
        inspect(&store.places(), &version.piece, stale, Check::Head)
    }

    fn inspected(store: &Store, version: &StoredVersion) -> Inspection {
        run(store, version, &scanned(store, version)).expect("the version is inspected")
    }

    fn condition(store: &Store, version: &StoredVersion) -> Condition {
        inspected(store, version).condition
    }

    fn head_damage(condition: Condition) -> String {
        match condition {
            Condition::Damaged(damages) => match damages.as_slice() {
                [Damage::Head { reason }] => reason.clone(),
                other => panic!("expected one head damage, found {other:?}"),
            },
            other => panic!("expected damage, found {other:?}"),
        }
    }

    #[test]
    fn a_sound_version_is_sound_and_hands_over_its_head() {
        let (store, version) = sound();
        let found = inspected(&store, &version);
        assert_eq!(found.condition, Condition::Sound);
        let head = found.head.expect("the head was read");
        assert_eq!(head.files, Some(2));
        assert_eq!(
            head.piece.map(|piece| piece.to_string()),
            Some("notes".into())
        );
        let size = fs::metadata(version.path(Archive))
            .expect("the archive")
            .len();
        assert_eq!(head.archive_bytes, Some(size));
    }

    #[test]
    fn a_missing_listing_is_damage() {
        let (store, version) = sound();
        version.remove(Listing);
        assert_eq!(
            condition(&store, &version),
            Condition::Damaged(vec![Damage::NoListing])
        );
    }

    #[test]
    fn a_missing_checksum_file_is_damage() {
        let (store, version) = sound();
        version.remove(Checksum);
        assert_eq!(
            condition(&store, &version),
            Condition::Damaged(vec![Damage::NoChecksum])
        );
    }

    #[test]
    fn both_sidecars_missing_are_both_reported() {
        let (store, version) = sound();
        version.remove(Listing);
        version.remove(Checksum);
        assert_eq!(
            condition(&store, &version),
            Condition::Damaged(vec![Damage::NoListing, Damage::NoChecksum])
        );
    }

    #[test]
    fn an_archive_of_another_size_than_recorded_is_damage() {
        let (store, version) = sound();
        let recorded = fs::metadata(version.path(Archive))
            .expect("the archive")
            .len();
        version.truncate(Archive, 5);
        assert_eq!(
            condition(&store, &version),
            Condition::Damaged(vec![Damage::ArchiveSize { found: 5, recorded }])
        );
    }

    #[test]
    fn sidecars_without_an_archive_are_an_incomplete_version() {
        let (store, version) = sound();
        version.remove(Archive);
        assert_eq!(condition(&store, &version), Condition::Incomplete);
    }

    #[test]
    fn sidecars_whose_archive_is_staged_are_being_stored() {
        let (store, version) = sound();
        version.stage_archive(&store);
        version.remove(Archive);
        assert_eq!(condition(&store, &version), Condition::BeingStored);
    }

    #[test]
    fn an_archive_staged_for_another_stamp_does_not_count() {
        let (store, version) = sound();
        let mut source = Tree::new();
        source.file("notes/a.txt", "alpha");
        let mut other_store = Store::new();
        let other = other_store.add_version(&source, "notes", "2026-11-04T090212");
        // Staged in this store under the other stamp.
        let staged = store
            .tmp_pack_dir()
            .join(other.stamp.to_string())
            .join("notes");
        fs::create_dir_all(&staged).expect("the staging folder");
        fs::copy(other.path(Archive), staged.join(&other.names.archive)).expect("the copy");
        version.remove(Archive);
        assert_eq!(condition(&store, &version), Condition::Incomplete);
    }

    #[test]
    fn a_listing_without_a_required_head_line_is_head_damage() {
        for key in ["piece", "archived", "archive-bytes"] {
            let (store, version) = sound();
            version.listing_without(key);
            let reason = head_damage(condition(&store, &version));
            assert!(reason.contains(key), "{key}: {reason}");
        }
    }

    #[test]
    fn a_head_damage_does_not_hide_the_size_of_the_archive() {
        let (store, version) = sound();
        let recorded = fs::metadata(version.path(Archive))
            .expect("the archive")
            .len();
        version.listing_without("piece");
        version.truncate(Archive, 5);
        match condition(&store, &version) {
            Condition::Damaged(damages) => assert!(
                matches!(
                    damages.as_slice(),
                    [Damage::Head { .. }, Damage::ArchiveSize { found: 5, recorded: r }] if *r == recorded
                ),
                "{damages:?}"
            ),
            other => panic!("expected damage, found {other:?}"),
        }
    }

    #[test]
    fn a_listing_that_is_not_compressed_is_head_damage() {
        let (store, version) = sound();
        version.overwrite(Listing, b"# antiq listing 1\n");
        head_damage(condition(&store, &version));
    }

    #[test]
    fn a_listing_that_starts_with_another_line_is_head_damage() {
        let (store, version) = sound();
        version.listing_from_text(b"hello\n");
        head_damage(condition(&store, &version));
    }

    #[test]
    fn a_listing_of_one_line_without_a_newline_is_head_damage() {
        let (store, version) = sound();
        version.listing_from_text(b"# antiq listing 1");
        head_damage(condition(&store, &version));
    }

    #[test]
    fn a_listing_cut_inside_its_first_block_is_head_damage() {
        let (store, version) = sound();
        version.truncate(Listing, 12);
        head_damage(condition(&store, &version));
    }

    #[test]
    fn a_checksum_file_of_two_lines_is_damage() {
        let (store, version) = sound();
        version.checksum_with_two_lines();
        assert!(matches!(
            condition(&store, &version),
            Condition::Damaged(damages) if matches!(damages.as_slice(), [Damage::Checksum { .. }])
        ));
    }

    #[test]
    fn a_checksum_file_for_another_archive_is_damage() {
        let (store, version) = sound();
        let other = ChecksumFile::new("0".repeat(64), "other.tar.zst".into());
        version.overwrite(Checksum, &other.to_bytes());
        match condition(&store, &version) {
            Condition::Damaged(damages) => match damages.as_slice() {
                [Damage::Checksum { reason }] => {
                    assert!(reason.contains("other.tar.zst"), "{reason}")
                }
                other => panic!("expected one checksum damage, found {other:?}"),
            },
            other => panic!("expected damage, found {other:?}"),
        }
    }

    fn doubled(kind: VersionFile) -> (Store, StoredVersion, Vec<OsString>) {
        let (store, version) = sound();
        let twin = version.add_twin(kind, "other-name");
        let mut files = vec![
            OsString::from(version.names.of(kind)),
            twin.file_name().expect("a file name").to_owned(),
        ];
        files.sort();
        (store, version, files)
    }

    fn assert_only_doubled(condition: Condition, kind: VersionFile, files: &[OsString]) {
        match condition {
            Condition::Damaged(damages) => match damages.as_slice() {
                [
                    Damage::Doubled {
                        kind: found,
                        files: names,
                    },
                ] => {
                    assert_eq!(*found, kind);
                    let mut names = names.clone();
                    names.sort();
                    assert_eq!(names, files);
                }
                other => panic!("expected one doubled slot, found {other:?}"),
            },
            other => panic!("expected damage, found {other:?}"),
        }
    }

    #[test]
    fn a_doubled_listing_slot_is_damage_naming_both_files() {
        let (store, version, files) = doubled(Listing);
        assert_only_doubled(condition(&store, &version), Listing, &files);
    }

    #[test]
    fn a_doubled_archive_slot_is_damage_naming_both_files() {
        let (store, version, files) = doubled(Archive);
        assert_only_doubled(condition(&store, &version), Archive, &files);
    }

    #[test]
    fn a_doubled_checksum_slot_is_damage_naming_both_files() {
        let (store, version, files) = doubled(Checksum);
        assert_only_doubled(condition(&store, &version), Checksum, &files);
    }

    #[test]
    fn a_doubled_slot_hides_what_no_file_can_be_picked_for() {
        let (store, version, files) = doubled(Archive);
        version.remove(Listing);
        assert_only_doubled(condition(&store, &version), Archive, &files);
    }

    #[test]
    fn two_doubled_slots_are_both_reported() {
        let (store, version) = sound();
        version.add_twin(Listing, "other-name");
        version.add_twin(Checksum, "other-name");
        match condition(&store, &version) {
            Condition::Damaged(damages) => {
                let kinds: Vec<VersionFile> = damages
                    .iter()
                    .map(|damage| match damage {
                        Damage::Doubled { kind, .. } => *kind,
                        other => panic!("expected doubled slots, found {other:?}"),
                    })
                    .collect();
                assert_eq!(kinds, [Listing, Checksum]);
            }
            other => panic!("expected damage, found {other:?}"),
        }
    }

    #[test]
    fn a_doubled_sidecar_without_an_archive_is_still_incomplete() {
        let (store, version) = sound();
        version.add_twin(Listing, "other-name");
        version.remove(Archive);
        assert_eq!(condition(&store, &version), Condition::Incomplete);
    }

    fn newer_format(version: &StoredVersion) {
        version.listing_from_text(b"# antiq listing 2\n# piece         notes\n");
    }

    #[test]
    fn a_listing_of_a_newer_format_is_neither_incomplete_nor_damaged() {
        let (store, version) = sound();
        newer_format(&version);
        assert_eq!(condition(&store, &version), Condition::NewerFormat);
    }

    #[test]
    fn a_missing_sidecar_is_damage_whatever_the_format() {
        let (store, version) = sound();
        newer_format(&version);
        version.remove(Checksum);
        assert_eq!(
            condition(&store, &version),
            Condition::Damaged(vec![Damage::NoChecksum])
        );
    }

    #[test]
    fn a_damaged_checksum_file_is_damage_whatever_the_format() {
        let (store, version) = sound();
        newer_format(&version);
        version.checksum_with_two_lines();
        assert!(matches!(
            condition(&store, &version),
            Condition::Damaged(damages) if matches!(damages.as_slice(), [Damage::Checksum { .. }])
        ));
    }

    #[test]
    fn a_sidecar_that_vanished_after_the_scan_is_missing() {
        let (store, version) = sound();
        let stale = scanned(&store, &version);
        version.remove(Listing);
        assert_eq!(
            run(&store, &version, &stale).expect("inspected").condition,
            Condition::Damaged(vec![Damage::NoListing])
        );
        version.remove(Checksum);
        assert_eq!(
            run(&store, &version, &stale).expect("inspected").condition,
            Condition::Damaged(vec![Damage::NoListing, Damage::NoChecksum])
        );
    }

    #[test]
    fn a_version_that_vanished_after_the_scan_is_incomplete() {
        let (store, version) = sound();
        let stale = scanned(&store, &version);
        version.remove(Archive);
        version.remove(Listing);
        assert_eq!(
            run(&store, &version, &stale).expect("inspected").condition,
            Condition::Incomplete
        );
    }

    #[test]
    fn an_archive_that_vanished_after_the_scan_is_being_stored_when_staged() {
        let (store, version) = sound();
        let stale = scanned(&store, &version);
        version.stage_archive(&store);
        version.remove(Archive);
        assert_eq!(
            run(&store, &version, &stale).expect("inspected").condition,
            Condition::BeingStored
        );
    }

    #[test]
    fn an_unreadable_listing_is_an_error_not_damage() {
        let (store, version) = sound();
        let listing = version.path(Listing);
        fs::set_permissions(&listing, fs::Permissions::from_mode(0o000)).expect("the mode");
        assert!(
            fs::File::open(&listing).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let result = run(&store, &version, &scanned(&store, &version));
        assert!(
            matches!(result, Err(VersionsError::Io { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn an_archive_that_cannot_be_looked_at_is_an_error() {
        let (store, version) = sound();
        // No sidecars to open, so the archive is the first file touched.
        let stored = StoreVersion {
            listings: Vec::new(),
            checksums: Vec::new(),
            ..scanned(&store, &version)
        };
        fs::set_permissions(&version.folder, fs::Permissions::from_mode(0o000)).expect("the mode");
        let locked_out = fs::read_dir(&version.folder).is_err();
        let result = run(&store, &version, &stored);
        fs::set_permissions(&version.folder, fs::Permissions::from_mode(0o755)).expect("the mode");
        assert!(locked_out, "the test needs a user that mode 000 locks out");
        assert!(
            matches!(result, Err(VersionsError::Io { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_store_that_is_not_there_is_an_error() {
        let (store, version) = sound();
        let stored = scanned(&store, &version);
        let nowhere = store.pieces_dir().join("gone");
        let places = Places {
            pieces_dir: &nowhere,
            tmp_pack_dir: &store.tmp_pack_dir(),
        };
        let result = inspect(&places, &version.piece, &stored, Check::Head);
        assert!(
            matches!(result, Err(VersionsError::Io { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_listing_cut_inside_its_entries_passes_a_check_of_the_head() {
        let (store, version) = sound();
        version.cut_listing();
        let found = inspected(&store, &version);
        assert!(found.head.is_some());
        assert_eq!(found.condition, Condition::Sound);
    }

    fn full(store: &Store, version: &StoredVersion) -> Inspection {
        let stored = scanned(store, version);
        inspect(&store.places(), &version.piece, &stored, Check::Full).expect("inspected")
    }

    #[test]
    fn a_check_in_full_finds_a_listing_cut_inside_its_entries() {
        let (store, version) = sound();
        version.cut_listing();
        assert_eq!(condition(&store, &version), Condition::Sound);
        match full(&store, &version).condition {
            Condition::Damaged(damages) => {
                assert!(
                    matches!(damages.as_slice(), [Damage::Listing { .. }]),
                    "{damages:?}"
                )
            }
            other => panic!("expected damage, found {other:?}"),
        }
    }

    #[test]
    fn a_check_in_full_finds_counts_that_differ_from_the_entries() {
        let (store, version) = sound();
        version.edit_listing(|text| text.replace("# files         2", "# files         3"));
        assert_eq!(condition(&store, &version), Condition::Sound);
        assert!(matches!(
            full(&store, &version).condition,
            Condition::Damaged(damages) if matches!(damages.as_slice(), [Damage::Listing { .. }])
        ));
    }

    #[test]
    fn a_check_in_full_finds_an_entry_line_out_of_format() {
        let (store, version) = sound();
        version.edit_listing(|text| text.replace("# file ", "# fyle "));
        assert!(matches!(
            full(&store, &version).condition,
            Condition::Damaged(damages) if matches!(damages.as_slice(), [Damage::Listing { .. }])
        ));
    }

    #[test]
    fn a_check_in_full_does_not_repeat_a_damaged_head() {
        let (store, version) = sound();
        version.listing_without("piece");
        let found = full(&store, &version);
        head_damage(found.condition);
    }

    #[test]
    fn a_check_in_full_keeps_the_other_damage_of_a_cut_listing() {
        let (store, version) = sound();
        version.cut_listing();
        version.remove(Checksum);
        match full(&store, &version).condition {
            Condition::Damaged(damages) => assert!(
                matches!(
                    damages.as_slice(),
                    [Damage::Listing { .. }, Damage::NoChecksum]
                ),
                "{damages:?}"
            ),
            other => panic!("expected damage, found {other:?}"),
        }
    }

    #[test]
    fn a_check_in_full_passes_a_sound_version_with_its_head() {
        let (store, version) = sound();
        let found = full(&store, &version);
        assert_eq!(found.condition, Condition::Sound);
        assert!(found.head.is_some());
    }

    #[test]
    fn a_check_in_full_leaves_a_newer_format_alone() {
        let (store, version) = sound();
        newer_format(&version);
        assert_eq!(full(&store, &version).condition, Condition::NewerFormat);
    }

    /// Nine versions of one piece, three of them damaged in different ways.
    fn nine_versions() -> (Store, Vec<(RelPath, StoreVersion)>) {
        let mut source = Tree::new();
        source.file("notes/a.txt", "alpha");
        let mut store = Store::new();
        for second in 1..=9 {
            let version = store.add_version(&source, "notes", &format!("2026-11-04T09020{second}"));
            match second {
                3 => version.cut_listing(),
                5 => version.remove(Checksum),
                7 => version.remove(Archive),
                _ => {}
            }
        }
        let items = store
            .scan()
            .pieces
            .into_iter()
            .flat_map(|piece| {
                let path = piece.path;
                piece
                    .versions
                    .into_iter()
                    .map(move |version| (path.clone(), version))
            })
            .collect();
        (store, items)
    }

    #[test]
    fn inspect_all_answers_in_the_order_of_its_items_whatever_the_workers() {
        let (store, items) = nine_versions();
        let places = store.places();
        let one_by_one: Vec<Inspection> = items
            .iter()
            .map(|(piece, version)| {
                inspect(&places, piece, version, Check::Full).expect("inspected")
            })
            .collect();
        assert!(
            one_by_one
                .iter()
                .any(|found| found.condition != Condition::Sound),
            "the versions hold damage"
        );
        for workers in [0, 1, 3, 20] {
            let all: Vec<Inspection> = inspect_all(&places, &items, Check::Full, workers)
                .into_iter()
                .map(|found| found.expect("inspected"))
                .collect();
            assert_eq!(all, one_by_one, "{workers} workers");
        }
    }

    #[test]
    fn inspect_all_of_nothing_is_nothing() {
        let (store, _) = nine_versions();
        assert!(inspect_all(&store.places(), &[], Check::Full, 4).is_empty());
    }

    #[test]
    fn the_files_of_a_doubled_slot_are_shown_by_the_rule_for_names() {
        use std::os::unix::ffi::OsStringExt;
        let doubled = Damage::Doubled {
            kind: Listing,
            files: vec![
                "a%b--s.sha256.zst".into(),
                OsString::from_vec(b"c\xff\n--s.sha256.zst".to_vec()),
            ],
        };
        assert_eq!(
            doubled.to_string(),
            "2 files for the listing: `a%25b--s.sha256.zst`, `c%FF%0A--s.sha256.zst`"
        );
    }

    #[test]
    fn the_damages_read_as_sentences() {
        let doubled = Damage::Doubled {
            kind: Listing,
            files: vec!["a--s.sha256.zst".into(), "b--s.sha256.zst".into()],
        };
        assert_eq!(
            doubled.to_string(),
            "2 files for the listing: `a--s.sha256.zst`, `b--s.sha256.zst`"
        );
        for (kind, slot) in [(Archive, "archive"), (Checksum, "checksum file")] {
            let doubled = Damage::Doubled {
                kind,
                files: vec!["a".into(), "b".into()],
            };
            assert_eq!(
                doubled.to_string(),
                format!("2 files for the {slot}: `a`, `b`")
            );
        }
        assert_eq!(Damage::NoListing.to_string(), "the listing is missing");
        assert_eq!(
            Damage::NoChecksum.to_string(),
            "the checksum file is missing"
        );
        assert_eq!(
            Damage::Head {
                reason: "the `piece` line is missing".into()
            }
            .to_string(),
            "the head of the listing is damaged: the `piece` line is missing"
        );
        assert_eq!(
            Damage::ArchiveSize {
                found: 10,
                recorded: 20
            }
            .to_string(),
            "the archive has 10 bytes, its listing records 20"
        );
        assert_eq!(
            Damage::Listing { reason: "x".into() }.to_string(),
            "the listing is damaged: x"
        );
        assert_eq!(
            Damage::Checksum { reason: "x".into() }.to_string(),
            "the checksum file is damaged: x"
        );
    }
}
