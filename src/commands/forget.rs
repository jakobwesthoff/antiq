// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `forget` operation: deletes one version of a piece, or all of them.

use crate::clock::Clock;
use crate::code_span::CodeSpan;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::plan::LockHeld;
use crate::reference::{self, CandidateSet, ReferenceError, SearchWord};
use crate::relpath::{RelPath, escape};
use crate::repair::Repair;
use crate::report::{self, CommandReport, Warning};
use crate::run::{self, RunError, RunGuard, Started};
use crate::versions::VersionsError;
use crate::versions::names::VersionFile;
use crate::versions::naming::{self, Candidate, VersionLookupError};
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StorePiece, StoreScan, StoreVersion};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Serialize, Serializer};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Why forget failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ForgetError {
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
    #[diagnostic(
        code(antiq::forget::all_versions_need_force),
        help("Add `--force` to confirm, and check the effect first with `-n`.")
    )]
    #[error(
        "`forget --all-versions` needs `--force`, as it deletes every stored version of {}, damaged and incomplete ones included, and that cannot be undone",
        CodeSpan(.piece)
    )]
    AllVersionsNeedForce { piece: String },
    #[error("{0}")]
    Version(
        #[from]
        #[diagnostic_source]
        VersionLookupError,
    ),
}

impl HasKind for ForgetError {
    fn kind(&self) -> ErrorKind {
        match self {
            ForgetError::Run(error) => error.kind(),
            ForgetError::Reference(error) => error.kind(),
            ForgetError::Usage(_) | ForgetError::AllVersionsNeedForce { .. } => ErrorKind::Usage,
            ForgetError::Version(error) => error.kind(),
            ForgetError::Versions(_) => ErrorKind::Other,
        }
    }
}

/// Which versions of the piece to delete.
#[derive(Debug, Clone, Copy)]
pub enum Which<'a> {
    /// The version the text names ([`naming::find_version`]).
    One(&'a str),
    /// Every version of the piece, incomplete ones included.
    All,
}

#[derive(Debug)]
pub struct ForgetRequest<'a> {
    pub collection: &'a Collection,
    /// The directory a piece given as a relative path starts at.
    pub cwd: &'a Path,
    /// A path. A search word is only offered, never acted on.
    pub piece: &'a OsStr,
    pub which: Which<'a>,
    /// Confirms [`Which::All`], which [`require_force`] refuses without it.
    pub force: bool,
}

/// A version that `forget` deleted, or would delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionEntry {
    pub piece: RelPath,
    #[serde(serialize_with = "as_text")]
    pub stamp: Stamp,
    /// The time of the stamp in UTC. The stamp has the counter this lacks.
    pub version: Timestamp,
    /// Sidecars without an archive.
    pub incomplete: bool,
    /// The bare names of the files, in the order they are deleted.
    pub files: Vec<String>,
}

fn as_text<S: Serializer>(stamp: &Stamp, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(stamp)
}

impl VersionEntry {
    fn of(piece: &RelPath, version: &StoreVersion) -> VersionEntry {
        let files = VersionFile::ALL
            .into_iter()
            .flat_map(|kind| version.files(kind))
            .map(|name| escape(name.as_bytes()))
            .collect();
        VersionEntry {
            piece: piece.clone(),
            stamp: version.stamp,
            version: version.stamp.time(),
            incomplete: version.is_incomplete(),
            files,
        }
    }
}

/// A version that could not be deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failed {
    pub piece: RelPath,
    #[serde(serialize_with = "as_text")]
    pub stamp: Stamp,
    pub reason: String,
}

/// One step of a run, as a plan lists it and as the report names the
/// repairs that were done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Repair(Repair),
    ForgetVersion(VersionEntry),
}

