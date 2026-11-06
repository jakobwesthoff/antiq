// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `unearth` operation.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::commands::restore::{
    Brought, Hooks, Placement, RealHooks, RestoreError, Skipped, Source, Step, TargetProblem,
    bring_back_to, check_destination, check_same_volume, choose_version, measured_tmp,
};
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::tree::TreeRoot;
use crate::markers::{self, RESTORED_FILE, Restored};
use crate::plan::{HeldDryRun, Plan, PlanAction, Step as PlanStep, serialize_repairs};
use crate::progress::{Observer, Operation, Outcome as Progress, Silent, Tracker};
use crate::relpath::RelPath;
use crate::repair::{Destination, Repair};
use crate::report::{CommandReport, Warning, setting_notes};
use crate::run::{self, RunError, Started};
use crate::select::{Rules, SelectError};
use crate::versions::VersionsError;
use crate::versions::damage::Places;
use crate::versions::stamp::Stamp;
use crate::versions::store;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;
use std::ffi::OsString;
use std::path::PathBuf;

/// Why unearth failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum UnearthError {
    #[error("{0}")]
    Run(
        #[from]
        #[diagnostic_source]
        RunError,
    ),
    #[error("{0}")]
    Restore(
        #[from]
        #[diagnostic_source]
        RestoreError,
    ),
    #[error("cannot read the patterns of the collection: {0}")]
    Select(
        #[from]
        #[diagnostic_source]
        SelectError,
    ),
    #[error("cannot open {path}: {source}")]
    Open {
        path: String,
        source: std::io::Error,
    },
}

impl HasKind for UnearthError {
    fn kind(&self) -> ErrorKind {
        match self {
            UnearthError::Run(error) => error.kind(),
            UnearthError::Restore(error) => error.kind(),
            UnearthError::Select(_) | UnearthError::Open { .. } => ErrorKind::Other,
        }
    }
}

/// What to unearth.
#[derive(Debug, Clone)]
pub struct UnearthRequest {
    pub cwd: PathBuf,
    pub piece: OsString,
    pub version: Option<String>,
    pub dry_run: bool,
}

/// The checks a run makes, in order. The member names are checked
/// against the volume during the extraction; a dry run does not run that
/// probe.
pub const CHECKS: [&str; 5] = [
    "version",
    "destination",
    "free_space",
    "member_names",
    "extracted_tree",
];

/// What a dry run would do for the unearth itself.
#[derive(Debug, Serialize)]
pub struct UnearthAction {
    pub kind: &'static str,
    pub piece: String,
    pub stamp: String,
    pub version: Timestamp,
    pub path: String,
    pub bytes: u64,
    pub checks: Vec<&'static str>,
    /// Set when the version would not fit: nothing would be unearthed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkippedNote>,
    /// The complete versions of the piece, which tell the local labels of
    /// the console apart.
    #[serde(skip)]
    pub siblings: Vec<Stamp>,
}

impl PlanAction for UnearthAction {
    fn describe(&self) -> String {
        format!("unearth {} (version {})", self.piece, self.stamp)
    }
}

/// A dry run: the repairs of the start of a run, then the unearth.
#[derive(Debug, Serialize)]
pub struct DryRunPlan {
    pub dry_run: bool,
    pub actions: Vec<PlanStep<UnearthAction>>,
    pub warnings: Vec<Warning>,
}

impl DryRunPlan {
    /// Whether the run would skip the version for want of room.
    pub fn skips(&self) -> bool {
        self.actions
            .iter()
            .any(|step| matches!(step, PlanStep::Command(action) if action.skipped.is_some()))
    }
}

/// Why no folder came back although nothing is wrong with the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedNote {
    pub reason: &'static str,
    pub needed: u64,
    pub available: u64,
}

/// What an unearth did.
#[derive(Debug, Serialize)]
pub struct UnearthReport {
    pub unearthed: bool,
    pub piece: String,
    pub stamp: String,
    pub version: Timestamp,
    pub path: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkippedNote>,
    #[serde(serialize_with = "serialize_repairs")]
    pub repairs: Vec<Repair>,
    pub warnings: Vec<Warning>,
    /// Set when the folder is in place but the version or the marker could
    /// not be dealt with.
    #[serde(skip)]
    pub unfinished: bool,
    /// The complete versions of the piece before the run.
    #[serde(skip)]
    pub siblings: Vec<Stamp>,
}

