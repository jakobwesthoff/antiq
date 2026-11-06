// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `unearth` command.
//!
//! [`plan`] reads the state and decides; it writes nothing, so a dry run
//! can print its result. What it refuses, it refuses before anything is
//! extracted.

pub mod live;
pub mod local;
pub mod remote;

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::ErrorKind;
use crate::fsx::FsxError;
use crate::fsx::space::available_space;
use crate::fsx::tree::TreeRoot;
use crate::id::{IdSource, ObjectId, RandomIds};
use crate::index::{Index, IndexError};
use crate::listing::{self, ListingError, ListingType};
use crate::machine::MachineError;
use crate::meta::{self, Payload, PayloadKind};
use crate::ops::fork::ForkError;
use crate::pack::live::LiveError;
use crate::pack::{PackError, PackageFormat};
use crate::plan::{Plan, PlanAction};
use crate::relpath::RelPath;
use crate::remote::away::AwayError;
use crate::remote::{Remote, RemoteError};
use crate::report::{CommandReport, Warning};
use crate::run::RunGuard;
use crate::select::{Rules, SelectError};
use crate::store::{Current, Store, StoreError};
use crate::tracking::{self, EntryState, PieceState, TrackingEntry, TrackingError};
use crate::tree::{EntryKind, TreeEntry};
use serde::Serialize;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Errors of the `unearth` command.
#[derive(Debug, thiserror::Error)]
pub enum UnearthError {
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Fsx(#[from] FsxError),
    #[error(transparent)]
    Listing(#[from] ListingError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Fork(#[from] ForkError),
    #[error(transparent)]
    Live(#[from] LiveError),
    #[error(transparent)]
    RemoteState(#[from] AwayError),
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error("{id} has no metadata on this machine")]
    NoMetadata { id: ObjectId },
    #[error("`{path}` has no archive to unearth from")]
    NoArchive { path: RelPath },
    #[error("`{path}` is not on this machine and no remote was given")]
    NoLocalArchive { path: RelPath },
    #[error("`{path}` has no live package to unearth from")]
    NoLivePackage { path: RelPath },
    #[error(
        "`{path}` is a live piece of another collection and has no archive versions to choose from"
    )]
    VersionOfLivePiece { path: RelPath },
    #[error(
        "the remote no longer has `{file}` of `{path}`; its metadata changed, run `unearth` again"
    )]
    NotOnRemote { file: String, path: RelPath },
    #[error("`{path}` has no archive of version {version}; it has {available}")]
    NoSuchVersion {
        path: RelPath,
        version: u32,
        available: String,
    },
    #[error("committed versions of {id} cannot be read: {versions}")]
    UnreadableVersions {
        id: ObjectId,
        versions: String,
        /// One of them states a format this version does not know.
        too_new: bool,
    },
    #[error("the archive {file} cannot be unearthed: {reason}")]
    UnsupportedPayload { file: String, reason: String },
    #[error("the archive {file} does not match its metadata: {what}")]
    ArchiveDiffers { file: String, what: String },
    #[error("cannot unearth into `{path}`: {problem}")]
    Target {
        path: RelPath,
        problem: TargetProblem,
    },
}

impl UnearthError {
    /// The library error the binary decides the exit code from. A remote
    /// error keeps its own variant, which `--if-reachable` looks at.
    pub fn into_error(self) -> crate::error::Error {
        match self {
            UnearthError::Remote(error) => crate::error::Error::Remote(error),
            other => crate::error::Error::Unearth(other),
        }
    }
}

impl crate::error::HasKind for UnearthError {
    fn kind(&self) -> ErrorKind {
        match self {
            UnearthError::Store(error) => error.kind(),
            UnearthError::Listing(error) => error.kind(),
            UnearthError::Remote(error) => error.kind(),
            UnearthError::Fork(error) => error.kind(),
            UnearthError::UnreadableVersions { too_new: true, .. } => ErrorKind::FormatTooNew,
            _ => ErrorKind::Other,
        }
    }
}

/// Why a target is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetProblem {
    /// The collection root.
    Root,
    /// `.antiq/` or something below it.
    InsideAntiq,
    /// No pattern selects the target, so it would not be a piece.
    NotSelected,
    /// The target is, or lies below, an excluded folder.
    Excluded { at: RelPath },
    /// A folder above the target is a piece, so the target would be inside
    /// it.
    InsidePiece { piece: RelPath },
    /// Something above the target is not a folder.
    NotAFolder { at: RelPath },
    /// The file system lists a folder above the target under another
    /// spelling than the target's.
    SpelledDifferently { at: RelPath },
    /// The target exists, or another entry holds it.
    Taken,
    /// A name of the target is one the volume cannot hold.
    Unholdable { reason: String },
}

impl fmt::Display for TargetProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetProblem::Root => f.write_str("it is the collection root"),
            TargetProblem::InsideAntiq => f.write_str("it is inside `.antiq/`"),
            TargetProblem::NotSelected => f.write_str("the patterns do not select it"),
            TargetProblem::Excluded { at } => write!(f, "`{at}` is excluded"),
            TargetProblem::InsidePiece { piece } => write!(f, "`{piece}` is a piece"),
            TargetProblem::NotAFolder { at } => {
                write!(f, "`{at}` is a file or a symlink, not a folder")
            }
            TargetProblem::SpelledDifferently { at } => {
                write!(f, "`{at}` is spelled differently on disk")
            }
            TargetProblem::Taken => f.write_str("it is taken"),
            TargetProblem::Unholdable { reason } => f.write_str(reason),
        }
    }
}

fn target_error(path: &RelPath, problem: TargetProblem) -> UnearthError {
    UnearthError::Target {
        path: path.clone(),
        problem,
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> UnearthError {
    let path = path.display().to_string();
    move |source| UnearthError::Io {
        action,
        path,
        source,
    }
}

// =========================================================
// Planning
// =========================================================

/// What is to be unearthed, with the references already resolved.
#[derive(Debug, Clone)]
pub struct UnearthRequest {
    pub id: ObjectId,
    /// Where to put the piece; the stored path when absent.
    pub to: Option<RelPath>,
    /// The archive payload version; the current archive when absent.
    pub version: Option<u32>,
}

/// An action of the plan, as a dry run prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UnearthAction {
    Unearth {
        id: ObjectId,
        path: RelPath,
        /// Left out of the JSON for a local archive, the only source
        /// before the remote was involved.
        #[serde(skip_serializing_if = "is_local_archive")]
        source: UnearthSource,
        /// Absent for a live package, which has no archive version.
        #[serde(skip_serializing_if = "Option::is_none")]
        payload_version: Option<u32>,
    },
    /// The run leaves the piece as it is.
    Skip {
        id: ObjectId,
        path: RelPath,
        #[serde(flatten)]
        why: Skipped,
    },
}

fn is_local_archive(source: &UnearthSource) -> bool {
    *source == UnearthSource::LocalArchive
}

impl PlanAction for UnearthAction {
    fn describe(&self) -> String {
        match self {
            UnearthAction::Unearth {
                path,
                source,
                payload_version,
                ..
            } => {
                // Only a live package has no archive version.
                let version =
                    payload_version.map_or_else(String::new, |version| version.to_string());
                match source {
                    UnearthSource::LocalArchive => {
                        format!("unearth {path} from archive version {version}")
                    }
                    UnearthSource::RemoteArchive => {
                        format!("fetch archive version {version} and unearth {path}")
                    }
                    UnearthSource::LivePackage => format!("unearth {path} from its live package"),
                    UnearthSource::Fork => format!(
                        "unearth {path} from the live package of another collection's piece, as a fork with a new ID"
                    ),
                }
            }
            UnearthAction::Skip {
                path,
                why: Skipped::NoSpace { needed, available },
                ..
            } => format!("skip {path}: it needs {needed} bytes and the volume has {available}"),
        }
    }
}

/// Where the tree of an unearth comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnearthSource {
    /// An archive this machine holds.
    LocalArchive,
    /// An archive only the remote holds.
    RemoteArchive,
    /// The live package of a piece this collection owns, which is gone.
    LivePackage,
    /// The live package of another collection's piece, which becomes a
    /// new piece here.
    Fork,
}

