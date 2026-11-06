// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `rebuild` operation: repairs a damaged version by writing the
//! listing or the checksum file it lacks, after checking the archive
//! against whatever survives.

use crate::archive::PackError;
use crate::archive::check::{
    ArchiveCheck, ChecksumFile, check_archive, check_members_against_listing, file_digest,
    file_sha256,
};
use crate::archive::read;
use crate::clock::{Clock, whole_seconds};
use crate::code_span::CodeSpan;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind, chain_text};
use crate::filesystem::tree::TreeRoot;
use crate::listing::{self, Header, ListingEntry, ListingError};
use crate::plan::LockHeld;
use crate::reference::{self, ReferenceError};
use crate::relpath::{Name, RelPath};
use crate::repair::Repair;
use crate::report::{self, Warning};
use crate::run::{self, RunError, RunGuard, Started};
use crate::versions::VersionsError;
use crate::versions::damage::{self, Check, Condition, Damage, Places};
use crate::versions::names::VersionFile;
use crate::versions::naming::{self, Candidate, VersionLookupError};
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StoreVersion};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Serialize, Serializer};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The folder in `tmp/` where the new files are written before they move
/// into the store. Repair 2 empties `tmp/` at the start of every run.
const STAGING: &str = "rebuild";

/// Why rebuild failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RebuildError {
    /// The lock is held, or the machine has no name to write into it.
    #[error("cannot start the run")]
    Run(
        #[from]
        #[diagnostic_source]
        RunError,
    ),
    #[error("cannot find the piece")]
    Reference(
        #[from]
        #[diagnostic_source]
        ReferenceError,
    ),
    /// A file of the store could not be read for another reason than being
    /// gone. The source says which.
    #[error("cannot read the versions in the store")]
    Versions(
        #[from]
        #[diagnostic_source]
        VersionsError,
    ),
    /// The piece or the version on the command line is empty.
    #[error("{0}")]
    Usage(&'static str),
    #[error("{0}")]
    Version(
        #[from]
        #[diagnostic_source]
        VersionLookupError,
    ),
    #[error("version {stamp} of {piece} is not damaged, there is nothing to rebuild")]
    NotDamaged { piece: String, stamp: String },
    #[error(
        "version {stamp} of {piece} has no archive, so rebuild cannot repair it: `antiq forget {piece} {stamp}` removes it"
    )]
    Incomplete { piece: String, stamp: String },
    #[diagnostic(
        code(antiq::rebuild::format_too_new),
        help("Use a version of antiq that reads the newer format.")
    )]
    #[error(
        "the listing of version {} of {} is of a newer format than antiq reads",
        CodeSpan(.stamp),
        CodeSpan(.piece)
    )]
    FormatTooNew { piece: String, stamp: String },
    /// The version cannot be repaired, or its files show that it must not
    /// be. `reason` says why.
    #[error(
        "cannot rebuild version {stamp} of {piece}: {reason}. `antiq forget {piece} {stamp}` removes the version; copying the version back from the target restores it"
    )]
    Refused {
        piece: String,
        stamp: String,
        reason: String,
    },
    #[error("cannot read the archive")]
    Archive(
        #[from]
        #[diagnostic_source]
        PackError,
    ),
    #[error("cannot write the listing")]
    Listing(
        #[from]
        #[diagnostic_source]
        ListingError,
    ),
    #[error("cannot {action} {path}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("cannot put the rebuilt files into the store")]
    Store(
        #[source]
        #[diagnostic_source]
        VersionsError,
    ),
}

impl HasKind for RebuildError {
    fn kind(&self) -> ErrorKind {
        match self {
            RebuildError::Run(error) => error.kind(),
            RebuildError::Reference(error) => error.kind(),
            RebuildError::Usage(_) => ErrorKind::Usage,
            RebuildError::FormatTooNew { .. } => ErrorKind::FormatTooNew,
            _ => ErrorKind::Other,
        }
    }
}

