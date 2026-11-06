// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `import` command: registering existing zstd tar archives as
//! archived pieces.
//!
//! Each file is inspected before anything of it is written: the walk over
//! its zstd frames, the tar inside, the structure `unearth` will need and
//! the path it would get are all checked without the run lock. Under the
//! lock the decisions that depend on the store are made again, and only
//! then does the file get its object.

use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::fsx::tree::TreeRoot;
use crate::id::{EntryTimes, IdSource, ObjectId, id_time};
use crate::index::{self, Index, IndexError};
use crate::listing::{self, ListingEntry, ListingError};
use crate::lock::Holder;
use crate::machine::MachineError;
use crate::meta::{
    Checksum, CreatedSource, FORMAT, FileName, HistoryAction, HistoryEntry, Lifecycle, MetaError,
    Metadata, Payload, PayloadKind,
};
use crate::pack::PackError;
use crate::pack::PackageFormat;
use crate::pack::extract::check_structure;
use crate::pack::frames;
use crate::pack::hashing::{HashingReader, hex};
use crate::pack::names::{MemberName, portability_warnings};
use crate::pack::read::read_package_from;
use crate::plan::PlanAction;
use crate::reference::{ReferenceError, resolve_path};
use crate::relpath::{self, Name, RelPath, escape};
use crate::report::{CommandReport, Warning};
use crate::run::{self, RunError, RunGuard, Started};
use crate::select::{Rules, SelectError};
use crate::store::{Source, Store, StoreError};
use crate::tracking::{
    self, EntryState, LocalPayload, TrackingEntry, TrackingError, VersionTakenOut,
};
use jiff::Timestamp;
use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Errors of the `import` command. A file that is refused is not an error
/// of the run: it is listed in the report.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error(transparent)]
    Listing(#[from] ListingError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Reference(#[from] ReferenceError),
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("the run stopped after {step}")]
    Interrupted {
        step: &'static str,
        source: io::Error,
    },
}

impl HasKind for ImportError {
    fn kind(&self) -> ErrorKind {
        match self {
            ImportError::Reference(error) => error.kind(),
            ImportError::Store(error) => error.kind(),
            ImportError::Meta(error) => error.kind(),
            ImportError::Listing(error) => error.kind(),
            ImportError::Run(error) => error.kind(),
            _ => ErrorKind::Other,
        }
    }
}

/// What the caller asks `import` to do.
#[derive(Debug)]
pub struct ImportRequest<'a> {
    pub collection: &'a Collection,
    /// The directory relative files and `--path` start at.
    pub cwd: &'a Path,
    pub files: &'a [PathBuf],
    /// The piece path; without it each archive gets its top-level
    /// folder's name at the collection root.
    pub path: Option<&'a Path>,
}

/// What a dry run says it would do for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ImportAction {
    Import { file: String, path: RelPath },
}

impl PlanAction for ImportAction {
    fn describe(&self) -> String {
        match self {
            ImportAction::Import { file, path } => format!("import {file} as {path}"),
        }
    }
}

/// A file that became an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Imported {
    pub id: ObjectId,
    pub file: String,
    pub path: RelPath,
    pub name: Name,
    pub created: Timestamp,
    pub size: u64,
    pub sha256: String,
    pub payload_version: u32,
}

/// Why a file was not imported although it is a good archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The collection has a payload with these bytes.
    AlreadyImported,
    /// The collection has them, and the tracking entry that an
    /// interrupted import did not write was written now.
    EntryCompleted,
    /// An earlier file of this run has these bytes.
    SameArchiveInThisRun,
}

/// A file that was skipped, and the object that has its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub reason: SkipReason,
}

/// A file that was refused, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refused {
    pub file: String,
    pub reason: String,
}

/// What a run of `import` did or would do.
#[derive(Debug, Serialize)]
pub struct ImportReport {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// What a dry run would do, an empty list included. A real run has
    /// none, and the key is left out of its JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<ImportAction>>,
    pub imported: Vec<Imported>,
    pub skipped: Vec<Skipped>,
    pub refused: Vec<Refused>,
    pub warnings: Vec<Warning>,
    /// The run that holds the lock while a dry run looks, if there is one.
    #[serde(skip)]
    pub lock_holder: Option<Holder>,
}

impl CommandReport for ImportReport {
    fn partial(&self) -> bool {
        !self.refused.is_empty()
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// The steps of importing one file after which a test can stop the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Walked,
    CopyMade,
    PayloadPlaced,
    ListingPlaced,
    MetadataWritten,
}

/// Lets a test end a run between two steps, as a crash would.
pub(crate) trait Hooks {
    fn after(&self, _step: Step) -> io::Result<()> {
        Ok(())
    }
}

struct NoHooks;

impl Hooks for NoHooks {}

/// Imports the files of `request`, or with `dry_run` says what it would
/// import.
pub fn import(
    request: &ImportRequest<'_>,
    dry_run: bool,
    clock: &dyn Clock,
    ids: &dyn IdSource,
) -> Result<ImportReport, ImportError> {
    import_with(request, dry_run, clock, ids, &NoHooks)
}

/// A file that passed every check that needs no lock.
struct Candidate {
    file: PathBuf,
    shown: String,
    /// Device and inode of the file that was inspected.
    identity: (u64, u64),
    size: u64,
    sha256: String,
    /// The piece's name: the last component of its path.
    name: Name,
    path: RelPath,
    entries: Vec<ListingEntry>,
    created: Timestamp,
    created_source: CreatedSource,
    warnings: Vec<Warning>,
}

/// The files that passed the checks that need no lock, and those that did
/// not.
pub(crate) struct Prepared {
    candidates: Vec<Candidate>,
    refused: Vec<Refused>,
}

/// Why a file does not become an object.
enum Failure {
    /// The file is refused; the run goes on with the others.
    Refused(String),
    /// The run cannot go on.
    Fatal(ImportError),
}

impl From<ImportError> for Failure {
    fn from(error: ImportError) -> Failure {
        Failure::Fatal(error)
    }
}

impl From<frames::FrameError> for Failure {
    fn from(error: frames::FrameError) -> Failure {
        Failure::Refused(error.to_string())
    }
}

impl From<PackError> for Failure {
    fn from(error: PackError) -> Failure {
        Failure::Refused(error.to_string())
    }
}

impl From<ListingError> for Failure {
    fn from(error: ListingError) -> Failure {
        Failure::Refused(error.to_string())
    }
}

fn refused(reason: impl Into<String>) -> Failure {
    Failure::Refused(reason.into())
}

pub(crate) fn import_with(
    request: &ImportRequest<'_>,
    dry_run: bool,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    hooks: &dyn Hooks,
) -> Result<ImportReport, ImportError> {
    // The checks come before the run guard: its repairs empty `tmp/`, and
    // a run in which every file is refused takes no lock at all.
    let prepared = prepare(request, clock.now(), hooks)?;
    if prepared.candidates.is_empty() {
        return Ok(ImportReport {
            dry_run,
            actions: dry_run.then(Vec::new),
            imported: Vec::new(),
            skipped: Vec::new(),
            refused: prepared.refused,
            warnings: Vec::new(),
            lock_holder: None,
        });
    }
    let started = run::start(request.collection, clock, dry_run)?;
    finish(request, prepared, &started, clock, ids, hooks)
}

// =========================================================
// Checks that need no lock
// =========================================================

/// Inspects every file. Nothing is written, so a refused file leaves no
/// trace, and a run that refuses everything never takes the lock.
fn prepare(
    request: &ImportRequest<'_>,
    now: Timestamp,
    hooks: &dyn Hooks,
) -> Result<Prepared, ImportError> {
    let config = &request.collection.config().collection;
    let rules = Rules::new(&config.patterns, &config.exclude)?;
    let mut prepared = Prepared {
        candidates: Vec::new(),
        refused: Vec::new(),
    };
    for file in request.files {
        let shown = escape(file.as_os_str().as_bytes());
        match prepare_file(request, &rules, file, &shown, now, hooks) {
            Ok(candidate) => prepared.candidates.push(candidate),
            Err(Failure::Refused(reason)) => prepared.refused.push(Refused {
                file: shown,
                reason,
            }),
            Err(Failure::Fatal(error)) => return Err(error),
        }
    }
    Ok(prepared)
}

/// What one pass over the file's bytes read.
struct Seen {
    hasher: Sha256,
    bytes: u64,
}

/// A pass over the open input that counts and hashes what it reads. The
/// passes share one handle and one set of bytes: each must see exactly the
/// bytes the frame walk saw, so that the listing, the structure and the
/// digest describe one file.
struct Tally<'a> {
    file: &'a File,
    seen: Rc<RefCell<Seen>>,
}

impl Read for Tally<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = (&*self.file).read(buf)?;
        let mut seen = self.seen.borrow_mut();
        seen.hasher.update(&buf[..count]);
        seen.bytes += count as u64;
        Ok(count)
    }
}

