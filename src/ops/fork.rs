// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fork` command.
//!
//! [`plan`] reads the state and decides; it writes nothing, so a dry run
//! can print its result. What it refuses, it refuses before anything is
//! renamed or written. [`apply`] then writes in the order the start-of-run
//! repairs rely on (spec "States").
//!
//! [`mint`], [`forked_old`] and [`forked_new`] are the parts other
//! commands share: a fork that is part of another operation mints the new
//! ID and writes the new metadata the same way, and writes the old ID's
//! metadata only where the operation forks an ID of this collection.

use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::error::ErrorKind;
use crate::fsx::FsxError;
use crate::fsx::moves::{Moved, move_dir};
use crate::fsx::tree::TreeRoot;
use crate::id::{IdSource, ObjectId};
use crate::index::{Index, IndexError};
use crate::lock::RunLock;
use crate::machine::MachineError;
use crate::meta::{
    self, CreatedSource, HistoryAction, HistoryEntry, Lifecycle, Metadata, PayloadKind,
};
use crate::ops::unearth::{
    TargetProblem, UnearthError, absolute, check_planned_target, check_target_on_disk,
};
use crate::plan::PlanAction;
use crate::relpath::{Name, RelPath, RelPathError, escape};
use crate::report::{CommandReport, Warning};
use crate::run::RunGuard;
use crate::select::{Rules, SelectError};
use crate::store::{Current, Store, StoreError};
use crate::tracking::{
    self, DecisionKind, EntryState, PieceState, TrackingEntry, TrackingError, VersionTakenOut,
};
use crate::tree::EntryKind;
use jiff::Timestamp;
use serde::Serialize;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Errors of the `fork` command.
#[derive(Debug, thiserror::Error)]
pub enum ForkError {
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
    Machine(#[from] MachineError),
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error("{id} has no metadata on this machine")]
    NoMetadata { id: ObjectId },
    #[error("`{name}` is not a name for a piece: {reason}")]
    InvalidName { name: String, reason: RelPathError },
    #[error("cannot fork `{path}`: {why}")]
    Refused { path: RelPath, why: Refusal },
    #[error("cannot rename the folder to `{path}`: {problem}")]
    Target {
        path: RelPath,
        problem: TargetProblem,
    },
    #[error("the new ID {id} exists already")]
    IdTaken { id: ObjectId },
}

impl crate::error::HasKind for ForkError {
    fn kind(&self) -> ErrorKind {
        match self {
            ForkError::Store(error) => error.kind(),
            ForkError::InvalidName { .. } => ErrorKind::Usage,
            _ => ErrorKind::Other,
        }
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> ForkError {
    let path = path.display().to_string();
    move |source| ForkError::Io {
        action,
        path,
        source,
    }
}

/// The checks of a rename target report a target problem or an I/O
/// failure; those are all `fork` makes through them.
fn placement(error: UnearthError) -> ForkError {
    match error {
        UnearthError::Target { path, problem } => ForkError::Target { path, problem },
        UnearthError::Io {
            action,
            path,
            source,
        } => ForkError::Io {
            action,
            path,
            source,
        },
        other => unreachable!("checking a rename target does not fail with: {other}"),
    }
}

/// Why a piece cannot be forked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The object is unearthed here but owned by another collection.
    OtherCollection,
    /// Nothing was archived from the piece.
    NotUnearthed,
    /// The folder at the path is a new piece, next to an archived object.
    NewFolder,
    /// The folder at the path belongs to another object, such as the fork
    /// this object made.
    ClaimedBy(ObjectId),
    /// There is no folder at the path.
    NoFolder,
    /// A live piece of another collection.
    AnotherCollectionsPiece,
    /// The tracking entry does not say `live`.
    Entry(EntryState),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::OtherCollection => f.write_str("the object belongs to another collection"),
            Refusal::NotUnearthed => f.write_str(
                "it is not unearthed: nothing was archived from it, so there is nothing to fork from",
            ),
            Refusal::NewFolder => f.write_str(
                "the folder is a new piece next to an archived object; `archive` or `sync` gives it an ID",
            ),
            Refusal::ClaimedBy(id) => write!(f, "the folder belongs to {id} now"),
            Refusal::NoFolder => f.write_str("there is no folder; `unearth` brings it back"),
            Refusal::AnotherCollectionsPiece => {
                f.write_str("it is another collection's live piece; `unearth` makes the fork")
            }
            Refusal::Entry(state) => {
                let state = format!("{state:?}").to_lowercase();
                write!(f, "its tracking entry says `{state}`, not `live`")
            }
        }
    }
}

// =========================================================
// Planning
// =========================================================

/// What is to be forked, with the reference already resolved.
#[derive(Debug, Clone)]
pub struct ForkRequest {
    pub id: ObjectId,
    /// The new name of the folder within its parent; the folder keeps its
    /// name when absent.
    pub name: Option<OsString>,
}

/// An action of the plan, as a dry run prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ForkAction {
    Fork {
        /// The ID the piece has now.
        from: ObjectId,
        /// Where the piece is afterwards.
        path: RelPath,
        old_path: RelPath,
    },
}

impl PlanAction for ForkAction {
    fn describe(&self) -> String {
        match self {
            ForkAction::Fork { path, old_path, .. } if path == old_path => {
                format!("fork {old_path}")
            }
            ForkAction::Fork { path, old_path, .. } => {
                format!("fork {old_path} and rename it to {path}")
            }
        }
    }
}

/// A fork that can go ahead.
#[derive(Debug)]
pub struct ForkPlan {
    id: ObjectId,
    current: Current,
    entry: TrackingEntry,
    from: RelPath,
    to: RelPath,
}

impl ForkPlan {
    pub fn action(&self) -> ForkAction {
        ForkAction::Fork {
            from: self.id,
            path: self.to.clone(),
            old_path: self.from.clone(),
        }
    }
}

/// Works out what forking `request` would do. Nothing is written.
pub fn plan(collection: &Collection, request: &ForkRequest) -> Result<ForkPlan, ForkError> {
    let id = request.id;
    let name = request.name.as_deref().map(parse_name).transpose()?;
    let store = Store::new(collection);
    let current = store.current(id)?.ok_or(ForkError::NoMetadata { id })?;
    let entry = tracking::read(collection, id)?;
    let root = TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;

    let is_folder = |path: &RelPath| {
        root.lstat(path)
            .is_ok_and(|found| found.kind == EntryKind::Directory)
    };
    let shown = entry
        .as_ref()
        .map_or_else(|| current.metadata.path.clone(), |entry| entry.path.clone());
    let refused = |why| ForkError::Refused {
        path: shown.clone(),
        why,
    };
    let state = tracking::derive_state(
        entry.as_ref(),
        Some(&current.metadata),
        entry.as_ref().is_some_and(|entry| is_folder(&entry.path)),
        collection.id(),
    )?;
    match state {
        PieceState::Unearthed => {}
        PieceState::Live => return Err(refused(Refusal::NotUnearthed)),
        PieceState::LiveOnAnotherMachine => return Err(refused(Refusal::AnotherCollectionsPiece)),
        // Whatever is at the path of an object that is not unearthed does
        // not belong to it.
        PieceState::Archived | PieceState::RemoteOnly | PieceState::Gone | PieceState::New => {
            if !is_folder(&shown) {
                return Err(refused(Refusal::NoFolder));
            }
            // After a fork the old ID is `gone` and the folder belongs to
            // the new ID, so advice to give the folder an ID would be wrong.
            let claimant = Index::compute(collection)?.at_path(&shown).claimant;
            return Err(refused(match claimant {
                Some(claimant) if claimant != id => Refusal::ClaimedBy(claimant),
                _ => Refusal::NewFolder,
            }));
        }
    }
    let entry = entry.expect("a piece is unearthed only with a tracking entry");
    // `derive_state` takes the folder of a live piece of another
    // collection for an unearthed piece too; forking it would rewrite
    // that collection's object.
    if current.metadata.collection != collection.id() {
        return Err(refused(Refusal::OtherCollection));
    }
    if entry.state != EntryState::Live {
        return Err(refused(Refusal::Entry(entry.state)));
    }
    // Every write of the fork goes through `write_pending`. What it
    // refuses is found out now, because the rename comes before the first
    // write and cannot be taken back.
    store.check_writable(id)?;

    let from = entry.path.clone();
    let to = name.map_or_else(|| from.clone(), |name| parent(&from).join(&name));
    if to != from {
        let rules = bound_rules(collection, &root)?;
        let index = Index::compute(collection)?;
        check_planned_target(&rules, &index, id, Some(&entry), &to).map_err(placement)?;
        check_target_on_disk(&root, &rules, &to).map_err(placement)?;
    }
    Ok(ForkPlan {
        id,
        current,
        entry,
        from,
        to,
    })
}