/// What the archive is checked against before the files are written, and
/// so what the written files can vouch for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// The listing is missing: the archive is hashed and compared with the
    /// checksum file.
    ChecksumFile,
    /// The checksum file is missing: every member of the archive is
    /// compared with the listing.
    Listing,
    /// Both are missing: the archive is read to its end.
    ReadableOnly,
}

impl Basis {
    /// The sidecars that are written, in the order they move into the
    /// store.
    fn written(self) -> &'static [VersionFile] {
        match self {
            Basis::ChecksumFile => &[VersionFile::Listing],
            Basis::Listing => &[VersionFile::Checksum],
            Basis::ReadableOnly => &[VersionFile::Listing, VersionFile::Checksum],
        }
    }
}

#[derive(Debug)]
pub struct RebuildRequest<'a> {
    pub collection: &'a Collection,
    /// The directory a piece given as a relative path starts at.
    pub cwd: &'a Path,
    /// A path or a search word.
    pub piece: &'a OsStr,
    pub version: &'a str,
}

fn as_text<S: Serializer>(stamp: &Stamp, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(stamp)
}

/// A version that `rebuild` repaired, or would repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RebuildEntry {
    pub piece: RelPath,
    #[serde(serialize_with = "as_text")]
    pub stamp: Stamp,
    /// The time of the stamp in UTC.
    pub version: Timestamp,
    pub basis: Basis,
    /// The bare name of the archive file.
    pub archive: String,
    /// The surviving file the archive is checked against; none when the
    /// archive is only read.
    pub against: Option<String>,
    /// The bare names of the files that are written, in order.
    pub write: Vec<String>,
}

/// One step of a run, as a plan lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Repair(Repair),
    RebuildVersion(RebuildEntry),
}

/// What a real run did.
#[derive(Debug, Serialize)]
pub struct RebuildReport {
    pub piece: RelPath,
    #[serde(serialize_with = "as_text")]
    pub stamp: Stamp,
    pub version: Timestamp,
    pub basis: Basis,
    /// The bare names of the files that were written, in order.
    pub written: Vec<String>,
    /// The repairs at the start of the run that were done.
    pub repairs: Vec<Action>,
    pub warnings: Vec<Warning>,
}

/// What a dry run found.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RebuildPlan {
    /// Another run holds the lock, so nothing else was read.
    Held {
        dry_run: bool,
        lock_held: LockHeld,
        actions: Vec<Action>,
    },
    /// The repairs a real run starts with, then the rebuild.
    Listed {
        dry_run: bool,
        actions: Vec<Action>,
        warnings: Vec<Warning>,
    },
}

/// Repairs the version the request names. The run lock is taken and the
/// repairs run first.
pub fn rebuild(
    request: &RebuildRequest<'_>,
    clock: &dyn Clock,
    zone: &TimeZone,
) -> Result<RebuildReport, RebuildError> {
    check_arguments(request)?;
    let guard = RunGuard::begin(request.collection, clock)?;
    let chosen = choose(request, zone)?;
    let basis = judge(request.collection, &chosen)?;
    let entry = entry_of(&chosen, basis);

    let rebuilt = whole_seconds(clock.now());
    write_missing(request.collection, &chosen, basis, rebuilt)?;

    let mut warnings = report::setting_notes(request.collection);
    warnings.extend(guard.warnings().iter().cloned());
    Ok(RebuildReport {
        piece: entry.piece,
        stamp: entry.stamp,
        version: entry.version,
        basis,
        written: entry.write,
        repairs: guard
            .repaired()
            .iter()
            .cloned()
            .map(Action::Repair)
            .collect(),
        warnings,
    })
}