/// Starts a new pass at the beginning of the file.
fn pass<'a>(file: &'a File, log: &RefCell<Vec<Rc<RefCell<Seen>>>>) -> io::Result<Tally<'a>> {
    (&*file).seek(SeekFrom::Start(0))?;
    let seen = Rc::new(RefCell::new(Seen {
        hasher: Sha256::new(),
        bytes: 0,
    }));
    log.borrow_mut().push(Rc::clone(&seen));
    Ok(Tally { file, seen })
}

/// Opens the input for reading. A fifo or a device would block an open
/// that waits, so the open never waits and the kind is checked on the
/// handle. A symlink is followed: the input is the user's own file.
fn open_input(path: &Path) -> Result<File, Failure> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY;
    let file = rustix::fs::open(path, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| refused(format!("cannot open it: {}", io::Error::from(error))))?;
    let metadata = file
        .metadata()
        .map_err(|error| refused(format!("cannot look at it: {error}")))?;
    if !metadata.is_file() {
        return Err(refused("it is not a regular file"));
    }
    let blocking =
        fcntl_getfl(&file).and_then(|flags| fcntl_setfl(&file, flags - OFlags::NONBLOCK));
    blocking.map_err(|error| refused(format!("cannot read it: {}", io::Error::from(error))))?;
    Ok(file)
}

fn prepare_file(
    request: &ImportRequest<'_>,
    rules: &Rules,
    file: &Path,
    shown: &str,
    now: Timestamp,
    hooks: &dyn Hooks,
) -> Result<Candidate, Failure> {
    let collection = request.collection;
    let input = request.cwd.join(file);
    let handle = open_input(&input)?;

    // The start-of-run repairs empty `.antiq/tmp/`, `staging/` and
    // `trash/`, which would remove an input that lies in one of them.
    let inside = fs::canonicalize(&input)
        .ok()
        .zip(fs::canonicalize(collection.antiq_dir()).ok())
        .is_some_and(|(input, antiq)| input.starts_with(antiq));
    if inside {
        return Err(refused(
            "it lies inside .antiq/; copy it out of there first",
        ));
    }
    let metadata = handle
        .metadata()
        .map_err(|error| refused(format!("cannot look at it: {error}")))?;

    let walked = frames::walk(&handle)?;
    stopped(hooks, Step::Walked, "the frame walk")?;
    let log = RefCell::new(Vec::new());
    // The structure check refuses an oversized long-name or pax record by
    // its header; the tar reader behind `read_package_from` would load it.
    let members = check_structure(|| pass(&handle, &log))?;
    let package = read_package_from(
        pass(&handle, &log).map_err(|error| refused(format!("cannot read it: {error}")))?,
    )?;
    for seen in log.borrow().iter() {
        let seen = seen.borrow();
        if seen.bytes != walked.size || hex(&seen.hasher.clone().finalize()) != walked.sha256 {
            return Err(refused("it changed while it was checked"));
        }
    }
    // The listing is written under rules of its own; finding out here that
    // a member breaks one keeps the refusal ahead of every write.
    listing::write(&package.entries, io::sink())?;

    let name_bytes = package.name.as_bytes().to_vec();
    let path = match request.path {
        Some(given) => resolve_path(collection, request.cwd, given).map_err(ImportError::from)?,
        None => RelPath::from_bytes(name_bytes).expect("a name is a path of one component"),
    };
    check_path(collection, rules, &path).map_err(Failure::Refused)?;
    let name = Name::from_bytes(
        path.components()
            .last()
            .expect("a piece path has a component")
            .to_vec(),
    )
    .expect("a component of a valid path is a valid name");

    // The oldest time of any member, the top-level directory included.
    let times = package
        .entries
        .iter()
        .map(|entry| entry.mtime)
        .chain([package.container.mtime])
        .filter_map(|seconds| Timestamp::from_second(seconds).ok())
        .map(|modified| EntryTimes {
            birth: None,
            modified,
        });
    let idt = id_time(&path, None, times, whole_seconds(now));
    let mut warnings: Vec<Warning> = idt.warning.into_iter().collect();
    let created_source = match idt.source {
        CreatedSource::Minted => CreatedSource::Minted,
        _ => CreatedSource::Import,
    };

    // The piece's own name is checked with the members: it becomes a
    // folder name when the piece is unearthed.
    let mut names = members;
    names.push(MemberName::plain(
        RelPath::from_bytes(name.as_bytes().to_vec()).expect("a name is a path of one component"),
    ));
    warnings.extend(
        portability_warnings(&names)
            .into_iter()
            .map(|warning| Warning::new(format!("{shown}: {warning}"))),
    );

    Ok(Candidate {
        file: input,
        shown: shown.to_string(),
        identity: (metadata.dev(), metadata.ino()),
        size: walked.size,
        sha256: walked.sha256,
        name,
        path,
        entries: package.entries,
        created: idt.time,
        created_source,
        warnings,
    })
}

/// Why `path` cannot be an imported piece's path, if it cannot.
fn check_path(collection: &Collection, rules: &Rules, path: &RelPath) -> Result<(), String> {
    if path.is_root() {
        return Err("the collection root cannot be a piece".into());
    }
    if path.components().next() == Some(b".antiq".as_slice()) {
        return Err(format!(
            "`{path}` is inside .antiq/, which holds the collection's own files"
        ));
    }
    let config = &collection.config().collection;
    for excluded in &config.exclude {
        if path == excluded || lies_below(path, excluded) {
            return Err(format!(
                "`{path}` is excluded from the collection (`{excluded}`)"
            ));
        }
    }
    // Where matches nest, the shallowest one is the piece, so a path below
    // a selected path is part of that piece.
    for ancestor in ancestors(path) {
        if rules.matches(&ancestor) {
            return Err(format!("`{path}` lies inside the piece `{ancestor}`"));
        }
    }
    if !rules.matches(path) {
        return Err(format!(
            "the patterns of the collection do not select `{path}`; give another one with --path"
        ));
    }
    Ok(())
}

/// The proper ancestors of `path`, shortest first, the root left out.
fn ancestors(path: &RelPath) -> Vec<RelPath> {
    let components: Vec<&[u8]> = path.components().collect();
    (1..components.len())
        .map(|length| {
            RelPath::from_bytes(components[..length].join(&b'/'))
                .expect("a prefix of a valid path is a valid path")
        })
        .collect()
}

/// Whether `path` is a proper descendant of `parent`.
fn lies_below(path: &RelPath, parent: &RelPath) -> bool {
    path.as_bytes()
        .strip_prefix(parent.as_bytes())
        .is_some_and(|rest| rest.starts_with(b"/"))
}

// =========================================================
// Decisions under the lock
// =========================================================

/// A duplicate whose tracking entry an interrupted import did not write.
struct Completion {
    id: ObjectId,
    path: RelPath,
    version: u32,
    file: FileName,
    source: Source,
}

struct Settled {
    items: Vec<Candidate>,
    completions: Vec<Completion>,
    skipped: Vec<Skipped>,
    refused: Vec<Refused>,
    warnings: Vec<Warning>,
}

struct Known {
    id: ObjectId,
    metadata: Metadata,
    source: Source,
}

/// Decides, against the store as it is now, which files become objects.
/// A file whose bytes the store already holds is skipped, and a path that
/// would nest with another piece's is refused.
fn settle(collection: &Collection, candidates: Vec<Candidate>) -> Result<Settled, ImportError> {
    let store = Store::new(collection);
    let index = Index::compute(collection)?;
    let root = TreeRoot::open(collection.root()).map_err(|source| ImportError::Io {
        action: "open",
        path: collection.root().display().to_string(),
        source,
    })?;
    let mut settled = Settled {
        items: Vec::new(),
        completions: Vec::new(),
        skipped: Vec::new(),
        refused: Vec::new(),
        warnings: Vec::new(),
    };

    // A SHA-256 counts as imported only when metadata names it. A payload
    // an interrupted run left in an object directory is not metadata.
    let mut known: HashMap<String, Known> = HashMap::new();
    for id in store.objects()? {
        match store.current(id) {
            Ok(Some(current)) => {
                for payload in &current.metadata.payloads {
                    if payload.checksum.algorithm == "sha256" {
                        known
                            .entry(payload.checksum.value.clone())
                            .or_insert_with(|| Known {
                                id,
                                metadata: current.metadata.clone(),
                                source: current.source,
                            });
                    }
                }
            }
            Ok(None) => settled.warnings.push(Warning::new(format!(
                "objects/{id} has no metadata, probably from an interrupted run, so it is not compared"
            ))),
            Err(error) => settled.warnings.push(Warning::new(format!(
                "objects/{id} cannot be read ({error}), so it is not compared"
            ))),
        }
    }

    let mut seen_here: HashSet<String> = HashSet::new();
    let mut paths_here: Vec<RelPath> = Vec::new();
    for candidate in candidates {
        if let Some(existing) = known.get(&candidate.sha256) {
            let reason = match completion_of(collection, existing, &candidate.sha256)? {
                Some(completion) => {
                    settled.completions.push(completion);
                    SkipReason::EntryCompleted
                }
                None => SkipReason::AlreadyImported,
            };
            settled.skipped.push(Skipped {
                file: candidate.shown,
                id: Some(existing.id),
                reason,
            });
            continue;
        }
        if !seen_here.insert(candidate.sha256.clone()) {
            settled.skipped.push(Skipped {
                file: candidate.shown,
                id: None,
                reason: SkipReason::SameArchiveInThisRun,
            });
            continue;
        }

        let path = &candidate.path;
        let others = index
            .entries()
            .iter()
            .map(|entry| &entry.path)
            .chain(
                index
                    .tracked()
                    .iter()
                    .filter_map(|tracked| tracked.path.as_ref()),
            )
            .chain(paths_here.iter());
        let mut nested = None;
        for other in others {
            if lies_below(path, other) {
                nested = Some(format!("`{path}` lies inside the piece `{other}`"));
            } else if lies_below(other, path) {
                nested = Some(format!("`{path}` would contain the piece `{other}`"));
            }
        }
        if let Some(reason) = nested {
            settled.refused.push(Refused {
                file: candidate.shown,
                reason,
            });
            continue;
        }

        let at = index.at_path(path);
        let mut holders: Vec<String> = Vec::new();
        if root.lstat(path).is_ok() {
            holders.push("a folder".to_string());
        }
        if at.claimant.is_some() || !at.others.is_empty() {
            holders.push("another object".to_string());
        }
        if paths_here.contains(path) {
            holders.push("another file of this run".to_string());
        }
        if !holders.is_empty() {
            settled.warnings.push(Warning::new(format!(
                "{}: `{path}` is already taken by {}; both are kept and `find` lists them",
                candidate.shown,
                holders.join(" and ")
            )));
        }
        paths_here.push(candidate.path.clone());
        settled.items.push(candidate);
    }
    Ok(settled)
}