/// Space the extraction needs and what the volume has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Shortfall {
    pub needed: u64,
    pub available: u64,
}

/// An unearth that can go ahead.
#[derive(Debug)]
pub struct UnearthPlan {
    pub(crate) id: ObjectId,
    pub(crate) current: Current,
    /// The entry this machine had before, restored when the unearth
    /// fails.
    pub(crate) entry: Option<TrackingEntry>,
    pub(crate) payload: Payload,
    pub(crate) source: UnearthSource,
    pub(crate) target: RelPath,
    /// The sum of the sizes of the files in the listing.
    pub(crate) needed: u64,
    pub(crate) shortfall: Option<Shortfall>,
}

impl UnearthPlan {
    pub fn target(&self) -> &RelPath {
        &self.target
    }

    pub fn source(&self) -> UnearthSource {
        self.source
    }

    pub fn payload_version(&self) -> u32 {
        self.payload.version
    }

    /// Set when the volume has less space than the extraction needs.
    pub fn shortfall(&self) -> Option<Shortfall> {
        self.shortfall
    }

    pub fn action(&self) -> UnearthAction {
        UnearthAction::Unearth {
            id: self.id,
            path: self.target.clone(),
            source: self.source,
            payload_version: (self.payload.kind == PayloadKind::Archive)
                .then_some(self.payload.version),
        }
    }
}

/// What planning found.
#[derive(Debug)]
pub enum Planned {
    Unearth(Box<UnearthPlan>),
    /// The piece is unearthed already, at `path`.
    AlreadyUnearthed {
        id: ObjectId,
        path: RelPath,
    },
}

impl Planned {
    /// The plan a dry run prints: nothing to do for a piece that is
    /// unearthed, a skip for one the volume has no room for.
    pub fn plan(&self) -> Plan<UnearthAction> {
        let mut plan = Plan::new();
        if let Planned::Unearth(unearth) = self {
            plan.push(match unearth.shortfall {
                Some(Shortfall { needed, available }) => UnearthAction::Skip {
                    id: unearth.id,
                    path: unearth.target.clone(),
                    why: Skipped::NoSpace { needed, available },
                },
                None => unearth.action(),
            });
        }
        plan
    }
}

// =========================================================
// Reporting and applying
// =========================================================

/// Why a piece was left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Skipped {
    /// The staging volume has too little room for the extraction.
    NoSpace { needed: u64, available: u64 },
}