/// Lists what [`rebuild`] would do, without taking the lock or changing
/// anything. The refusals that need only the sidecars and the size of the
/// archive are raised; the archive itself is neither hashed nor read.
pub fn plan(
    request: &RebuildRequest<'_>,
    clock: &dyn Clock,
    zone: &TimeZone,
) -> Result<RebuildPlan, RebuildError> {
    check_arguments(request)?;
    let started = run::start(request.collection, clock, true)?;
    let Started::DryRun {
        holder,
        repairs,
        warnings: unreadable,
    } = started
    else {
        unreachable!("a dry run starts no guarded run");
    };
    if let Some(holder) = holder {
        return Ok(RebuildPlan::Held {
            dry_run: true,
            lock_held: LockHeld::of(&holder),
            actions: Vec::new(),
        });
    }

    let chosen = choose(request, zone)?;
    let basis = judge(request.collection, &chosen)?;

    let mut actions: Vec<Action> = repairs.into_iter().map(Action::Repair).collect();
    actions.push(Action::RebuildVersion(entry_of(&chosen, basis)));
    let mut warnings = report::setting_notes(request.collection);
    warnings.extend(unreadable);
    Ok(RebuildPlan::Listed {
        dry_run: true,
        actions,
        warnings,
    })
}

/// The empty piece or version is a flaw of the command line: the shell
/// passed a variable that was not set.
fn check_arguments(request: &RebuildRequest<'_>) -> Result<(), RebuildError> {
    if request.piece.is_empty() {
        return Err(RebuildError::Usage("the piece is empty"));
    }
    if request.version.is_empty() {
        return Err(RebuildError::Usage("the version is empty"));
    }
    Ok(())
}

// =========================================================
// Choosing the version
// =========================================================

struct Chosen {
    folder: RelPath,
    version: StoreVersion,
}

fn choose(request: &RebuildRequest<'_>, zone: &TimeZone) -> Result<Chosen, RebuildError> {
    let collection = request.collection;
    let scan = store::scan(&collection.pieces_dir())?;
    let folder = reference::resolve_stored_piece(collection, request.cwd, request.piece, &scan)?;
    let found = scan
        .pieces
        .iter()
        .find(|candidate| candidate.path == folder)
        .expect("the candidates are the folders of the scan");

    let named: Vec<Candidate> = found
        .versions
        .iter()
        .map(|version| Candidate {
            stamp: version.stamp,
            incomplete: version.is_incomplete(),
        })
        .collect();
    let stamp = naming::resolve_version(&folder.to_string(), request.version, &named, zone)?;
    let version = found
        .versions
        .iter()
        .find(|version| version.stamp == stamp)
        .expect("the lookup names a version of the folder")
        .clone();
    Ok(Chosen { folder, version })
}

// =========================================================
// Judging the version
// =========================================================