fn parse_name(name: &std::ffi::OsStr) -> Result<Name, ForkError> {
    Name::from_bytes(name.as_bytes().to_vec()).map_err(|reason| ForkError::InvalidName {
        name: escape(name.as_bytes()),
        reason,
    })
}

fn last_name(path: &RelPath) -> &[u8] {
    path.components()
        .last()
        .expect("the path of a piece has a name")
}

fn parent(path: &RelPath) -> RelPath {
    let mut names: Vec<&[u8]> = path.components().collect();
    names.pop();
    RelPath::from_bytes(names.join(&b'/')).expect("the names of a valid path are a valid path")
}

fn bound_rules(collection: &Collection, root: &TreeRoot) -> Result<Rules, ForkError> {
    Ok(Rules::new(
        &collection.config().collection.patterns,
        &collection.config().collection.exclude,
    )?
    .bind(root)?)
}

// =========================================================
// Applying
// =========================================================

/// What a run of `fork` did.
#[derive(Debug, Serialize)]
pub struct ForkReport {
    /// The new ID.
    pub id: ObjectId,
    /// The ID the piece had.
    pub from: ObjectId,
    pub path: RelPath,
    pub old_path: RelPath,
    pub abs_path: String,
    pub warnings: Vec<Warning>,
}

impl CommandReport for ForkReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// What the steps of a fork reach through. Tests replace a part to fail a
/// step or to watch the order; the real parts are what the command uses.
pub(crate) trait Steps {
    fn rename(&self, root: &TreeRoot, from: &RelPath, to: &RelPath) -> Result<Moved, FsxError>;

    fn write_metadata(
        &self,
        store: &Store<'_>,
        lock: &RunLock,
        metadata: &Metadata,
    ) -> Result<(), StoreError>;

    fn write_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        clock: &dyn Clock,
        entry: &mut TrackingEntry,
    ) -> Result<(), TrackingError>;
}

pub(crate) struct RealSteps;

impl Steps for RealSteps {
    fn rename(&self, root: &TreeRoot, from: &RelPath, to: &RelPath) -> Result<Moved, FsxError> {
        move_dir(root, from, root, to)
    }

    fn write_metadata(
        &self,
        store: &Store<'_>,
        lock: &RunLock,
        metadata: &Metadata,
    ) -> Result<(), StoreError> {
        store.write_pending(lock, metadata)
    }

    fn write_entry(
        &self,
        collection: &Collection,
        lock: &RunLock,
        clock: &dyn Clock,
        entry: &mut TrackingEntry,
    ) -> Result<(), TrackingError> {
        tracking::write(collection, lock, clock, entry)
    }
}

/// Carries out a plan under the run lock `guard` holds.
pub fn apply(
    collection: &Collection,
    plan: &ForkPlan,
    guard: &RunGuard,
    clock: &dyn Clock,
    ids: &dyn IdSource,
) -> Result<ForkReport, ForkError> {
    apply_with(
        collection,
        plan,
        guard.lock(),
        clock,
        ids,
        &RealSteps,
        guard.warnings().to_vec(),
    )
}

/// The rename comes first: it is the one step that can still be refused
/// for reasons outside the tool (a taken name, a name past the limit, a
/// folder that cannot be written to), and nothing is written before it, so
/// a refusal leaves the collection as it was. After it come the writes of
/// spec "States": the old ID's archived metadata, the old entry `gone`
/// with its local payloads, the new ID's metadata and the new entry.
pub(crate) fn apply_with(
    collection: &Collection,
    plan: &ForkPlan,
    lock: &RunLock,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    steps: &dyn Steps,
    mut warnings: Vec<Warning>,
) -> Result<ForkReport, ForkError> {
    let machine = collection.machine_name()?;
    let old = &plan.current.metadata;

    // A fork that stopped after the old metadata was written is finished
    // under the time it began with, so the old ID ends with one `fork`
    // history entry.
    let resumed = fork_recorded(old, &machine);
    let time = resumed.unwrap_or_else(|| whole_seconds(clock.now()));
    let new_id = mint(ids, collection, time)?;

    if plan.to != plan.from {
        let root =
            TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;
        let rules = bound_rules(collection, &root)?;
        // The world may have changed since planning: this is the check that
        // the name is free right before the rename.
        check_target_on_disk(&root, &rules, &plan.to).map_err(placement)?;
        let moved = steps.rename(&root, &plan.from, &plan.to)?;
        if let Some(error) = moved.mode_restore_error {
            warnings.push(Warning::new(format!(
                "the mode of `{}` could not be put back after the rename: {error}",
                plan.to
            )));
        }
    }

    let store = Store::new(collection);
    if resumed.is_none() {
        steps.write_metadata(&store, lock, &forked_old(old, &machine, time))?;
    }

    // The folder belongs to the new ID now, so what the old entry knew
    // about the folder goes with it. A version conflict and a superseded
    // archive are about the old ID's metadata and payloads and stay.
    let mut old_entry = plan.entry.clone();
    old_entry.state = EntryState::Gone;
    old_entry.unearthed = None;
    old_entry.unearthed_payload_version = None;
    old_entry.removing_payload = None;
    old_entry.version_taken_out = VersionTakenOut::Pending;
    old_entry.set_diverged(false);
    old_entry.resolve_decision(DecisionKind::ReplacingWithLoss);
    steps.write_entry(collection, lock, clock, &mut old_entry)?;

    let forked = forked_new(new_id, collection.id(), &plan.to, &machine, time);
    steps.write_metadata(&store, lock, &forked)?;

    // The folder keeps the time it was unearthed, which the idle clock
    // starts from.
    let mut new_entry = TrackingEntry::new(
        new_id,
        plan.to.clone(),
        EntryState::Live,
        VersionTakenOut::Pending,
        time,
    );
    new_entry.unearthed = plan.entry.unearthed;
    steps.write_entry(collection, lock, clock, &mut new_entry)?;

    Ok(ForkReport {
        id: new_id,
        from: plan.id,
        abs_path: absolute(collection, &plan.to),
        path: plan.to.clone(),
        old_path: plan.from.clone(),
        warnings,
    })
}

/// The time of the `fork` that archived `metadata` on `machine`, when its
/// history ends with one.
fn fork_recorded(metadata: &Metadata, machine: &str) -> Option<Timestamp> {
    let last = metadata.history.last()?;
    (metadata.lifecycle == Lifecycle::Archived
        && last.action == HistoryAction::Fork.as_str()
        && last.machine == machine)
        .then_some(last.time)
}

// =========================================================
// What other operations share
// =========================================================

/// Mints the ID of a fork made at `time`. An ID that has an object
/// directory or a tracking entry already is refused.
pub fn mint(
    ids: &dyn IdSource,
    collection: &Collection,
    time: Timestamp,
) -> Result<ObjectId, ForkError> {
    let id = ids.mint(time);
    for taken in [collection.object_dir(id), collection.tracking_path(id)] {
        if taken.try_exists().map_err(io_error("look at", &taken))? {
            return Err(ForkError::IdTaken { id });
        }
    }
    Ok(id)
}

/// The metadata the old ID gets when its folder is forked away: archived,
/// without its live payload, and the fork in its history.
pub fn forked_old(metadata: &Metadata, machine: &str, time: Timestamp) -> Metadata {
    let mut changed = metadata.clone();
    changed.lifecycle = Lifecycle::Archived;
    changed
        .payloads
        .retain(|payload| payload.kind != PayloadKind::Live);
    changed
        .history
        .push(HistoryEntry::new(time, machine, HistoryAction::Fork));
    changed
}