/// What a run of `unearth` did.
#[derive(Debug, Serialize)]
pub struct UnearthReport {
    /// False when the piece was unearthed before, or was skipped.
    pub unearthed: bool,
    pub id: ObjectId,
    /// Where the piece is, or would have been put.
    pub path: RelPath,
    pub abs_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_version: Option<u32>,
    /// The ID of the other collection's piece this one was forked from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<ObjectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Skipped>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for UnearthReport {
    fn partial(&self) -> bool {
        self.skipped.is_some()
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

impl UnearthReport {
    /// The report of a piece that is unearthed already, at `path`.
    pub fn already_unearthed(
        collection: &Collection,
        id: ObjectId,
        path: RelPath,
        warnings: Vec<Warning>,
    ) -> UnearthReport {
        UnearthReport {
            unearthed: false,
            id,
            abs_path: absolute(collection, &path),
            path,
            payload_version: None,
            from: None,
            skipped: None,
            warnings,
        }
    }
}

/// The absolute path of a path in the collection.
pub(crate) fn absolute(collection: &Collection, path: &RelPath) -> String {
    collection
        .root()
        .join(OsStr::from_bytes(path.as_bytes()))
        .display()
        .to_string()
}

/// Carries out a plan under the run lock `guard` holds.
pub fn apply(
    collection: &Collection,
    plan: &UnearthPlan,
    guard: &RunGuard,
    clock: &dyn Clock,
) -> Result<UnearthReport, UnearthError> {
    apply_with_remote(collection, plan, guard, clock, &RandomIds, None)
}

/// Carries out a plan that may need `remote` for the archive. Without one,
/// a plan that does fails with [`UnearthError::NoLocalArchive`].
pub fn apply_with_remote(
    collection: &Collection,
    plan: &UnearthPlan,
    guard: &RunGuard,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    remote: Option<&dyn Remote>,
) -> Result<UnearthReport, UnearthError> {
    match plan.source {
        UnearthSource::LocalArchive | UnearthSource::RemoteArchive => local::unearth_archive(
            collection,
            plan,
            guard.lock(),
            clock,
            &local::Environment::real(),
            guard.warnings().to_vec(),
            remote,
        ),
        UnearthSource::LivePackage | UnearthSource::Fork => live::unearth_live(
            collection,
            plan,
            guard.lock(),
            clock,
            &local::Environment::real(),
            guard.warnings().to_vec(),
            ids,
            remote,
        ),
    }
}

/// Works out what unearthing `request` would do. Nothing is written.
pub fn plan(collection: &Collection, request: &UnearthRequest) -> Result<Planned, UnearthError> {
    plan_with(collection, request, &available_space)
}

type FreeSpace<'a> = &'a dyn Fn(&Path) -> Result<u64, FsxError>;

pub(crate) fn plan_with(
    collection: &Collection,
    request: &UnearthRequest,
    free_space: FreeSpace<'_>,
) -> Result<Planned, UnearthError> {
    plan_from(collection, request, None, free_space)
}

pub(crate) fn plan_from(
    collection: &Collection,
    request: &UnearthRequest,
    current: Option<Current>,
    free_space: FreeSpace<'_>,
) -> Result<Planned, UnearthError> {
    let id = request.id;
    let store = Store::new(collection);
    let current = match current {
        Some(current) => current,
        None => store.current(id)?.ok_or(UnearthError::NoMetadata { id })?,
    };
    let metadata = &current.metadata;
    let entry = tracking::read(collection, id)?;
    let root = TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;

    let folder_present = entry.as_ref().is_some_and(|entry| {
        root.lstat(&entry.path)
            .is_ok_and(|found| found.kind == EntryKind::Directory)
    });
    let state = tracking::derive_state(
        entry.as_ref(),
        Some(metadata),
        folder_present,
        collection.id(),
    )?;
    match state {
        PieceState::Unearthed | PieceState::Live => {
            let path = entry
                .as_ref()
                .map_or_else(|| metadata.path.clone(), |entry| entry.path.clone());
            return Ok(Planned::AlreadyUnearthed { id, path });
        }
        PieceState::Archived
        | PieceState::RemoteOnly
        | PieceState::Gone
        | PieceState::LiveOnAnotherMachine => {}
        // A piece without a folder and without an entry has nothing this
        // collection could unearth.
        PieceState::New => {
            return Err(UnearthError::NoArchive {
                path: metadata.path.clone(),
            });
        }
    }

    let (payload, source) = choose_source(&store, &current, state, request.version)?;
    if !current.unreadable.is_empty() {
        let versions: Vec<String> = current
            .unreadable
            .iter()
            .map(|unreadable| format!("{} ({})", unreadable.number, unreadable.reason))
            .collect();
        return Err(UnearthError::UnreadableVersions {
            id,
            versions: versions.join(", "),
            too_new: current.unreadable.iter().any(|unreadable| {
                unreadable
                    .format
                    .is_some_and(|format| format > meta::FORMAT)
            }),
        });
    }

    let needed = space_needed(&store, id, &payload, source)?;

    let target = request.to.clone().unwrap_or_else(|| metadata.path.clone());
    let rules = Rules::new(
        &collection.config().collection.patterns,
        &collection.config().collection.exclude,
    )?
    .bind(&root)?;
    let index = Index::compute(collection)?;
    check_planned_target(&rules, &index, id, entry.as_ref(), &target)?;
    check_target_on_disk(&root, &rules, &target)?;

    let staging = collection.staging_dir();
    let available = free_space(&staging)?;
    let shortfall = (available < needed).then_some(Shortfall { needed, available });

    Ok(Planned::Unearth(Box::new(UnearthPlan {
        id,
        current,
        entry,
        payload,
        source,
        target,
        needed,
        shortfall,
    })))
}

fn unsupported(payload: &Payload, reason: &str) -> UnearthError {
    UnearthError::UnsupportedPayload {
        file: payload.file.as_str().to_string(),
        reason: reason.to_string(),
    }
}

/// The space an unearth needs below the staging directory. An archive
/// that is fetched also needs room for itself, and its listing may be
/// missing here, in which case only the archive is counted. A live package
/// has no listing, so its size is a lower bound for the extracted tree.
fn space_needed(
    store: &Store<'_>,
    id: ObjectId,
    payload: &Payload,
    source: UnearthSource,
) -> Result<u64, UnearthError> {
    if matches!(source, UnearthSource::LivePackage | UnearthSource::Fork) {
        return Ok(payload.size);
    }
    let listing_name = payload
        .listing
        .as_ref()
        .ok_or_else(|| unsupported(payload, "it has no listing"))?;
    let listing_path = store.file_path(id, listing_name);
    let fetched = source == UnearthSource::RemoteArchive;
    let archive = if fetched { payload.size } else { 0 };
    if fetched && !listing_path.exists() {
        return Ok(archive);
    }
    let listing =
        listing::read_all(File::open(&listing_path).map_err(io_error("open", &listing_path))?)?;
    let extracted: u64 = listing
        .iter()
        .filter(|member| member.kind == ListingType::File)
        .map(|member| member.size)
        .sum();
    Ok(archive + extracted)
}

/// Which payload the unearth reads and where it comes from. The state of
/// the piece decides: another collection's live piece is only ever forked
/// from its live package, a gone piece of this collection that was never
/// archived comes back from its live package, and the rest comes from an
/// archive.
fn choose_source(
    store: &Store<'_>,
    current: &Current,
    state: PieceState,
    version: Option<u32>,
) -> Result<(Payload, UnearthSource), UnearthError> {
    let metadata = &current.metadata;
    let live = metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Live)
        .max_by_key(|payload| payload.version);
    let archived = metadata
        .payloads
        .iter()
        .any(|payload| payload.kind == PayloadKind::Archive);
    let (payload, source) = match state {
        PieceState::LiveOnAnotherMachine => {
            if version.is_some() {
                return Err(UnearthError::VersionOfLivePiece {
                    path: metadata.path.clone(),
                });
            }
            let live = live.ok_or_else(|| UnearthError::NoLivePackage {
                path: metadata.path.clone(),
            })?;
            (live.clone(), UnearthSource::Fork)
        }
        PieceState::Gone if !archived && version.is_none() => {
            let live = live.ok_or_else(|| UnearthError::NoArchive {
                path: metadata.path.clone(),
            })?;
            (live.clone(), UnearthSource::LivePackage)
        }
        _ => choose_payload(store, current, version)?,
    };
    check_supported(&payload)?;
    Ok((payload, source))
}

/// The archive to unearth: the one of `version`, else the newest that did
/// not lose a conflict. Where two machines made the same version, the one
/// this machine holds comes first, then the newer.
fn choose_payload(
    store: &Store<'_>,
    current: &Current,
    version: Option<u32>,
) -> Result<(Payload, UnearthSource), UnearthError> {
    let metadata = &current.metadata;
    let archives: Vec<&Payload> = metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Archive)
        .collect();
    let pool = archives.iter().copied().filter(|payload| match version {
        Some(version) => payload.version == version,
        None => !payload.superseded,
    });
    let held = |payload: &Payload| store.file_path(metadata.id, &payload.file).exists();
    let best = pool.max_by_key(|payload| (payload.version, held(payload), payload.created));
    let Some(best) = best else {
        return Err(match version {
            Some(version) if !archives.is_empty() => {
                let mut versions: Vec<u32> =
                    archives.iter().map(|payload| payload.version).collect();
                versions.sort_unstable();
                versions.dedup();
                UnearthError::NoSuchVersion {
                    path: metadata.path.clone(),
                    version,
                    available: versions
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            }
            _ => UnearthError::NoArchive {
                path: metadata.path.clone(),
            },
        });
    };
    let source = if held(best) {
        UnearthSource::LocalArchive
    } else {
        UnearthSource::RemoteArchive
    };
    Ok((best.clone(), source))
}

/// The package format and checksum algorithm this version reads.
fn check_supported(payload: &Payload) -> Result<(), UnearthError> {
    if payload.package_format != PackageFormat::TAR_ZSTD_1.as_str() {
        return Err(unsupported(
            payload,
            &format!("its package format is {}", payload.package_format),
        ));
    }
    if payload.checksum.algorithm != "sha256" {
        return Err(unsupported(
            payload,
            &format!("its checksum algorithm is {}", payload.checksum.algorithm),
        ));
    }
    Ok(())
}

/// The proper ancestors of `path`, shallowest first.
fn ancestors(path: &RelPath) -> Vec<RelPath> {
    let names: Vec<&[u8]> = path.components().collect();
    (1..names.len())
        .map(|taken| {
            RelPath::from_bytes(names[..taken].join(&b'/'))
                .expect("a prefix of a valid path is a valid path")
        })
        .collect()
}

/// The checks that read the index and the patterns. They are made once, at
/// planning: the index counts the entry an unearth writes itself, so they
/// cannot be repeated once that entry is written.
pub(crate) fn check_planned_target(
    rules: &Rules,
    index: &Index,
    id: ObjectId,
    entry: Option<&TrackingEntry>,
    target: &RelPath,
) -> Result<(), UnearthError> {
    let refuse = |problem| target_error(target, problem);
    if target.is_root() {
        return Err(refuse(TargetProblem::Root));
    }
    if target
        .components()
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(b".antiq"))
    {
        return Err(refuse(TargetProblem::InsideAntiq));
    }
    if !rules.matches(target) {
        return Err(refuse(TargetProblem::NotSelected));
    }
    for ancestor in ancestors(target) {
        if rules.matches(&ancestor) || index.at_path(&ancestor).claimant.is_some() {
            return Err(refuse(TargetProblem::InsidePiece { piece: ancestor }));
        }
    }
    if let Some(claimant) = index.at_path(target).claimant {
        // This object's own `live` or `gone` entry says nothing about the
        // target: its folder is not there, and an `unearthing` one without a
        // folder is what a dead run left, which the next run drops. One in
        // the middle of leaving does: its tree may come back.
        let own_and_idle = claimant == id
            && entry.is_some_and(|entry| {
                matches!(
                    entry.state,
                    EntryState::Live | EntryState::Gone | EntryState::Unearthing
                )
            });
        if !own_and_idle {
            return Err(refuse(TargetProblem::Taken));
        }
    }
    Ok(())
}

/// The checks that look at the file system. They are made at planning and
/// again right before the move. Returns the folders above the target that
/// do not exist yet, shallowest first.
pub(crate) fn check_target_on_disk(
    root: &TreeRoot,
    rules: &Rules,
    target: &RelPath,
) -> Result<Vec<RelPath>, UnearthError> {
    let refuse = |problem| target_error(target, problem);

    let mut missing = Vec::new();
    let mut parent = RelPath::root();
    for ancestor in ancestors(target) {
        match look_at(root, &ancestor)? {
            Some(found) => {
                if found.kind != EntryKind::Directory {
                    return Err(refuse(TargetProblem::NotAFolder { at: ancestor }));
                }
                if rules.excludes(&ancestor, found.device, found.inode) {
                    return Err(refuse(TargetProblem::Excluded { at: ancestor }));
                }
                if !lists_exactly(root, &parent, &ancestor)? {
                    return Err(refuse(TargetProblem::SpelledDifferently { at: ancestor }));
                }
            }
            None => {
                if rules.excludes(&ancestor, 0, 0) {
                    return Err(refuse(TargetProblem::Excluded { at: ancestor }));
                }
                missing.push(ancestor.clone());
            }
        }
        parent = ancestor;
    }
    if rules.excludes(target, 0, 0) {
        return Err(refuse(TargetProblem::Excluded { at: target.clone() }));
    }
    match look_at(root, target)? {
        Some(_) => Err(refuse(TargetProblem::Taken)),
        None => Ok(missing),
    }
}

