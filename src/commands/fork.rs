// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fork` operation.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::commands::restore::{
    Brought, Hooks, Picked, Placement, RealHooks, RestoreError, Skipped, Source, TargetProblem,
    bring_back_to, check_destination, check_same_volume, choose_version_to_fork, measured_tmp,
};
use crate::commands::unearth::{complete_stamps, concerns, gives_back_to};
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::tree::TreeRoot;
use crate::plan::{HeldDryRun, Plan, PlanAction, Step as PlanStep, serialize_repairs};
use crate::reference::{self, ReferenceError};
use crate::relpath::RelPath;
use crate::repair::Repair;
use crate::report::{CommandReport, Warning, setting_notes};
use crate::run::{self, RunError, Started};
use crate::select::{Rules, SelectError};
use crate::versions::damage::Places;
use crate::versions::stamp::Stamp;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Why fork failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ForkError {
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
    #[error("cannot restore to `{path}`: it is taken; `--name` gives the copy another path")]
    Taken { path: RelPath },
}

impl HasKind for ForkError {
    fn kind(&self) -> ErrorKind {
        match self {
            ForkError::Run(error) => error.kind(),
            ForkError::Restore(error) => error.kind(),
            ForkError::Select(_) | ForkError::Open { .. } | ForkError::Taken { .. } => {
                ErrorKind::Other
            }
        }
    }
}

/// What to fork.
#[derive(Debug, Clone)]
pub struct ForkRequest {
    pub cwd: PathBuf,
    pub piece: OsString,
    pub version: Option<String>,
    /// The path of the copy; the piece's own path without it.
    pub name: Option<OsString>,
    pub dry_run: bool,
}

/// What a dry run would do for the fork itself.
#[derive(Debug, Serialize)]
pub struct ForkAction {
    pub kind: &'static str,
    pub piece: String,
    pub stamp: String,
    pub version: Timestamp,
    pub path: String,
    /// The unpacked size from the listing; none without a usable listing.
    pub bytes: Option<u64>,
    pub checks: Vec<&'static str>,
    /// What is wrong with the version; left out for a sound one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub damage: Vec<String>,
    /// Set when the version would not fit: nothing would be forked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkippedNote>,
    /// The complete versions of the piece, which tell the local labels of
    /// the console apart.
    #[serde(skip)]
    pub siblings: Vec<Stamp>,
}

impl PlanAction for ForkAction {
    fn describe(&self) -> String {
        format!(
            "fork {} (version {}) to {}",
            self.piece, self.stamp, self.path
        )
    }
}

/// A dry run: the repairs of the start of a run, then the fork.
#[derive(Debug, Serialize)]
pub struct DryRunPlan {
    pub dry_run: bool,
    pub actions: Vec<PlanStep<ForkAction>>,
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

/// Why no folder came about although nothing is wrong with the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedNote {
    pub reason: &'static str,
    /// None when the volume ran full during the extraction of a version
    /// whose size is not known.
    pub needed: Option<u64>,
    pub available: u64,
}

/// What a fork did.
#[derive(Debug, Serialize)]
pub struct ForkReport {
    pub forked: bool,
    pub piece: String,
    pub stamp: String,
    pub version: Timestamp,
    pub path: String,
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkippedNote>,
    #[serde(serialize_with = "serialize_repairs")]
    pub repairs: Vec<Repair>,
    pub warnings: Vec<Warning>,
    /// Set when the folder is in place but not known to be durable.
    #[serde(skip)]
    pub unfinished: bool,
    /// The complete versions of the piece before the run.
    #[serde(skip)]
    pub siblings: Vec<Stamp>,
}

impl CommandReport for ForkReport {
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
    Done(ForkReport),
}

/// Extracts a working copy of a version and leaves the version alone.
pub fn fork(
    collection: &Collection,
    request: &ForkRequest,
    clock: &dyn Clock,
    zone: &TimeZone,
) -> Result<Outcome, ForkError> {
    fork_with(collection, request, clock, zone, &RealHooks)
}