/// Decides from the damage of the version what `rebuild` does, or why it
/// refuses. The order is fixed: a doubled slot, a newer format, a size
/// that no rebuild can repair, a sidecar that is present but corrupt, and
/// then the missing files.
fn judge(collection: &Collection, chosen: &Chosen) -> Result<Basis, RebuildError> {
    let pieces_dir = collection.pieces_dir();
    let tmp_pack = collection.tmp_dir().join("pack");
    let places = Places {
        pieces_dir: &pieces_dir,
        tmp_pack_dir: &tmp_pack,
    };
    let Chosen { folder, version } = chosen;
    let piece = folder.to_string();
    let stamp = version.stamp.to_string();
    let refuse = |reason: String| RebuildError::Refused {
        piece: piece.clone(),
        stamp: stamp.clone(),
        reason,
    };

    let inspection = damage::inspect(&places, folder, version, Check::Full)?;
    let damages = match inspection.condition {
        Condition::Sound => return Err(RebuildError::NotDamaged { piece, stamp }),
        Condition::Incomplete | Condition::BeingStored => {
            return Err(RebuildError::Incomplete { piece, stamp });
        }
        Condition::NewerFormat => return Err(RebuildError::FormatTooNew { piece, stamp }),
        Condition::Damaged(damages) => damages,
    };

    let doubled: Vec<String> = damages
        .iter()
        .filter(|damage| matches!(damage, Damage::Doubled { .. }))
        .map(ToString::to_string)
        .collect();
    if !doubled.is_empty() {
        return Err(refuse(format!(
            "{}. antiq cannot tell which of these files belongs to the version: remove the ones that do not by hand",
            doubled.join("; ")
        )));
    }

    // `inspect` reports a newer format only when nothing else is damaged,
    // so a newer listing beside a missing or corrupt checksum file shows up
    // as damage of that file.
    if listing_is_newer(&places, chosen, &damages)? {
        return Err(RebuildError::FormatTooNew { piece, stamp });
    }

    if let Some(Damage::ArchiveSize { found, recorded }) = damages
        .iter()
        .find(|damage| matches!(damage, Damage::ArchiveSize { .. }))
    {
        return Err(refuse(format!(
            "the archive has {found} bytes, its listing records {recorded}. rebuild cannot repair it, since the archive's hash can only differ from the one in the checksum file"
        )));
    }

    let corrupt: Vec<String> = damages
        .iter()
        .filter_map(|damage| {
            let kind = match damage {
                Damage::Head { .. } | Damage::Listing { .. } => VersionFile::Listing,
                Damage::Checksum { .. } => VersionFile::Checksum,
                _ => return None,
            };
            let file = version.files(kind)[0].to_string_lossy();
            Some(format!(
                "{damage}. rebuild does not overwrite `{file}`: delete it to have it rebuilt, or copy it back from the target"
            ))
        })
        .collect();
    if !corrupt.is_empty() {
        return Err(refuse(corrupt.join("; ")));
    }

    let no_listing = damages.contains(&Damage::NoListing);
    let no_checksum = damages.contains(&Damage::NoChecksum);
    match (no_listing, no_checksum) {
        (true, true) => Ok(Basis::ReadableOnly),
        (true, false) => Ok(Basis::ChecksumFile),
        (false, true) => Ok(Basis::Listing),
        (false, false) => unreachable!("every other damage was refused above"),
    }
}

/// Whether the listing, when there is one, names a higher format.
fn listing_is_newer(
    places: &Places<'_>,
    chosen: &Chosen,
    damages: &[Damage],
) -> Result<bool, RebuildError> {
    if damages.contains(&Damage::NoListing) {
        return Ok(false);
    }
    let name = &chosen.version.files(VersionFile::Listing)[0];
    let path = places
        .pieces_dir
        .join(OsStr::from_bytes(chosen.folder.as_bytes()))
        .join(name);
    let file = File::open(&path).map_err(io_error("open", &path))?;
    Ok(matches!(
        listing::read_head(file),
        Err(ListingError::FormatTooNew { .. })
    ))
}

fn entry_of(chosen: &Chosen, basis: Basis) -> RebuildEntry {
    let version = &chosen.version;
    let name = |kind: VersionFile| version.files(kind)[0].to_string_lossy().into_owned();
    let archive = name(VersionFile::Archive);
    let against = match basis {
        Basis::ChecksumFile => Some(name(VersionFile::Checksum)),
        Basis::Listing => Some(name(VersionFile::Listing)),
        Basis::ReadableOnly => None,
    };
    RebuildEntry {
        piece: chosen.folder.clone(),
        stamp: version.stamp,
        version: version.stamp.time(),
        basis,
        write: basis
            .written()
            .iter()
            .map(|kind| sidecar_name(&archive, *kind))
            .collect(),
        archive,
        against,
    }
}

/// The name of the sidecar `kind` beside the archive `archive`: the archive
/// name with its suffix replaced.
fn sidecar_name(archive: &str, kind: VersionFile) -> String {
    let stem = archive
        .strip_suffix(VersionFile::Archive.suffix())
        .expect("the scan found the archive by its suffix");
    format!("{stem}{}", kind.suffix())
}

// =========================================================
// Writing what is missing
// =========================================================