/// `lstat` of `path`, `None` when nothing is there. Nothing is followed.
fn look_at(root: &TreeRoot, path: &RelPath) -> Result<Option<TreeEntry>, UnearthError> {
    match root.lstat(path) {
        Ok(found) => Ok(Some(found)),
        // A name past the volume's limit cannot exist; the names are held
        // to the volume's rules where the unearth reports them.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ENAMETOOLONG) =>
        {
            Ok(None)
        }
        Err(error) => Err(io_error("look at", Path::new(&path.to_string()))(error)),
    }
}

/// Whether the listing of `parent` holds the name of `path` byte for byte.
/// A volume that folds case or Unicode normalisation finds `projects` when
/// asked for `Projects`; the piece would then be tracked under a path the
/// selection never reports.
fn lists_exactly(root: &TreeRoot, parent: &RelPath, path: &RelPath) -> Result<bool, UnearthError> {
    let name = path
        .components()
        .last()
        .expect("an ancestor has at least one name");
    let listed = root
        .read_dir(parent)
        .map_err(io_error("list", Path::new(&parent.to_string())))?;
    Ok(listed.iter().any(|item| item.name.as_bytes() == name))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::config::Config;
    use crate::error::{ErrorKind, HasKind};
    use crate::lock::{self, RunLock};
    use crate::meta::{
        self, Checksum, CreatedSource, FileName, HistoryAction, HistoryEntry, Lifecycle, Metadata,
    };
    use crate::relpath::Name;
    use crate::testing::package::{Packaged, pack_piece};
    use crate::testing::tree::{COLLECTION_ID, TempCollection, Tree};
    use crate::tracking::{LocalPayload, VersionTakenOut};
    use serde_json::{Map, json};
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    pub(crate) const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
    pub(crate) const OTHER_ID: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    pub(crate) const NOW: &str = "2026-11-04T12:00:00Z";
    const ARCHIVED_AT: &str = "2026-11-03T10:00:00Z";
    const FILE_TIME: i64 = 1_000_000_000;
    pub(crate) const FILES: &[(&str, &str)] =
        &[("notes.txt", "hello"), ("src/main.rs", "fn main() {}")];

    pub(crate) fn id(text: &str) -> ObjectId {
        text.parse().expect("a valid ID")
    }

    pub(crate) fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    pub(crate) fn time(text: &str) -> jiff::Timestamp {
        text.parse().expect("a valid time")
    }

    /// A collection that selects `projects/*`, with the run lock held and
    /// the clock at [`NOW`], and helpers that leave an archived object in
    /// the store the way `archive` does.
    pub(crate) struct Fixture {
        pub(crate) temp: TempCollection,
        pub(crate) collection: Collection,
        pub(crate) clock: FixedClock,
        pub(crate) lock: RunLock,
    }

    impl Fixture {
        pub(crate) fn new() -> Fixture {
            Fixture::configured(|_| {})
        }

        pub(crate) fn configured(configure: impl FnOnce(&mut Config)) -> Fixture {
            let temp = TempCollection::minimal();
            let mut config = Config::new(id(COLLECTION_ID));
            config.machine.name = Some("imperius".to_string());
            config.collection.patterns = vec!["projects/*".to_string()];
            configure(&mut config);
            let collection = Collection::new(temp.root().to_path_buf(), config);
            let clock = FixedClock::new(time(NOW));
            let lock = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("a lock");
            Fixture {
                temp,
                collection,
                clock,
                lock,
            }
        }

        pub(crate) fn tree(&mut self) -> &mut Tree {
            self.temp.tree()
        }

        pub(crate) fn at(&self, relative: &str) -> PathBuf {
            self.temp.root().join(relative)
        }

        pub(crate) fn request(&self, object: &str) -> UnearthRequest {
            UnearthRequest {
                id: id(object),
                to: None,
                version: None,
            }
        }

        pub(crate) fn plan(&self, request: &UnearthRequest) -> Result<Planned, UnearthError> {
            plan_with(&self.collection, request, &|_| Ok(u64::MAX))
        }

        /// The plan of an unearth that is expected to go ahead.
        pub(crate) fn planned(&self, request: &UnearthRequest) -> Box<UnearthPlan> {
            match self.plan(request).expect("the unearth is planned") {
                Planned::Unearth(plan) => plan,
                other => panic!("expected a plan, got {other:?}"),
            }
        }

        pub(crate) fn refusal(&self, request: &UnearthRequest) -> UnearthError {
            self.plan(request).expect_err("the unearth is refused")
        }

        pub(crate) fn write_entry(&self, entry: &mut TrackingEntry) {
            tracking::write(&self.collection, &self.lock, &self.clock, entry).expect("an entry");
        }

        pub(crate) fn entry(&self, object: &str) -> TrackingEntry {
            tracking::read(&self.collection, id(object))
                .expect("the entry reads")
                .expect("the entry exists")
        }

        pub(crate) fn edit_meta(&self, object: &str, edit: impl FnOnce(&mut serde_json::Value)) {
            let path = self.collection.object_dir(id(object)).join("meta.1");
            let mut document: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).expect("reads")).expect("json");
            edit(&mut document);
            fs::write(&path, serde_json::to_vec(&document).expect("json")).expect("written");
        }

        /// An archived piece as `archive` leaves it: the payload, its
        /// listing and committed metadata in the store, an entry `gone`
        /// that keeps the local payload, and no folder.
        pub(crate) fn archived(
            &mut self,
            object: &str,
            piece: &str,
            files: &[(&str, &str)],
        ) -> FileName {
            self.archived_with(object, piece, files, |_| {})
        }

        /// [`Fixture::archived`] with more in the piece than plain files.
        pub(crate) fn archived_with(
            &mut self,
            object: &str,
            piece: &str,
            files: &[(&str, &str)],
            prepare: impl FnOnce(&mut Tree),
        ) -> FileName {
            for (name, content) in files {
                self.tree()
                    .file_with(&format!("{piece}/{name}"), content, 0o644, FILE_TIME);
            }
            prepare(self.tree());
            // Directories carry the time they were made, which would give
            // every build of the same scenario other bytes.
            let fixed = filetime::FileTime::from_unix_time(FILE_TIME, 0);
            let mut directories = vec![self.at(piece)];
            for (name, _) in files {
                let mut parent = self.at(&format!("{piece}/{name}"));
                while parent.pop() && parent != self.at(piece) {
                    directories.push(parent.clone());
                }
            }
            for directory in directories {
                filetime::set_file_mtime(directory, fixed).expect("the time can be set");
            }
            let packaged = pack_piece(self.tree(), piece);
            self.archive_package(object, piece, &packaged)
        }

        /// Puts `packaged` into the store as the archive of `object` at
        /// `piece`, as `archive` does, and removes the folder.
        pub(crate) fn archive_package(
            &mut self,
            object: &str,
            piece: &str,
            packaged: &Packaged,
        ) -> FileName {
            let sha = packaged.packed.sha256.clone();
            let payload = FileName::new(format!("{}-a.tar.zst", &sha[..16])).expect("a name");
            let listing_file = listing::file_name(&sha).expect("a listing name");

            let object_dir = self.collection.object_dir(id(object));
            fs::create_dir_all(&object_dir).expect("the object directory");
            fs::write(object_dir.join(payload.as_str()), &packaged.bytes).expect("the payload");
            let out = fs::File::create(object_dir.join(listing_file.as_str())).expect("a file");
            listing::write(packaged.packed.listing.iter(), out).expect("the listing is written");
            let root = TreeRoot::open(self.temp.root()).expect("the collection opens");
            crate::fsx::remove::remove_tree(&root, &path(piece))
                .expect("the folder is archived away");

            let top = path(piece);
            let name = top
                .components()
                .last()
                .map(|name| Name::from_bytes(name.to_vec()).expect("a name"))
                .expect("a piece has a name");
            let archived_at = time(ARCHIVED_AT);
            let metadata = Metadata {
                format: meta::FORMAT,
                id: id(object),
                collection: id(COLLECTION_ID),
                path: top.clone(),
                name,
                created: archived_at,
                created_source: CreatedSource::Birth,
                lifecycle: Lifecycle::Archived,
                payloads: vec![Payload {
                    kind: PayloadKind::Archive,
                    version: 1,
                    file: payload.clone(),
                    package_format: "tar+zstd/1".to_string(),
                    size: packaged.packed.size,
                    checksum: Checksum::sha256(sha),
                    listing: Some(listing_file),
                    machine: "imperius".to_string(),
                    created: archived_at,
                    verified: None,
                    superseded: false,
                    encoder: None,
                    extra: Map::new(),
                }],
                history: vec![HistoryEntry::new(
                    archived_at,
                    "imperius",
                    HistoryAction::Archive,
                )],
                extra: Map::new(),
            };
            fs::write(
                object_dir.join("meta.1"),
                meta::to_bytes(&metadata).expect("the metadata serialises"),
            )
            .expect("the metadata is stored");

            let mut entry = TrackingEntry::new(
                id(object),
                top,
                EntryState::Gone,
                VersionTakenOut::Committed(1),
                archived_at,
            );
            entry.local_payloads = vec![LocalPayload {
                kind: PayloadKind::Archive,
                version: 1,
                file: payload.clone(),
            }];
            self.write_entry(&mut entry);
            payload
        }

        /// A further archive payload in the metadata, its file present.
        pub(crate) fn add_archive(&self, object: &str, version: u32, superseded: bool) -> String {
            let file = format!("{version:016x}-b.tar.zst");
            self.edit_meta(object, |document| {
                let mut payload = document["payloads"][0].clone();
                payload["version"] = json!(version);
                payload["file"] = json!(file);
                payload["created"] = json!("2026-11-03T11:00:00Z");
                if superseded {
                    payload["superseded"] = json!(true);
                }
                document["payloads"]
                    .as_array_mut()
                    .expect("payloads")
                    .push(payload);
            });
            fs::write(
                self.collection.object_dir(id(object)).join(&file),
                "payload",
            )
            .expect("the payload file");
            file
        }

        /// The only payload becomes a live package, as it is for a piece
        /// that was never archived.
        pub(crate) fn live_only(&self, object: &str) -> String {
            self.edit_meta(object, |document| {
                let payload = document["payloads"][0].as_object_mut().expect("a payload");
                payload.insert("kind".into(), json!("live"));
                payload.insert("size".into(), json!(77));
                payload.remove("listing");
            });
            let document: serde_json::Value = serde_json::from_slice(
                &fs::read(self.collection.object_dir(id(object)).join("meta.1")).expect("reads"),
            )
            .expect("json");
            document["payloads"][0]["file"]
                .as_str()
                .expect("a file")
                .to_string()
        }

        /// A live package payload in the metadata next to the others.
        pub(crate) fn add_live(&self, object: &str, version: u32) -> String {
            let file = format!("{version:016x}-l.tar.zst");
            self.edit_meta(object, |document| {
                let mut payload = document["payloads"][0].clone();
                payload["kind"] = json!("live");
                payload["version"] = json!(version);
                payload["file"] = json!(file);
                payload["size"] = json!(77);
                payload
                    .as_object_mut()
                    .expect("a payload")
                    .remove("listing");
                document["payloads"]
                    .as_array_mut()
                    .expect("payloads")
                    .push(payload);
            });
            file
        }

        /// The metadata says the live piece belongs to another collection.
        pub(crate) fn make_foreign(&self, object: &str) {
            self.edit_meta(object, |document| {
                document["lifecycle"] = json!("live");
                document["collection"] = json!(OTHER_ID);
            });
        }

        /// The state after an unearth: the folder is in place and the entry
        /// says `live`.
        pub(crate) fn unearthed(&mut self, object: &str, piece: &str) {
            self.tree().file(&format!("{piece}/notes.txt"), "hello");
            let mut entry = self.entry(object);
            entry.state = EntryState::Live;
            entry.unearthed = Some(time(NOW));
            entry.unearthed_payload_version = Some(1);
            self.write_entry(&mut entry);
            self.edit_meta(object, |document| {
                document["lifecycle"] = json!("live");
            });
        }

        /// The entry of another object that holds `piece` with a folder.
        pub(crate) fn hold(&self, object: &str, piece: &str, state: EntryState) {
            let mut entry = TrackingEntry::new(
                id(object),
                path(piece),
                state,
                VersionTakenOut::Committed(1),
                time(ARCHIVED_AT),
            );
            self.write_entry(&mut entry);
        }
    }

    fn target(text: &str) -> UnearthRequest {
        UnearthRequest {
            id: id(ID),
            to: Some(path(text)),
            version: None,
        }
    }

    fn problem(error: UnearthError) -> TargetProblem {
        match error {
            UnearthError::Target { problem, .. } => problem,
            other => panic!("expected a target problem, got {other}"),
        }
    }

    #[test]
    fn every_problem_of_a_target_says_what_it_is() {
        let at = path("projects/x");
        let texts: Vec<String> = [
            TargetProblem::Root,
            TargetProblem::InsideAntiq,
            TargetProblem::NotSelected,
            TargetProblem::Excluded { at: at.clone() },
            TargetProblem::InsidePiece { piece: at.clone() },
            TargetProblem::NotAFolder { at: at.clone() },
            TargetProblem::SpelledDifferently { at },
            TargetProblem::Taken,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            texts,
            [
                "it is the collection root",
                "it is inside `.antiq/`",
                "the patterns do not select it",
                "`projects/x` is excluded",
                "`projects/x` is a piece",
                "`projects/x` is a file or a symlink, not a folder",
                "`projects/x` is spelled differently on disk",
                "it is taken",
            ]
        );
    }

    #[test]
    fn an_object_that_is_live_here_without_an_entry_is_an_error_of_that_piece() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("live"));
        tracking::remove(&fx.collection, &fx.lock, id(ID)).expect("no entry");

        let error = fx.refusal(&fx.request(ID));

        assert!(matches!(error, UnearthError::Tracking(_)), "{error}");
    }

    #[test]
    fn a_pattern_that_does_not_parse_is_an_error() {
        let mut fx = Fixture::configured(|config| config.collection.patterns = vec!["[".into()]);
        fx.archived(ID, "projects/a", FILES);

        let error = fx.refusal(&fx.request(ID));

        assert!(matches!(error, UnearthError::Select(_)), "{error}");
    }

    // ---------------------------------------------------------------------
    // What is unearthed
    // ---------------------------------------------------------------------

    #[test]
    fn an_archived_piece_is_planned_from_its_current_archive_to_its_stored_path() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);

        let plan = fx.planned(&fx.request(ID));

        assert_eq!(plan.target(), &path("projects/a"));
        assert_eq!(plan.payload_version(), 1);
        assert_eq!(plan.payload.file, payload);
        assert_eq!(plan.shortfall(), None);
    }

    #[test]
    fn a_version_picks_that_archive_and_a_missing_one_names_the_available() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.add_archive(ID, 2, false);

        assert_eq!(fx.planned(&fx.request(ID)).payload_version(), 2);
        let first = UnearthRequest {
            version: Some(1),
            ..fx.request(ID)
        };
        assert_eq!(fx.planned(&first).payload_version(), 1);

        let missing = UnearthRequest {
            version: Some(9),
            ..fx.request(ID)
        };
        let error = fx.refusal(&missing);
        assert!(
            matches!(&error, UnearthError::NoSuchVersion { version: 9, available, .. } if available == "1, 2"),
            "{error}"
        );
    }

    #[test]
    fn a_superseded_archive_is_not_the_current_one() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.add_archive(ID, 2, true);

        assert_eq!(fx.planned(&fx.request(ID)).payload_version(), 1);
        let asked = UnearthRequest {
            version: Some(2),
            ..fx.request(ID)
        };
        assert_eq!(fx.planned(&asked).payload_version(), 2);
    }

    #[test]
    fn of_two_payloads_with_one_version_the_one_this_machine_holds_wins() {
        let mut fx = Fixture::new();
        let held = fx.archived(ID, "projects/a", FILES);
        let other = fx.add_archive(ID, 1, false);
        fs::remove_file(fx.collection.object_dir(id(ID)).join(&other)).expect("not held");

        assert_eq!(fx.planned(&fx.request(ID)).payload.file, held);
    }

    #[test]
    fn a_piece_that_is_unearthed_is_planned_as_such_whatever_version_is_asked() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.unearthed(ID, "projects/a");
        let asked = UnearthRequest {
            version: Some(9),
            ..fx.request(ID)
        };

        let planned = fx.plan(&asked).expect("planned");

        assert!(
            matches!(&planned, Planned::AlreadyUnearthed { path: at, .. } if *at == path("projects/a")),
            "{planned:?}"
        );
        assert_eq!(planned.plan().lines(), ["Nothing to do."]);
    }

    #[test]
    fn a_gone_piece_with_an_archive_is_unearthed_from_it_although_its_entry_says_live() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("live"));
        let mut entry = fx.entry(ID);
        entry.state = EntryState::Live;
        fx.write_entry(&mut entry);

        let plan = fx.planned(&fx.request(ID));

        assert_eq!(plan.target(), &path("projects/a"));
    }

    #[test]
    fn a_gone_piece_without_archive_is_planned_from_its_live_package() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("live"));
        fx.live_only(ID);

        let plan = fx.planned(&fx.request(ID));

        assert_eq!(plan.source(), UnearthSource::LivePackage);
        assert_eq!(plan.target(), &path("projects/a"));
        assert_eq!(plan.needed, 77);
    }

    #[test]
    fn a_gone_piece_without_any_payload_has_none_to_unearth_from() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| {
            document["lifecycle"] = json!("live");
            document["payloads"] = json!([]);
        });

        let error = fx.refusal(&fx.request(ID));

        assert!(matches!(error, UnearthError::NoArchive { .. }), "{error}");
    }

    #[test]
    fn a_version_for_a_gone_piece_without_archive_does_not_fall_back_to_its_live_package() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("live"));
        fx.live_only(ID);
        let asked = UnearthRequest {
            version: Some(1),
            ..fx.request(ID)
        };

        let error = fx.refusal(&asked);

        assert!(matches!(error, UnearthError::NoArchive { .. }), "{error}");
    }

    #[test]
    fn a_live_piece_of_another_collection_is_planned_as_a_fork() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.make_foreign(ID);
        fx.live_only(ID);

        let plan = fx.planned(&fx.request(ID));

        assert_eq!(plan.source(), UnearthSource::Fork);
        assert_eq!(plan.target(), &path("projects/a"));
    }

    #[test]
    fn a_live_piece_of_another_collection_with_an_archive_is_still_a_fork() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.make_foreign(ID);
        let live = fx.add_live(ID, 2);

        let plan = fx.planned(&fx.request(ID));

        assert_eq!(plan.source(), UnearthSource::Fork);
        assert_eq!(plan.payload.file.as_str(), live);
    }

    #[test]
    fn a_version_for_a_foreign_live_piece_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.make_foreign(ID);
        fx.add_live(ID, 2);
        let asked = UnearthRequest {
            version: Some(1),
            ..fx.request(ID)
        };

        let error = fx.refusal(&asked);

        assert!(
            matches!(error, UnearthError::VersionOfLivePiece { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_live_piece_without_a_live_package_has_none_to_unearth_from() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.make_foreign(ID);

        let error = fx.refusal(&fx.request(ID));

        assert!(
            matches!(error, UnearthError::NoLivePackage { .. }),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "`projects/a` has no live package to unearth from"
        );
    }

    #[test]
    fn a_live_package_of_another_format_is_unsupported() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.make_foreign(ID);
        fx.add_live(ID, 2);
        fx.edit_meta(ID, |document| {
            document["payloads"][1]["package_format"] = json!("tar+zstd/2");
        });

        let error = fx.refusal(&fx.request(ID));

        assert!(
            matches!(&error, UnearthError::UnsupportedPayload { reason, .. } if reason.contains("tar+zstd/2")),
            "{error}"
        );
    }

    #[test]
    fn an_archive_that_is_not_on_this_machine_is_planned_as_a_fetch() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);
        let object_dir = fx.collection.object_dir(id(ID));
        let size = fs::metadata(object_dir.join(payload.as_str()))
            .expect("the archive")
            .len();
        fs::remove_file(object_dir.join(payload.as_str())).expect("gone");

        let plan = fx.planned(&fx.request(ID));

        assert_eq!(plan.source(), UnearthSource::RemoteArchive);
        assert_eq!(plan.payload_version(), 1);
        let listed: u64 = FILES.iter().map(|(_, content)| content.len() as u64).sum();
        assert_eq!(plan.needed, size + listed);
    }

    #[test]
    fn a_fetch_is_planned_although_its_listing_is_not_on_this_machine_either() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);
        let object_dir = fx.collection.object_dir(id(ID));
        let size = fs::metadata(object_dir.join(payload.as_str()))
            .expect("the archive")
            .len();
        fs::remove_file(object_dir.join(payload.as_str())).expect("gone");
        for entry in fs::read_dir(&object_dir).expect("lists") {
            let name = entry.expect("an entry").file_name();
            if name.to_string_lossy().ends_with(".listing.zst") {
                fs::remove_file(object_dir.join(name)).expect("gone");
            }
        }

        let plan = fx.planned(&fx.request(ID));

        assert_eq!(plan.source(), UnearthSource::RemoteArchive);
        assert_eq!(plan.needed, size);
    }

    #[test]
    fn the_given_current_is_what_planning_works_from() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let mut current = Store::new(&fx.collection)
            .current(id(ID))
            .expect("reads")
            .expect("exists");
        current.metadata.path = path("projects/b");

        let planned = plan_from(&fx.collection, &fx.request(ID), Some(current), &|_| {
            Ok(u64::MAX)
        })
        .expect("planned");

        assert!(
            matches!(&planned, Planned::Unearth(plan) if plan.target() == &path("projects/b")),
            "{planned:?}"
        );
    }

    #[test]
    fn an_unknown_object_has_no_metadata() {
        let fx = Fixture::new();
        let error = fx.refusal(&fx.request(OTHER_ID));
        assert!(matches!(error, UnearthError::NoMetadata { .. }), "{error}");
    }

    #[test]
    fn an_unreadable_committed_version_refuses_before_anything_is_extracted() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fs::write(fx.collection.object_dir(id(ID)).join("meta.2"), "not json").expect("meta.2");

        let error = fx.refusal(&fx.request(ID));

        assert!(
            matches!(error, UnearthError::UnreadableVersions { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_error_of_a_fork_keeps_its_kind() {
        let fork = UnearthError::Fork(ForkError::InvalidName {
            name: "a/../b".to_string(),
            reason: RelPath::parse("a/../b").expect_err("not a path"),
        });

        assert_eq!(fork.kind(), ErrorKind::Usage);
    }

    #[test]
    fn metadata_of_a_later_format_is_reported_with_its_own_kind() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fs::write(
            fx.collection.object_dir(id(ID)).join("meta.2"),
            r#"{"format": 2}"#,
        )
        .expect("meta.2");

        let error = fx.refusal(&fx.request(ID));

        assert_eq!(error.kind(), ErrorKind::FormatTooNew, "{error}");
    }

    #[test]
    fn a_broken_later_version_is_an_error_of_the_generic_kind() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fs::write(fx.collection.object_dir(id(ID)).join("meta.2"), "not json").expect("meta.2");

        let error = fx.refusal(&fx.request(ID));

        assert_eq!(error.kind(), ErrorKind::Other, "{error}");
    }

    #[test]
    fn a_listing_of_a_later_format_and_unreadable_metadata_keep_their_kinds() {
        let listing = UnearthError::Listing(ListingError::FormatTooNew { found: 2 });
        assert_eq!(listing.kind(), ErrorKind::FormatTooNew);
        let store = UnearthError::Store(StoreError::NothingReadable {
            unreadable: vec![crate::store::Unreadable {
                number: 1,
                format: Some(2),
                reason: "too new".into(),
            }],
        });
        assert_eq!(store.kind(), ErrorKind::FormatTooNew);
    }

    #[test]
    fn a_package_format_other_than_tar_zstd_is_refused_by_name() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| {
            document["payloads"][0]["package_format"] = json!("zip/1");
        });

        let error = fx.refusal(&fx.request(ID));

        assert!(error.to_string().contains("zip/1"), "{error}");
    }

    #[test]
    fn a_checksum_other_than_sha256_is_refused_by_name() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| {
            document["payloads"][0]["checksum"]["algorithm"] = json!("md5");
        });

        let error = fx.refusal(&fx.request(ID));

        assert!(error.to_string().contains("md5"), "{error}");
    }

    #[test]
    fn an_archive_without_a_listing_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| {
            document["payloads"][0]
                .as_object_mut()
                .expect("a payload")
                .remove("listing");
        });

        let error = fx.refusal(&fx.request(ID));

        assert!(error.to_string().contains("no listing"), "{error}");
    }

    #[test]
    fn a_listing_that_is_missing_from_the_store_is_an_error() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        for item in fs::read_dir(fx.collection.object_dir(id(ID))).expect("lists") {
            let item = item.expect("an item").path();
            if item.to_string_lossy().ends_with(".listing.zst") {
                fs::remove_file(item).expect("removed");
            }
        }

        let error = fx.refusal(&fx.request(ID));

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
    }

    #[test]
    fn too_little_free_space_is_reported_as_a_shortfall_and_not_as_an_error() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);

        let planned = plan_with(&fx.collection, &fx.request(ID), &|_| Ok(10)).expect("planned");

        let Planned::Unearth(plan) = planned else {
            panic!("expected a plan");
        };
        assert_eq!(
            plan.shortfall(),
            Some(Shortfall {
                needed: 17,
                available: 10
            })
        );
    }

    #[test]
    fn a_volume_whose_space_cannot_be_read_is_an_error() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let refuse = |_: &Path| {
            Err(FsxError::Io {
                action: "stat",
                path: "staging".to_string(),
                source: io::Error::other("refused by the test"),
            })
        };

        let error = plan_with(&fx.collection, &fx.request(ID), &refuse).expect_err("fails");

        assert!(matches!(error, UnearthError::Fsx(_)), "{error}");
    }

    #[test]
    fn the_real_free_space_of_the_staging_volume_is_enough_for_a_small_piece() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);

        let planned = plan(&fx.collection, &fx.request(ID)).expect("planned");

        assert!(matches!(planned, Planned::Unearth(plan) if plan.shortfall().is_none()));
    }

    #[test]
    fn a_dry_run_on_a_volume_without_room_says_the_piece_would_be_skipped() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let planned = plan_with(&fx.collection, &fx.request(ID), &|_| Ok(10)).expect("planned");

        let plan = planned.plan();

        assert_eq!(
            plan.lines(),
            ["Would skip projects/a: it needs 17 bytes and the volume has 10"]
        );
        assert_eq!(
            serde_json::to_value(plan.dry_run()).expect("serialises"),
            json!({
                "dry_run": true,
                "actions": [{
                    "action": "skip",
                    "id": ID,
                    "path": "projects/a",
                    "reason": "no_space",
                    "needed": 17,
                    "available": 10,
                }],
            })
        );
    }

    #[test]
    fn a_dry_run_prints_one_would_line_and_a_json_plan() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let planned = Planned::Unearth(fx.planned(&fx.request(ID)));

        let plan = planned.plan();

        assert_eq!(
            plan.lines(),
            ["Would unearth projects/a from archive version 1"]
        );
        assert_eq!(
            serde_json::to_value(plan.dry_run()).expect("serialises"),
            json!({
                "dry_run": true,
                "actions": [{
                    "action": "unearth",
                    "id": ID,
                    "path": "projects/a",
                    "payload_version": 1,
                }],
            })
        );
    }

    #[test]
    fn a_dry_run_names_the_source_in_its_lines_and_json() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);
        fs::remove_file(fx.collection.object_dir(id(ID)).join(payload.as_str())).expect("gone");
        fx.archived("01D39ZY06FGSCTVN4T2V9PKHFZ", "projects/b", FILES);
        fx.edit_meta(OTHER_ID, |document| document["lifecycle"] = json!("live"));
        fx.live_only(OTHER_ID);
        fx.archived("01ARZ3NDEKTSV4RRFFQ69G5FAV", "projects/c", FILES);
        fx.make_foreign("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        fx.live_only("01ARZ3NDEKTSV4RRFFQ69G5FAV");

        let lines_and_json = |object: &str| {
            let plan = Planned::Unearth(fx.planned(&fx.request(object))).plan();
            (
                plan.lines(),
                serde_json::to_value(plan.dry_run()).expect("serialises")["actions"][0].clone(),
            )
        };

        let (lines, json) = lines_and_json(ID);
        assert_eq!(
            lines,
            ["Would fetch archive version 1 and unearth projects/a"]
        );
        assert_eq!(json["source"], "remote_archive");
        assert_eq!(json["payload_version"], 1);
        let (lines, json) = lines_and_json(OTHER_ID);
        assert_eq!(lines, ["Would unearth projects/b from its live package"]);
        assert_eq!(json["source"], "live_package");
        assert!(json.get("payload_version").is_none(), "{json}");
        let (lines, json) = lines_and_json("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(
            lines,
            [
                "Would unearth projects/c from the live package of another collection's piece, as a fork with a new ID"
            ]
        );
        assert_eq!(json["source"], "fork");
    }

    #[test]
    fn a_report_leaves_out_what_does_not_apply_and_says_when_it_is_partial() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);

        let already = UnearthReport::already_unearthed(
            &fx.collection,
            id(ID),
            path("projects/a"),
            vec![Warning::new("a note")],
        );

        assert!(!CommandReport::partial(&already));
        assert_eq!(CommandReport::warnings(&already).len(), 1);
        assert_eq!(
            serde_json::to_value(&already).expect("serialises"),
            json!({
                "unearthed": false,
                "id": ID,
                "path": "projects/a",
                "abs_path": fx.at("projects/a").display().to_string(),
                "warnings": [{"message": "a note"}],
            })
        );
        let skipped = UnearthReport {
            skipped: Some(Skipped::NoSpace {
                needed: 17,
                available: 10,
            }),
            ..already
        };
        assert!(CommandReport::partial(&skipped));
        assert_eq!(
            serde_json::to_value(skipped.skipped).expect("serialises"),
            json!({"reason": "no_space", "needed": 17, "available": 10})
        );
    }

    // ---------------------------------------------------------------------
    // Where it goes
    // ---------------------------------------------------------------------

    #[test]
    fn the_collection_root_is_no_target() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let request = UnearthRequest {
            to: Some(RelPath::root()),
            ..fx.request(ID)
        };
        assert_eq!(problem(fx.refusal(&request)), TargetProblem::Root);
    }

    #[test]
    fn nothing_inside_the_antiq_directory_is_a_target() {
        let mut fx = Fixture::configured(|config| config.collection.patterns = vec!["*".into()]);
        fx.archived(ID, "projects/a", FILES);
        for text in [".antiq/x", ".ANTIQ/x", ".antiq"] {
            assert_eq!(
                problem(fx.refusal(&target(text))),
                TargetProblem::InsideAntiq,
                "{text}"
            );
        }
    }

    #[test]
    fn a_target_the_patterns_do_not_select_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        assert_eq!(
            problem(fx.refusal(&target("elsewhere/a"))),
            TargetProblem::NotSelected
        );
    }

    #[test]
    fn a_target_on_the_exclude_list_is_refused() {
        let mut fx =
            Fixture::configured(|config| config.collection.exclude = vec![path("projects/a")]);
        fx.archived(ID, "projects/b", FILES);
        assert_eq!(
            problem(fx.refusal(&target("projects/a"))),
            TargetProblem::Excluded {
                at: path("projects/a")
            }
        );
    }

    #[test]
    fn a_missing_folder_above_the_target_that_is_excluded_is_refused() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
            config.collection.exclude = vec![path("projects/skip")];
        });
        fx.archived(ID, "projects/b/a", FILES);
        assert_eq!(
            problem(fx.refusal(&target("projects/skip/a"))),
            TargetProblem::Excluded {
                at: path("projects/skip")
            }
        );
    }

    #[test]
    fn an_existing_folder_above_the_target_that_is_excluded_is_refused() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
            config.collection.exclude = vec![path("projects/skip")];
        });
        fx.archived(ID, "projects/b/a", FILES);
        fx.tree().dir("projects/skip");
        assert_eq!(
            problem(fx.refusal(&target("projects/skip/a"))),
            TargetProblem::Excluded {
                at: path("projects/skip")
            }
        );
    }

    #[test]
    fn a_folder_above_the_target_that_the_patterns_select_is_a_piece() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["*".into(), "projects/*".into()];
        });
        fx.archived(ID, "projects/a", FILES);
        assert_eq!(
            problem(fx.refusal(&target("projects/b"))),
            TargetProblem::InsidePiece {
                piece: path("projects")
            }
        );
    }

    #[test]
    fn a_folder_above_the_target_that_holds_a_tracked_piece_is_a_piece() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        fx.tree().dir("projects/x");
        fx.hold(OTHER_ID, "projects/x", EntryState::Live);
        assert_eq!(
            problem(fx.refusal(&target("projects/x/a"))),
            TargetProblem::InsidePiece {
                piece: path("projects/x")
            }
        );
    }

    #[test]
    fn a_file_above_the_target_refuses() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(fx.at("projects")).expect("the parent folder goes");
        fx.tree().file("projects", "not a folder");
        assert_eq!(
            problem(fx.refusal(&fx.request(ID))),
            TargetProblem::NotAFolder {
                at: path("projects")
            }
        );
    }

    #[test]
    fn a_symlink_above_the_target_refuses_and_nothing_is_written_through_it() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(fx.at("projects")).expect("the parent folder goes");
        let outside = tempfile::tempdir().expect("a folder outside");
        symlink(outside.path(), fx.at("projects")).expect("a link");
        assert_eq!(
            problem(fx.refusal(&fx.request(ID))),
            TargetProblem::NotAFolder {
                at: path("projects")
            }
        );
        assert_eq!(fs::read_dir(outside.path()).expect("lists").count(), 0);
    }

    #[test]
    fn a_target_that_exists_in_any_form_is_taken() {
        for make in ["folder", "file", "dangling symlink"] {
            let mut fx = Fixture::new();
            fx.archived(ID, "projects/a", FILES);
            match make {
                "folder" => {
                    fx.tree().dir("projects/a");
                }
                "file" => {
                    fx.tree().file("projects/a", "x");
                }
                _ => {
                    fx.tree().symlink("projects/a", "nowhere");
                }
            }
            assert_eq!(
                problem(fx.refusal(&fx.request(ID))),
                TargetProblem::Taken,
                "{make}"
            );
        }
    }

    #[test]
    fn a_target_that_another_tracked_piece_holds_is_taken_although_its_folder_is_missing() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.hold(OTHER_ID, "projects/a", EntryState::Live);
        assert_eq!(problem(fx.refusal(&fx.request(ID))), TargetProblem::Taken);
    }

    #[test]
    fn a_target_that_this_objects_unfinished_removal_holds_is_taken() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let mut entry = fx.entry(ID);
        entry.state = EntryState::Removing;
        fx.write_entry(&mut entry);
        assert_eq!(problem(fx.refusal(&fx.request(ID))), TargetProblem::Taken);
    }

    #[test]
    fn an_unearthing_entry_of_this_object_without_a_folder_does_not_block_the_target() {
        // A dry run does not repair, so it can meet what a dead run left.
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let mut entry = fx.entry(ID);
        entry.state = EntryState::Unearthing;
        fx.write_entry(&mut entry);

        assert_eq!(fx.planned(&fx.request(ID)).target(), &path("projects/a"));
    }

    #[test]
    fn an_archived_object_at_the_same_path_does_not_block_the_target() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.archived(OTHER_ID, "projects/a", FILES);
        assert_eq!(fx.planned(&fx.request(ID)).target(), &path("projects/a"));
    }

    #[test]
    fn missing_folders_above_the_target_are_no_obstacle() {
        let mut fx =
            Fixture::configured(|config| config.collection.patterns = vec!["projects/*/*".into()]);
        fx.archived(ID, "projects/b/a", FILES);
        let plan = fx.planned(&target("projects/new/a"));
        assert_eq!(plan.target(), &path("projects/new/a"));
    }

    /// Takes the right to enter `relative` away, and gives it back when the
    /// test ends.
    fn lock_out(fx: &Fixture, relative: &str, mode: u32) -> crate::fsx::RestoreModes {
        use std::os::unix::fs::PermissionsExt;
        let folder = fx.at(relative);
        fs::set_permissions(&folder, fs::Permissions::from_mode(mode)).expect("chmod");
        let guard = crate::fsx::RestoreModes(vec![folder.clone()]);
        assert!(
            fs::read_dir(folder.join("a")).is_err() || fs::metadata(folder.join("a")).is_err(),
            "this test needs a user that mode {mode:o} locks out"
        );
        guard
    }

    #[test]
    fn a_folder_above_the_target_that_cannot_be_entered_is_an_error() {
        let mut fx =
            Fixture::configured(|config| config.collection.patterns = vec!["projects/*/*".into()]);
        fx.archived(ID, "projects/b/a", FILES);
        let _guard = lock_out(&fx, "projects", 0o000);

        let error = fx.refusal(&target("projects/b/c"));

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
    }

    #[test]
    fn a_target_that_cannot_be_looked_at_is_an_error() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.tree().dir("projects/x");
        let _guard = lock_out(&fx, "projects", 0o000);

        let error = fx.refusal(&target("projects/c"));

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
    }

    #[test]
    fn a_parent_that_cannot_be_listed_is_an_error() {
        let mut fx =
            Fixture::configured(|config| config.collection.patterns = vec!["projects/*/*".into()]);
        fx.archived(ID, "projects/b/a", FILES);
        let _guard = lock_out(&fx, "projects", 0o100);

        let error = fx.refusal(&target("projects/b/c"));

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
    }

    #[test]
    fn a_name_is_listed_exactly_or_not_at_all() {
        let mut fx = Fixture::new();
        fx.tree().dir("projects");
        let root = TreeRoot::open(fx.temp.root()).expect("opens");
        assert!(lists_exactly(&root, &RelPath::root(), &path("projects")).expect("lists"));
        assert!(!lists_exactly(&root, &RelPath::root(), &path("Projects")).expect("lists"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_folder_above_the_target_that_the_volume_finds_under_another_case_refuses() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fs::remove_dir_all(fx.at("projects")).expect("the parent folder goes");
        fx.tree().dir("Projects");
        assert!(
            fx.at("projects").exists(),
            "this test needs the case-insensitive volume macOS uses by default"
        );
        assert_eq!(
            problem(fx.refusal(&fx.request(ID))),
            TargetProblem::SpelledDifferently {
                at: path("projects")
            }
        );
    }
}