/// The metadata of a forked ID: owned by `collection`, live at `path`,
/// without a payload until it is archived.
pub fn forked_new(
    id: ObjectId,
    collection: ObjectId,
    path: &RelPath,
    machine: &str,
    time: Timestamp,
) -> Metadata {
    Metadata {
        format: meta::FORMAT,
        id,
        collection,
        path: path.clone(),
        name: Name::from_bytes(last_name(path).to_vec())
            .expect("the last name of a path is a valid name"),
        created: whole_seconds(time),
        created_source: CreatedSource::Fork,
        lifecycle: Lifecycle::Live,
        payloads: Vec::new(),
        history: vec![HistoryEntry::new(time, machine, HistoryAction::Fork)],
        extra: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::HasKind;
    use crate::id::SeqIds;
    use crate::meta::{
        CreatedSource, HistoryAction, HistoryEntry, Lifecycle, MetaError, PayloadKind,
    };
    use crate::ops::unearth::tests::{FILES, Fixture, ID, NOW, OTHER_ID, id, path, time};
    use crate::ops::unearth::{self, UnearthError, UnearthRequest};
    use crate::repair::{self, RealEffects};
    use crate::testing::tree::COLLECTION_ID;
    use crate::tracking::{self, DecisionKind, PieceState, VersionTakenOut};
    use crate::tree::EntryKind;
    use jiff::SignedDuration;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    const PIECE: &str = "projects/a";

    /// The clock reads this many seconds after [`NOW`] when a fork runs,
    /// with a fraction that the fork must drop.
    const LATER: SignedDuration = SignedDuration::from_millis(90_500);

    /// The second of the fork.
    fn fork_time() -> Timestamp {
        time("2026-11-04T12:01:30Z")
    }

    /// An unearthed piece at [`PIECE`], unearthed at [`NOW`], with the
    /// clock moved on to the time of the fork.
    fn unearthed() -> Fixture {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, FILES);
        fx.unearthed(ID, PIECE);
        fx.clock.advance(LATER);
        fx
    }

    fn request(name: Option<&str>) -> ForkRequest {
        ForkRequest {
            id: id(ID),
            name: name.map(OsString::from),
        }
    }

    /// The ID the first fork of a test gets.
    fn minted() -> ObjectId {
        SeqIds::new(1).mint(fork_time())
    }

    fn planned(fx: &Fixture, name: Option<&str>) -> Result<ForkPlan, ForkError> {
        plan(&fx.collection, &request(name))
    }

    fn fork_with(
        fx: &Fixture,
        name: Option<&str>,
        steps: &dyn Steps,
    ) -> Result<ForkReport, ForkError> {
        let plan = planned(fx, name)?;
        apply_with(
            &fx.collection,
            &plan,
            &fx.lock,
            &fx.clock,
            &SeqIds::new(1),
            steps,
            Vec::new(),
        )
    }

    fn fork(fx: &Fixture, name: Option<&str>) -> Result<ForkReport, ForkError> {
        fork_with(fx, name, &RealSteps)
    }

    fn forked(fx: &Fixture, name: Option<&str>) -> ForkReport {
        fork(fx, name).expect("the fork runs")
    }

    fn refusal(fx: &Fixture, name: Option<&str>) -> ForkError {
        planned(fx, name).expect_err("the fork is refused")
    }

    fn why(error: ForkError) -> Refusal {
        match error {
            ForkError::Refused { why, .. } => why,
            other => panic!("expected a refusal, got {other}"),
        }
    }

    fn problem(error: ForkError) -> TargetProblem {
        match error {
            ForkError::Target { problem, .. } => problem,
            other => panic!("expected a target problem, got {other}"),
        }
    }

    fn current(fx: &Fixture, object: ObjectId) -> Current {
        Store::new(&fx.collection)
            .current(object)
            .expect("the metadata reads")
            .expect("the object exists")
    }

    fn entry_of(fx: &Fixture, object: ObjectId) -> TrackingEntry {
        tracking::read(&fx.collection, object)
            .expect("the entry reads")
            .expect("the entry exists")
    }

    fn state_of(fx: &Fixture, object: ObjectId) -> PieceState {
        let current = current(fx, object);
        let entry = tracking::read(&fx.collection, object).expect("the entry reads");
        let root = TreeRoot::open(fx.collection.root()).expect("the collection opens");
        let present = entry.as_ref().is_some_and(|entry| {
            root.lstat(&entry.path)
                .is_ok_and(|found| found.kind == EntryKind::Directory)
        });
        tracking::derive_state(
            entry.as_ref(),
            Some(&current.metadata),
            present,
            fx.collection.id(),
        )
        .expect("the state derives")
    }

    /// Every path below the collection with its content, directories as
    /// `None`, apart from the lock file.
    fn snapshot(fx: &Fixture) -> BTreeMap<String, Option<Vec<u8>>> {
        fn walk(base: &Path, directory: &Path, seen: &mut BTreeMap<String, Option<Vec<u8>>>) {
            for item in fs::read_dir(directory).expect("lists") {
                let path = item.expect("an item").path();
                let name = path
                    .strip_prefix(base)
                    .expect("below")
                    .display()
                    .to_string();
                if name == ".antiq/lock" {
                    continue;
                }
                if path.is_dir() && !path.is_symlink() {
                    seen.insert(name, None);
                    walk(base, &path, seen);
                } else {
                    seen.insert(name, fs::read(&path).ok());
                }
            }
        }
        let mut seen = BTreeMap::new();
        walk(fx.collection.root(), fx.collection.root(), &mut seen);
        seen
    }

    fn not_root() {
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "these tests assume a non-root user"
        );
    }

    // ---------------------------------------------------------------------
    // The steps, watched and failed
    // ---------------------------------------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        Rename,
        OldMetadata,
        OldEntry,
        NewMetadata,
        NewEntry,
    }

    impl Step {
        fn label(self) -> &'static str {
            match self {
                Step::Rename => "rename",
                Step::OldMetadata => "metadata old",
                Step::OldEntry => "entry old",
                Step::NewMetadata => "metadata new",
                Step::NewEntry => "entry new",
            }
        }
    }

    /// The real steps, logged, and one of them failing where asked. A step
    /// that fails does nothing; the steps before it ran for real.
    struct Recorder {
        old: ObjectId,
        fail: Option<Step>,
        log: RefCell<Vec<&'static str>>,
        mode_problem: bool,
    }

    impl Recorder {
        fn new(fail: Option<Step>) -> Recorder {
            Recorder {
                old: id(ID),
                fail,
                log: RefCell::new(Vec::new()),
                mode_problem: false,
            }
        }

        /// Notes `step` and says whether it is the one that fails.
        fn enter(&self, step: Step) -> bool {
            self.log.borrow_mut().push(step.label());
            self.fail == Some(step)
        }

        fn log(&self) -> Vec<&'static str> {
            self.log.borrow().clone()
        }
    }

    fn injected() -> io::Error {
        io::Error::other("injected")
    }

    impl Steps for Recorder {
        fn rename(&self, root: &TreeRoot, from: &RelPath, to: &RelPath) -> Result<Moved, FsxError> {
            if self.enter(Step::Rename) {
                return Err(FsxError::Io {
                    action: "rename",
                    path: from.to_string(),
                    source: injected(),
                });
            }
            let mut moved = RealSteps.rename(root, from, to)?;
            if self.mode_problem {
                moved.mode_restore_error = Some(FsxError::Io {
                    action: "set the mode of",
                    path: to.to_string(),
                    source: injected(),
                });
            }
            Ok(moved)
        }

        fn write_metadata(
            &self,
            store: &Store<'_>,
            lock: &crate::lock::RunLock,
            metadata: &Metadata,
        ) -> Result<(), StoreError> {
            let step = if metadata.id == self.old {
                Step::OldMetadata
            } else {
                Step::NewMetadata
            };
            if self.enter(step) {
                return Err(StoreError::Io {
                    action: "write",
                    path: metadata.id.to_string(),
                    source: injected(),
                });
            }
            RealSteps.write_metadata(store, lock, metadata)
        }

        fn write_entry(
            &self,
            collection: &Collection,
            lock: &crate::lock::RunLock,
            clock: &dyn Clock,
            entry: &mut TrackingEntry,
        ) -> Result<(), TrackingError> {
            let step = if entry.id == self.old {
                Step::OldEntry
            } else {
                Step::NewEntry
            };
            if self.enter(step) {
                return Err(TrackingError::Io {
                    action: "write",
                    path: entry.id.to_string(),
                    source: injected(),
                });
            }
            RealSteps.write_entry(collection, lock, clock, entry)
        }
    }

    /// Runs the fork with `step` failing and checks that it failed there.
    fn crash_at(fx: &Fixture, name: Option<&str>, step: Step) {
        let recorder = Recorder::new(Some(step));
        let error = fork_with(fx, name, &recorder).expect_err("the step fails");
        assert!(error.to_string().contains("injected"), "{error}");
        assert_eq!(recorder.log().last(), Some(&step.label()));
    }

    /// The repairs of the next run.
    fn next_run(fx: &Fixture) {
        repair::run(&fx.collection, &fx.lock, &fx.clock, &RealEffects).expect("the repairs run");
    }

    // ---------------------------------------------------------------------
    // What a fork writes
    // ---------------------------------------------------------------------

    #[test]
    fn a_fork_mints_an_id_with_the_time_of_the_fork() {
        let fx = unearthed();

        let report = forked(&fx, None);

        assert_eq!(report.id, minted());
        let found = current(&fx, report.id).metadata;
        assert_eq!(found.id, report.id);
        assert_eq!(found.created, fork_time());
        assert_eq!(found.created_source, CreatedSource::Fork);
        assert_eq!(found.lifecycle, Lifecycle::Live);
        assert_eq!(found.collection, id(COLLECTION_ID));
        assert_eq!(found.path, path(PIECE));
        assert_eq!(found.name.as_bytes(), b"a");
        assert!(found.payloads.is_empty());
        assert_eq!(
            found.history,
            [HistoryEntry::new(
                fork_time(),
                "imperius",
                HistoryAction::Fork
            )]
        );
        assert_eq!(
            current(&fx, report.id).source,
            crate::store::Source::Pending
        );
    }

    #[test]
    fn the_new_entry_keeps_the_unearthed_time() {
        let fx = unearthed();

        let report = forked(&fx, None);

        let entry = entry_of(&fx, report.id);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path(PIECE));
        assert_eq!(entry.unearthed, Some(time(NOW)));
        assert_eq!(entry.unearthed_payload_version, None);
        assert!(entry.local_payloads.is_empty());
        assert!(entry.committed_versions.is_empty());
        assert!(entry.pending_decisions.is_empty());
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
        assert_eq!(state_of(&fx, report.id), PieceState::Live);
    }

    #[test]
    fn the_old_id_is_archived_and_loses_its_live_payload() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| {
            let mut live = document["payloads"][0].clone();
            live["kind"] = json!("live");
            live["file"] = json!("0123456789abcdef-a.tar.zst");
            document["payloads"]
                .as_array_mut()
                .expect("payloads")
                .push(live);
        });
        let before = current(&fx, id(ID)).metadata;
        assert_eq!(before.payloads.len(), 2);

        forked(&fx, None);

        let after = current(&fx, id(ID));
        assert_eq!(after.source, crate::store::Source::Pending);
        assert_eq!(after.metadata.lifecycle, Lifecycle::Archived);
        assert_eq!(after.metadata.payloads, before.payloads[..1]);
        assert!(
            after
                .metadata
                .payloads
                .iter()
                .all(|payload| payload.kind == PayloadKind::Archive)
        );
        assert_eq!(after.metadata.path, before.path);
        assert_eq!(after.metadata.collection, before.collection);
        let mut expected = before.history.clone();
        expected.push(HistoryEntry::new(
            fork_time(),
            "imperius",
            HistoryAction::Fork,
        ));
        assert_eq!(after.metadata.history, expected);
    }

    #[test]
    fn the_old_entry_is_gone_and_keeps_its_local_payloads() {
        let fx = unearthed();
        let mut before = entry_of(&fx, id(ID));
        before.committed_versions = vec![1];
        before.removing_payload = Some(before.local_payloads[0].file.clone());
        fx.write_entry(&mut before);

        forked(&fx, None);

        let after = entry_of(&fx, id(ID));
        assert_eq!(after.state, EntryState::Gone);
        assert_eq!(after.path, path(PIECE));
        assert_eq!(after.unearthed, None);
        assert_eq!(after.unearthed_payload_version, None);
        assert_eq!(after.removing_payload, None);
        assert_eq!(after.version_taken_out, VersionTakenOut::Pending);
        assert_eq!(after.local_payloads, before.local_payloads);
        assert_eq!(after.committed_versions, vec![1]);
        assert!(fx.at("projects/a/notes.txt").exists());
        assert_eq!(state_of(&fx, id(ID)), PieceState::Archived);
    }

    #[test]
    fn replacing_with_loss_is_dropped_from_the_old_entry() {
        let fx = unearthed();
        let mut entry = entry_of(&fx, id(ID));
        entry.decide(DecisionKind::ReplacingWithLoss);
        fx.write_entry(&mut entry);

        let report = forked(&fx, None);

        assert!(entry_of(&fx, id(ID)).pending_decisions.is_empty());
        assert!(entry_of(&fx, report.id).pending_decisions.is_empty());
    }

    #[test]
    fn a_diverged_folder_forks_and_the_new_entry_is_not_diverged() {
        let fx = unearthed();
        let mut entry = entry_of(&fx, id(ID));
        entry.decide(DecisionKind::DivergedFolder);
        fx.write_entry(&mut entry);

        let report = forked(&fx, None);

        let old = entry_of(&fx, id(ID));
        assert!(!old.diverged);
        assert!(old.pending_decisions.is_empty());
        let new = entry_of(&fx, report.id);
        assert!(!new.diverged);
        assert!(new.pending_decisions.is_empty());
    }

    #[test]
    fn a_version_conflict_stays_on_the_old_entry() {
        let fx = unearthed();
        let mut entry = entry_of(&fx, id(ID));
        entry.decide(DecisionKind::VersionConflict);
        fx.write_entry(&mut entry);

        let report = forked(&fx, None);

        let kinds: Vec<DecisionKind> = entry_of(&fx, id(ID))
            .pending_decisions
            .iter()
            .map(|decision| decision.kind)
            .collect();
        assert_eq!(kinds, [DecisionKind::VersionConflict]);
        assert!(entry_of(&fx, report.id).pending_decisions.is_empty());
    }

    #[test]
    fn a_superseded_archive_decision_stays_on_the_old_entry() {
        let fx = unearthed();
        let mut entry = entry_of(&fx, id(ID));
        entry.decide(DecisionKind::SupersededArchive);
        fx.write_entry(&mut entry);

        let report = forked(&fx, None);

        let kinds: Vec<DecisionKind> = entry_of(&fx, id(ID))
            .pending_decisions
            .iter()
            .map(|decision| decision.kind)
            .collect();
        assert_eq!(kinds, [DecisionKind::SupersededArchive]);
        assert!(entry_of(&fx, report.id).pending_decisions.is_empty());
    }

    #[test]
    fn the_steps_run_in_the_order_of_the_spec() {
        let fx = unearthed();
        let recorder = Recorder::new(None);

        fork_with(&fx, None, &recorder).expect("the fork runs");

        assert_eq!(
            recorder.log(),
            ["metadata old", "entry old", "metadata new", "entry new"]
        );
    }

    #[test]
    fn the_rename_comes_before_the_steps_of_the_spec() {
        let fx = unearthed();
        let recorder = Recorder::new(None);

        fork_with(&fx, Some("b"), &recorder).expect("the fork runs");

        assert_eq!(
            recorder.log(),
            [
                "rename",
                "metadata old",
                "entry old",
                "metadata new",
                "entry new"
            ]
        );
    }

    #[test]
    fn the_report_names_both_ids_and_both_paths() {
        let fx = unearthed();

        let report = forked(&fx, Some("b"));

        assert_eq!(report.id, minted());
        assert_eq!(report.from, id(ID));
        assert_eq!(report.path, path("projects/b"));
        assert_eq!(report.old_path, path(PIECE));
        assert_eq!(report.abs_path, fx.at("projects/b").display().to_string());
        assert!(report.warnings.is_empty());
        assert!(!CommandReport::partial(&report));
        assert!(CommandReport::warnings(&report).is_empty());
    }

    #[test]
    fn the_command_applies_a_plan_under_the_guard_of_the_run() {
        let fx = unearthed();
        let plan = planned(&fx, Some("b")).expect("planned");
        let Fixture {
            collection,
            clock,
            lock,
            temp: _temp,
        } = fx;
        drop(lock);
        let crate::run::Started::Guarded(guard) =
            crate::run::start(&collection, &clock, false).expect("the lock is free")
        else {
            unreachable!("a real run is guarded");
        };

        let report = apply(&collection, &plan, &guard, &clock, &SeqIds::new(1)).expect("works");

        assert_eq!(report.id, minted());
        assert!(collection.root().join("projects/b/notes.txt").exists());
        assert_eq!(
            tracking::read(&collection, report.id)
                .expect("reads")
                .map(|entry| entry.state),
            Some(EntryState::Live)
        );
    }

    #[test]
    fn what_the_start_of_the_run_found_is_part_of_the_report() {
        let fx = unearthed();
        let plan = planned(&fx, None).expect("planned");

        let report = apply_with(
            &fx.collection,
            &plan,
            &fx.lock,
            &fx.clock,
            &SeqIds::new(1),
            &RealSteps,
            vec![Warning::new("found something")],
        )
        .expect("the fork runs");

        let messages: Vec<&str> = report
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert_eq!(messages, ["found something"]);
    }

    #[test]
    fn a_mode_that_cannot_be_put_back_becomes_a_warning() {
        let fx = unearthed();
        let mut recorder = Recorder::new(None);
        recorder.mode_problem = true;

        let report = fork_with(&fx, Some("b"), &recorder).expect("the fork runs");

        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0]
                .message
                .starts_with("the mode of `projects/b` could not be put back"),
            "{}",
            report.warnings[0].message
        );
    }

    // ---------------------------------------------------------------------
    // The plan
    // ---------------------------------------------------------------------

    #[test]
    fn the_plan_names_the_fork_and_writes_nothing() {
        let fx = unearthed();
        let before = snapshot(&fx);

        let plan = planned(&fx, Some("b")).expect("planned");

        assert_eq!(
            serde_json::to_value(plan.action()).expect("serialises"),
            json!({
                "action": "fork",
                "from": ID,
                "path": "projects/b",
                "old_path": "projects/a",
            })
        );
        assert_eq!(
            plan.action().describe(),
            "fork projects/a and rename it to projects/b"
        );
        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn a_plan_without_a_name_describes_the_fork_alone() {
        let fx = unearthed();

        let plan = planned(&fx, None).expect("planned");

        assert_eq!(plan.action().describe(), "fork projects/a");
    }

    // ---------------------------------------------------------------------
    // What is refused
    // ---------------------------------------------------------------------

    #[test]
    fn an_object_without_metadata_is_refused() {
        let fx = unearthed();

        let error = plan(
            &fx.collection,
            &ForkRequest {
                id: id(OTHER_ID),
                name: None,
            },
        )
        .expect_err("refused");

        assert!(matches!(error, ForkError::NoMetadata { .. }), "{error}");
    }

    #[test]
    fn a_live_piece_without_an_archive_is_refused() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| document["payloads"] = json!([]));
        assert_eq!(state_of(&fx, id(ID)), PieceState::Live);

        let error = refusal(&fx, None);

        assert_eq!(why(error), Refusal::NotUnearthed);
    }

    #[test]
    fn an_archived_piece_is_refused_with_the_advice_to_unearth_it() {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, FILES);
        assert_eq!(state_of(&fx, id(ID)), PieceState::Archived);

        assert_eq!(why(refusal(&fx, None)), Refusal::NoFolder);
    }

    #[test]
    fn a_remote_only_piece_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, FILES);
        tracking::remove(&fx.collection, &fx.lock, id(ID)).expect("no entry");
        assert_eq!(state_of(&fx, id(ID)), PieceState::RemoteOnly);

        assert_eq!(why(refusal(&fx, None)), Refusal::NoFolder);
    }

    #[test]
    fn a_gone_piece_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, FILES);
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("live"));
        assert_eq!(state_of(&fx, id(ID)), PieceState::Gone);

        assert_eq!(why(refusal(&fx, None)), Refusal::NoFolder);
    }

    #[test]
    fn another_collections_live_piece_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, FILES);
        tracking::remove(&fx.collection, &fx.lock, id(ID)).expect("no entry");
        fx.edit_meta(ID, |document| {
            document["lifecycle"] = json!("live");
            document["collection"] = json!(OTHER_ID);
        });
        assert_eq!(state_of(&fx, id(ID)), PieceState::LiveOnAnotherMachine);

        assert_eq!(why(refusal(&fx, None)), Refusal::AnotherCollectionsPiece);
    }

    #[test]
    fn an_entry_that_does_not_say_live_is_refused_with_its_state() {
        let fx = unearthed();
        let mut entry = entry_of(&fx, id(ID));
        entry.state = EntryState::Unearthing;
        fx.write_entry(&mut entry);

        assert_eq!(
            why(refusal(&fx, None)),
            Refusal::Entry(EntryState::Unearthing)
        );
    }

    #[test]
    fn an_unearthed_piece_of_another_collection_is_refused_and_nothing_changes() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| document["collection"] = json!(OTHER_ID));
        assert_eq!(state_of(&fx, id(ID)), PieceState::Unearthed);
        let before = snapshot(&fx);

        let error = fork(&fx, Some("b")).expect_err("refused");

        assert_eq!(why(error), Refusal::OtherCollection);
        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn the_folder_of_a_finished_fork_belongs_to_the_new_id() {
        let fx = unearthed();
        let report = forked(&fx, None);

        let error = refusal(&fx, None);

        assert_eq!(why(error), Refusal::ClaimedBy(report.id));
    }

    #[test]
    fn a_new_folder_next_to_an_archived_object_is_a_new_piece() {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, FILES);
        fs::create_dir_all(fx.at(PIECE)).expect("the folder is created");

        let error = refusal(&fx, None);

        assert_eq!(why(error), Refusal::NewFolder);
    }

    #[test]
    fn the_refusal_of_a_claimed_folder_names_the_claimant() {
        let fx = unearthed();
        let report = forked(&fx, None);

        let error = refusal(&fx, None);

        assert_eq!(
            error.to_string(),
            format!(
                "cannot fork `{PIECE}`: the folder belongs to {} now",
                report.id
            )
        );
    }

    #[test]
    fn an_unreadable_newer_version_refuses_before_anything_is_renamed() {
        let fx = unearthed();
        fs::write(fx.collection.object_dir(id(ID)).join("meta.2"), "garbage").expect("written");
        let before = snapshot(&fx);

        let error = fork(&fx, Some("b")).expect_err("refused");

        assert!(
            matches!(
                &error,
                ForkError::Store(StoreError::UnreadableHighest { number: 2 })
            ),
            "{error}"
        );
        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(snapshot(&fx), before);
        assert!(fx.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn a_newer_format_refuses_with_its_own_kind() {
        let fx = unearthed();
        fs::write(
            fx.collection.object_dir(id(ID)).join("meta.2"),
            r#"{"format": 2}"#,
        )
        .expect("written");

        let error = refusal(&fx, None);

        assert!(
            matches!(
                &error,
                ForkError::Store(StoreError::Meta(MetaError::FormatTooNew { found: 2 }))
            ),
            "{error}"
        );
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
    }

    #[test]
    fn every_refusal_says_what_to_do() {
        let texts: Vec<String> = [
            Refusal::OtherCollection,
            Refusal::NotUnearthed,
            Refusal::NewFolder,
            Refusal::ClaimedBy(id(OTHER_ID)),
            Refusal::NoFolder,
            Refusal::AnotherCollectionsPiece,
            Refusal::Entry(EntryState::Unearthing),
            Refusal::Entry(EntryState::Removing),
            Refusal::Entry(EntryState::Gone),
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            texts,
            [
                "the object belongs to another collection",
                "it is not unearthed: nothing was archived from it, so there is nothing to fork from",
                "the folder is a new piece next to an archived object; `archive` or `sync` gives it an ID",
                format!("the folder belongs to {} now", id(OTHER_ID)).as_str(),
                "there is no folder; `unearth` brings it back",
                "it is another collection's live piece; `unearth` makes the fork",
                "its tracking entry says `unearthing`, not `live`",
                "its tracking entry says `removing`, not `live`",
                "its tracking entry says `gone`, not `live`",
            ]
        );
    }

    #[test]
    fn an_object_that_is_live_here_without_an_entry_is_an_error_of_that_piece() {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, FILES);
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("live"));
        tracking::remove(&fx.collection, &fx.lock, id(ID)).expect("no entry");

        let error = refusal(&fx, None);

        assert!(matches!(error, ForkError::Tracking(_)), "{error}");
    }

    #[test]
    fn metadata_of_a_newer_format_is_refused_with_its_own_kind() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| document["format"] = json!(2));

        let error = fork(&fx, Some("b")).expect_err("refused");

        assert!(
            matches!(
                &error,
                ForkError::Store(StoreError::Meta(MetaError::FormatTooNew { found: 2 }))
            ),
            "{error}"
        );
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
        assert!(fx.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn a_recorded_change_does_not_hide_a_committed_version_of_a_newer_format() {
        let fx = unearthed();
        let committed = fx.collection.object_dir(id(ID)).join("meta.1");
        let mut newer: serde_json::Value =
            serde_json::from_slice(&fs::read(&committed).expect("reads")).expect("json");
        fs::copy(&committed, fx.collection.meta_pending_path(id(ID))).expect("a recorded change");
        newer["format"] = json!(2);
        fs::write(
            fx.collection.object_dir(id(ID)).join("meta.2"),
            serde_json::to_vec(&newer).expect("json"),
        )
        .expect("written");

        let error = fork(&fx, Some("b")).expect_err("refused");

        assert!(
            matches!(
                &error,
                ForkError::Store(StoreError::Meta(MetaError::FormatTooNew { found: 2 }))
            ),
            "{error}"
        );
        assert!(fx.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn the_error_texts_name_the_piece() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| document["payloads"] = json!([]));

        let error = refusal(&fx, None);

        assert_eq!(
            error.to_string(),
            "cannot fork `projects/a`: it is not unearthed: nothing was archived from it, so there is nothing to fork from"
        );
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_pattern_that_does_not_parse_is_an_error_when_a_name_is_given() {
        let mut fx = Fixture::configured(|config| config.collection.patterns = vec!["[".into()]);
        fx.archived(ID, PIECE, FILES);
        fx.unearthed(ID, PIECE);

        let error = refusal(&fx, Some("b"));

        assert!(matches!(error, ForkError::Select(_)), "{error}");
    }

    #[test]
    fn a_damaged_tracking_entry_is_an_error() {
        let fx = unearthed();
        fs::write(fx.collection.tracking_path(id(ID)), "not json").expect("written");

        let error = refusal(&fx, None);

        assert!(matches!(error, ForkError::Tracking(_)), "{error}");
    }

    #[test]
    fn a_recorded_change_that_cannot_be_read_is_an_error() {
        let fx = unearthed();
        fs::write(fx.collection.meta_pending_path(id(ID)), "not json").expect("written");

        let error = refusal(&fx, None);

        assert!(matches!(error, ForkError::Store(_)), "{error}");
    }

    // ---------------------------------------------------------------------
    // Minting
    // ---------------------------------------------------------------------

    #[test]
    fn an_id_that_has_an_object_directory_is_not_minted() {
        let fx = unearthed();
        fs::create_dir_all(fx.collection.object_dir(minted())).expect("a directory");
        let before = snapshot(&fx);

        let error = fork(&fx, Some("b")).expect_err("refused");

        assert!(
            matches!(error, ForkError::IdTaken { id } if id == minted()),
            "{error}"
        );
        assert_eq!(snapshot(&fx), before);
        assert!(fx.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn an_id_that_has_a_tracking_entry_is_not_minted() {
        let fx = unearthed();
        fs::write(fx.collection.tracking_path(minted()), "{}").expect("written");

        let error = fork(&fx, None).expect_err("refused");

        assert!(matches!(error, ForkError::IdTaken { .. }), "{error}");
    }

    #[test]
    fn a_tracking_directory_that_cannot_be_looked_into_is_an_error() {
        not_root();
        let fx = unearthed();
        let tracking_dir = fx.collection.tracking_dir();
        fs::set_permissions(&tracking_dir, fs::Permissions::from_mode(0o000)).expect("locked");

        let result = mint(&SeqIds::new(1), &fx.collection, fork_time());

        fs::set_permissions(&tracking_dir, fs::Permissions::from_mode(0o755)).expect("unlocked");
        assert!(matches!(result, Err(ForkError::Io { .. })), "{result:?}");
    }

    #[test]
    fn the_minted_id_carries_the_time_it_was_asked_for() {
        let fx = unearthed();

        let minted = mint(&SeqIds::new(7), &fx.collection, fork_time()).expect("minted");

        assert_eq!(
            minted.timestamp_ms(),
            u64::try_from(fork_time().as_millisecond()).expect("after 1970")
        );
    }

    // ---------------------------------------------------------------------
    // The metadata of the two IDs
    // ---------------------------------------------------------------------

    #[test]
    fn the_old_metadata_of_a_fork_is_archived_with_a_history_entry() {
        let fx = unearthed();
        let before = current(&fx, id(ID)).metadata;

        let changed = forked_old(&before, "laptop", fork_time());

        assert_eq!(changed.lifecycle, Lifecycle::Archived);
        assert_eq!(changed.history.len(), before.history.len() + 1);
        let last = changed.history.last().expect("a history");
        assert_eq!(last.action, "fork");
        assert_eq!(last.machine, "laptop");
        assert_eq!(last.time, fork_time());
    }

    #[test]
    fn the_new_metadata_of_a_fork_has_no_payload_and_the_source_fork() {
        let changed = forked_new(
            id(OTHER_ID),
            id(COLLECTION_ID),
            &path("projects/b"),
            "laptop",
            fork_time(),
        );

        assert_eq!(changed.id, id(OTHER_ID));
        assert_eq!(changed.collection, id(COLLECTION_ID));
        assert_eq!(changed.path, path("projects/b"));
        assert_eq!(changed.name.as_bytes(), b"b");
        assert_eq!(changed.created, fork_time());
        assert_eq!(changed.created_source, CreatedSource::Fork);
        assert_eq!(changed.lifecycle, Lifecycle::Live);
        assert!(changed.payloads.is_empty());
        assert_eq!(changed.history.len(), 1);
        assert_eq!(changed.history[0].machine, "laptop");
    }

    // ---------------------------------------------------------------------
    // --name
    // ---------------------------------------------------------------------

    #[test]
    fn a_name_renames_the_folder_and_only_the_new_id_follows() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".to_string()];
        });
        fx.archived(ID, "projects/team/a", FILES);
        fx.unearthed(ID, "projects/team/a");
        fx.clock.advance(LATER);

        let report = forked(&fx, Some("b"));

        assert!(!fx.at("projects/team/a").exists());
        assert!(fx.at("projects/team/b/notes.txt").exists());
        let new = current(&fx, report.id).metadata;
        assert_eq!(new.path, path("projects/team/b"));
        assert_eq!(new.name.as_bytes(), b"b");
        assert_eq!(entry_of(&fx, report.id).path, path("projects/team/b"));
        assert_eq!(current(&fx, id(ID)).metadata.path, path("projects/team/a"));
        assert_eq!(entry_of(&fx, id(ID)).path, path("projects/team/a"));
    }

    #[test]
    fn the_name_the_folder_has_is_a_plain_fork() {
        let fx = unearthed();

        let report = forked(&fx, Some("a"));

        assert_eq!(report.path, path(PIECE));
        assert!(fx.at("projects/a/notes.txt").exists());
        assert_eq!(entry_of(&fx, report.id).path, path(PIECE));
    }

    #[test]
    fn a_name_that_a_folder_has_is_taken() {
        let mut fx = unearthed();
        fx.tree().file("projects/b/x.txt", "x");
        let before = snapshot(&fx);

        let error = refusal(&fx, Some("b"));

        assert_eq!(problem(error), TargetProblem::Taken);
        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn a_name_that_a_file_has_is_taken() {
        let mut fx = unearthed();
        fx.tree().file("projects/b", "x");

        assert_eq!(problem(refusal(&fx, Some("b"))), TargetProblem::Taken);
    }

    #[test]
    fn a_name_that_a_dangling_symlink_has_is_taken() {
        let fx = unearthed();
        symlink("nowhere", fx.at("projects/b")).expect("a symlink");

        assert_eq!(problem(refusal(&fx, Some("b"))), TargetProblem::Taken);
    }

    #[test]
    fn a_name_that_another_piece_holds_is_taken() {
        let fx = unearthed();
        fx.hold(OTHER_ID, "projects/b", EntryState::Live);

        assert_eq!(problem(refusal(&fx, Some("b"))), TargetProblem::Taken);
    }

    #[test]
    fn a_name_an_archived_object_is_recorded_at_does_not_refuse() {
        let mut fx = Fixture::new();
        fx.archived(OTHER_ID, "projects/b", FILES);
        fx.archived(ID, PIECE, FILES);
        fx.unearthed(ID, PIECE);
        fx.clock.advance(LATER);

        let report = forked(&fx, Some("b"));

        assert_eq!(report.path, path("projects/b"));
        assert!(fx.at("projects/b/notes.txt").exists());
    }

    macro_rules! invalid_name {
        ($test:ident, $bytes:expr) => {
            #[test]
            fn $test() {
                let fx = unearthed();
                let before = snapshot(&fx);

                let error = plan(
                    &fx.collection,
                    &ForkRequest {
                        id: id(ID),
                        name: Some(OsString::from_vec($bytes.to_vec())),
                    },
                )
                .expect_err("refused");

                assert!(matches!(error, ForkError::InvalidName { .. }), "{error}");
                assert_eq!(error.kind(), ErrorKind::Usage);
                assert_eq!(snapshot(&fx), before);
            }
        };
    }

    invalid_name!(an_empty_name_is_a_usage_error, b"");
    invalid_name!(a_dot_is_a_usage_error, b".");
    invalid_name!(two_dots_are_a_usage_error, b"..");
    invalid_name!(a_name_with_a_slash_is_a_usage_error, b"a/b");
    invalid_name!(a_name_with_a_nul_is_a_usage_error, b"a\0b");

    #[test]
    fn an_invalid_name_says_what_is_wrong_with_it() {
        let fx = unearthed();

        let error = refusal(&fx, Some("a/b"));

        assert_eq!(
            error.to_string(),
            "`a/b` is not a name for a piece: a name cannot be `/` inside"
        );
    }

    #[test]
    fn a_name_cannot_put_the_piece_into_the_antiq_directory() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["*".to_string()];
        });
        fx.archived(ID, "a", FILES);
        fx.unearthed(ID, "a");

        assert_eq!(
            problem(refusal(&fx, Some(".antiq"))),
            TargetProblem::InsideAntiq
        );
        assert_eq!(
            problem(refusal(&fx, Some(".ANTIQ"))),
            TargetProblem::InsideAntiq
        );
    }

    #[test]
    fn a_name_the_patterns_do_not_select_is_refused() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/[a-m]*".to_string()];
        });
        fx.archived(ID, PIECE, FILES);
        fx.unearthed(ID, PIECE);

        assert_eq!(
            problem(refusal(&fx, Some("zed"))),
            TargetProblem::NotSelected
        );
    }

    #[test]
    fn a_name_on_the_exclude_list_is_refused() {
        let mut fx = Fixture::configured(|config| {
            config.collection.exclude = vec![path("projects/b")];
        });
        fx.archived(ID, PIECE, FILES);
        fx.unearthed(ID, PIECE);

        assert_eq!(
            problem(refusal(&fx, Some("b"))),
            TargetProblem::Excluded {
                at: path("projects/b")
            }
        );
    }

    #[test]
    fn a_name_inside_another_piece_is_refused() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".to_string()];
        });
        fx.archived(ID, "projects/team/a", FILES);
        fx.unearthed(ID, "projects/team/a");
        fx.hold(OTHER_ID, "projects/team", EntryState::Live);

        assert_eq!(
            problem(refusal(&fx, Some("b"))),
            TargetProblem::InsidePiece {
                piece: path("projects/team")
            }
        );
    }

    #[test]
    fn a_name_past_the_name_limit_fails_with_nothing_written() {
        let fx = unearthed();
        let before = snapshot(&fx);

        let error = fork(&fx, Some(&"x".repeat(300))).expect_err("the rename fails");

        assert!(matches!(error, ForkError::Fsx(_)), "{error}");
        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn a_parent_that_cannot_be_written_to_fails_with_nothing_written() {
        not_root();
        let fx = unearthed();
        let parent = fx.at("projects");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).expect("locked");
        let before = snapshot(&fx);

        let result = fork(&fx, Some("b"));

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("unlocked");
        assert!(matches!(result, Err(ForkError::Fsx(_))), "{result:?}");
        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn a_parent_that_cannot_be_listed_is_an_error() {
        not_root();
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".to_string()];
        });
        fx.archived(ID, "projects/team/a", FILES);
        fx.unearthed(ID, "projects/team/a");
        let projects = fx.at("projects");
        fs::set_permissions(&projects, fs::Permissions::from_mode(0o311)).expect("locked");

        let result = planned(&fx, Some("b"));

        fs::set_permissions(&projects, fs::Permissions::from_mode(0o755)).expect("unlocked");
        assert!(
            matches!(&result, Err(ForkError::Io { action: "list", .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_target_that_appears_after_the_plan_is_refused_and_nothing_is_written() {
        let fx = unearthed();
        let plan = planned(&fx, Some("b")).expect("planned");
        fs::create_dir(fx.at("projects/b")).expect("the target appears");
        let before = snapshot(&fx);

        let result = apply_with(
            &fx.collection,
            &plan,
            &fx.lock,
            &fx.clock,
            &SeqIds::new(1),
            &RealSteps,
            Vec::new(),
        );

        assert_eq!(problem(result.expect_err("refused")), TargetProblem::Taken);
        assert_eq!(snapshot(&fx), before);
    }

    // ---------------------------------------------------------------------
    // Unearthing the old ID afterwards
    // ---------------------------------------------------------------------

    #[test]
    fn the_old_id_cannot_be_unearthed_to_the_taken_path() {
        let fx = unearthed();
        forked(&fx, None);

        let error = unearth::plan(&fx.collection, &fx.request(ID)).expect_err("refused");
        assert!(
            matches!(
                &error,
                UnearthError::Target {
                    problem: TargetProblem::Taken,
                    ..
                }
            ),
            "{error}"
        );

        let elsewhere = UnearthRequest {
            to: Some(path("projects/elsewhere")),
            ..fx.request(ID)
        };
        let planned = unearth::plan(&fx.collection, &elsewhere).expect("planned");
        assert!(matches!(planned, unearth::Planned::Unearth(_)));
    }

    // ---------------------------------------------------------------------
    // A crash between two steps, and the next run
    // ---------------------------------------------------------------------

    #[test]
    fn without_a_name_a_failure_writing_the_old_metadata_changes_nothing() {
        let fx = unearthed();
        let before = snapshot(&fx);

        crash_at(&fx, None, Step::OldMetadata);

        assert_eq!(snapshot(&fx), before);
        assert_eq!(state_of(&fx, id(ID)), PieceState::Unearthed);
    }

    #[test]
    fn without_a_name_a_crash_before_the_old_entry_is_finished_by_a_second_run() {
        let fx = unearthed();
        crash_at(&fx, None, Step::OldEntry);
        assert_eq!(state_of(&fx, id(ID)), PieceState::Unearthed);
        let entry = entry_of(&fx, id(ID));
        let metadata = current(&fx, id(ID)).metadata;

        next_run(&fx);

        assert_eq!(entry_of(&fx, id(ID)), entry);
        assert_eq!(current(&fx, id(ID)).metadata, metadata);
        fx.clock.advance(SignedDuration::from_secs(60));
        let report = forked(&fx, None);
        assert_eq!(
            current(&fx, report.id).metadata.created,
            fork_time(),
            "the second run keeps the time of the first"
        );
        let forks = current(&fx, id(ID))
            .metadata
            .history
            .iter()
            .filter(|entry| entry.action == "fork")
            .count();
        assert_eq!(forks, 1);
        assert_eq!(entry_of(&fx, id(ID)).state, EntryState::Gone);
    }

    #[test]
    fn a_second_run_with_a_name_finishes_a_fork_that_stopped_before_the_old_entry() {
        let fx = unearthed();
        crash_at(&fx, None, Step::OldEntry);
        fx.clock.advance(SignedDuration::from_secs(60));

        let report = forked(&fx, Some("b"));

        let forks = current(&fx, id(ID))
            .metadata
            .history
            .iter()
            .filter(|entry| entry.action == "fork")
            .count();
        assert_eq!(forks, 1);
        assert_eq!(report.path, path("projects/b"));
        assert_eq!(current(&fx, report.id).metadata.created, fork_time());
    }

    #[test]
    fn a_fork_entry_of_another_machine_is_not_a_fork_of_this_run() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| {
            document["lifecycle"] = json!("archived");
            document["history"].as_array_mut().expect("history").push(
                json!({"time": "2026-11-04T11:00:00Z", "machine": "laptop", "action": "fork"}),
            );
        });

        forked(&fx, None);

        let history = current(&fx, id(ID)).metadata.history;
        let forks: Vec<(&str, Timestamp)> = history
            .iter()
            .filter(|entry| entry.action == "fork")
            .map(|entry| (entry.machine.as_str(), entry.time))
            .collect();
        assert_eq!(
            forks,
            [
                ("laptop", time("2026-11-04T11:00:00Z")),
                ("imperius", fork_time())
            ]
        );
    }

    #[test]
    fn an_archived_object_whose_last_change_is_not_a_fork_gets_the_fork_recorded() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("archived"));
        assert_eq!(state_of(&fx, id(ID)), PieceState::Unearthed);

        forked(&fx, None);

        let history = current(&fx, id(ID)).metadata.history;
        let last = history.last().expect("a history");
        assert_eq!((last.action.as_str(), last.time), ("fork", fork_time()));
    }

    #[test]
    fn a_fork_entry_of_a_live_object_is_not_a_fork_in_progress() {
        let fx = unearthed();
        fx.edit_meta(ID, |document| {
            document["history"].as_array_mut().expect("history").push(
                json!({"time": "2026-11-04T11:00:00Z", "machine": "imperius", "action": "fork"}),
            );
        });

        forked(&fx, None);

        let history = current(&fx, id(ID)).metadata.history;
        let last = history.last().expect("a history");
        assert_eq!((last.action.as_str(), last.time), ("fork", fork_time()));
        assert_eq!(current(&fx, id(ID)).metadata.lifecycle, Lifecycle::Archived);
    }

    #[test]
    fn without_a_name_a_crash_before_the_new_metadata_leaves_a_folder_next_to_an_archived_object() {
        let fx = unearthed();
        crash_at(&fx, None, Step::NewMetadata);
        assert_eq!(state_of(&fx, id(ID)), PieceState::Archived);

        next_run(&fx);

        assert_eq!(state_of(&fx, id(ID)), PieceState::Archived);
        assert!(fx.at("projects/a/notes.txt").exists());
        assert_eq!(why(refusal(&fx, None)), Refusal::NewFolder);
    }

    #[test]
    fn without_a_name_a_crash_before_the_new_entry_is_repaired_without_the_unearthed_time() {
        let fx = unearthed();
        crash_at(&fx, None, Step::NewEntry);
        assert!(
            tracking::read(&fx.collection, minted())
                .expect("reads")
                .is_none()
        );

        next_run(&fx);

        let entry = entry_of(&fx, minted());
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path(PIECE));
        assert_eq!(entry.unearthed, None);
        assert_eq!(state_of(&fx, minted()), PieceState::Live);
    }

    #[test]
    fn without_a_name_a_finished_fork_is_left_alone_by_the_next_run() {
        let fx = unearthed();
        forked(&fx, None);
        let before = snapshot(&fx);

        next_run(&fx);

        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn with_a_name_a_failing_rename_changes_nothing() {
        let fx = unearthed();
        let before = snapshot(&fx);

        crash_at(&fx, Some("b"), Step::Rename);

        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn with_a_name_a_crash_after_the_rename_leaves_a_live_entry_without_a_folder() {
        let fx = unearthed();
        crash_at(&fx, Some("b"), Step::OldMetadata);
        assert_eq!(state_of(&fx, id(ID)), PieceState::Gone);

        next_run(&fx);

        assert_eq!(state_of(&fx, id(ID)), PieceState::Gone);
        assert!(fx.at("projects/b/notes.txt").exists());
        let restore = unearth::plan(&fx.collection, &fx.request(ID)).expect("planned");
        match restore {
            unearth::Planned::Unearth(plan) => assert_eq!(plan.target(), &path(PIECE)),
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn with_a_name_a_crash_before_the_old_entry_leaves_the_old_id_archived() {
        let fx = unearthed();
        crash_at(&fx, Some("b"), Step::OldEntry);
        assert_eq!(state_of(&fx, id(ID)), PieceState::Archived);

        next_run(&fx);

        assert_eq!(state_of(&fx, id(ID)), PieceState::Archived);
        assert_eq!(why(refusal(&fx, Some("c"))), Refusal::NoFolder);
        assert!(fx.at("projects/b/notes.txt").exists());
    }

    #[test]
    fn with_a_name_a_crash_before_the_new_metadata_leaves_the_old_id_archived() {
        let fx = unearthed();
        crash_at(&fx, Some("b"), Step::NewMetadata);

        next_run(&fx);

        assert_eq!(state_of(&fx, id(ID)), PieceState::Archived);
        assert_eq!(entry_of(&fx, id(ID)).state, EntryState::Gone);
        assert_eq!(why(refusal(&fx, Some("c"))), Refusal::NoFolder);
    }

    #[test]
    fn with_a_name_a_crash_before_the_new_entry_is_repaired_at_the_new_path() {
        let fx = unearthed();
        crash_at(&fx, Some("b"), Step::NewEntry);

        next_run(&fx);

        let entry = entry_of(&fx, minted());
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path("projects/b"));
        assert_eq!(entry.unearthed, None);
    }

    #[test]
    fn with_a_name_a_finished_fork_is_left_alone_by_the_next_run() {
        let fx = unearthed();
        forked(&fx, Some("b"));
        let before = snapshot(&fx);

        next_run(&fx);

        assert_eq!(snapshot(&fx), before);
    }

    #[test]
    fn the_collection_still_reads_after_a_fork() {
        let fx = unearthed();
        let report = forked(&fx, Some("b"));

        let index = crate::index::Index::compute(&fx.collection).expect("the index reads");

        assert_eq!(index.at_path(&path("projects/b")).claimant, Some(report.id));
        assert_eq!(index.at_path(&path(PIECE)).claimant, None);
        assert_eq!(index.at_path(&path(PIECE)).others, [id(ID)]);
    }
}