/// Checks the archive as `basis` says, writes the missing files into the
/// staging folder in `tmp/`, moves them into the store folder and removes
/// the staging folder.
fn write_missing(
    collection: &Collection,
    chosen: &Chosen,
    basis: Basis,
    rebuilt: Timestamp,
) -> Result<(), RebuildError> {
    let Chosen { folder, version } = chosen;
    let piece = folder.to_string();
    let stamp = version.stamp.to_string();
    let from_pack = |error: PackError| match error {
        PackError::ReadFile { .. } | PackError::OpenPackage(_) => RebuildError::Archive(error),
        refusal => RebuildError::Refused {
            piece: piece.clone(),
            stamp: stamp.clone(),
            reason: chain_text(&refusal),
        },
    };

    let store_folder = collection
        .pieces_dir()
        .join(OsStr::from_bytes(folder.as_bytes()));
    let sidecar = |kind: VersionFile| store_folder.join(&version.files(kind)[0]);
    let archive_name = version.files(VersionFile::Archive)[0]
        .to_string_lossy()
        .into_owned();
    let archive = sidecar(VersionFile::Archive);

    let header = |archive_bytes: u64| Header {
        piece: folder.clone(),
        archived: version.stamp.time(),
        archive: archive_name.clone(),
        archive_bytes,
        imported: None,
        rebuilt: Some(rebuilt),
    };
    let listing_bytes = |header: &Header, entries: &[ListingEntry]| {
        let mut bytes = Vec::new();
        listing::write(header, entries, &mut bytes)?;
        Ok::<_, RebuildError>(bytes)
    };

    let files: Vec<(String, Vec<u8>)> = match basis {
        Basis::ChecksumFile => {
            let path = sidecar(VersionFile::Checksum);
            let line = std::fs::read(&path).map_err(io_error("read", &path))?;
            let checksum = ChecksumFile::parse(&line).map_err(from_pack)?;
            check_archive(&archive, ArchiveCheck::Checksum(&checksum)).map_err(from_pack)?;
            let entries = read::list(&archive).map_err(from_pack)?;
            let size = std::fs::metadata(&archive)
                .map_err(io_error("look at", &archive))?
                .len();
            vec![(
                sidecar_name(&archive_name, VersionFile::Listing),
                listing_bytes(&header(size), &entries)?,
            )]
        }
        Basis::Listing => {
            let path = sidecar(VersionFile::Listing);
            let file = File::open(&path).map_err(io_error("open", &path))?;
            let listing = listing::read_full(file)?;
            check_members_against_listing(&archive, &listing.entries).map_err(from_pack)?;
            let hash = file_sha256(&archive).map_err(from_pack)?;
            vec![(
                sidecar_name(&archive_name, VersionFile::Checksum),
                ChecksumFile::new(hash, archive_name.clone()).to_bytes(),
            )]
        }
        Basis::ReadableOnly => {
            let entries = read::list(&archive).map_err(from_pack)?;
            let digest = file_digest(&archive).map_err(from_pack)?;
            vec![
                (
                    sidecar_name(&archive_name, VersionFile::Listing),
                    listing_bytes(&header(digest.bytes), &entries)?,
                ),
                (
                    sidecar_name(&archive_name, VersionFile::Checksum),
                    ChecksumFile::new(digest.sha256, archive_name.clone()).to_bytes(),
                ),
            ]
        }
    };

    let staged = stage(collection, &files);
    let moved = staged.and_then(|staging| {
        let pieces_dir = collection.pieces_dir();
        let pieces = TreeRoot::open(&pieces_dir).map_err(io_error("open", &pieces_dir))?;
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        store::move_sidecars_in(&staging, &pieces, folder, &names).map_err(RebuildError::Store)
    });
    // A staging folder that stays is emptied by repair 2 at the start of the
    // next run.
    remove_staging(collection, &files);
    moved
}

/// An I/O error as the error of the step `action` on `path`.
fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> RebuildError {
    let path = path.display().to_string();
    move |source| RebuildError::Io {
        action,
        path,
        source,
    }
}