/// Whether the object that has these bytes lacks only the tracking entry
/// that this machine writes last: its own, with the archive file in the
/// object directory and no entry.
fn completion_of(
    collection: &Collection,
    existing: &Known,
    sha256: &str,
) -> Result<Option<Completion>, ImportError> {
    let metadata = &existing.metadata;
    if metadata.collection != collection.id() {
        return Ok(None);
    }
    let Some(payload) = metadata
        .payloads
        .iter()
        .find(|payload| payload.kind == PayloadKind::Archive && payload.checksum.value == sha256)
    else {
        return Ok(None);
    };
    if tracking::read(collection, existing.id)?.is_some() {
        return Ok(None);
    }
    let stored = Store::new(collection).file_path(existing.id, &payload.file);
    let intact = File::open(&stored)
        .and_then(|mut file| {
            let mut reader = HashingReader::new(&mut file);
            io::copy(&mut reader, &mut io::sink())?;
            Ok(reader.bytes_read() == payload.size && reader.sha256() == sha256)
        })
        .unwrap_or(false);
    if !intact {
        return Ok(None);
    }
    Ok(Some(Completion {
        id: existing.id,
        path: metadata.path.clone(),
        version: payload.version,
        file: payload.file.clone(),
        source: existing.source,
    }))
}

// =========================================================
// Writing
// =========================================================

fn finish(
    request: &ImportRequest<'_>,
    prepared: Prepared,
    started: &Started,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    hooks: &dyn Hooks,
) -> Result<ImportReport, ImportError> {
    let collection = request.collection;
    let mut report = ImportReport {
        dry_run: matches!(started, Started::DryRun { .. }),
        actions: None,
        imported: Vec::new(),
        skipped: Vec::new(),
        refused: prepared.refused,
        warnings: Vec::new(),
        lock_holder: match started {
            Started::DryRun { holder } => holder.clone(),
            Started::Guarded(_) => None,
        },
    };
    if let Started::Guarded(guard) = started {
        report.warnings.extend(guard.warnings().iter().cloned());
    }

    let mut settled = settle(collection, prepared.candidates)?;
    report.skipped = std::mem::take(&mut settled.skipped);
    report.refused.append(&mut settled.refused);
    report.warnings.append(&mut settled.warnings);
    for item in &settled.items {
        report.warnings.extend(item.warnings.iter().cloned());
    }

    let Started::Guarded(guard) = started else {
        report.actions = Some(
            settled
                .items
                .iter()
                .map(|item| ImportAction::Import {
                    file: item.shown.clone(),
                    path: item.path.clone(),
                })
                .collect(),
        );
        return Ok(report);
    };

    for completion in &settled.completions {
        complete(collection, guard, clock, completion)?;
    }
    for item in &settled.items {
        match import_one(collection, guard, clock, ids, hooks, item)? {
            Placed::Imported(imported) => report.imported.push(imported),
            Placed::Refused(reason) => report.refused.push(Refused {
                file: item.shown.clone(),
                reason,
            }),
        }
    }
    if !report.imported.is_empty() || !settled.completions.is_empty() {
        let updated = index::update(collection, guard.lock())?;
        report
            .warnings
            .extend(updated.skipped.iter().map(|(id, reason)| {
                Warning::new(format!("the member list of {id} was not stored: {reason}"))
            }));
    }
    Ok(report)
}

/// Writes the tracking entry of an object whose import was interrupted
/// after its metadata.
fn complete(
    collection: &Collection,
    guard: &RunGuard,
    clock: &dyn Clock,
    completion: &Completion,
) -> Result<(), ImportError> {
    let taken_out = match completion.source {
        Source::Pending => VersionTakenOut::Pending,
        Source::Committed(number) => VersionTakenOut::Committed(number),
    };
    let mut entry = TrackingEntry::new(
        completion.id,
        completion.path.clone(),
        EntryState::Gone,
        taken_out,
        whole_seconds(clock.now()),
    );
    entry.local_payloads.push(LocalPayload {
        kind: PayloadKind::Archive,
        version: completion.version,
        file: completion.file.clone(),
    });
    tracking::write(collection, guard.lock(), clock, &mut entry)?;
    Ok(())
}

enum Placed {
    Imported(Imported),
    Refused(String),
}

/// A file in `.antiq/tmp/` that is removed when it goes out of scope
/// unless it was renamed away.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> ImportError {
    let path = path.display().to_string();
    move |source| ImportError::Io {
        action,
        path,
        source,
    }
}

fn stopped(hooks: &dyn Hooks, step: Step, name: &'static str) -> Result<(), ImportError> {
    hooks
        .after(step)
        .map_err(|source| ImportError::Interrupted { step: name, source })
}

/// The bytes and SHA-256 of a file.
fn measure(path: &Path) -> io::Result<(u64, String)> {
    let mut reader = HashingReader::new(File::open(path)?);
    io::copy(&mut reader, &mut io::sink())?;
    Ok((reader.bytes_read(), reader.sha256()))
}