impl CommandReport for UnearthReport {
    fn partial(&self) -> bool {
        self.skipped.is_some() || self.unfinished
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

#[derive(Debug)]
pub enum Outcome {
    Held(HeldDryRun),
    DryRun(DryRunPlan),
    Done(UnearthReport),
}

/// Brings a version of a piece back as a folder and deletes the version.
///
/// A real run repairs what an earlier run left, then extracts, verifies and
/// moves the version into place, and only then deletes it from the store
/// and rewrites the marker to the time alone. A failure before the rename
/// is an error. After the rename the folder is in place: a failure to
/// delete the version or to rewrite the marker is a warning of the report,
/// which is then partial, and the next run's repairs finish the unearth.
pub fn unearth(
    collection: &Collection,
    request: &UnearthRequest,
    clock: &dyn Clock,
    zone: &TimeZone,
) -> Result<Outcome, UnearthError> {
    unearth_with(collection, request, clock, zone, &RealHooks)
}

/// [`unearth`], telling `observer` what a real run does. A dry run has
/// nothing to tell.
pub fn unearth_observed(
    collection: &Collection,
    request: &UnearthRequest,
    clock: &dyn Clock,
    zone: &TimeZone,
    observer: &dyn Observer,
) -> Result<Outcome, UnearthError> {
    unearth_watched(collection, request, clock, zone, &RealHooks, Some(observer))
}

pub(crate) fn unearth_with(
    collection: &Collection,
    request: &UnearthRequest,
    clock: &dyn Clock,
    zone: &TimeZone,
    hooks: &dyn Hooks,
) -> Result<Outcome, UnearthError> {
    unearth_watched(collection, request, clock, zone, hooks, None)
}

fn unearth_watched(
    collection: &Collection,
    request: &UnearthRequest,
    clock: &dyn Clock,
    zone: &TimeZone,
    hooks: &dyn Hooks,
    observer: Option<&dyn Observer>,
) -> Result<Outcome, UnearthError> {
    let started = run::start(collection, clock, request.dry_run)?;
    if let Started::DryRun {
        holder: Some(holder),
        ..
    } = &started
    {
        return Ok(Outcome::Held(HeldDryRun::new(holder)));
    }

    let root = TreeRoot::open(collection.root()).map_err(|source| UnearthError::Open {
        path: collection.root().display().to_string(),
        source,
    })?;
    let config = &collection.config().collection;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    let pieces_dir = collection.pieces_dir();
    let tmp_pack_dir = collection.tmp_dir().join("pack");
    let places = Places {
        pieces_dir: &pieces_dir,
        tmp_pack_dir: &tmp_pack_dir,
    };
    let run = Run {
        collection,
        request,
        zone,
        places: &places,
        rules: &rules,
        root: &root,
        observer,
    };
    match started {
        Started::DryRun {
            repairs, warnings, ..
        } => run.dry_run(repairs, warnings, hooks),
        Started::Guarded(guard) => run.real(&guard, clock, hooks),
    }
}

struct Run<'a> {
    collection: &'a Collection,
    request: &'a UnearthRequest,
    zone: &'a TimeZone,
    places: &'a Places<'a>,
    rules: &'a Rules,
    root: &'a TreeRoot,
    observer: Option<&'a dyn Observer>,
}

/// Whether a repair of the start of a run concerns the piece the person
/// named, by its path.
pub(crate) fn concerns(repair: &Repair, piece: &str) -> bool {
    match repair {
        Repair::DeleteSidecars { piece: at, .. }
        | Repair::DeleteRestoredVersion { piece: at, .. }
        | Repair::RemoveEmptyFolder { folder: at } => at.to_string() == piece,
        _ => false,
    }
}

/// Whether `repair` gives a tree back to the live path `piece`.
pub(crate) fn gives_back_to(repair: &Repair, piece: &RelPath) -> bool {
    matches!(
        repair,
        Repair::GiveBack { tree, destination: Destination::Live, .. } if tree == piece
    )
}

/// The stamps of the complete versions of `piece`; none when the store
/// cannot be read, which only costs the console its disambiguation.
pub(crate) fn complete_stamps(places: &Places, piece: &RelPath) -> Vec<Stamp> {
    store::scan(places.pieces_dir)
        .ok()
        .and_then(|scan| scan.pieces.into_iter().find(|stored| &stored.path == piece))
        .map(|stored| {
            let mut stamps: Vec<Stamp> = stored
                .versions
                .iter()
                .filter(|version| !version.is_incomplete())
                .map(|version| version.stamp)
                .collect();
            stamps.sort();
            stamps
        })
        .unwrap_or_default()
}

impl Run<'_> {
    fn choose(&self) -> Result<crate::commands::restore::Chosen, RestoreError> {
        choose_version(
            self.collection,
            self.places,
            &self.request.cwd,
            &self.request.piece,
            self.request.version.as_deref(),
            self.zone,
        )
    }

    fn siblings(&self, piece: &RelPath) -> Vec<Stamp> {
        complete_stamps(self.places, piece)
    }

    /// What a run would start with and do, without changing anything. A
    /// refusal that a planned repair concerns is a warning, as the repair
    /// may remove its cause. The checks that write into `.antiq/tmp/` are
    /// not made.
    fn dry_run(
        &self,
        repairs: Vec<Repair>,
        planned_warnings: Vec<Warning>,
        hooks: &dyn Hooks,
    ) -> Result<Outcome, UnearthError> {
        let mut warnings = setting_notes(self.collection);
        warnings.extend(planned_warnings);
        let mut plan = Plan::after_repairs(repairs.clone());
        let concerned = |piece: &str| repairs.iter().any(|repair| concerns(repair, piece));
        let unjudged = |error: &RestoreError| {
            Warning::new(format!("cannot be judged before the repairs run: {error}"))
        };

        let given = self.request.piece.to_string_lossy().into_owned();
        let chosen = match self.choose() {
            Ok(chosen) => chosen,
            Err(error) if concerned(&given) => {
                warnings.push(unjudged(&error));
                return Ok(Outcome::DryRun(dry_plan(plan, warnings)));
            }
            Err(error) => return Err(error.into()),
        };
        let needed = chosen.listing.bytes;
        let available = hooks
            .available_space(&self.collection.measured_tmp_dir())
            .map_err(RestoreError::from)?;
        // The real run measures the room before it looks at the destination,
        // so a dry run that finds no room does not look at it either.
        if available >= needed {
            match check_destination(self.root, self.rules, &chosen.piece) {
                Ok(missing) => {
                    // A tree that repair 4 gives back to the path is there
                    // when the unearth starts.
                    if repairs
                        .iter()
                        .any(|repair| gives_back_to(repair, &chosen.piece))
                    {
                        return Err(RestoreError::Target {
                            path: chosen.piece.clone(),
                            problem: TargetProblem::Taken,
                        }
                        .into());
                    }
                    let staging = measured_tmp(self.collection);
                    check_same_volume(self.root, hooks, &staging, &chosen.piece, &missing)?;
                }
                Err(error) if concerned(&chosen.piece.to_string()) => {
                    warnings.push(unjudged(&error))
                }
                Err(error) => return Err(error.into()),
            }
        }
        plan.push_command(UnearthAction {
            kind: "unearth",
            piece: chosen.piece.to_string(),
            stamp: chosen.stamp.to_string(),
            version: chosen.stamp.time(),
            path: chosen.piece.to_string(),
            bytes: chosen.listing.bytes,
            checks: CHECKS.to_vec(),
            skipped: (available < needed).then_some(SkippedNote {
                reason: "no_space",
                needed,
                available,
            }),
            siblings: self.siblings(&chosen.piece),
        });
        Ok(Outcome::DryRun(dry_plan(plan, warnings)))
    }