/// What a real run did.
#[derive(Debug, Serialize)]
pub struct ForgetReport {
    pub forgotten: Vec<VersionEntry>,
    pub failed: Vec<Failed>,
    /// The repairs at the start of the run that were done.
    pub repairs: Vec<Action>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for ForgetReport {
    fn partial(&self) -> bool {
        !self.failed.is_empty()
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// Deletes the versions the request names. The run lock is taken and the
/// repairs run first.
///
/// A version that cannot be deleted is recorded in the report and the run
/// goes on with the next one. Whatever the delete had removed by then stays
/// removed, the archive first, so a rerun of the same `forget` finds the
/// rest as an incomplete version.
pub fn forget(
    request: &ForgetRequest<'_>,
    clock: &dyn Clock,
    zone: &TimeZone,
) -> Result<ForgetReport, ForgetError> {
    check_arguments(request)?;
    let guard = RunGuard::begin(request.collection, clock)?;
    let chosen = choose(request, zone, &[])?;

    let pieces = crate::filesystem::tree::TreeRoot::open(&request.collection.pieces_dir())
        .map_err(|source| VersionsError::Io {
            action: "open",
            path: request.collection.pieces_dir().display().to_string(),
            source,
        })?;
    let mut forgotten = Vec::new();
    let mut failed = Vec::new();
    // The versions are deleted oldest first, so a run that stops halfway
    // leaves the newest ones in place.
    for version in &chosen.versions {
        match store::delete_version(&pieces, &chosen.folder, version) {
            Ok(()) => forgotten.push(VersionEntry::of(&chosen.folder, version)),
            Err(error) => failed.push(Failed {
                piece: chosen.folder.clone(),
                stamp: version.stamp,
                reason: error.to_string(),
            }),
        }
    }

    // The report lists stamps newest first like every other stamp list.
    forgotten.reverse();
    failed.reverse();

    let mut warnings = report::setting_notes(request.collection);
    warnings.extend(guard.warnings().iter().cloned());
    Ok(ForgetReport {
        forgotten,
        failed,
        repairs: guard
            .repaired()
            .iter()
            .cloned()
            .map(Action::Repair)
            .collect(),
        warnings,
    })
}

/// What a dry run found.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ForgetPlan {
    /// Another run holds the lock, so nothing else was read.
    Held {
        dry_run: bool,
        lock_held: LockHeld,
        actions: Vec<Action>,
    },
    /// The repairs a real run starts with, then the versions it deletes.
    Listed {
        dry_run: bool,
        actions: Vec<Action>,
        warnings: Vec<Warning>,
    },
}

/// Lists what [`forget`] would do, without taking the lock or changing
/// anything. The versions are chosen as if the repairs had been done.
pub fn plan(
    request: &ForgetRequest<'_>,
    clock: &dyn Clock,
    zone: &TimeZone,
) -> Result<ForgetPlan, ForgetError> {
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
        return Ok(ForgetPlan::Held {
            dry_run: true,
            lock_held: LockHeld::of(&holder),
            actions: Vec::new(),
        });
    }

    // Repair 1 deletes sidecars whose archive is staged, which `settled` leaves
    // out already; only repair 5 removes a version the scan shows whole.
    let deleted_by_repairs: Vec<(RelPath, Stamp)> = repairs
        .iter()
        .filter_map(|repair| match repair {
            Repair::DeleteRestoredVersion {
                piece,
                version,
                delete: true,
            } => Some((piece.clone(), *version)),
            _ => None,
        })
        .collect();
    let chosen = choose(request, zone, &deleted_by_repairs)?;

    let mut actions: Vec<Action> = repairs.into_iter().map(Action::Repair).collect();
    actions.extend(
        chosen
            .versions
            .iter()
            .rev()
            .map(|version| Action::ForgetVersion(VersionEntry::of(&chosen.folder, version))),
    );
    let mut warnings = report::setting_notes(request.collection);
    warnings.extend(unreadable);
    Ok(ForgetPlan::Listed {
        dry_run: true,
        actions,
        warnings,
    })
}

/// Refuses [`Which::All`] without `force`. A caller that has to report the
/// refusal before it looks for the collection calls this on its own; the
/// operations call it again, so no caller can skip it.
pub fn require_force(piece: &OsStr, which: Which<'_>, force: bool) -> Result<(), ForgetError> {
    if matches!(which, Which::All) && !force {
        return Err(ForgetError::AllVersionsNeedForce {
            piece: piece.to_string_lossy().into_owned(),
        });
    }
    Ok(())
}

/// The empty piece or version is a flaw of the command line: the shell
/// passed a variable that was not set.
fn check_arguments(request: &ForgetRequest<'_>) -> Result<(), ForgetError> {
    require_force(request.piece, request.which, request.force)?;
    if request.piece.is_empty() {
        return Err(ForgetError::Usage("the piece is empty"));
    }
    if matches!(request.which, Which::One("")) {
        return Err(ForgetError::Usage("the version is empty"));
    }
    Ok(())
}

/// The store folder the request names and the versions of it to delete.
struct Chosen {
    folder: RelPath,
    versions: Vec<StoreVersion>,
}

fn choose(
    request: &ForgetRequest<'_>,
    zone: &TimeZone,
    repaired: &[(RelPath, Stamp)],
) -> Result<Chosen, ForgetError> {
    let collection = request.collection;
    let scan = settled(collection, store::scan(&collection.pieces_dir())?, repaired)?;
    let candidates = CandidateSet::store_and_sidecar_only(collection, &scan);
    let folder = reference::resolve_piece(
        collection,
        request.cwd,
        request.piece,
        &candidates,
        SearchWord::OnlyOffers,
    )?;
    let found: &StorePiece = scan
        .pieces
        .iter()
        .chain(&scan.sidecar_only)
        .find(|candidate| candidate.path == folder)
        .expect("the candidates are the folders of the scan");

    let versions = match request.which {
        Which::All => found.versions.clone(),
        Which::One(input) => {
            let named: Vec<Candidate> = found
                .versions
                .iter()
                .map(|version| Candidate {
                    stamp: version.stamp,
                    incomplete: version.is_incomplete(),
                })
                .collect();
            let stamp = naming::resolve_version(&folder.to_string(), input, &named, zone)?;
            found
                .versions
                .iter()
                .filter(|version| version.stamp == stamp)
                .cloned()
                .collect()
        }
    };
    Ok(Chosen { folder, versions })
}

/// `scan` without the versions that a run is still moving into the store:
/// sidecars whose archive is staged in `tmp/pack/` belong to repair 1, not
/// to the user. A version in `repaired` is one that a repair of the start
/// of the run deletes, which a dry run has not done yet. A folder left
/// without a version is no piece to forget.
fn settled(
    collection: &Collection,
    mut scan: StoreScan,
    repaired: &[(RelPath, Stamp)],
) -> Result<StoreScan, VersionsError> {
    let staging = collection.tmp_dir().join("pack");
    for folder in scan.pieces.iter_mut().chain(&mut scan.sidecar_only) {
        let mut kept = Vec::new();
        for version in std::mem::take(&mut folder.versions) {
            let staged = version.is_incomplete()
                && store::is_being_stored(&staging, &folder.path, &version.stamp)?;
            let repaired = repaired.contains(&(folder.path.clone(), version.stamp));
            if !staged && !repaired {
                kept.push(version);
            }
        }
        folder.versions = kept;
    }
    scan.pieces.retain(|folder| !folder.versions.is_empty());
    scan.sidecar_only
        .retain(|folder| !folder.versions.is_empty());
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::LockError;
    use miette::Diagnostic;

    #[test]
    fn a_usage_error_is_a_usage_kind_and_a_held_lock_keeps_its_kind() {
        assert_eq!(ForgetError::Usage("x").kind(), ErrorKind::Usage);
        let held = ForgetError::Run(RunError::Lock(LockError::Held {
            pid: 7,
            started: "2026-11-04T09:02:11Z".parse().expect("a timestamp"),
        }));
        assert_eq!(held.kind(), ErrorKind::LockHeld);
    }

    #[test]
    fn all_versions_without_force_is_refused_as_a_usage_error_naming_the_piece() {
        let refused = require_force(OsStr::new("tools/scan"), Which::All, false)
            .expect_err("all versions need the force");
        assert_eq!(refused.kind(), ErrorKind::Usage);
        assert_eq!(
            refused.code().expect("the refusal has a code").to_string(),
            "antiq::forget::all_versions_need_force"
        );
        assert!(refused.to_string().contains("`tools/scan`"), "{refused}");
        assert!(require_force(OsStr::new("tools/scan"), Which::All, true).is_ok());
        assert!(require_force(OsStr::new("tools/scan"), Which::One("v1"), false).is_ok());
    }

    #[test]
    fn a_reference_keeps_its_kind() {
        let unknown = ForgetError::Reference(ReferenceError::Unknown {
            reference: "zzz".to_string(),
            closest: Vec::new(),
        });
        assert_eq!(unknown.kind(), ErrorKind::UnknownReference);
    }

    #[test]
    fn a_store_that_cannot_be_read_is_an_error_of_no_kind_of_its_own() {
        let error = ForgetError::Versions(VersionsError::InvalidStamp("x".to_string()));
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_version_that_is_not_there_is_an_error_of_no_kind_of_its_own() {
        let none = ForgetError::Version(VersionLookupError::NoSuch {
            piece: "tools/scan".to_string(),
            input: "2020".to_string(),
            stamps: vec!["2026-01-01T000000".to_string()],
        });
        assert_eq!(none.kind(), ErrorKind::Other);
    }
}