/// Gives one file its object: payload, listing, metadata, entry, in the
/// order that leaves a crash with at most files no metadata names.
fn import_one(
    collection: &Collection,
    guard: &RunGuard,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    hooks: &dyn Hooks,
    item: &Candidate,
) -> Result<Placed, ImportError> {
    let lock = guard.lock();
    let store = Store::new(collection);
    let now = whole_seconds(clock.now());
    let machine = collection.machine_name()?;
    let id = ids.mint(item.created);

    // The copy reopens the input by path. A different file there (or a
    // fifo, which a copy would wait on) is found by its identity before
    // the copy opens it, and a rewritten one by its digest after.
    let identity = fs::metadata(&item.file).map(|found| (found.dev(), found.ino()));
    if identity.ok() != Some(item.identity) {
        return Ok(Placed::Refused("it changed after it was checked".into()));
    }
    let copy = TempFile(collection.tmp_dir().join(format!("import-{id}.part")));
    fs::copy(&item.file, &copy.0).map_err(io_error("copy", &item.file))?;
    let (size, sha256) = measure(&copy.0).map_err(io_error("read", &copy.0))?;
    if size != item.size || sha256 != item.sha256 {
        return Ok(Placed::Refused("it changed after it was checked".into()));
    }
    stopped(hooks, Step::CopyMade, "the copy")?;

    let payload_name = FileName::new(format!(
        "{}-{}.tar.zst",
        &sha256[..16],
        relpath::name_part(&item.name)
    ))?;
    match store.put_file(lock, id, &payload_name, &copy.0, &sha256) {
        Ok(_) => {}
        Err(StoreError::PayloadConflict { name }) => {
            return Ok(Placed::Refused(format!(
                "`{name}` exists in the store with other content"
            )));
        }
        Err(error) => return Err(error.into()),
    }
    stopped(hooks, Step::PayloadPlaced, "the payload was placed")?;

    let listing_name = listing::file_name(&sha256)?;
    let listing_temp = TempFile(
        collection
            .tmp_dir()
            .join(format!("import-{id}.listing.part")),
    );
    let written = {
        let file = File::create(&listing_temp.0).map_err(io_error("create", &listing_temp.0))?;
        let mut out = BufWriter::new(file);
        let written = listing::write(&item.entries, &mut out)?;
        out.flush().map_err(io_error("write", &listing_temp.0))?;
        written
    };
    match store.put_file(lock, id, &listing_name, &listing_temp.0, &written.sha256) {
        Ok(_) => {}
        Err(StoreError::PayloadConflict { name }) => {
            return Ok(Placed::Refused(format!(
                "`{name}` exists in the store with other content"
            )));
        }
        Err(error) => return Err(error.into()),
    }
    stopped(hooks, Step::ListingPlaced, "the listing was placed")?;

    let metadata = Metadata {
        format: FORMAT,
        id,
        collection: collection.id(),
        path: item.path.clone(),
        name: item.name.clone(),
        created: item.created,
        created_source: item.created_source,
        lifecycle: Lifecycle::Archived,
        payloads: vec![Payload {
            kind: PayloadKind::Archive,
            version: 1,
            file: payload_name.clone(),
            package_format: PackageFormat::TAR_ZSTD_1.as_str().to_string(),
            size,
            checksum: Checksum::sha256(sha256.clone()),
            listing: Some(listing_name),
            machine: machine.clone(),
            created: now,
            verified: Some(None),
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        }],
        history: vec![HistoryEntry::new(now, machine, HistoryAction::Import)],
        extra: serde_json::Map::new(),
    };
    store.write_pending(lock, &metadata)?;
    stopped(hooks, Step::MetadataWritten, "the metadata was written")?;

    let mut entry = TrackingEntry::new(
        id,
        item.path.clone(),
        EntryState::Gone,
        VersionTakenOut::Pending,
        now,
    );
    entry.local_payloads.push(LocalPayload {
        kind: PayloadKind::Archive,
        version: 1,
        file: payload_name,
    });
    tracking::write(collection, lock, clock, &mut entry)?;

    Ok(Placed::Imported(Imported {
        id,
        file: item.shown.clone(),
        path: item.path.clone(),
        name: item.name.clone(),
        created: whole_seconds(item.created),
        size,
        sha256,
        payload_version: 1,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::id::SeqIds;
    use crate::meta::{HistoryAction, Lifecycle, PayloadKind};
    use crate::pack::zstd::{ZstdSettings, encoder};
    use crate::run;
    use crate::store::{Source, Store};
    use crate::testing::package::{RawMember, compress, pack_piece, raw_tar};
    use crate::testing::tree::{COLLECTION_ID, TempCollection, Tree};
    use crate::tracking::{self, EntryState, VersionTakenOut};
    use sha2::Digest;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    /// A collection, a directory with the files to import and the clock
    /// and IDs of a test.
    struct World {
        temp: TempCollection,
        collection: Collection,
        inputs: tempfile::TempDir,
        clock: FixedClock,
        ids: SeqIds,
    }

    impl World {
        fn new() -> World {
            let temp = TempCollection::minimal();
            let collection = Collection::find(temp.root()).expect("a collection");
            World {
                temp,
                collection,
                inputs: tempfile::tempdir().expect("a temp dir"),
                clock: FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time")),
                ids: SeqIds::new(1),
            }
        }

        fn configure(&mut self, lines: &str) {
            fs::write(
                self.temp.root().join(".antiq/config.toml"),
                format!("[collection]\nid = \"{COLLECTION_ID}\"\n{lines}\n"),
            )
            .expect("a config can be written");
            self.collection = Collection::find(self.temp.root()).expect("a collection");
        }

        /// A file in the inputs directory.
        fn input(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.inputs.path().join(name);
            fs::write(&path, bytes).expect("the input is written");
            path
        }

        /// A package made by antiq of the piece `proj` that `build` fills.
        fn package(&self, name: &str, build: impl FnOnce(&mut Tree)) -> PathBuf {
            let mut tree = Tree::new();
            build(&mut tree);
            self.input(name, &pack_piece(&tree, "proj").bytes)
        }

        /// A package with a file from 2001 inside `proj`.
        fn simple(&self, name: &str) -> PathBuf {
            self.package(name, |tree| {
                tree.file_with("proj/a.txt", "hello", 0o644, 1_000_000_000);
            })
        }

        fn request<'a>(
            &'a self,
            files: &'a [PathBuf],
            path: Option<&'a Path>,
        ) -> ImportRequest<'a> {
            ImportRequest {
                collection: &self.collection,
                cwd: self.collection.root(),
                files,
                path,
            }
        }

        fn run_with(
            &self,
            files: &[PathBuf],
            path: Option<&Path>,
            hooks: &dyn Hooks,
        ) -> Result<ImportReport, ImportError> {
            import_with(
                &self.request(files, path),
                false,
                &self.clock,
                &self.ids,
                hooks,
            )
        }

        fn run(&self, files: &[PathBuf]) -> ImportReport {
            self.run_with(files, None, &NoHooks)
                .expect("the import runs")
        }

        fn dry(&self, files: &[PathBuf]) -> ImportReport {
            import(&self.request(files, None), true, &self.clock, &self.ids)
                .expect("the dry run works")
        }

        fn store(&self) -> Store<'_> {
            Store::new(&self.collection)
        }

        fn names_in(&self, dir: &str) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.temp.root().join(dir))
                .expect("lists")
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }

        /// Nothing below `.antiq/` was written for an import.
        fn assert_nothing_written(&self) {
            assert_eq!(self.names_in(".antiq/objects"), Vec::<String>::new());
            assert_eq!(self.names_in(".antiq/tracking"), Vec::<String>::new());
            assert_eq!(self.names_in(".antiq/tmp"), Vec::<String>::new());
        }
    }

    fn refusal_of(report: &ImportReport) -> &str {
        assert_eq!(report.refused.len(), 1, "{report:?}");
        &report.refused[0].reason
    }

    // =========================================================
    // A good archive
    // =========================================================

    #[test]
    fn an_archive_becomes_an_archived_piece_with_its_payload_listing_metadata_and_entry() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let report = world.run(std::slice::from_ref(&file));

        assert_eq!(report.imported.len(), 1, "{report:?}");
        let imported = &report.imported[0];
        assert_eq!(imported.path.to_string(), "proj");
        assert_eq!(imported.name.as_bytes(), b"proj");
        assert_eq!(imported.created.to_string(), "2001-09-09T01:46:40Z");
        assert_eq!(imported.payload_version, 1);
        assert!(report.skipped.is_empty() && report.refused.is_empty());

        let bytes = fs::read(&file).expect("the input reads");
        let object_dir = world.collection.object_dir(imported.id);
        let payload = fs::read(object_dir.join(format!("{}-proj.tar.zst", &imported.sha256[..16])))
            .expect("the payload is stored under its name");
        assert_eq!(payload, bytes);
        assert_eq!(imported.size, bytes.len() as u64);

        let current = world
            .store()
            .current(imported.id)
            .expect("reads")
            .expect("metadata exists");
        assert_eq!(current.source, Source::Pending);
        let metadata = current.metadata;
        assert_eq!(metadata.collection.to_string(), COLLECTION_ID);
        assert_eq!(metadata.path.to_string(), "proj");
        assert_eq!(metadata.created_source, CreatedSource::Import);
        assert_eq!(metadata.lifecycle, Lifecycle::Archived);
        assert_eq!(metadata.payloads.len(), 1);
        let payload = &metadata.payloads[0];
        assert_eq!(payload.kind, PayloadKind::Archive);
        assert_eq!(payload.version, 1);
        assert_eq!(payload.package_format, "tar+zstd/1");
        assert_eq!(payload.checksum.value, imported.sha256);
        assert_eq!(payload.verified, Some(None));
        assert_eq!(payload.created.to_string(), "2026-11-04T10:00:00Z");
        let listing_name = payload.listing.clone().expect("a listing is named");
        assert_eq!(
            listing_name.as_str(),
            format!("{}.listing.zst", &imported.sha256[..16])
        );
        let listing = crate::listing::read_all(
            fs::File::open(object_dir.join(listing_name.as_str())).expect("the listing exists"),
        )
        .expect("the listing reads");
        assert_eq!(
            listing
                .iter()
                .map(|e| e.path.to_string())
                .collect::<Vec<_>>(),
            ["a.txt"]
        );
        assert_eq!(metadata.history.len(), 1);
        assert_eq!(metadata.history[0].action, HistoryAction::Import.as_str());
        assert_eq!(metadata.history[0].time.to_string(), "2026-11-04T10:00:00Z");

        let entry = tracking::read(&world.collection, imported.id)
            .expect("reads")
            .expect("an entry exists");
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
        assert_eq!(entry.local_payloads.len(), 1);
        assert_eq!(entry.local_payloads[0].file, payload.file);

        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
        assert_eq!(
            world.names_in(".antiq/index"),
            [format!("{}.json", imported.id)],
            "the member list is stored"
        );
    }

    #[test]
    fn the_input_is_untouched_and_the_payload_is_another_file() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let before = fs::metadata(&file).expect("stat");
        let bytes = fs::read(&file).expect("reads");
        let report = world.run(std::slice::from_ref(&file));
        let after = fs::metadata(&file).expect("stat");
        assert_eq!(fs::read(&file).expect("reads"), bytes);
        assert_eq!(
            (
                before.ino(),
                before.mtime(),
                before.mtime_nsec(),
                after.nlink()
            ),
            (after.ino(), after.mtime(), after.mtime_nsec(), 1)
        );
        let id = report.imported[0].id;
        let stored = world
            .collection
            .object_dir(id)
            .join(format!("{}-proj.tar.zst", &report.imported[0].sha256[..16]));
        assert_ne!(fs::metadata(stored).expect("stat").ino(), before.ino());
    }

    #[test]
    fn the_id_carries_the_oldest_member_time_the_top_directory_included() {
        let world = World::new();
        let file = world.package("a.tar.zst", |tree| {
            tree.file_with("proj/a.txt", "x", 0o644, 1_500_000_000);
            let dir = tree.dir("proj");
            filetime::set_file_mtime(&dir, filetime::FileTime::from_unix_time(1_200_000_000, 0))
                .expect("a time");
        });
        let report = world.run(&[file]);
        let imported = &report.imported[0];
        assert_eq!(imported.created.to_string(), "2008-01-10T21:20:00Z");
        assert_eq!(
            imported.id.timestamp_ms(),
            1_200_000_000_000,
            "the ID is minted for that time"
        );
    }

    #[test]
    fn an_archive_without_a_time_from_1990_on_gets_the_minting_time_and_the_source_minted() {
        let world = World::new();
        let file = world.package("old.tar.zst", |tree| {
            tree.file_with("proj/a.txt", "x", 0o644, 100_000_000);
            let dir = tree.dir("proj");
            filetime::set_file_mtime(&dir, filetime::FileTime::from_unix_time(100_000_000, 0))
                .expect("a time");
        });
        let report = world.run(&[file]);
        let imported = &report.imported[0];
        assert_eq!(imported.created.to_string(), "2026-11-04T10:00:00Z");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("no time from 1990 on")),
            "{report:?}"
        );
        let metadata = world
            .store()
            .current(imported.id)
            .expect("reads")
            .expect("exists")
            .metadata;
        assert_eq!(metadata.created_source, CreatedSource::Minted);
    }

    // =========================================================
    // Files that are refused
    // =========================================================

    fn refused_with(world: &World, file: PathBuf, expected: &str) {
        let report = world.run(&[file]);
        let reason = refusal_of(&report);
        assert!(reason.contains(expected), "{reason}");
        assert!(report.imported.is_empty());
        assert!(report.partial());
        world.assert_nothing_written();
    }

    fn valid_tar() -> Vec<u8> {
        raw_tar(&[RawMember::dir(b"proj/"), RawMember::file(b"proj/a", b"1")])
    }

    #[test]
    fn a_gzip_file_is_refused_naming_gzip() {
        let world = World::new();
        let file = world.input("a.tar.gz", &[0x1F, 0x8B, 8, 0, 0, 0, 0, 0]);
        refused_with(&world, file, "gzip");
    }

    #[test]
    fn an_xz_file_is_refused_naming_xz() {
        let world = World::new();
        let file = world.input("a.tar.xz", &[0xFD, b'7', b'z', b'X', b'Z', 0, 0, 4]);
        refused_with(&world, file, "xz");
    }

    #[test]
    fn a_window_of_2_pow_28_is_refused_naming_the_repair() {
        let world = World::new();
        let mut encoder = ::zstd::stream::write::Encoder::new(Vec::new(), 1).expect("an encoder");
        encoder.include_checksum(true).expect("on");
        encoder.window_log(28).expect("a window log");
        encoder.write_all(&valid_tar()).expect("writes");
        let file = world.input("a.tar.zst", &encoder.finish().expect("ends"));
        refused_with(&world, file, "zstd -19 --long=27");
    }

    #[test]
    fn a_frame_without_the_content_checksum_is_refused() {
        let world = World::new();
        let mut encoder = ::zstd::stream::write::Encoder::new(Vec::new(), 1).expect("an encoder");
        encoder.include_checksum(false).expect("off");
        encoder.write_all(&valid_tar()).expect("writes");
        let file = world.input("a.tar.zst", &encoder.finish().expect("ends"));
        refused_with(&world, file, "no content checksum");
    }

    #[test]
    fn a_long_name_record_above_the_cap_is_refused_as_such_before_it_is_read() {
        let world = World::new();
        let mut tar = raw_tar(&[RawMember::dir(b"proj/")]);
        tar.truncate(512);
        let mut header = tar::Header::new_gnu();
        header.as_old_mut().name[..13].copy_from_slice(b"././@LongLink");
        header.set_entry_type(tar::EntryType::GNULongName);
        header.set_size(1 << 30);
        header.set_cksum();
        tar.extend_from_slice(header.as_bytes());
        let file = world.input("a.tar.zst", &compress(&tar));
        refused_with(&world, file, "is larger than");
    }

    #[test]
    fn two_top_level_folders_are_refused() {
        let world = World::new();
        let tar = raw_tar(&[
            RawMember::dir(b"proj/"),
            RawMember::file(b"proj/a", b"1"),
            RawMember::dir(b"other/"),
            RawMember::file(b"other/b", b"2"),
        ]);
        let file = world.input("a.tar.zst", &compress(&tar));
        refused_with(
            &world,
            file,
            "lies outside the archive's top-level directory",
        );
    }

    #[test]
    fn a_member_with_a_parent_component_is_refused() {
        let world = World::new();
        let tar = raw_tar(&[
            RawMember::dir(b"proj/"),
            RawMember::file(b"proj/../x", b"1"),
        ]);
        let file = world.input("a.tar.zst", &compress(&tar));
        refused_with(&world, file, "../x");
    }

    #[test]
    fn an_absolute_member_is_refused() {
        let world = World::new();
        let tar = raw_tar(&[RawMember::dir(b"proj/"), RawMember::file(b"/etc/x", b"1")]);
        let file = world.input("a.tar.zst", &compress(&tar));
        refused_with(&world, file, "its path is absolute");
    }

    #[test]
    fn a_member_whose_parent_directory_is_missing_is_refused_as_unearth_would() {
        let world = World::new();
        let tar = raw_tar(&[RawMember::dir(b"proj/"), RawMember::file(b"proj/d/x", b"1")]);
        let file = world.input("a.tar.zst", &compress(&tar));
        refused_with(&world, file, "directory it belongs in was not extracted");
    }

    #[test]
    fn a_tar_that_does_not_start_with_a_directory_is_refused() {
        let world = World::new();
        let tar = raw_tar(&[RawMember::file(b"proj", b"1")]);
        let file = world.input("a.tar.zst", &compress(&tar));
        refused_with(&world, file, "first member is not the directory");
    }

    #[test]
    fn a_listing_rule_breaking_member_is_refused_before_anything_is_written() {
        let world = World::new();
        let mut directory = RawMember::dir(b"proj/d/");
        directory.data = &[7; 512];
        let tar = raw_tar(&[RawMember::dir(b"proj/"), directory]);
        let file = world.input("a.tar.zst", &compress(&tar));
        refused_with(&world, file, "only a file has a size");
    }

    #[test]
    fn a_stream_cut_at_a_frame_boundary_is_refused() {
        let world = World::new();
        let mut tar = valid_tar();
        tar.truncate(tar.len() - 1024);
        let (first, second) = tar.split_at(512);
        let file = world.input("a.tar.zst", &[compress(first), compress(second)].concat());
        refused_with(&world, file, "end-of-archive");
    }

    #[test]
    fn trailing_bytes_after_the_last_frame_are_refused() {
        let world = World::new();
        let mut bytes = compress(&valid_tar());
        bytes.extend_from_slice(b"junk");
        let file = world.input("a.tar.zst", &bytes);
        refused_with(&world, file, "not a frame");
    }

    #[test]
    fn a_file_that_is_not_there_is_refused() {
        let world = World::new();
        let file = world.inputs.path().join("missing.tar.zst");
        refused_with(&world, file, "cannot open");
    }

    #[test]
    fn a_directory_is_refused() {
        let world = World::new();
        let file = world.inputs.path().to_path_buf();
        refused_with(&world, file, "not a regular file");
    }

    #[test]
    fn an_empty_file_is_refused() {
        let world = World::new();
        let file = world.input("empty.tar.zst", b"");
        refused_with(&world, file, "empty");
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let world = World::new();
        let mut tree = Tree::new();
        let fifo = tree.fifo("queue");
        refused_with(&world, fifo, "not a regular file");
    }

    #[test]
    fn a_file_inside_the_antiq_directory_is_refused() {
        let mut world = World::new();
        world
            .temp
            .tree()
            .file(".antiq/tmp/keep.tar.zst", compress(&valid_tar()));
        let file = world.collection.root().join(".antiq/tmp/keep.tar.zst");
        let report = world.run(std::slice::from_ref(&file));
        assert!(refusal_of(&report).contains("inside .antiq"), "{report:?}");
        assert!(file.exists(), "the file is still there");
    }

    #[test]
    fn a_good_file_is_imported_next_to_a_refused_one() {
        let world = World::new();
        let bad = world.input("bad.tar.gz", &[0x1F, 0x8B, 8]);
        let good = world.simple("good.tar.zst");
        let report = world.run(&[bad, good]);
        assert_eq!(report.imported.len(), 1);
        assert_eq!(report.refused.len(), 1);
        assert!(report.partial());
    }

    // =========================================================
    // Paths
    // =========================================================

    #[test]
    fn path_gives_the_piece_its_path_and_the_last_component_its_name() {
        let mut world = World::new();
        world.configure("patterns = [\"*/*\"]");
        let file = world.simple("proj.tar.zst");
        let report = world
            .run_with(&[file], Some(Path::new("clients/acme")), &NoHooks)
            .expect("runs");
        let imported = &report.imported[0];
        assert_eq!(imported.path.to_string(), "clients/acme");
        assert_eq!(imported.name.as_bytes(), b"acme");
    }

    fn refused_path(world: &World, path: &str, expected: &str) {
        let file = world.package("candidate.tar.zst", |tree| {
            tree.file_with("proj/candidate.txt", path, 0o644, 1_000_000_002);
        });
        let before = world.names_in(".antiq/objects").len();
        let report = world
            .run_with(&[file], Some(Path::new(path)), &NoHooks)
            .expect("runs");
        let reason = refusal_of(&report);
        assert!(reason.contains(expected), "{reason}");
        assert_eq!(world.names_in(".antiq/objects").len(), before);
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
    }

    #[test]
    fn the_collection_root_is_refused_as_a_path() {
        refused_path(&World::new(), ".", "collection root");
    }

    #[test]
    fn a_path_below_the_antiq_directory_is_refused() {
        // Patterns that select it, so that only its place refuses it.
        let mut world = World::new();
        world.configure("patterns = [\".antiq/*\"]");
        refused_path(&world, ".antiq/x", "is inside .antiq/");
    }

    #[test]
    fn a_path_outside_the_collection_is_an_error_of_the_run() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let outside = world.inputs.path().join("x");
        let error = world
            .run_with(&[file], Some(&outside), &NoHooks)
            .expect_err("outside");
        assert_eq!(error.kind(), ErrorKind::OutsideCollection);
    }

    #[test]
    fn a_path_the_patterns_do_not_select_is_refused() {
        let mut world = World::new();
        world.configure("patterns = [\"projects/*\"]");
        refused_path(&world, "other/acme", "do not select");
    }

    #[test]
    fn the_default_path_is_checked_against_the_patterns_too() {
        let mut world = World::new();
        world.configure("patterns = [\"projects/*\"]");
        let file = world.simple("proj.tar.zst");
        let report = world.run(&[file]);
        assert!(refusal_of(&report).contains("--path"), "{report:?}");
    }

    #[test]
    fn a_path_inside_an_excluded_path_is_refused() {
        let mut world = World::new();
        world.configure("patterns = [\"**\"]\nexclude = [\"archive\"]");
        refused_path(&world, "archive/acme", "excluded");
    }

    #[test]
    fn a_path_inside_a_selected_piece_is_refused() {
        let mut world = World::new();
        world.configure("patterns = [\"*\", \"clients/*\"]");
        refused_path(&world, "clients/acme", "lies inside the piece `clients`");
    }

    #[test]
    fn a_path_below_an_existing_object_is_refused() {
        let mut world = World::new();
        world.configure("patterns = [\"*/*\"]");
        let first = world.simple("first.tar.zst");
        let report = world
            .run_with(&[first], Some(Path::new("a/b")), &NoHooks)
            .expect("runs");
        assert_eq!(report.imported.len(), 1, "{report:?}");
        world.configure("patterns = [\"*/*/*\"]");
        refused_path(&world, "a/b/c", "lies inside the piece `a/b`");
    }

    #[test]
    fn a_path_above_an_existing_object_is_refused() {
        let mut world = World::new();
        world.configure("patterns = [\"*/*\"]");
        let first = world.simple("first.tar.zst");
        world
            .run_with(&[first], Some(Path::new("a/b")), &NoHooks)
            .expect("runs");
        world.configure("patterns = [\"*\"]");
        refused_path(&world, "a", "would contain the piece `a/b`");
    }

    #[test]
    fn a_taken_path_warns_and_both_objects_exist() {
        let mut world = World::new();
        world.temp.tree().dir("proj");
        let file = world.simple("proj.tar.zst");
        let first = world.run(std::slice::from_ref(&file));
        assert!(
            first
                .warnings
                .iter()
                .any(|w| w.message.contains("already taken")),
            "{first:?}"
        );
        let other = world.package("again.tar.zst", |tree| {
            tree.file_with("proj/b.txt", "other", 0o644, 1_000_000_000);
        });
        let second = world.run(&[other]);
        assert!(
            second
                .warnings
                .iter()
                .any(|w| w.message.contains("already taken")),
            "{second:?}"
        );
        assert_eq!(world.store().objects().expect("lists").len(), 2);
        let index = crate::index::Index::compute(&world.collection).expect("index");
        let at = index.at_path(&RelPath::parse("proj").expect("valid"));
        assert_eq!(at.claimant.into_iter().chain(at.others).count(), 2);
    }

    // =========================================================
    // Files that are skipped
    // =========================================================

    #[test]
    fn the_same_file_a_second_time_is_skipped_with_the_id_it_has() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let first = world.run(std::slice::from_ref(&file));
        let second = world.run(&[file]);
        assert!(second.imported.is_empty());
        assert_eq!(second.skipped.len(), 1);
        assert_eq!(second.skipped[0].id, Some(first.imported[0].id));
        assert_eq!(second.skipped[0].reason, SkipReason::AlreadyImported);
        assert!(!second.partial());
        assert_eq!(world.store().objects().expect("lists").len(), 1);
    }

    #[test]
    fn the_same_bytes_under_another_name_are_skipped() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let copy = world.input("copy.tar.zst", &fs::read(&file).expect("reads"));
        world.run(&[file]);
        let report = world.run(&[copy]);
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn the_same_file_twice_in_one_call_is_imported_once() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let report = world.run(&[file.clone(), file]);
        assert_eq!(report.imported.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::SameArchiveInThisRun);
        assert_eq!(report.skipped[0].id, None);
    }

    #[test]
    fn an_object_that_cannot_be_read_gives_a_warning_and_the_run_goes_on() {
        let mut world = World::new();
        world.temp.tree().file(
            ".antiq/objects/01BX5ZZKBKACTAV9WEVGEMMVRZ/meta.1",
            "{ not json",
        );
        world
            .temp
            .tree()
            .dir(".antiq/objects/01D39ZY06FGSCTVN4T2V9PKHFZ");
        let file = world.simple("proj.tar.zst");
        let report = world.run(&[file]);
        assert_eq!(report.imported.len(), 1);
        let messages: Vec<&str> = report.warnings.iter().map(|w| w.message.as_str()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("01BX5ZZKBKACTAV9WEVGEMMVRZ")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("01D39ZY06FGSCTVN4T2V9PKHFZ") && m.contains("no metadata")),
            "{messages:?}"
        );
    }

    // =========================================================
    // Interrupted runs
    // =========================================================

    /// Stops the run after one step.
    struct StopAfter(Step);

    impl Hooks for StopAfter {
        fn after(&self, step: Step) -> io::Result<()> {
            if step == self.0 {
                Err(io::Error::other("crash"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_crash_after_the_payload_is_placed_leaves_no_object_a_rerun_imports_again() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let error = world
            .run_with(
                std::slice::from_ref(&file),
                None,
                &StopAfter(Step::PayloadPlaced),
            )
            .expect_err("the run stops");
        assert!(
            matches!(error, ImportError::Interrupted { .. }),
            "{error:?}"
        );
        // The payload is in the store, but no metadata names it.
        assert!(
            world
                .store()
                .current(world.store().objects().expect("lists")[0])
                .expect("reads")
                .is_none()
        );
        let report = world
            .run_with(&[file], None, &NoHooks)
            .expect("the rerun runs");
        assert_eq!(report.imported.len(), 1, "{report:?}");
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn a_payload_with_the_name_the_rerun_wants_is_reused_when_the_bytes_agree() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let bytes = fs::read(&file).expect("reads");
        let sha = crate::pack::hashing::hex(&sha2::Sha256::digest(&bytes));
        // SeqIds(1) mints the first ID of the run.
        let id = SeqIds::new(1).mint("2001-09-09T01:46:40Z".parse().expect("a time"));
        let dir = world.collection.object_dir(id);
        fs::create_dir_all(&dir).expect("the directory is made");
        fs::write(dir.join(format!("{}-proj.tar.zst", &sha[..16])), &bytes).expect("a leftover");
        let report = world.run(&[file]);
        assert_eq!(report.imported[0].id, id);
        assert!(report.refused.is_empty());
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
    }

    #[test]
    fn a_payload_with_the_name_the_rerun_wants_and_other_bytes_refuses_the_file() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let bytes = fs::read(&file).expect("reads");
        let sha = crate::pack::hashing::hex(&sha2::Sha256::digest(&bytes));
        let id = SeqIds::new(1).mint("2001-09-09T01:46:40Z".parse().expect("a time"));
        let dir = world.collection.object_dir(id);
        fs::create_dir_all(&dir).expect("the directory is made");
        fs::write(dir.join(format!("{}-proj.tar.zst", &sha[..16])), b"other").expect("a leftover");
        let report = world.run(&[file]);
        assert!(report.imported.is_empty());
        assert!(refusal_of(&report).contains("other content"), "{report:?}");
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
    }

    #[test]
    fn a_crash_between_the_metadata_and_the_entry_is_completed_when_the_next_run_starts() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        world
            .run_with(
                std::slice::from_ref(&file),
                None,
                &StopAfter(Step::MetadataWritten),
            )
            .expect_err("the run stops");
        let id = world.store().objects().expect("lists")[0];
        assert!(
            tracking::read(&world.collection, id)
                .expect("reads")
                .is_none()
        );

        // The repairs at the start of the rerun write the entry; the file
        // is then a plain duplicate.
        let report = world.run(&[file]);
        assert!(report.imported.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::AlreadyImported);
        assert_eq!(report.skipped[0].id, Some(id));
        assert!(!report.partial());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("stopped before its tracking entry")),
            "{report:?}"
        );
        let entry = tracking::read(&world.collection, id)
            .expect("reads")
            .expect("the entry exists now");
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.local_payloads.len(), 1);
        assert_eq!(world.store().objects().expect("lists").len(), 1);
    }

    #[test]
    fn a_duplicate_whose_payload_file_is_missing_is_skipped_without_an_entry() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let first = world.run(std::slice::from_ref(&file));
        let id = first.imported[0].id;
        tracking::remove(
            &world.collection,
            run::start(&world.collection, &world.clock, false)
                .map(|started| match started {
                    Started::Guarded(guard) => guard,
                    Started::DryRun { .. } => unreachable!(),
                })
                .expect("a guard")
                .lock(),
            id,
        )
        .expect("the entry is removed");
        let object_dir = world.collection.object_dir(id);
        let payload = format!("{}-proj.tar.zst", &first.imported[0].sha256[..16]);
        fs::remove_file(object_dir.join(payload)).expect("the payload is removed");
        let report = world.run(&[file]);
        assert_eq!(report.skipped[0].reason, SkipReason::AlreadyImported);
        assert!(
            tracking::read(&world.collection, id)
                .expect("reads")
                .is_none()
        );
    }

    /// The first object of the store, changed by `edit`, with its tracking
    /// entry removed.
    fn rewrite_first_object(world: &World, edit: impl FnOnce(&mut Metadata)) -> ObjectId {
        let id = world.store().objects().expect("lists")[0];
        let mut metadata = world
            .store()
            .current(id)
            .expect("reads")
            .expect("exists")
            .metadata;
        edit(&mut metadata);
        let Started::Guarded(guard) =
            run::start(&world.collection, &world.clock, false).expect("a guard")
        else {
            unreachable!("not a dry run")
        };
        world
            .store()
            .write_pending(guard.lock(), &metadata)
            .expect("written");
        tracking::remove(&world.collection, guard.lock(), id).expect("removed");
        id
    }

    #[test]
    fn an_object_of_another_collection_is_skipped_without_an_entry() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        world.run(std::slice::from_ref(&file));
        let id = rewrite_first_object(&world, |metadata| {
            metadata.collection = "01BX5ZZKBKACTAV9WEVGEMMVRZ".parse().expect("an ID");
        });
        let report = world.run(&[file]);
        assert_eq!(report.skipped[0].reason, SkipReason::AlreadyImported);
        assert!(
            tracking::read(&world.collection, id)
                .expect("reads")
                .is_none()
        );
    }

    #[test]
    fn an_object_whose_payload_with_these_bytes_is_a_live_package_is_skipped_without_an_entry() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        world.run(std::slice::from_ref(&file));
        let id = rewrite_first_object(&world, |metadata| {
            metadata.payloads[0].kind = PayloadKind::Live;
        });
        let report = world.run(&[file]);
        assert_eq!(report.skipped[0].reason, SkipReason::AlreadyImported);
        assert!(
            tracking::read(&world.collection, id)
                .expect("reads")
                .is_none()
        );
    }

    #[test]
    fn a_payload_file_with_other_bytes_is_not_taken_for_the_import() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let first = world.run(std::slice::from_ref(&file));
        let id = rewrite_first_object(&world, |_| {});
        let payload = world
            .collection
            .object_dir(id)
            .join(format!("{}-proj.tar.zst", &first.imported[0].sha256[..16]));
        let mut bytes = fs::read(&payload).expect("reads");
        bytes[0] ^= 0xFF;
        fs::write(&payload, bytes).expect("rewritten");
        let report = world.run(&[file]);
        assert_eq!(report.skipped[0].reason, SkipReason::AlreadyImported);
        assert!(
            tracking::read(&world.collection, id)
                .expect("reads")
                .is_none()
        );
    }

    #[test]
    fn the_checksum_of_another_algorithm_is_not_compared() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        world.run(std::slice::from_ref(&file));
        rewrite_first_object(&world, |metadata| {
            metadata.payloads[0].checksum.algorithm = "blake3".into();
        });
        let report = world.run(&[file]);
        assert_eq!(report.imported.len(), 1, "{report:?}");
    }

    #[test]
    fn an_entry_missing_for_a_committed_version_is_written_with_that_version() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let first = world.run(std::slice::from_ref(&file));
        let id = first.imported[0].id;
        let object_dir = world.collection.object_dir(id);
        fs::rename(object_dir.join("meta.pending"), object_dir.join("meta.1")).expect("committed");
        rewrite_first_object(&world, |_| {});
        fs::rename(object_dir.join("meta.pending"), object_dir.join("meta.2")).expect("committed");
        let report = world.run(&[file]);
        assert_eq!(report.skipped[0].reason, SkipReason::EntryCompleted);
        let entry = tracking::read(&world.collection, id)
            .expect("reads")
            .expect("written");
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(2));
    }

    #[test]
    fn a_listing_with_the_name_the_rerun_wants_and_other_bytes_refuses_the_file() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let bytes = fs::read(&file).expect("reads");
        let sha = crate::pack::hashing::hex(&sha2::Sha256::digest(&bytes));
        let id = SeqIds::new(1).mint("2001-09-09T01:46:40Z".parse().expect("a time"));
        let dir = world.collection.object_dir(id);
        fs::create_dir_all(&dir).expect("the directory is made");
        fs::write(dir.join(format!("{}.listing.zst", &sha[..16])), b"other").expect("a leftover");
        let report = world.run(&[file]);
        assert!(report.imported.is_empty());
        assert!(refusal_of(&report).contains("other content"), "{report:?}");
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
    }

    #[test]
    fn a_path_an_earlier_import_has_warns_about_the_object_only() {
        let world = World::new();
        let first = world.simple("one.tar.zst");
        let second = world.package("two.tar.zst", |tree| {
            tree.file_with("proj/b.txt", "second", 0o644, 1_000_000_001);
        });
        world.run(&[first]);
        let report = world.run(&[second]);
        let taken: Vec<&str> = report
            .warnings
            .iter()
            .map(|w| w.message.as_str())
            .filter(|m| m.contains("already taken"))
            .collect();
        assert_eq!(taken.len(), 1, "{report:?}");
        assert!(taken[0].contains("by another object;"), "{taken:?}");
    }

    #[test]
    fn two_files_for_the_same_path_in_one_call_warn() {
        let world = World::new();
        let first = world.simple("one.tar.zst");
        let second = world.package("two.tar.zst", |tree| {
            tree.file_with("proj/b.txt", "second", 0o644, 1_000_000_001);
        });
        let report = world.run(&[first, second]);
        assert_eq!(report.imported.len(), 2, "{report:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("another file of this run")),
            "{report:?}"
        );
    }

    #[test]
    fn the_excluded_path_itself_is_refused() {
        let mut world = World::new();
        world.configure("exclude = [\"archive\"]");
        refused_path(&world, "archive", "excluded");
    }

    #[test]
    fn errors_have_the_kinds_of_what_went_wrong() {
        let reference = ImportError::Reference(ReferenceError::Outside {
            path: "x".into(),
            root: "y".into(),
        });
        assert_eq!(reference.kind(), ErrorKind::OutsideCollection);
        let store = ImportError::Store(StoreError::NoPending);
        assert_eq!(store.kind(), ErrorKind::Other);
        let meta = ImportError::Meta(MetaError::FormatTooNew { found: 9 });
        assert_eq!(meta.kind(), ErrorKind::FormatTooNew);
        let listing = ImportError::Listing(ListingError::FormatTooNew { found: 9 });
        assert_eq!(listing.kind(), ErrorKind::FormatTooNew);
        let run = ImportError::Run(RunError::Lock(crate::lock::LockError::Held {
            pid: 1,
            started: "2026-01-01T00:00:00Z".parse().expect("a time"),
        }));
        assert_eq!(run.kind(), ErrorKind::LockHeld);
        let other = ImportError::Interrupted {
            step: "x",
            source: io::Error::other("y"),
        };
        assert_eq!(other.kind(), ErrorKind::Other);
    }

    #[test]
    fn the_report_lists_its_warnings() {
        let world = World::new();
        let file = world.package("old.tar.zst", |tree| {
            tree.file_with("proj/a.txt", "x", 0o644, 100_000_000);
            let dir = tree.dir("proj");
            filetime::set_file_mtime(&dir, filetime::FileTime::from_unix_time(100_000_000, 0))
                .expect("a time");
        });
        let report = world.run(&[file]);
        assert_eq!(CommandReport::warnings(&report), &report.warnings[..]);
        assert!(!CommandReport::warnings(&report).is_empty());
    }

    #[test]
    fn an_import_the_repairs_do_not_know_is_completed_with_the_version_pending() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        world.run(std::slice::from_ref(&file));
        let id = rewrite_first_object(&world, |metadata| {
            // Two history entries: not the shape the repairs recognise.
            let first = metadata.history[0].clone();
            metadata.history.push(first);
        });
        let report = world.run(&[file]);
        assert_eq!(report.skipped[0].reason, SkipReason::EntryCompleted);
        let entry = tracking::read(&world.collection, id)
            .expect("reads")
            .expect("written");
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
    }

    #[test]
    fn an_exclude_entry_that_does_not_apply_lets_the_import_through() {
        let mut world = World::new();
        world.configure("exclude = [\"archive\"]");
        let file = world.simple("proj.tar.zst");
        let report = world.run(&[file]);
        assert_eq!(report.imported.len(), 1, "{report:?}");
    }

    /// Makes `dir` unwritable and checks that this keeps a file out, so a
    /// user that mode bits do not bind fails the test with a reason.
    fn lock_out(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).expect("chmod");
        let probe = dir.join("probe");
        assert!(
            fs::write(&probe, b"x").is_err(),
            "this test needs a user that mode 555 keeps out of {}",
            dir.display()
        );
    }

    fn let_in(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn a_store_that_cannot_be_written_is_an_error_of_the_run() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let objects = world.temp.root().join(".antiq/objects");
        lock_out(&objects);
        let result = world.run_with(&[file], None, &NoHooks);
        let_in(&objects);
        assert!(matches!(result, Err(ImportError::Store(_))), "{result:?}");
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
    }

    #[test]
    fn a_tmp_directory_that_cannot_be_written_is_an_error_of_the_run() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let tmp = world.temp.root().join(".antiq/tmp");
        // The repairs at the start of the run leave it as it is.
        lock_out(&tmp);
        let result = world.run_with(&[file], None, &NoHooks);
        let_in(&tmp);
        assert!(
            matches!(result, Err(ImportError::Io { action: "copy", .. })),
            "{result:?}"
        );
    }

    /// Locks the object directory once the payload is in it.
    struct LockAfterPayload(PathBuf);

    impl Hooks for LockAfterPayload {
        fn after(&self, step: Step) -> io::Result<()> {
            if step == Step::PayloadPlaced {
                use std::os::unix::fs::PermissionsExt;
                for entry in fs::read_dir(&self.0)? {
                    fs::set_permissions(entry?.path(), fs::Permissions::from_mode(0o555))?;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn a_listing_that_cannot_be_stored_is_an_error_of_the_run() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let objects = world.temp.root().join(".antiq/objects");
        let result = world.run_with(&[file], None, &LockAfterPayload(objects.clone()));
        for entry in fs::read_dir(&objects).expect("lists") {
            let_in(&entry.expect("an entry").path());
        }
        assert!(matches!(result, Err(ImportError::Store(_))), "{result:?}");
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
    }

    // =========================================================
    // Dry runs, locks and changing inputs
    // =========================================================

    #[test]
    fn a_dry_run_names_the_action_and_writes_nothing() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let report = world.dry(std::slice::from_ref(&file));
        assert!(report.dry_run);
        let actions = report.actions.as_deref().expect("a dry run has actions");
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0].describe(),
            format!("import {} as proj", file.display())
        );
        assert!(report.imported.is_empty());
        world.assert_nothing_written();
        assert!(!world.collection.lock_path().exists(), "no lock was taken");
    }

    #[test]
    fn a_dry_run_reports_refusals_and_skips() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        world.run(std::slice::from_ref(&file));
        let bad = world.input("bad.tar.gz", &[0x1F, 0x8B, 8]);
        let report = world.dry(&[file, bad]);
        assert_eq!(report.actions.as_deref(), Some(&[][..]));
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.refused.len(), 1);
    }

    #[test]
    fn a_file_that_became_another_between_the_checks_and_the_copy_is_refused() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let other = world.simple("other.tar.zst");
        // Replace the input by another file before the run copies it: the
        // identity of the file changes.
        let started = run::start(&world.collection, &world.clock, false).expect("a guard");
        let request_files = [file.clone()];
        let prepared = prepare_for_test(&world, &request_files);
        fs::remove_file(&file).expect("removed");
        fs::rename(&other, &file).expect("replaced");
        let report = apply_for_test(&world, &started, prepared);
        assert!(refusal_of(&report).contains("changed"), "{report:?}");
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
        assert_eq!(world.names_in(".antiq/objects"), Vec::<String>::new());
    }

    /// Replaces the input by another package once it was walked.
    struct SwapAfterWalk {
        file: PathBuf,
        replacement: Vec<u8>,
    }

    impl Hooks for SwapAfterWalk {
        fn after(&self, step: Step) -> io::Result<()> {
            if step == Step::Walked {
                fs::write(&self.file, &self.replacement)?;
            }
            Ok(())
        }
    }

    #[test]
    fn a_file_replaced_in_place_between_the_walk_and_the_decode_is_refused() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let other = world.package("other.tar.zst", |tree| {
            tree.file_with("proj/other.txt", "other", 0o644, 1_000_000_001);
        });
        let hooks = SwapAfterWalk {
            file: file.clone(),
            replacement: fs::read(other).expect("reads"),
        };
        let report = world
            .run_with(&[file], None, &hooks)
            .expect("the run goes on");
        assert!(
            refusal_of(&report).contains("changed while it was checked"),
            "{report:?}"
        );
        world.assert_nothing_written();
    }

    #[test]
    fn a_file_rewritten_with_the_same_identity_is_refused_by_its_hash() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let started = run::start(&world.collection, &world.clock, false).expect("a guard");
        let files = [file.clone()];
        let prepared = prepare_for_test(&world, &files);
        let mut bytes = fs::read(&file).expect("reads");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&file, bytes).expect("rewritten in place");
        let report = apply_for_test(&world, &started, prepared);
        assert!(refusal_of(&report).contains("changed"), "{report:?}");
        assert_eq!(world.names_in(".antiq/tmp"), Vec::<String>::new());
    }

    #[test]
    fn a_file_imported_by_someone_else_after_the_checks_is_skipped() {
        let world = World::new();
        let file = world.simple("proj.tar.zst");
        let files = [file.clone()];
        let prepared = prepare_for_test(&world, &files);
        world.run(&[file]);
        let started = run::start(&world.collection, &world.clock, false).expect("a guard");
        let report = apply_for_test(&world, &started, prepared);
        assert!(report.imported.is_empty());
        assert_eq!(report.skipped[0].reason, SkipReason::AlreadyImported);
        assert_eq!(world.store().objects().expect("lists").len(), 1);
    }

    fn prepare_for_test(world: &World, files: &[PathBuf]) -> Prepared {
        prepare(&world.request(files, None), world.clock.now(), &NoHooks)
            .expect("the files are prepared")
    }

    fn apply_for_test(world: &World, started: &Started, prepared: Prepared) -> ImportReport {
        let files: [PathBuf; 0] = [];
        finish(
            &world.request(&files, None),
            prepared,
            started,
            &world.clock,
            &world.ids,
            &NoHooks,
        )
        .expect("the import runs")
    }

    // =========================================================
    // Names
    // =========================================================

    #[test]
    fn names_a_volume_would_refuse_give_warnings_and_the_import_completes() {
        let world = World::new();
        let tar = raw_tar(&[
            RawMember::dir(b"proj/"),
            RawMember::file(b"proj/README", b"1"),
            RawMember::file(b"proj/readme", b"2"),
        ]);
        let file = world.input("a.tar.zst", &compress(&tar));
        let report = world.run(&[file]);
        assert_eq!(report.imported.len(), 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("on macOS") && w.message.contains("one name")),
            "{report:?}"
        );
    }

    #[test]
    fn the_name_of_the_piece_is_checked_too() {
        let world = World::new();
        let tar = raw_tar(&[
            RawMember::dir(b"pr\xFFoj/"),
            RawMember::file(b"pr\xFFoj/a", b"1"),
        ]);
        let file = world.input("a.tar.zst", &compress(&tar));
        let report = world.run(&[file]);
        assert_eq!(report.imported.len(), 1, "{report:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("on macOS") && w.message.contains("pr%FFoj")),
            "{report:?}"
        );
    }

    #[test]
    fn a_crate_made_archive_with_a_pledged_size_is_imported() {
        let world = World::new();
        let mut encoder = encoder(Vec::new(), &ZstdSettings::archive(1)).expect("an encoder");
        encoder.write_all(&valid_tar()).expect("writes");
        let file = world.input("a.tar.zst", &encoder.finish().expect("ends"));
        let report = world.run(&[file]);
        assert_eq!(report.imported.len(), 1, "{report:?}");
    }
}