    fn real(
        &self,
        guard: &run::RunGuard,
        clock: &dyn Clock,
        hooks: &dyn Hooks,
    ) -> Result<Outcome, UnearthError> {
        let mut warnings = setting_notes(self.collection);
        warnings.extend(guard.warnings().iter().cloned());

        let chosen = self.choose()?;
        let name = chosen.piece.to_string();
        let tracker = Tracker::new(self.observer.unwrap_or(&Silent), &name);
        let bytes = chosen.listing.bytes;
        let siblings: Vec<String> = self
            .siblings(&chosen.piece)
            .iter()
            .map(Stamp::to_string)
            .collect();
        tracker.started(Some(bytes), Some(&chosen.stamp.to_string()), &siblings);
        let brought = bring_back_to(
            self.collection,
            self.places,
            self.rules,
            &Source::sound(&chosen),
            &Placement {
                destination: &chosen.piece,
                marker_version: Some(chosen.stamp),
                tracker,
            },
            clock.now(),
            hooks,
        )?;
        let mut report = UnearthReport {
            unearthed: false,
            piece: chosen.piece.to_string(),
            stamp: chosen.stamp.to_string(),
            version: chosen.stamp.time(),
            path: chosen.piece.to_string(),
            bytes: chosen.listing.bytes,
            skipped: None,
            repairs: guard.repaired().to_vec(),
            warnings: Vec::new(),
            unfinished: false,
            siblings: self.siblings(&chosen.piece),
        };
        match brought {
            Brought::Skipped {
                reason: Skipped::NoSpace { needed, available },
                warnings: more,
            } => {
                report.skipped = Some(SkippedNote {
                    reason: "no_space",
                    needed: needed.expect("a sound version has a listing"),
                    available,
                });
                warnings.extend(more.into_iter().map(Warning::new));
                tracker.finished(
                    bytes,
                    &Progress::NoRoom {
                        needed: Some(needed.expect("a sound version has a listing")),
                        available,
                    },
                );
            }
            Brought::Done {
                time,
                warnings: more,
                failure,
                ..
            } => {
                report.unearthed = true;
                warnings.extend(more.into_iter().map(Warning::new));
                // The version is the only durable copy until the folder is
                // known to be, so a failure after the rename ends the run
                // here and the repairs of the next one finish it.
                let unfinished = match failure {
                    Some(error) => Some(in_place(
                        &chosen.piece,
                        format!("it could not be made durable: {error}"),
                    )),
                    None => self.finish(&chosen, time, hooks, tracker)?,
                };
                let outcome = match &unfinished {
                    Some(warning) => Progress::Unfinished(warning.message.clone()),
                    None => Progress::Files(chosen.listing.files),
                };
                tracker.finished(bytes, &outcome);
                if let Some(warning) = unfinished {
                    report.unfinished = true;
                    warnings.push(warning);
                }
            }
        }
        report.warnings = warnings;
        Ok(Outcome::Done(report))
    }

    /// Deletes the version and rewrites the marker to the time alone. A
    /// failure of either leaves the folder in place and is returned as the
    /// warning to tell; the repairs of the next run finish what is left.
    fn finish(
        &self,
        chosen: &crate::commands::restore::Chosen,
        time: Timestamp,
        hooks: &dyn Hooks,
        tracker: Tracker<'_>,
    ) -> Result<Option<Warning>, UnearthError> {
        let piece = &chosen.piece;
        tracker.operation(Operation::Removing);
        if let Err(error) = self.delete_version(chosen) {
            return Ok(Some(in_place(
                piece,
                format!("its version {} could not be deleted: {error}", chosen.stamp),
            )));
        }
        hooks.reached(Step::VersionDeleted)?;

        let bare = Restored {
            time,
            version: None,
        }
        .render();
        if let Err(error) = markers::put(
            self.root,
            piece,
            RESTORED_FILE,
            bare.as_bytes(),
            hooks.site(),
        ) {
            return Ok(Some(in_place(
                piece,
                format!("its version is deleted and the marker could not be rewritten: {error}"),
            )));
        }
        hooks.reached(Step::MarkerRewritten)?;
        Ok(None)
    }

    fn delete_version(
        &self,
        chosen: &crate::commands::restore::Chosen,
    ) -> Result<(), VersionsError> {
        let pieces =
            TreeRoot::open(self.places.pieces_dir).map_err(|source| VersionsError::Io {
                action: "open",
                path: self.places.pieces_dir.display().to_string(),
                source,
            })?;
        // The folder was there when the version was chosen. One removed
        // since then is a failure to delete, which the report tells.
        let folder =
            store::lookup_exact(&pieces, &chosen.piece)?.ok_or_else(|| VersionsError::Io {
                action: "find the store folder of",
                path: chosen.piece.to_string(),
                source: std::io::ErrorKind::NotFound.into(),
            })?;
        store::delete_version(&pieces, &folder, &chosen.version)
    }
}

/// The warning for a folder that is in place while the run could not finish
/// the unearth.
fn in_place(piece: &RelPath, what: String) -> Warning {
    Warning::marked(
        piece,
        format!(" is in place, but {what}; the next run finishes it"),
    )
}