pub(crate) fn fork_with(
    collection: &Collection,
    request: &ForkRequest,
    clock: &dyn Clock,
    zone: &TimeZone,
    hooks: &dyn Hooks,
) -> Result<Outcome, ForkError> {
    let started = run::start(collection, clock, request.dry_run)?;
    if let Started::DryRun {
        holder: Some(holder),
        ..
    } = &started
    {
        return Ok(Outcome::Held(HeldDryRun::new(holder)));
    }

    let root = TreeRoot::open(collection.root()).map_err(|source| ForkError::Open {
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
    request: &'a ForkRequest,
    zone: &'a TimeZone,
    places: &'a Places<'a>,
    rules: &'a Rules,
    root: &'a TreeRoot,
}

impl Run<'_> {
    /// The path the copy goes to: the one `--name` gives, else the
    /// piece's own, which only the chosen version knows.
    fn named(&self) -> Result<Option<RelPath>, RestoreError> {
        let Some(name) = &self.request.name else {
            return Ok(None);
        };
        if name.is_empty() {
            return Err(RestoreError::Usage("the name is empty".to_string()));
        }
        match reference::resolve_path(self.collection, &self.request.cwd, Path::new(name)) {
            Ok(path) => Ok(Some(path)),
            Err(ReferenceError::Path(error)) => Err(RestoreError::Usage(format!(
                "`{}` is not a path: {error}",
                name.to_string_lossy()
            ))),
            Err(error) => Err(error.into()),
        }
    }

    fn choose(&self) -> Result<Picked, RestoreError> {
        choose_version_to_fork(
            self.collection,
            self.places,
            &self.request.cwd,
            &self.request.piece,
            self.request.version.as_deref(),
            self.zone,
        )
    }

    /// A refusal of the original path as taken tells where the way out is.
    fn refuse(&self, error: RestoreError) -> ForkError {
        match error {
            RestoreError::Target {
                path,
                problem: TargetProblem::Taken,
            } if self.request.name.is_none() => ForkError::Taken { path },
            other => other.into(),
        }
    }

    /// The warnings a version that is not sound or has no listing earns.
    fn notes(&self, picked: &Picked) -> Vec<Warning> {
        let mut notes = Vec::new();
        if !picked.damage.is_empty() {
            notes.push(Warning::new(format!(
                "version {} of {} is damaged: {}; it is extracted without the comparison against its listing",
                picked.stamp,
                picked.piece,
                picked.damage.join("; ")
            )));
        }
        if picked.listing.is_none() {
            notes.push(Warning::new(
                "the version has no listing that reads, so the free space is not checked"
                    .to_string(),
            ));
        }
        notes
    }

    /// What a dry run would start with and do, without changing anything.
    /// A refusal of the piece that a planned repair concerns is a warning,
    /// as the repair may remove its cause. The checks that write into
    /// `.antiq/tmp/` are not made.
    fn dry_run(
        &self,
        repairs: Vec<Repair>,
        planned_warnings: Vec<Warning>,
        hooks: &dyn Hooks,
    ) -> Result<Outcome, ForkError> {
        let mut warnings = setting_notes(self.collection);
        warnings.extend(planned_warnings);
        let mut plan = Plan::after_repairs(repairs.clone());
        let concerned = |piece: &str| repairs.iter().any(|repair| concerns(repair, piece));
        let unjudged = |error: &RestoreError| {
            Warning::new(format!("cannot be judged before the repairs run: {error}"))
        };

        let named = self.named()?;
        let given = self.request.piece.to_string_lossy().into_owned();
        let picked = match self.choose() {
            Ok(picked) => picked,
            Err(error) if concerned(&given) => {
                warnings.push(unjudged(&error));
                return Ok(Outcome::DryRun(dry_plan(plan, warnings)));
            }
            Err(error) => return Err(error.into()),
        };
        let destination = named.unwrap_or_else(|| picked.piece.clone());
        let bytes = picked.listing.as_ref().map(|listing| listing.bytes);
        let skipped = match bytes {
            Some(needed) => {
                let available = hooks
                    .available_space(&self.collection.measured_tmp_dir())
                    .map_err(RestoreError::from)?;
                (available < needed).then_some(SkippedNote {
                    reason: "no_space",
                    needed: Some(needed),
                    available,
                })
            }
            None => None,
        };
        // The real run measures the room before it looks at the destination,
        // so a dry run that finds no room does not look at it either.
        if skipped.is_none() {
            match check_destination(self.root, self.rules, &destination) {
                Ok(missing) => {
                    // A tree that repair 4 gives back to the path is there
                    // when the fork starts.
                    if repairs
                        .iter()
                        .any(|repair| gives_back_to(repair, &destination))
                    {
                        return Err(self.refuse(RestoreError::Target {
                            path: destination,
                            problem: TargetProblem::Taken,
                        }));
                    }
                    let staging = measured_tmp(self.collection);
                    check_same_volume(self.root, hooks, &staging, &destination, &missing)
                        .map_err(|error| self.refuse(error))?;
                }
                Err(error)
                    if self.request.name.is_none() && concerned(&destination.to_string()) =>
                {
                    warnings.push(unjudged(&error));
                }
                Err(error) => return Err(self.refuse(error)),
            }
        }
        warnings.extend(self.notes(&picked));

        let mut checks = vec!["version", "destination"];
        if bytes.is_some() {
            checks.push("free_space");
        }
        checks.push("member_names");
        if picked.damage.is_empty() {
            checks.push("extracted_tree");
        }
        plan.push_command(ForkAction {
            kind: "fork",
            piece: picked.piece.to_string(),
            stamp: picked.stamp.to_string(),
            version: picked.stamp.time(),
            path: destination.to_string(),
            bytes,
            checks,
            damage: picked.damage.clone(),
            skipped,
            siblings: complete_stamps(self.places, &picked.piece),
        });
        Ok(Outcome::DryRun(dry_plan(plan, warnings)))
    }

    fn real(
        &self,
        guard: &run::RunGuard,
        clock: &dyn Clock,
        hooks: &dyn Hooks,
    ) -> Result<Outcome, ForkError> {
        let mut warnings = setting_notes(self.collection);
        warnings.extend(guard.warnings().iter().cloned());

        let named = self.named()?;
        let picked = self.choose()?;
        let destination = named.unwrap_or_else(|| picked.piece.clone());
        warnings.extend(self.notes(&picked));
        let brought = bring_back_to(
            self.collection,
            self.places,
            self.rules,
            &Source::picked(&picked),
            &Placement {
                destination: &destination,
                marker_version: None,
            },
            clock.now(),
            hooks,
        )
        .map_err(|error| self.refuse(error))?;
        let mut report = ForkReport {
            forked: false,
            piece: picked.piece.to_string(),
            stamp: picked.stamp.to_string(),
            version: picked.stamp.time(),
            path: destination.to_string(),
            bytes: picked.listing.as_ref().map(|listing| listing.bytes),
            skipped: None,
            repairs: guard.repaired().to_vec(),
            warnings: Vec::new(),
            unfinished: false,
            siblings: complete_stamps(self.places, &picked.piece),
        };
        match brought {
            Brought::Skipped {
                reason: Skipped::NoSpace { needed, available },
                warnings: more,
            } => {
                report.skipped = Some(SkippedNote {
                    reason: "no_space",
                    needed,
                    available,
                });
                warnings.extend(more.into_iter().map(Warning::new));
            }
            Brought::Done {
                warnings: more,
                failure,
                ..
            } => {
                report.forked = true;
                warnings.extend(more.into_iter().map(Warning::new));
                if let Some(error) = failure {
                    report.unfinished = true;
                    warnings.push(Warning::new(format!(
                        "`{destination}` is in place, but it could not be made durable: {error}"
                    )));
                }
            }
        }
        report.warnings = warnings;
        Ok(Outcome::Done(report))
    }
}

fn dry_plan(plan: Plan<PlanStep<ForkAction>>, warnings: Vec<Warning>) -> DryRunPlan {
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
    use crate::commands::restore::Step;
    use crate::error::Error;
    use crate::filesystem::FilesystemError;
    use crate::lock;
    use crate::testing::tree::{TempCollection, Tree, snapshot};
    use crate::testing::versions::Store;
    use crate::versions::names::VersionFile;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io;

    const OLD: &str = "2026-11-03T081500";
    const NEW: &str = "2026-11-04T090211";

    type Callback = Box<dyn Fn() -> Result<(), RestoreError>>;

    /// Hooks that let a test stop the run at a step and set the free space.
    #[derive(Default)]
    struct Probe {
        callbacks: RefCell<Vec<(Step, Callback)>>,
        space: Cell<Option<u64>>,
    }

    impl Probe {
        fn stop_at(&self, step: Step) {
            self.callbacks.borrow_mut().push((
                step,
                Box::new(|| {
                    Err(RestoreError::Io {
                        action: "stop",
                        path: String::new(),
                        source: io::Error::from_raw_os_error(5),
                    })
                }),
            ));
        }
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
    }

    fn source() -> Tree {
        let mut tree = Tree::new();
        tree.file("projects/scanner/main.rs", "fn main() {}");
        tree.file("projects/scanner/src/lib.rs", "pub fn f() {}");
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
                    "[collection]\nid = \"{}\"\npatterns = [\"projects/*\", \"tools/*\", \"work/*/*\"]\n",
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

        fn request(&self, name: Option<&str>, dry_run: bool) -> ForkRequest {
            ForkRequest {
                cwd: self.root().to_path_buf(),
                piece: OsString::from("projects/scanner"),
                version: None,
                name: name.map(OsString::from),
                dry_run,
            }
        }

        fn run_with(&self, request: &ForkRequest, hooks: &dyn Hooks) -> Result<Outcome, ForkError> {
            fork_with(
                &self.collection(),
                request,
                &*self.clock,
                &TimeZone::UTC,
                hooks,
            )
        }

        fn fork(&self, name: Option<&str>) -> Result<Outcome, ForkError> {
            self.run_with(&self.request(name, false), &Probe::default())
        }

        fn plan(&self, name: Option<&str>) -> Result<Outcome, ForkError> {
            self.run_with(&self.request(name, true), &Probe::default())
        }

        fn store_files(&self) -> Vec<String> {
            let mut names: Vec<String> =
                fs::read_dir(self.root().join(".antiq/pieces/projects/scanner"))
                    .expect("the store folder")
                    .map(|item| {
                        item.expect("item")
                            .file_name()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
            names.sort();
            names
        }

        fn version_path(&self, kind: VersionFile) -> PathBuf {
            let name = self
                .store_files()
                .into_iter()
                .find(|name| match kind {
                    VersionFile::Archive => name.ends_with(".tar.zst"),
                    VersionFile::Listing => name.ends_with(".sha256.zst"),
                    VersionFile::Checksum => name.ends_with(".tar.zst.sha256"),
                })
                .expect("the file exists");
            self.root()
                .join(".antiq/pieces/projects/scanner")
                .join(name)
        }

        fn marker(&self, at: &str) -> String {
            fs::read_to_string(self.root().join(at).join(".antiq-restored"))
                .expect("the marker exists")
        }
    }

    fn report(outcome: Result<Outcome, ForkError>) -> ForkReport {
        match outcome.expect("the fork runs") {
            Outcome::Done(report) => report,
            other => panic!("expected a report, got {other:?}"),
        }
    }

    fn plan(outcome: Result<Outcome, ForkError>) -> DryRunPlan {
        match outcome.expect("the dry run runs") {
            Outcome::DryRun(plan) => plan,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    fn texts(report: &ForkReport) -> Vec<String> {
        report.warnings.iter().map(|w| w.message.clone()).collect()
    }

    // ----- the copy

    #[test]
    fn a_fork_to_the_original_path_keeps_the_version_and_writes_a_bare_marker() {
        let world = World::new(&[OLD, NEW]);
        let before = world.store_files();
        let report = report(world.fork(None));
        assert!(report.forked);
        assert!(!report.partial());
        assert_eq!(report.piece, "projects/scanner");
        assert_eq!(report.stamp, NEW);
        assert_eq!(report.path, "projects/scanner");
        assert!(report.bytes.is_some_and(|bytes| bytes > 0));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(world.root().join("projects/scanner/src/lib.rs").exists());
        let marker = world.marker("projects/scanner");
        assert!(marker.starts_with("restored "), "{marker}");
        assert!(!marker.contains("version"), "{marker}");
        assert_eq!(world.store_files(), before);
    }

    #[test]
    fn a_fork_with_a_name_leaves_the_original_path_free() {
        let world = World::new(&[NEW]);
        let report = report(world.fork(Some("tools/copy")));
        assert!(report.forked);
        assert_eq!(report.path, "tools/copy");
        assert_eq!(report.piece, "projects/scanner");
        assert!(world.root().join("tools/copy/main.rs").exists());
        assert!(!world.root().join("projects").exists());
        assert!(!world.marker("tools/copy").contains("version"));
    }

    #[test]
    fn a_name_with_folders_above_it_makes_them() {
        let world = World::new(&[NEW]);
        let report = report(world.fork(Some("work/a/copy")));
        assert!(report.forked);
        assert!(world.root().join("work/a/copy/main.rs").exists());
    }

    #[test]
    fn a_name_is_taken_from_the_working_directory() {
        let world = World::new(&[NEW]);
        fs::create_dir_all(world.root().join("tools")).expect("tools");
        let mut request = world.request(Some("copy"), false);
        request.cwd = world.root().join("tools");
        let report = report(world.run_with(&request, &Probe::default()));
        assert_eq!(report.path, "tools/copy");
    }

    #[test]
    fn a_second_fork_of_the_same_version_works() {
        let world = World::new(&[NEW]);
        report(world.fork(Some("tools/one")));
        let second = report(world.fork(Some("tools/two")));
        assert!(second.forked);
        assert!(world.root().join("tools/one/main.rs").exists());
        assert!(world.root().join("tools/two/main.rs").exists());
    }

    #[test]
    fn a_taken_original_path_names_the_name_option() {
        let world = World::new(&[NEW]);
        fs::create_dir_all(world.root().join("projects/scanner")).expect("takes the place");
        let error = world.fork(None).expect_err("the path is taken");
        assert!(matches!(&error, ForkError::Taken { .. }), "{error:?}");
        assert!(error.to_string().contains("`--name`"), "{error}");
    }

    #[test]
    fn a_taken_name_is_refused_without_the_hint() {
        let world = World::new(&[NEW]);
        fs::create_dir_all(world.root().join("tools/copy")).expect("takes the place");
        let error = world.fork(Some("tools/copy")).expect_err("taken");
        assert!(
            matches!(
                &error,
                ForkError::Restore(RestoreError::Target {
                    problem: TargetProblem::Taken,
                    ..
                })
            ),
            "{error:?}"
        );
        assert!(!error.to_string().contains("--name"), "{error}");
    }

    #[test]
    fn a_name_that_is_no_piece_of_the_patterns_is_refused() {
        let world = World::new(&[NEW]);
        let error = world.fork(Some("elsewhere/copy")).expect_err("refused");
        assert!(
            error.to_string().contains("patterns do not select"),
            "{error}"
        );
        assert!(!world.root().join("elsewhere").exists());
    }

    #[test]
    fn an_empty_name_and_a_name_that_is_no_path_are_usage_errors() {
        let world = World::new(&[NEW]);
        for name in ["", "tools/a\0b"] {
            let error = world.fork(Some(name)).expect_err("usage");
            assert_eq!(error.kind(), ErrorKind::Usage, "{name:?}: {error}");
        }
    }

    #[test]
    fn a_name_outside_the_collection_has_its_own_kind() {
        let world = World::new(&[NEW]);
        let error = world.fork(Some("/somewhere/else")).expect_err("outside");
        assert_eq!(error.kind(), ErrorKind::OutsideCollection, "{error}");
    }

    #[test]
    fn the_collection_root_is_no_place_for_a_copy() {
        let world = World::new(&[NEW]);
        let error = world.fork(Some(".")).expect_err("the root");
        assert!(error.to_string().contains("collection root"), "{error}");
    }

    // ----- damage

    #[test]
    fn a_damaged_version_is_forked_with_a_warning_and_without_the_comparison() {
        let world = World::new(&[NEW]);
        // The listing names a file the archive does not hold, so a
        // comparison would refuse the tree.
        let listing = world.version_path(VersionFile::Listing);
        let text =
            zstd::stream::decode_all(&fs::read(&listing).expect("reads")[..]).expect("decodes");
        let text = String::from_utf8(text)
            .expect("text")
            .replace("main.rs", "mine.rs");
        fs::write(
            &listing,
            zstd::stream::encode_all(text.as_bytes(), 3).expect("compresses"),
        )
        .expect("writes");
        fs::write(
            world.version_path(VersionFile::Checksum),
            "not a checksum line",
        )
        .expect("writes");

        let report = report(world.fork(Some("tools/copy")));

        assert!(report.forked && !report.partial());
        assert!(world.root().join("tools/copy/main.rs").exists());
        let warnings = texts(&report);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("is damaged"), "{warnings:?}");
        assert!(warnings[0].contains("checksum file"), "{warnings:?}");
        assert!(
            warnings[0].contains("without the comparison"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_version_without_a_listing_is_forked_and_the_space_is_not_checked() {
        let world = World::new(&[NEW]);
        fs::remove_file(world.version_path(VersionFile::Listing)).expect("removes");
        let probe = Probe::default();
        probe.space.set(Some(0));

        let report = report(world.run_with(&world.request(Some("tools/copy"), false), &probe));

        assert!(report.forked);
        assert_eq!(report.bytes, None);
        let warnings = texts(&report);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("is damaged"), "{warnings:?}");
        assert!(
            warnings[1].contains("free space is not checked"),
            "{warnings:?}"
        );
        assert!(world.root().join("tools/copy/main.rs").exists());
    }

    #[test]
    fn a_damaged_version_with_a_listing_still_needs_room_for_its_bytes() {
        let world = World::new(&[NEW]);
        fs::write(
            world.version_path(VersionFile::Checksum),
            "not a checksum line",
        )
        .expect("writes");
        let probe = Probe::default();
        probe.space.set(Some(3));
        let before = snapshot(world.root());

        let report = report(world.run_with(&world.request(Some("tools/copy"), false), &probe));

        assert!(!report.forked && report.partial());
        let skipped = report.skipped.expect("skipped");
        assert_eq!(skipped.reason, "no_space");
        assert!(skipped.needed.is_some_and(|needed| needed > 3));
        assert_eq!(skipped.available, 3);
        let mut after = snapshot(world.root());
        after.remove(".antiq/lock");
        assert_eq!(after, before);
    }

    #[test]
    fn a_doubled_slot_is_refused_without_the_advice_to_fork() {
        let world = World::new(&[NEW]);
        fs::write(
            world
                .root()
                .join(".antiq/pieces/projects/scanner/other--2026-11-04T090211.tar.zst"),
            "x",
        )
        .expect("writes");
        let error = world.fork(None).expect_err("a doubled slot");
        assert!(
            matches!(error, ForkError::Restore(RestoreError::Doubled { .. })),
            "{error:?}"
        );
    }

    // ----- no room

    #[test]
    fn no_room_for_the_listings_bytes_is_a_skip_and_nothing_is_written() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        probe.space.set(Some(3));
        let before = snapshot(world.root());

        let report = report(world.run_with(&world.request(None, false), &probe));

        assert!(!report.forked && report.partial());
        let skipped = report.skipped.expect("skipped");
        assert!(skipped.needed.is_some_and(|needed| needed > 3));
        let mut after = snapshot(world.root());
        after.remove(".antiq/lock");
        assert_eq!(after, before);
    }

    // ----- after the rename

    #[test]
    fn a_failure_after_the_rename_is_a_partial_report_with_the_folder_in_place() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        probe.stop_at(Step::Renamed);

        let report = report(world.run_with(&world.request(Some("tools/copy"), false), &probe));

        assert!(report.forked && report.unfinished && report.partial());
        let warnings = texts(&report);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("`tools/copy` is in place")),
            "{warnings:?}"
        );
        assert!(world.root().join("tools/copy/main.rs").exists());
    }

    // ----- dry run

    #[test]
    fn a_dry_run_names_the_fork_and_changes_nothing() {
        let world = World::new(&[OLD, NEW]);
        let before = snapshot(world.root());

        let plan = plan(world.plan(Some("tools/copy")));

        assert!(plan.dry_run && !plan.skips());
        assert_eq!(plan.actions.len(), 1);
        let PlanStep::Command(action) = &plan.actions[0] else {
            panic!("expected the fork action");
        };
        assert_eq!(action.kind, "fork");
        assert_eq!(action.stamp, NEW);
        assert_eq!(action.path, "tools/copy");
        assert_eq!(
            action.checks,
            [
                "version",
                "destination",
                "free_space",
                "member_names",
                "extracted_tree"
            ]
        );
        assert!(action.damage.is_empty());
        assert_eq!(snapshot(world.root()), before);
    }

    #[test]
    fn a_dry_run_of_a_damaged_version_without_a_listing_drops_the_checks_it_cannot_make() {
        let world = World::new(&[NEW]);
        fs::remove_file(world.version_path(VersionFile::Listing)).expect("removes");

        let plan = plan(world.plan(None));

        let PlanStep::Command(action) = &plan.actions[0] else {
            panic!("expected the fork action");
        };
        assert_eq!(action.checks, ["version", "destination", "member_names"]);
        assert_eq!(action.bytes, None);
        assert!(!action.damage.is_empty());
        let warnings: Vec<&str> = plan.warnings.iter().map(|w| w.message.as_str()).collect();
        assert!(
            warnings.iter().any(|w| w.contains("is damaged")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("free space is not checked")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_dry_run_without_room_names_the_skip() {
        let world = World::new(&[NEW]);
        let probe = Probe::default();
        probe.space.set(Some(3));

        let plan = plan(world.run_with(&world.request(None, true), &probe));

        assert!(plan.skips());
    }

    #[test]
    fn a_dry_run_checks_the_room_before_the_destination_as_the_real_run_does() {
        let world = World::new(&[NEW]);
        fs::create_dir_all(world.root().join("projects/scanner")).expect("takes the place");
        let probe = Probe::default();
        probe.space.set(Some(3));

        let plan = plan(world.run_with(&world.request(None, true), &probe));

        assert!(
            plan.skips(),
            "the dry run predicts the skip of the real run"
        );
    }

    #[test]
    fn a_dry_run_with_a_taken_original_path_names_the_name_option() {
        let world = World::new(&[NEW]);
        fs::create_dir_all(world.root().join("projects/scanner")).expect("takes the place");
        let error = world.plan(None).expect_err("taken");
        assert!(matches!(&error, ForkError::Taken { .. }), "{error:?}");
    }

    fn put_piece_in_trash(world: &World) {
        fs::create_dir_all(world.root().join("projects")).expect("the parent");
        let trashed = world
            .root()
            .join(format!(".antiq/trash/{NEW}/projects/scanner"));
        fs::create_dir_all(&trashed).expect("trash");
        fs::write(trashed.join("main.rs"), "fn main() {}").expect("file");
    }

    #[test]
    fn a_tree_repair_4_gives_back_to_the_destination_makes_it_taken() {
        let world = World::new(&[NEW]);
        put_piece_in_trash(&world);
        let taken = |error: &ForkError| matches!(error, ForkError::Taken { .. });
        let error = world.plan(None).expect_err("the dry run refuses");
        assert!(taken(&error), "{error:?}");
        let error = world.fork(None).expect_err("the run refuses");
        assert!(taken(&error), "{error:?}");
    }

    // ----- the lock

    #[test]
    fn a_held_lock_is_an_error_with_the_holder() {
        let world = World::new(&[NEW]);
        let _held =
            lock::acquire(&world.collection().lock_path(), "other", &*world.clock).expect("lock");
        let error = Error::from(world.fork(None).expect_err("the lock is held"));
        assert_eq!(error.kind(), ErrorKind::LockHeld);
        let details = error.details();
        assert!(
            details.contains_key("pid") && details.contains_key("started"),
            "{details:?}"
        );
    }

    #[test]
    fn a_dry_run_with_the_lock_held_names_the_holder_and_lists_no_repairs() {
        let world = World::new(&[NEW]);
        let _held =
            lock::acquire(&world.collection().lock_path(), "other", &*world.clock).expect("lock");
        match world.plan(None).expect("dry run") {
            Outcome::Held(held) => assert!(held.lines()[0].starts_with("The lock is held by pid")),
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn the_lock_is_free_again_after_a_fork() {
        let world = World::new(&[NEW]);
        report(world.fork(Some("tools/copy")));
        let collection = world.collection();
        let _free = lock::retry_while_held(
            |result: &Result<lock::RunLock, lock::LockError>| result.is_err(),
            || lock::acquire(&collection.lock_path(), "test", &*world.clock),
        )
        .expect("the lock is free after the run");
    }

    #[test]
    fn a_dry_run_of_an_unknown_piece_is_refused() {
        let world = World::new(&[NEW]);
        let mut request = world.request(None, true);
        request.piece = OsString::from("nothing-like-it");
        let error = world
            .run_with(&request, &Probe::default())
            .expect_err("unknown");
        assert_eq!(error.kind(), ErrorKind::UnknownReference);
    }

    // ----- repairs a dry run plans

    /// An unearth that died after the rename: the folder is in place with a
    /// marker that names the version, and the version is still stored.
    fn died_unearth(world: &World) {
        let folder = world.root().join("projects/scanner");
        fs::create_dir_all(&folder).expect("the folder");
        fs::write(folder.join("main.rs"), "fn main() {}").expect("file");
        fs::write(
            folder.join(".antiq-restored"),
            format!("restored 2026-11-05T09:00:00Z version {NEW}\n"),
        )
        .expect("marker");
    }

    #[test]
    fn a_taken_original_path_a_planned_repair_may_free_is_a_warning_in_a_dry_run() {
        let world = World::new(&[NEW]);
        died_unearth(&world);
        let plan = plan(world.plan(None));
        assert!(
            plan.warnings.iter().any(|w| w
                .message
                .contains("cannot be judged before the repairs run")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn a_taken_name_is_refused_even_when_a_planned_repair_names_the_piece() {
        let world = World::new(&[NEW]);
        died_unearth(&world);
        let error = world.plan(Some("projects/scanner")).expect_err("refused");
        assert!(
            matches!(
                &error,
                ForkError::Restore(RestoreError::Target {
                    problem: TargetProblem::Taken,
                    ..
                })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_choice_a_planned_repair_may_change_is_a_warning_and_names_no_action() {
        use crate::versions::names::file_name;
        let world = World::new(&[NEW]);
        // A died archive: the sidecars are stored, the archive is still
        // staged, and repair 1 deletes the sidecars.
        let piece = RelPath::parse("projects/scanner").expect("path");
        let stamp: Stamp = NEW.parse().expect("stamp");
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

        let plan = plan(world.plan(None));

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

    #[test]
    fn the_action_and_the_report_describe_themselves() {
        let world = World::new(&[NEW]);
        let plan = plan(world.plan(Some("tools/copy")));
        let PlanStep::Command(action) = &plan.actions[0] else {
            panic!("expected the fork action");
        };
        assert_eq!(
            action.describe(),
            format!("fork projects/scanner (version {NEW}) to tools/copy")
        );
        let report = report(world.fork(Some("tools/copy")));
        assert!(CommandReport::warnings(&report).is_empty());
    }

    // ----- what is left behind

    /// Every entry below `.antiq/tmp/`, relative to it.
    fn left_in_tmp(world: &World) -> Vec<String> {
        snapshot(&world.root().join(".antiq/tmp"))
            .into_keys()
            .collect()
    }

    #[test]
    fn a_fork_leaves_tmp_empty_so_the_next_run_plans_no_repair() {
        let world = World::new(&[NEW]);
        report(world.fork(Some("tools/copy")));
        assert_eq!(left_in_tmp(&world), Vec::<String>::new());
        let plan = plan(lock::retry_while_held(
            |result: &Result<Outcome, ForkError>| matches!(result, Ok(Outcome::Held(_))),
            || world.plan(Some("tools/other")),
        ));
        assert!(
            plan.actions
                .iter()
                .all(|step| matches!(step, PlanStep::Command(_))),
            "{:?}",
            plan.actions
        );
    }
}