/// Writes `files` into the staging folder of `tmp/` and opens it.
fn stage(collection: &Collection, files: &[(String, Vec<u8>)]) -> Result<TreeRoot, RebuildError> {
    let tmp_dir = collection.tmp_dir();
    let tmp = TreeRoot::open(&tmp_dir).map_err(io_error("open", &tmp_dir))?;
    let folder = staging_folder();
    let shown = tmp_dir.join(STAGING);
    tmp.create_dir(&folder, 0o755)
        .map_err(io_error("create", &shown))?;
    for (name, bytes) in files {
        let path = shown.join(name);
        let relative =
            folder.join(&Name::from_bytes(name.as_bytes().to_vec()).expect("a file name"));
        let mut file = tmp
            .create_file(&relative, 0o644)
            .map_err(io_error("create", &path))?;
        file.write_all(bytes).map_err(io_error("write", &path))?;
    }
    tmp.sub(&folder).map_err(io_error("open", &shown))
}

fn staging_folder() -> RelPath {
    RelPath::root().join(&Name::from_bytes(STAGING.as_bytes().to_vec()).expect("a plain name"))
}

/// Removes what [`stage`] wrote and is still there, then the folder.
fn remove_staging(collection: &Collection, files: &[(String, Vec<u8>)]) {
    let Ok(tmp) = TreeRoot::open(&collection.tmp_dir()) else {
        return;
    };
    let folder = staging_folder();
    for (name, _) in files {
        let relative =
            folder.join(&Name::from_bytes(name.as_bytes().to_vec()).expect("a file name"));
        let _ = tmp.remove_file(&relative);
    }
    let _ = tmp.remove_dir(&folder);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::LockError;

    #[test]
    fn a_usage_error_is_a_usage_kind_and_a_held_lock_keeps_its_kind() {
        assert_eq!(RebuildError::Usage("x").kind(), ErrorKind::Usage);
        let held = RebuildError::Run(RunError::Lock(LockError::Held {
            pid: 7,
            started: "2026-11-04T09:02:11Z".parse().expect("a timestamp"),
        }));
        assert_eq!(held.kind(), ErrorKind::LockHeld);
    }

    #[test]
    fn a_reference_keeps_its_kind() {
        let unknown = RebuildError::Reference(ReferenceError::Unknown {
            reference: "zzz".to_string(),
            closest: Vec::new(),
        });
        assert_eq!(unknown.kind(), ErrorKind::UnknownReference);
    }

    #[test]
    fn a_newer_format_has_its_own_kind_and_every_other_refusal_none() {
        let newer = RebuildError::FormatTooNew {
            piece: "tools/scan".to_string(),
            stamp: "2026-11-04T090211".to_string(),
        };
        assert_eq!(newer.kind(), ErrorKind::FormatTooNew);
        let refused = RebuildError::Refused {
            piece: "tools/scan".to_string(),
            stamp: "2026-11-04T090211".to_string(),
            reason: "the archive is gone".to_string(),
        };
        assert_eq!(refused.kind(), ErrorKind::Other);
        assert_eq!(
            refused.to_string(),
            "cannot rebuild version 2026-11-04T090211 of tools/scan: the archive is gone. `antiq forget tools/scan 2026-11-04T090211` removes the version; copying the version back from the target restores it"
        );
    }

    #[test]
    fn a_refusal_names_the_causes_below_the_error() {
        let error = PackError::Archive(io::Error::other("unexpected end of file"));
        assert_eq!(
            chain_text(&error),
            "cannot read the package: unexpected end of file"
        );
    }

    #[test]
    fn sidecars_are_named_after_the_archive() {
        let archive = "tools-scan--2026-11-04T090211.tar.zst";
        assert_eq!(
            sidecar_name(archive, VersionFile::Listing),
            "tools-scan--2026-11-04T090211.sha256.zst"
        );
        assert_eq!(
            sidecar_name(archive, VersionFile::Checksum),
            "tools-scan--2026-11-04T090211.tar.zst.sha256"
        );
    }

    #[test]
    fn a_basis_writes_what_is_missing() {
        assert_eq!(Basis::ChecksumFile.written(), [VersionFile::Listing]);
        assert_eq!(Basis::Listing.written(), [VersionFile::Checksum]);
        assert_eq!(
            Basis::ReadableOnly.written(),
            [VersionFile::Listing, VersionFile::Checksum]
        );
    }
}