fn dry_plan(plan: Plan<PlanStep<UnearthAction>>, warnings: Vec<Warning>) -> DryRunPlan {
    DryRunPlan {
        dry_run: true,
        actions: plan.actions,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::commands::restore::TargetProblem;
    use crate::error::Error;
    use crate::filesystem::FilesystemError;
    use crate::filesystem::atomic::{Durable, RealDurable};
    use crate::lock;
    use crate::markers::Site;
    use crate::repair;
    use crate::testing::tree::{TempCollection, Tree, snapshot};
    use crate::testing::versions::Store;
    use jiff::SignedDuration;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io;
    use std::path::Path;

    const OLD: &str = "2026-11-03T081500";
    const NEW: &str = "2026-11-04T090211";

    type Callback = Box<dyn Fn() -> Result<(), RestoreError>>;

    /// Hooks that let a test stop the run at a step, run code there and
    /// make the second marker write fail.
    #[derive(Default)]
    struct Probe {
        callbacks: RefCell<Vec<(Step, Callback)>>,
        space: Cell<Option<u64>>,
        fail_marker_from: Cell<Option<usize>>,
        marker_writes: Cell<usize>,
        other_volume: Cell<bool>,
        site: ProbeSite,
    }

    #[derive(Default)]
    struct ProbeSite {
        fail_rename: Cell<bool>,
    }

    impl Site for ProbeSite {
        fn durable(&self) -> &dyn Durable {
            &RealDurable
        }
        fn before_rename(&self) -> io::Result<()> {
            if self.fail_rename.get() {
                Err(io::Error::from_raw_os_error(5))
            } else {
                Ok(())
            }
        }
    }

    impl Probe {
        fn on(&self, step: Step, run: impl Fn() -> Result<(), RestoreError> + 'static) {
            self.callbacks.borrow_mut().push((step, Box::new(run)));
        }

        fn stop_at(&self, step: Step) {
            self.on(step, || {
                Err(RestoreError::Io {
                    action: "stop",
                    path: String::new(),
                    source: io::Error::from_raw_os_error(5),
                })
            });
        }

        /// Makes the run die at `step`: the panic unwinds past the cleanup
        /// of a failed run, so the staging and the folders made so far stay
        /// as a crash leaves them.
        fn crash_at(&self, step: Step) {
            self.on(step, move || panic!("the process dies at {step:?}"));
        }
    }

    /// Runs and expects the run to die at a step the probe crashes at.
    fn crashed(world: &World, probe: &Probe) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.run(probe)));
        assert!(outcome.is_err(), "the run reached the crash");
    }

    impl Hooks for Probe {
        fn reached(&self, step: Step) -> Result<(), RestoreError> {
            for (at, run) in self.callbacks.borrow().iter() {
                if *at == step {
                    run()?;
                }
            }
            Ok(())
        }
        fn available_space(&self, path: &Path) -> Result<u64, FilesystemError> {
            match self.space.get() {
                Some(space) => Ok(space),
                None => crate::filesystem::space::available_space(path),
            }
        }
        fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
            if self.other_volume.get() {
                Ok(u64::from(!path.to_string().starts_with(".antiq")))
            } else {
                Ok(root.lstat(path)?.device)
            }
        }
        fn site(&self) -> &dyn Site {
            self.site.fail_rename.set(
                self.fail_marker_from
                    .get()
                    .is_some_and(|from| self.marker_writes.get() >= from),
            );
            self.marker_writes.set(self.marker_writes.get() + 1);
            &self.site
        }
    }

    fn source() -> Tree {
        let mut tree = Tree::new();
        tree.file("projects/scanner/main.rs", "fn main() {}");
        tree.file("projects/scanner/src/lib.rs", "pub fn f() {}");
        tree.file("tools/other/run.sh", "echo");
        tree
    }

    fn copy_all(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("creates");
        for item in fs::read_dir(from).expect("lists") {
            let item = item.expect("item");
            let target = to.join(item.file_name());
            if item.file_type().expect("type").is_dir() {
                copy_all(&item.path(), &target);
            } else {
                fs::copy(item.path(), &target).expect("copies");
            }
        }
    }

    struct World {
        temp: TempCollection,
        clock: std::sync::Arc<FixedClock>,
        _store: Store,
    }

    impl World {
        fn new(stamps: &[&str]) -> World {
            let mut temp = TempCollection::minimal();
            temp.tree().file(
                ".antiq/config.toml",
                format!(
                    "[collection]\nid = \"{}\"\npatterns = [\"projects/*\", \"tools/*\"]\n",
                    crate::testing::tree::COLLECTION_ID
                ),
            );
            let source = source();
            let mut store = Store::new();
            for stamp in stamps {
                store.add_version(&source, "projects/scanner", stamp);
            }
            copy_all(&store.pieces_dir(), &temp.root().join(".antiq/pieces"));
            World {
                temp,
                clock: std::sync::Arc::new(FixedClock::new(
                    "2026-11-05T10:00:00Z".parse().expect("time"),
                )),
                _store: store,
            }
        }

        fn root(&self) -> &Path {
            self.temp.root()
        }

        fn collection(&self) -> Collection {
            Collection::find(self.root()).expect("collection")
        }

        fn request(&self, dry_run: bool) -> UnearthRequest {
            UnearthRequest {
                cwd: self.root().to_path_buf(),
                piece: OsString::from("projects/scanner"),
                version: None,
                dry_run,
            }
        }

        fn run_with(
            &self,
            request: &UnearthRequest,
            hooks: &dyn Hooks,
        ) -> Result<Outcome, UnearthError> {
            unearth_with(
                &self.collection(),
                request,
                &*self.clock,
                &TimeZone::UTC,
                hooks,
            )
        }

        fn run(&self, hooks: &dyn Hooks) -> Result<Outcome, UnearthError> {
            self.run_with(&self.request(false), hooks)
        }

        fn run_observed(&self, hooks: &dyn Hooks, observer: &dyn Observer) -> Outcome {
            unearth_watched(
                &self.collection(),
                &self.request(false),
                &*self.clock,
                &TimeZone::UTC,
                hooks,
                Some(observer),
            )
            .expect("the run ends")
        }

        fn store_files(&self, folder: &str) -> Vec<String> {
            let mut names: Vec<String> =
                fs::read_dir(self.root().join(".antiq/pieces").join(folder))
                    .map(|items| {
                        items
                            .map(|item| {
                                item.expect("item")
                                    .file_name()
                                    .to_string_lossy()
                                    .into_owned()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            names.sort();
            names
        }

        fn marker(&self) -> String {
            fs::read_to_string(self.root().join("projects/scanner/.antiq-restored"))
                .expect("the marker exists")
        }

        /// The folder and the store as a comparison between runs sees them.
        fn state(&self) -> Vec<(String, String)> {
            let mut seen = Vec::new();
            for area in ["projects", ".antiq/pieces"] {
                for (name, entry) in snapshot(&self.root().join(area)) {
                    seen.push((format!("{area}/{name}"), format!("{entry:?}")));
                }
            }
            seen
        }
    }

    /// Runs again right after a run ended, trying again while the lock the
    /// run released still reads as held.
    fn run_after_release(world: &World, request: &UnearthRequest) -> Result<Outcome, UnearthError> {
        lock::retry_while_held(
            |result| {
                matches!(
                    result,
                    Err(UnearthError::Run(RunError::Lock(_))) | Ok(Outcome::Held(_))
                )
            },
            || world.run_with(request, &Probe::default()),
        )
    }

    fn rerun_after_release(world: &World) -> Result<Outcome, UnearthError> {
        run_after_release(world, &world.request(false))
    }

    /// Takes the lock as the repairs of a next run would, once the run
    /// before has let go of it.
    fn lock_after_release(world: &World) -> lock::RunLock {
        let collection = world.collection();
        lock::retry_while_held(
            |result: &Result<lock::RunLock, lock::LockError>| result.is_err(),
            || lock::acquire(&collection.lock_path(), "test", &*world.clock),
        )
        .expect("the lock is free after the run")
    }

    fn report(outcome: Result<Outcome, UnearthError>) -> UnearthReport {
        match outcome.expect("the unearth runs") {
            Outcome::Done(report) => report,
            other => panic!("expected a report, got {other:?}"),
        }
    }

    #[test]
    fn the_version_comes_back_and_is_deleted_and_the_older_one_stays() {
        let world = World::new(&[OLD, NEW]);
        let report = report(world.run(&Probe::default()));
        assert!(report.unearthed);
        assert_eq!(report.piece, "projects/scanner");
        assert_eq!(report.stamp, NEW);
        assert_eq!(report.version.to_string(), "2026-11-04T09:02:11Z");
        assert_eq!(report.path, "projects/scanner");
        assert!(report.bytes > 0);
        assert!(report.skipped.is_none());
        assert!(!report.partial());
        assert!(world.root().join("projects/scanner/main.rs").exists());
        let files = world.store_files("projects/scanner");
        assert!(files.iter().all(|name| name.contains(OLD)), "{files:?}");
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn the_marker_holds_the_time_alone_at_the_end() {
        let world = World::new(&[NEW]);
        report(world.run(&Probe::default()));
        assert_eq!(world.marker(), "restored 2026-11-05T10:00:00Z\n");
    }

    #[test]
    fn the_last_version_takes_the_store_folder_and_its_parents_with_it() {
        let world = World::new(&[NEW]);
        report(world.run(&Probe::default()));
        assert!(!world.root().join(".antiq/pieces/projects").exists());
    }

    #[test]
    fn a_second_unearth_of_the_piece_finds_no_version() {
        let world = World::new(&[NEW]);
        report(world.run(&Probe::default()));
        let error = rerun_after_release(&world).expect_err("no version is left");
        assert!(
            matches!(error, UnearthError::Restore(RestoreError::Reference(_))),
            "{error:?}"
        );
        assert_eq!(Error::from(error).kind(), ErrorKind::UnknownReference);
    }

    #[test]
    fn the_version_is_deleted_only_after_the_folder_is_in_place_and_whole() {
        let world = World::new(&[NEW]);
        let root = world.root().to_path_buf();
        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let probe = Probe::default();
        for step in [
            Step::ParentFlushed,
            Step::VersionDeleted,
            Step::MarkerRewritten,
        ] {
            let (root, seen) = (root.clone(), seen.clone());
            probe.on(step, move || {
                let store = fs::read_dir(root.join(".antiq/pieces/projects/scanner"))
                    .map(|items| items.count())
                    .unwrap_or(0);
                let marker = fs::read_to_string(root.join("projects/scanner/.antiq-restored"))
                    .unwrap_or_default();
                seen.borrow_mut().push(format!(
                    "{step:?}: store files {store}, whole {}, marker {}",
                    root.join("projects/scanner/src/lib.rs").exists(),
                    marker.trim()
                ));
                Ok(())
            });
        }
        report(world.run(&probe));
        assert_eq!(
            *seen.borrow(),
            [
                format!(
                    "ParentFlushed: store files 3, whole true, marker restored 2026-11-05T10:00:00Z version {NEW}"
                ),
                format!(
                    "VersionDeleted: store files 0, whole true, marker restored 2026-11-05T10:00:00Z version {NEW}"
                ),
                "MarkerRewritten: store files 0, whole true, marker restored 2026-11-05T10:00:00Z"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn the_marker_rewrite_keeps_the_time_the_marker_was_written_with() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        let clock = world.clock.clone();
        probe.on(Step::Renamed, move || {
            clock.advance(SignedDuration::from_secs(90));
            Ok(())
        });
        report(world.run(&probe));
        assert_eq!(world.marker(), "restored 2026-11-05T10:00:00Z\n");
    }

    #[test]
    fn a_failure_after_the_rename_is_a_report_and_the_repairs_finish_it() {
        let clean = World::new(&[NEW]);
        report(clean.run(&Probe::default()));
        for step in [Step::Renamed, Step::ParentFlushed] {
            let world = World::new(&[NEW]);
            let probe = Probe::default();
            probe.stop_at(step);
            let report = report(world.run(&probe));
            assert!(report.unearthed && report.partial(), "{step:?}");
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.message.contains("is in place")
                        && w.message.contains("the next run finishes it")),
                "{step:?}: {:?}",
                report.warnings
            );
            assert_eq!(world.store_files("projects/scanner").len(), 3, "{step:?}");
            let collection = world.collection();
            let lock = lock_after_release(&world);
            repair::run(&collection, &lock);
            drop(lock);
            assert_eq!(world.state(), clean.state(), "{step:?}");
        }
    }

    #[test]
    fn a_crash_at_each_step_after_the_rename_is_finished_by_the_repairs() {
        let clean = World::new(&[NEW]);
        report(clean.run(&Probe::default()));
        for step in [Step::Renamed, Step::ParentFlushed, Step::VersionDeleted] {
            let world = World::new(&[NEW]);
            let probe = Probe::default();
            probe.crash_at(step);
            crashed(&world, &probe);
            assert!(
                world.root().join("projects/scanner/main.rs").exists(),
                "{step:?}"
            );
            let collection = world.collection();
            let lock = lock_after_release(&world);
            repair::run(&collection, &lock);
            drop(lock);
            assert_eq!(world.state(), clean.state(), "{step:?}");
            assert_eq!(world.marker(), clean.marker(), "{step:?}");
        }
    }

    #[test]
    fn a_crash_after_the_archive_is_gone_leaves_the_sidecars_to_the_repairs() {
        let clean = World::new(&[NEW]);
        report(clean.run(&Probe::default()));
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        probe.crash_at(Step::ParentFlushed);
        crashed(&world, &probe);
        let folder = world.root().join(".antiq/pieces/projects/scanner");
        for item in fs::read_dir(&folder).expect("lists") {
            let path = item.expect("item").path();
            if path.to_string_lossy().ends_with(".tar.zst") {
                fs::remove_file(path).expect("removes the archive");
            }
        }
        let collection = world.collection();
        let lock = lock_after_release(&world);
        repair::run(&collection, &lock);
        drop(lock);
        assert_eq!(world.state(), clean.state());
    }

    #[test]
    fn a_crash_before_the_rename_leaves_the_version_and_the_next_unearth_repairs_tmp() {
        for step in [
            Step::Extracted,
            Step::Verified,
            Step::MarkerWritten,
            Step::Flushed,
        ] {
            let world = World::new(&[NEW]);
            let probe = Probe::default();
            probe.crash_at(step);
            crashed(&world, &probe);
            assert!(!world.root().join("projects").exists(), "{step:?}");
            assert!(
                world.root().join(".antiq/tmp/extract/1").exists(),
                "{step:?}: a crash leaves the staging"
            );
            assert_eq!(world.store_files("projects/scanner").len(), 3, "{step:?}");
            let again = report(rerun_after_release(&world));
            assert!(again.unearthed, "{step:?}");
            assert!(
                again
                    .repairs
                    .iter()
                    .any(|repair| matches!(repair, Repair::DeleteTemporary { .. })),
                "{step:?}: {:?}",
                again.repairs
            );
            assert!(!world.root().join(".antiq/tmp/extract/1").exists());
            assert!(world.root().join("projects/scanner/main.rs").exists());
        }
    }

    #[test]
    fn a_crash_after_the_parents_are_made_leaves_them_empty_and_the_next_unearth_uses_them() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        probe.crash_at(Step::ParentsMade);
        crashed(&world, &probe);
        let parent = world.root().join("projects");
        assert_eq!(fs::read_dir(&parent).expect("the parent exists").count(), 0);
        assert!(!parent.join("scanner").exists());
        assert_eq!(world.store_files("projects/scanner").len(), 3);
        let again = report(rerun_after_release(&world));
        assert!(again.unearthed);
        assert!(parent.join("scanner/main.rs").exists());
    }

    #[test]
    fn a_failed_marker_rewrite_is_a_report_with_the_stamp_left_and_the_version_gone() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        // The first marker write is the one at the staged top.
        probe.fail_marker_from.set(Some(1));
        let report = report(world.run(&probe));
        assert!(report.unearthed);
        assert!(report.partial());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("is in place")),
            "{:?}",
            report.warnings
        );
        assert!(world.store_files("projects/scanner").is_empty());
        assert_eq!(
            world.marker(),
            format!("restored 2026-11-05T10:00:00Z version {NEW}\n")
        );
    }

    #[test]
    fn a_failed_deletion_is_a_report_and_the_version_stays() {
        let world = World::new(&[NEW]);
        let folder = world.root().join(".antiq/pieces/projects/scanner");
        let probe = Probe::default();
        let locked = folder.clone();
        probe.on(Step::ParentFlushed, move || {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("mode");
            Ok(())
        });
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        struct Unlock(PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let _unlock = Unlock(folder.clone());
        let report = report(world.run(&probe));
        assert!(report.unearthed && report.partial());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("the next run finishes")),
            "{:?}",
            report.warnings
        );
        assert!(world.root().join("projects/scanner/main.rs").exists());
    }

    #[test]
    fn a_store_folder_removed_during_the_run_is_a_warning_not_a_panic() {
        let world = World::new(&[NEW]);
        let folder = world.root().join(".antiq/pieces/projects");
        let probe = Probe::default();
        probe.on(Step::ParentFlushed, move || {
            fs::remove_dir_all(&folder).expect("removes the store folder");
            Ok(())
        });
        let report = report(world.run(&probe));
        assert!(report.unearthed && report.partial());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("could not be deleted")),
            "{:?}",
            report.warnings
        );
        assert!(world.root().join("projects/scanner/main.rs").exists());
    }

    #[test]
    fn no_room_is_a_report_and_nothing_changes() {
        let world = World::new(&[NEW]);
        let before = world.state();
        let probe = Probe::default();
        probe.space.set(Some(3));
        let report = report(world.run(&probe));
        assert!(!report.unearthed);
        assert!(report.partial());
        let skipped = report.skipped.expect("skipped");
        assert_eq!((skipped.reason, skipped.available), ("no_space", 3));
        assert!(skipped.needed > 3);
        assert_eq!(world.state(), before);
    }

    #[test]
    fn a_held_lock_is_an_error_with_the_holder() {
        let world = World::new(&[NEW]);
        let collection = world.collection();
        let _held = lock::acquire(&collection.lock_path(), "other", &*world.clock).expect("lock");
        let error = world.run(&Probe::default()).expect_err("the lock is held");
        let error = Error::from(error);
        assert_eq!(error.kind(), ErrorKind::LockHeld);
        let details = error.details();
        assert!(
            details.contains_key("pid") && details.contains_key("started"),
            "{details:?}"
        );
    }

    #[test]
    fn a_dry_run_with_the_lock_held_names_the_holder() {
        let world = World::new(&[NEW]);
        let collection = world.collection();
        let _held = lock::acquire(&collection.lock_path(), "other", &*world.clock).expect("lock");
        match world
            .run_with(&world.request(true), &Probe::default())
            .expect("dry run")
        {
            Outcome::Held(held) => assert!(held.lines()[0].starts_with("The lock is held by pid")),
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn a_dry_run_names_the_unearth_and_changes_nothing() {
        let world = World::new(&[OLD, NEW]);
        let before = world.state();
        let all_before = snapshot(world.root());
        match world
            .run_with(&world.request(true), &Probe::default())
            .expect("dry run")
        {
            Outcome::DryRun(plan) => {
                assert!(plan.dry_run);
                assert_eq!(plan.actions.len(), 1);
                let PlanStep::Command(action) = &plan.actions[0] else {
                    panic!("expected the unearth");
                };
                assert_eq!(action.kind, "unearth");
                assert_eq!(action.stamp, NEW);
                assert_eq!(action.path, "projects/scanner");
                assert_eq!(action.checks, CHECKS);
            }
            other => panic!("expected a plan, got {other:?}"),
        }
        assert_eq!(world.state(), before);
        assert_eq!(snapshot(world.root()), all_before);
    }

    #[test]
    fn a_dry_run_lists_the_repairs_first() {
        let world = World::new(&[OLD]);
        // A leftover version of a died unearth of another piece.
        fs::create_dir_all(world.root().join(".antiq/tmp/extract/9")).expect("leftover");
        match world
            .run_with(&world.request(true), &Probe::default())
            .expect("dry run")
        {
            Outcome::DryRun(plan) => {
                assert!(plan.actions.len() >= 2, "{:?}", plan.actions);
                assert!(matches!(plan.actions[0], PlanStep::Repair(_)));
                assert!(matches!(plan.actions.last(), Some(PlanStep::Command(_))));
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn a_dry_run_refuses_a_damaged_version_and_a_taken_destination() {
        let world = World::new(&[NEW]);
        let archive = fs::read_dir(world.root().join(".antiq/pieces/projects/scanner"))
            .expect("lists")
            .map(|item| item.expect("item").path())
            .find(|path| path.to_string_lossy().ends_with(".tar.zst"))
            .expect("the archive");
        let content = fs::read(&archive).expect("reads");
        fs::write(&archive, &content[..content.len() / 2]).expect("truncates");
        let error = world
            .run_with(&world.request(true), &Probe::default())
            .expect_err("refused");
        assert!(
            matches!(error, UnearthError::Restore(RestoreError::Damaged { .. })),
            "{error:?}"
        );

        let world = World::new(&[NEW]);
        fs::create_dir_all(world.root().join("projects/scanner")).expect("takes the place");
        let error = world
            .run_with(&world.request(true), &Probe::default())
            .expect_err("refused");
        match error {
            UnearthError::Restore(RestoreError::Target { problem, .. }) => {
                assert_eq!(problem, TargetProblem::Taken);
            }
            other => panic!("expected a target problem, got {other:?}"),
        }
    }

    fn dry_plan_of(world: &World, probe: &Probe) -> DryRunPlan {
        match world
            .run_with(&world.request(true), probe)
            .expect("dry run")
        {
            Outcome::DryRun(plan) => plan,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn a_dry_run_without_room_names_the_skip_and_changes_nothing() {
        let world = World::new(&[NEW]);
        let before = snapshot(world.root());
        let probe = Probe::default();
        probe.space.set(Some(3));
        let plan = dry_plan_of(&world, &probe);
        assert!(plan.skips());
        let PlanStep::Command(action) = &plan.actions[0] else {
            panic!("expected the unearth");
        };
        let skipped = action.skipped.as_ref().expect("skipped");
        assert_eq!((skipped.reason, skipped.available), ("no_space", 3));
        assert!(skipped.needed > 3);
        assert_eq!(snapshot(world.root()), before);

        let plan = dry_plan_of(&world, &Probe::default());
        assert!(!plan.skips());
    }

    #[test]
    fn a_dry_run_checks_the_room_before_the_destination_as_the_real_run_does() {
        let world = World::new(&[NEW]);
        fs::create_dir_all(world.root().join("projects/scanner")).expect("takes the place");
        let probe = Probe::default();
        probe.space.set(Some(3));

        let real = report(world.run(&probe));
        assert!(real.skipped.is_some(), "the real run skips for the room");

        let plan = dry_plan_of(&world, &probe);
        assert!(plan.skips(), "the dry run predicts it");
    }

    #[test]
    fn a_dry_run_refuses_a_destination_on_another_volume_than_tmp() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        probe.other_volume.set(true);
        let error = world
            .run_with(&world.request(true), &probe)
            .expect_err("refused");
        match error {
            UnearthError::Restore(RestoreError::Target { problem, .. }) => {
                assert_eq!(problem, TargetProblem::MountPoint);
            }
            other => panic!("expected a mount point, got {other:?}"),
        }
    }

    /// A tree of the piece in `trash/`, which repair 4 gives back to its
    /// live path, as the folder above it exists.
    fn put_piece_in_trash(world: &World) {
        fs::create_dir_all(world.root().join("projects")).expect("the parent");
        let trashed = world
            .root()
            .join(format!(".antiq/trash/{NEW}/projects/scanner"));
        fs::create_dir_all(&trashed).expect("trash");
        fs::write(trashed.join("main.rs"), "fn main() {}").expect("file");
    }

    #[test]
    fn a_tree_repair_4_gives_back_to_the_path_makes_the_unearth_refuse_taken() {
        let world = World::new(&[NEW]);
        put_piece_in_trash(&world);
        let error = world
            .run_with(&world.request(true), &Probe::default())
            .expect_err("the dry run refuses");
        assert!(
            matches!(
                &error,
                UnearthError::Restore(RestoreError::Target {
                    problem: TargetProblem::Taken,
                    ..
                })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_tree_repair_4_gives_back_to_the_path_makes_the_real_unearth_refuse_taken() {
        let world = World::new(&[NEW]);
        put_piece_in_trash(&world);
        let error = world.run(&Probe::default()).expect_err("the run refuses");
        assert!(
            matches!(
                &error,
                UnearthError::Restore(RestoreError::Target {
                    problem: TargetProblem::Taken,
                    ..
                })
            ),
            "{error:?}"
        );
        assert_eq!(world.store_files("projects/scanner").len(), 3);
    }

    #[test]
    fn a_damaged_version_is_refused_although_a_give_back_names_the_path() {
        let world = World::new(&[NEW]);
        put_piece_in_trash(&world);
        let archive = fs::read_dir(world.root().join(".antiq/pieces/projects/scanner"))
            .expect("lists")
            .map(|item| item.expect("item").path())
            .find(|path| path.to_string_lossy().ends_with(".tar.zst"))
            .expect("the archive");
        let content = fs::read(&archive).expect("reads");
        fs::write(&archive, &content[..content.len() / 2]).expect("truncates");
        let error = world
            .run_with(&world.request(true), &Probe::default())
            .expect_err("a give-back lifts no damage");
        assert!(
            matches!(error, UnearthError::Restore(RestoreError::Damaged { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_refusal_a_planned_repair_may_remove_is_a_warning_in_a_dry_run() {
        let world = World::new(&[NEW]);
        // A died unearth: the folder is in place, the version still there.
        let probe = Probe::default();
        probe.crash_at(Step::ParentFlushed);
        crashed(&world, &probe);
        match run_after_release(&world, &world.request(true)).expect("dry run") {
            Outcome::DryRun(plan) => {
                assert!(
                    plan.warnings.iter().any(|w| w
                        .message
                        .contains("cannot be judged before the repairs run")),
                    "{:?}",
                    plan.warnings
                );
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn a_choice_a_planned_repair_may_change_is_a_warning_and_names_no_action() {
        use crate::versions::names::{VersionFile, file_name};
        let world = World::new(&[NEW]);
        // A died archive: the sidecars are stored, the archive is still
        // staged, and repair 1 deletes the sidecars.
        let piece = RelPath::parse("projects/scanner").expect("path");
        let stamp: crate::versions::stamp::Stamp = NEW.parse().expect("stamp");
        let archive = file_name(&piece, &stamp, VersionFile::Archive);
        fs::remove_file(
            world
                .root()
                .join(".antiq/pieces/projects/scanner")
                .join(&archive),
        )
        .expect("removes the archive");
        let staged = world
            .root()
            .join(format!(".antiq/tmp/pack/{NEW}/projects/scanner"));
        fs::create_dir_all(&staged).expect("staging");
        fs::write(staged.join(&archive), "x").expect("staged archive");
        match world
            .run_with(&world.request(true), &Probe::default())
            .expect("dry run")
        {
            Outcome::DryRun(plan) => {
                assert!(
                    plan.warnings.iter().any(|w| w
                        .message
                        .contains("cannot be judged before the repairs run")),
                    "{:?}",
                    plan.warnings
                );
                assert!(
                    plan.actions
                        .iter()
                        .all(|a| matches!(a, PlanStep::Repair(_)))
                );
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn the_action_and_the_report_describe_themselves() {
        let action = UnearthAction {
            kind: "unearth",
            piece: "projects/scanner".to_string(),
            stamp: NEW.to_string(),
            version: Timestamp::UNIX_EPOCH,
            path: "projects/scanner".to_string(),
            bytes: 1,
            checks: CHECKS.to_vec(),
            skipped: None,
            siblings: Vec::new(),
        };
        assert_eq!(
            action.describe(),
            format!("unearth projects/scanner (version {NEW})")
        );
        let world = World::new(&[NEW]);
        let report = report(world.run(&Probe::default()));
        assert!(CommandReport::warnings(&report).is_empty());
    }

    // =========================================================
    // Progress events
    // =========================================================

    use crate::testing::recording::Recorder;

    #[test]
    fn an_unearth_reports_its_operations_with_counters_that_reach_their_totals() {
        let world = World::new(&[OLD]);
        let recorder = Recorder::new();

        world.run_observed(&Probe::default(), &recorder);

        assert_eq!(
            recorder.lines(),
            [
                "started projects/scanner 25 2026-11-03T081500",
                "projects/scanner Extracting 2/2",
                "projects/scanner Verifying 2/2",
                "projects/scanner Removing 0/0",
                "finished projects/scanner 25 Files(2)",
            ]
        );
    }

    #[test]
    fn an_unearth_without_room_ends_as_skipped_after_it_started() {
        let world = World::new(&[OLD]);
        let probe = Probe::default();
        probe.space.set(Some(3));
        let recorder = Recorder::new();

        world.run_observed(&probe, &recorder);

        assert_eq!(
            recorder.lines(),
            [
                "started projects/scanner 25 2026-11-03T081500",
                "finished projects/scanner 25 NoRoom { needed: Some(25), available: 3 }",
            ]
        );
    }

    #[test]
    fn a_folder_in_place_whose_marker_cannot_be_rewritten_ends_as_unfinished() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        probe.fail_marker_from.set(Some(1));
        let recorder = Recorder::new();

        world.run_observed(&probe, &recorder);

        let lines = recorder.lines();
        let last = lines.last().expect("events came");
        assert!(
            last.starts_with("finished projects/scanner 25 Unfinished(")
                && last.contains("is in place"),
            "{last}"
        );
    }

    #[test]
    fn an_unearth_that_fails_before_the_rename_ends_without_a_finish() {
        let world = World::new(&[OLD]);
        let probe = Probe::default();
        probe.stop_at(Step::Extracted);
        let recorder = Recorder::new();

        let result = unearth_watched(
            &world.collection(),
            &world.request(false),
            &*world.clock,
            &TimeZone::UTC,
            &probe,
            Some(&recorder),
        );

        assert!(result.is_err());
        let lines = recorder.lines();
        assert!(
            lines.iter().all(|line| !line.starts_with("finished")),
            "{lines:?}"
        );
    }
}
