// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `sync` command: pulls what other machines committed, and later
//! phases upload and commit what this machine recorded (spec "sync").

pub mod commit;
pub mod conflict;
pub mod live;
pub mod pull;
pub mod tiers;
pub mod upload;

#[cfg(test)]
mod fixture;

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::{Error, ErrorKind, HasKind};
use crate::id::{IdSource, ObjectId, RandomIds};
use crate::index::IndexError;
use crate::listing::ListingError;
use crate::lock::{Holder, RunLock};
use crate::meta::MetaError;
use crate::ops::status::{self, StatusError};
use crate::pack::manifest::ManifestError;
use crate::pack::pool::Pool;
use crate::plan::{Plan, PlanAction};
use crate::relpath::RelPath;
use crate::remote::away::{self, AwayError};
use crate::remote::cleanup;
use crate::remote::{Assessment, Reachability, Remote, RemoteError};
use crate::report::{self, CommandReport, Warning};
use crate::run::{self, Started};
use crate::store::{Store, StoreError};
use crate::tracking::TrackingError;
use jiff::SignedDuration;
use serde::Serialize;
use std::sync::Mutex;

/// Errors of the `sync` command.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
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
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Away(#[from] AwayError),
    #[error(transparent)]
    Status(#[from] StatusError),
    /// The remote failed. [`sync_with`] hands it on as `Error::Remote`, so
    /// that the away policy sees it.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// A step on one object failed for a reason that has no error type of
    /// its own.
    #[error("{0}")]
    Failed(String),
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: std::io::Error,
    },
}

impl HasKind for SyncError {
    /// The errors whose kind a caller acts on keep it; the others are
    /// `other`.
    fn kind(&self) -> ErrorKind {
        match self {
            SyncError::Store(error) => error.kind(),
            SyncError::Meta(error) => error.kind(),
            SyncError::Remote(error) => error.kind(),
            _ => ErrorKind::Other,
        }
    }
}

/// A remote error keeps its own variant of `Error`, since the away policy
/// looks for `Error::Remote`.
fn public(error: SyncError) -> Error {
    match error {
        SyncError::Remote(error) => Error::Remote(error),
        other => Error::Sync(other),
    }
}

/// A version pulled from the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PulledVersion {
    pub id: ObjectId,
    pub version: u32,
}

/// Why an object was left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Its metadata or listing could not be taken from the remote.
    Pull,
    /// A local or remote operation on the object failed.
    Error,
    Conflict,
    Contested,
    Refused,
}

/// An object `sync` left alone, which makes the run partial (exit 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedObject {
    pub id: ObjectId,
    pub path: Option<RelPath>,
    pub reason: SkipReason,
    pub message: String,
}

/// Why a piece got no live backup in this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnbackedReason {
    /// Something in the piece cannot be read.
    Unreadable,
    /// Another filesystem is mounted inside the piece.
    MountPoint,
    /// An entry of the exclude list lies inside the piece.
    Excluded,
    /// A collection of its own lies inside the piece.
    NestedCollection,
    /// The piece kept changing while it was read.
    Stale,
    /// A local operation on the piece failed.
    Error,
}

impl UnbackedReason {
    /// The reasons that make the run partial. A piece that selection or
    /// the exclude list keeps out of the archive is not a failure of the
    /// run.
    fn makes_partial(self) -> bool {
        matches!(
            self,
            UnbackedReason::Unreadable | UnbackedReason::Stale | UnbackedReason::Error
        )
    }
}

/// A piece `sync` could not back up or give an ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnbackedPiece {
    /// `null` for a piece that has no ID yet.
    pub id: Option<ObjectId>,
    pub path: RelPath,
    pub reason: UnbackedReason,
    pub message: String,
}

/// A new piece that got its ID in this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MintedPiece {
    pub id: ObjectId,
    pub path: RelPath,
}

/// A payload or listing that was written to the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UploadedFile {
    pub id: ObjectId,
    pub file: String,
    pub size: u64,
}

/// A metadata version of this machine that is committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommittedVersion {
    pub id: ObjectId,
    pub version: u32,
}

/// What the cleanup of the remote did.
#[derive(Debug, Default, Serialize)]
pub struct CleanupReport {
    pub removed: usize,
    pub skipped: usize,
    /// The cleanup could not run to its end.
    #[serde(skip)]
    pub failed: bool,
}

/// What a run of `sync` did.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub pulled: Vec<PulledVersion>,
    /// Objects whose listing was fetched.
    pub listings: Vec<ObjectId>,
    pub uploaded: Vec<UploadedFile>,
    pub committed: Vec<CommittedVersion>,
    /// New pieces that got their ID in this run.
    pub minted: Vec<MintedPiece>,
    pub skipped: Vec<SkippedObject>,
    /// Pieces that got no live backup or no ID.
    pub unbacked: Vec<UnbackedPiece>,
    pub cleanup: CleanupReport,
    pub warnings: Vec<Warning>,
}

impl CommandReport for SyncReport {
    fn partial(&self) -> bool {
        !self.skipped.is_empty()
            || self
                .unbacked
                .iter()
                .any(|piece| piece.reason.makes_partial())
            || self.cleanup.failed
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// One step of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SyncAction {
    PullMetadata {
        id: ObjectId,
        version: u32,
    },
    FetchListing {
        id: ObjectId,
        file: String,
    },
    /// The recorded change is the committed version `version`.
    Settle {
        id: ObjectId,
        version: u32,
    },
    /// The recorded change conflicts with the newest version.
    Conflict {
        id: ObjectId,
    },
    Upload {
        id: ObjectId,
        file: String,
        size: u64,
    },
    Commit {
        id: ObjectId,
    },
    /// The live package of a piece is made again.
    PackLive {
        id: ObjectId,
        path: RelPath,
    },
    /// A new piece gets an ID and live metadata.
    MintId {
        path: RelPath,
    },
    /// A removal on the remote that cleanup would make; `age_unknown` is
    /// set where the file's age decides and a dry run does not read the
    /// remote's clock.
    RemoveLeftover {
        id: ObjectId,
        name: String,
        what: cleanup::What,
        age_unknown: bool,
    },
}

impl PlanAction for SyncAction {
    fn describe(&self) -> String {
        match self {
            SyncAction::PullMetadata { id, version } => {
                format!("pull version {version} of {id}")
            }
            SyncAction::FetchListing { id, file } => format!("fetch {file} of {id}"),
            SyncAction::Settle { id, version } => {
                format!("record version {version} of {id} as committed")
            }
            SyncAction::Conflict { id } => {
                format!("skip {id}, its change conflicts with the newest version")
            }
            SyncAction::Upload { id, file, .. } => format!("upload {file} of {id}"),
            SyncAction::Commit { id } => format!("commit the change to {id}"),
            SyncAction::PackLive { id, path } => format!("pack {path} ({id}) as a live package"),
            SyncAction::MintId { path } => format!("give {path} an ID"),
            SyncAction::RemoveLeftover {
                id,
                name,
                what,
                age_unknown,
            } => {
                let suffix = if *age_unknown {
                    " (age unknown in a dry run)"
                } else {
                    ""
                };
                format!("remove {what} {name} of {id}{suffix}")
            }
        }
    }
}

/// The result of [`sync`].
#[derive(Debug)]
pub enum Synced {
    Planned {
        plan: Plan<SyncAction>,
        notes: Vec<Warning>,
        holder: Option<Holder>,
    },
    Done(SyncReport),
}

/// What every phase of a real run works with.
pub struct SyncContext<'a> {
    pub collection: &'a Collection,
    pub remote: &'a dyn Remote,
    pub lock: &'a RunLock,
    pub clock: &'a dyn Clock,
    pub machine: &'a str,
    /// Mints the IDs of new pieces.
    pub ids: &'a dyn IdSource,
    /// The compression workers live packages are made with.
    pub pool: &'a Pool,
    /// At most this many remote operations run at once.
    pub jobs: usize,
}

/// Exchanges metadata and archives with the remote the config names.
pub fn sync(collection: &Collection, clock: &dyn Clock, dry_run: bool) -> Result<Synced, Error> {
    let remote = crate::remote::open(collection)?;
    sync_with(collection, clock, remote.as_ref(), dry_run)
}

/// [`sync`] with the remote given, so that a test can put a faulty one in
/// its place.
pub fn sync_with(
    collection: &Collection,
    clock: &dyn Clock,
    remote: &dyn Remote,
    dry_run: bool,
) -> Result<Synced, Error> {
    // A dry run packs nothing, so it needs no workers.
    let pool = if dry_run {
        None
    } else {
        Some(
            Pool::for_machine(collection.config().collection.memory_percent())
                .map_err(Error::from)?,
        )
    };
    sync_using(&SyncRequest {
        collection,
        clock,
        remote,
        ids: &RandomIds,
        pool: pool.as_ref(),
        dry_run,
    })
}

/// Everything a run of [`sync_using`] depends on.
pub struct SyncRequest<'a> {
    pub collection: &'a Collection,
    pub clock: &'a dyn Clock,
    pub remote: &'a dyn Remote,
    pub ids: &'a dyn IdSource,
    /// The workers for live packages. A dry run needs none; a real run
    /// without one fails.
    pub pool: Option<&'a Pool>,
    pub dry_run: bool,
}

/// [`sync_with`] with the source of IDs given.
pub fn sync_using(request: &SyncRequest<'_>) -> Result<Synced, Error> {
    let SyncRequest {
        collection,
        clock,
        remote,
        ids,
        pool,
        dry_run,
    } = *request;
    let started = run::start(collection, clock, dry_run)?;
    let now = clock.now();
    let machine = collection.machine_name()?;

    // A remote that fails its own probe with away or broken says the same
    // as one that assesses itself so.
    let assessment = match remote.reachability() {
        Ok(assessment) => assessment,
        Err(RemoteError::Away { message, .. }) => Assessment::new(Reachability::Away, message),
        Err(RemoteError::Broken(message)) => Assessment::new(Reachability::Broken, message),
        Err(error) => return Err(error.into()),
    };
    if let Started::Guarded(guard) = &started {
        match assessment.reachability {
            Reachability::Reachable => away::record_reachable(collection, guard.lock(), now),
            Reachability::Away => away::record_away(collection, guard.lock(), now),
            Reachability::Broken => Ok(()),
        }
        .map_err(local)?;
    }
    // The probe of work that waits for the remote runs only for a remote
    // that is away. A status that cannot be computed counts as work, so
    // that a damaged collection does not hide an overdue remote.
    let pending_work = || match status::status(collection, clock) {
        Ok(report) => status::has_pending_remote_work(&report.pieces),
        Err(_) => true,
    };
    away::with_overdue(
        assessment.into_result().map_err(Error::from),
        collection,
        pending_work,
        now,
    )?;

    let outcome = (|| {
        remote.begin_phase()?;
        match &started {
            Started::Guarded(guard) => {
                let pool = pool.ok_or_else(|| {
                    SyncError::Failed("a real run needs a pool of compression workers".into())
                })?;
                let ctx = SyncContext {
                    collection,
                    remote,
                    lock: guard.lock(),
                    clock,
                    machine: &machine,
                    ids,
                    pool,
                    jobs: collection.config().collection.remote_jobs.max(1) as usize,
                };
                let mut report = SyncReport {
                    warnings: report::setting_notes(collection),
                    ..SyncReport::default()
                };
                report.warnings.extend_from_slice(guard.warnings());
                real_run(&ctx, &mut report)?;
                Ok(Synced::Done(report))
            }
            Started::DryRun { holder } => {
                let mut plan = Plan::new();
                let mut notes = report::setting_notes(collection);
                let would_pull = pull::plan_pull(collection, remote, &mut plan, &mut notes)?;
                live::plan(collection, clock, &machine, &mut plan)?;
                if let Some(would_pull) = would_pull {
                    commit::plan_changes(collection, &machine, &would_pull, &mut plan)?;
                }
                plan_cleanup(collection, remote, &machine, &mut plan)?;
                Ok(Synced::Planned {
                    plan,
                    notes,
                    holder: holder.clone(),
                })
            }
        }
    })();
    remote.finish();
    outcome.map_err(public)
}

/// The removals cleanup would make. A dry run reads no clock, so the
/// removals that depend on an age are named as such.
fn plan_cleanup(
    collection: &Collection,
    remote: &dyn Remote,
    machine: &str,
    plan: &mut Plan<SyncAction>,
) -> Result<(), SyncError> {
    let keep = commit::pending_temp_names(collection, machine)?;
    let settings = cleanup::Settings {
        machine,
        collection: collection.id(),
        grace: SignedDuration::from_hours(i64::from(collection.config().collection.grace_hours)),
        keep: &keep,
    };
    for step in cleanup::plan(remote, &settings, &cleanup::Mode::DryRun)?.steps {
        let (id, name, what, age_unknown) = match step {
            cleanup::Step::Remove { id, name, what } => (id, name, what, false),
            cleanup::Step::AgeUnknown { id, name, what } => (id, name, what, true),
        };
        plan.push(SyncAction::RemoveLeftover {
            id,
            name,
            what,
            age_unknown,
        });
    }
    Ok(())
}

/// The phases of a real run, in the order of the spec.
fn real_run(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<(), SyncError> {
    let pull_failed = pull::run(ctx, report)?;
    live::refresh(ctx, report, pull_failed)?;
    if pull_failed {
        commit::skip_all(
            ctx,
            report,
            SkipReason::Pull,
            "the metadata on the remote could not be read",
        )?;
    } else {
        commit::run(ctx, report)?;
        if !report.committed.is_empty() {
            pull::fetch_listings_and_index(ctx, report)?;
        }
    }
    cleanup(ctx, report)?;
    live::delete_committed(ctx)?;
    tiers::apply(ctx)?;
    away::record_sync(ctx.collection, ctx.lock, ctx.clock.now())?;
    Ok(())
}

/// Removes what interrupted and finished commits left on the remote. Uploads
/// that are still pending keep their temp files, since the next run
/// continues them. A cleanup that fails for another reason than the
/// remote's being gone is a warning and makes the run partial.
fn cleanup(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<(), SyncError> {
    let keep = match commit::pending_temp_names(ctx.collection, ctx.machine) {
        Ok(keep) => keep,
        Err(error) => {
            report.cleanup.failed = true;
            report.warnings.push(Warning::new(format!(
                "cleaning the remote was skipped, as the uploads that wait could not be listed: {error}"
            )));
            return Ok(());
        }
    };
    let settings = cleanup::Settings {
        machine: ctx.machine,
        collection: ctx.collection.id(),
        grace: SignedDuration::from_hours(i64::from(
            ctx.collection.config().collection.grace_hours,
        )),
        keep: &keep,
    };
    let applied = ctx.remote.clock().and_then(|now| {
        let mode = cleanup::Mode::Run {
            lock: ctx.lock,
            now,
        };
        let steps = cleanup::plan(ctx.remote, &settings, &mode)?;
        cleanup::apply(ctx.remote, &settings, ctx.lock, now, &steps)
    });
    match applied {
        Ok(applied) => {
            report.cleanup.removed = applied.removed;
            report.cleanup.skipped = applied.skipped;
        }
        Err(error) if ends_run(ctx.remote, &error) => return Err(error.into()),
        Err(error) => {
            report.cleanup.failed = true;
            report
                .warnings
                .push(Warning::new(format!("cleaning the remote failed: {error}")));
        }
    }
    Ok(())
}

fn local(error: impl Into<SyncError>) -> Error {
    public(error.into())
}

/// Whether a failed remote operation ends the run. An away or broken
/// answer to one operation does not say the remote is gone, so the remote
/// is asked; one that answers as reachable fails only the operation's object.
pub(super) fn ends_run(remote: &dyn Remote, error: &RemoteError) -> bool {
    match error {
        RemoteError::Away { .. } | RemoteError::Broken(_) => !matches!(
            remote.reachability(),
            Ok(assessment) if assessment.reachability == Reachability::Reachable
        ),
        _ => false,
    }
}

/// Runs `job` for every item on at most `workers` threads and returns the
/// results in the order of the items.
pub(super) fn run_bounded<T: Send, R: Send>(
    workers: usize,
    items: Vec<T>,
    job: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    let count = items.len();
    let queue = Mutex::new(items.into_iter().enumerate());
    let results = Mutex::new(Vec::with_capacity(count));
    std::thread::scope(|scope| {
        // At least one worker, so that a configuration of zero still runs.
        for _ in 0..workers.clamp(1, count.max(1)) {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().expect("a job panicked").next();
                    let Some((position, item)) = next else {
                        break;
                    };
                    let result = job(item);
                    results
                        .lock()
                        .expect("a job panicked")
                        .push((position, result));
                }
            });
        }
    });
    let mut results = results.into_inner().expect("a job panicked");
    results.sort_by_key(|(position, _)| *position);
    results.into_iter().map(|(_, result)| result).collect()
}

/// The path of the piece an object belongs to, when its metadata reads.
pub(super) fn path_of(collection: &Collection, id: ObjectId) -> Option<RelPath> {
    let current = Store::new(collection).current(id).ok().flatten()?;
    Some(current.metadata.path)
}

/// Leaves an object alone in this run, once.
pub(super) fn skip(
    collection: &Collection,
    report: &mut SyncReport,
    id: ObjectId,
    reason: SkipReason,
    message: String,
) {
    if report.skipped.iter().any(|skipped| skipped.id == id) {
        return;
    }
    report.skipped.push(SkippedObject {
        id,
        path: path_of(collection, id),
        reason,
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::fixture::{Machine, Shared};
    use super::*;
    use crate::meta::HistoryAction;
    use crate::remote::testing::{Fault, Op};
    use serde_json::Value;
    use std::fs;

    fn done(synced: Result<Synced, Error>) -> SyncReport {
        match synced.expect("the sync runs") {
            Synced::Done(report) => report,
            other => panic!("expected a real run, got {other:?}"),
        }
    }

    fn state(machine: &Machine) -> Value {
        let bytes = fs::read(machine.collection.remote_state_path()).expect("the state exists");
        serde_json::from_slice(&bytes).expect("JSON")
    }

    #[test]
    fn a_collection_without_a_remote_cannot_sync() {
        let temp = crate::testing::tree::TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        let clock = crate::clock::FixedClock::new("2026-11-04T10:00:00Z".parse().expect("time"));
        let error = sync(&collection, &clock, false).expect_err("there is no remote");
        assert_eq!(error.kind(), ErrorKind::NoRemote);
    }

    #[test]
    fn metadata_another_machine_committed_is_stored_byte_for_byte() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);

        let report = done(sync_with(&ben.collection, &ben.clock, &ben.remote(), false));

        assert_eq!(report.pulled, vec![PulledVersion { id, version: 1 }]);
        let remote_bytes = fs::read(shared.objects().join(id.to_string()).join("meta.1"))
            .expect("the remote version");
        let local_bytes = fs::read(ben.store().meta_path(id, 1)).expect("the local version");
        assert_eq!(local_bytes, remote_bytes);
    }

    #[test]
    fn the_listing_of_a_pulled_archive_is_fetched_and_indexed() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);

        let report = done(sync_with(&ben.collection, &ben.clock, &ben.remote(), false));

        assert_eq!(report.listings, vec![id]);
        let listings: Vec<String> = fs::read_dir(ben.collection.object_dir(id))
            .expect("lists")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .filter(|name: &String| name.ends_with(".listing.zst"))
            .collect();
        assert_eq!(listings.len(), 1, "the listing is stored: {listings:?}");
        let index = crate::index::Index::compute(&ben.collection).expect("the index");
        let members = index
            .members(&ben.collection, id)
            .expect("the members are readable")
            .expect("the object has members");
        assert!(!members.is_empty(), "the cache holds the members");
    }

    #[test]
    fn a_second_sync_pulls_and_fetches_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        done(sync_with(&ben.collection, &ben.clock, &ben.remote(), false));

        let remote = ben.remote();
        let report = done(sync_with(&ben.collection, &ben.clock, &remote, false));

        assert!(report.pulled.is_empty() && report.listings.is_empty());
        // Only the cleanup reads files, and it reads metadata versions.
        let reads: Vec<String> = remote
            .calls()
            .into_iter()
            .filter(|call| call.op == Op::ReadFile)
            .map(|call| call.name)
            .collect();
        assert!(
            reads.iter().all(|name| name.starts_with("meta.")),
            "{reads:?}"
        );
    }

    #[test]
    fn a_listing_that_does_not_parse_skips_only_its_object() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let broken = anna.import_piece("broken", "one");
        let fine = anna.import_piece("fine", "two");
        anna.publish_by_hand(&shared, broken, 1);
        anna.publish_by_hand(&shared, fine, 1);
        let dir = shared.objects().join(broken.to_string());
        for entry in fs::read_dir(&dir).expect("lists") {
            let path = entry.expect("entry").path();
            if path.to_string_lossy().ends_with(".listing.zst") {
                fs::write(path, b"not a listing").expect("overwritten");
            }
        }

        let report = done(sync_with(&ben.collection, &ben.clock, &ben.remote(), false));

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].id, broken);
        assert_eq!(report.skipped[0].reason, SkipReason::Pull);
        assert_eq!(report.listings, vec![fine]);
        assert!(report.partial());
        let leftovers: Vec<_> = fs::read_dir(ben.collection.tmp_dir())
            .expect("lists")
            .collect();
        assert!(leftovers.is_empty(), "no temp file stays: {leftovers:?}");
    }

    #[test]
    fn a_reachable_remote_is_recorded_and_the_end_of_the_run_too() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        fs::write(
            ben.collection.remote_state_path(),
            r#"{"later_key": 1, "away_first_seen": "2026-11-01T00:00:00Z"}"#,
        )
        .expect("state written");

        done(sync_with(&ben.collection, &ben.clock, &ben.remote(), false));

        let state = state(&ben);
        assert_eq!(state["last_reachable"], "2026-11-04T10:00:00Z");
        assert_eq!(state["last_sync"], "2026-11-04T10:00:00Z");
        assert_eq!(state["later_key"], 1);
        assert!(state.get("away_first_seen").is_none());
    }

    #[test]
    fn an_away_remote_is_recorded_and_ends_the_run_without_last_sync() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let remote = ben.remote();
        remote.fail(Op::Reachability, 1, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, false).expect_err("the remote is away");

        assert_eq!(error.kind(), ErrorKind::RemoteAway);
        let state = state(&ben);
        assert_eq!(state["away_first_seen"], "2026-11-04T10:00:00Z");
        assert!(state.get("last_sync").is_none());
    }

    #[test]
    fn a_broken_remote_ends_the_run_and_is_not_recorded_as_reachable() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let remote = ben.remote();
        remote.fail(Op::Reachability, 1, Fault::Broken);

        let error = sync_with(&ben.collection, &ben.clock, &remote, false)
            .expect_err("the remote is broken");

        assert_eq!(error.kind(), ErrorKind::RemoteBroken);
        assert!(!ben.collection.remote_state_path().exists());
    }

    #[test]
    fn a_pull_that_fails_once_is_asked_again() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Io);

        let report = done(sync_with(&ben.collection, &ben.clock, &remote, false));

        assert_eq!(report.pulled.len(), 1);
        assert_eq!(remote.count(Op::PullMetadata), 2);
    }

    #[test]
    fn a_pull_that_fails_twice_is_a_warning_and_pulls_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Io);
        remote.fail(Op::PullMetadata, 2, Fault::Io);

        let report = done(sync_with(&ben.collection, &ben.clock, &remote, false));

        assert!(report.pulled.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("cannot read the metadata")),
            "{:?}",
            report.warnings
        );
        assert!(ben.store().objects().expect("lists").is_empty());
    }

    #[test]
    fn a_version_of_a_later_format_is_stored_and_does_not_stop_the_run() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let id = crate::remote::testing::object(7);
        let dir = shared.objects().join(id.to_string());
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("meta.1"), br#"{"format": 99}"#).expect("written");

        let report = done(sync_with(&ben.collection, &ben.clock, &ben.remote(), false));

        assert_eq!(report.pulled, vec![PulledVersion { id, version: 1 }]);
        assert!(!report.partial());
        let error = ben
            .store()
            .current(id)
            .expect_err("the version cannot be read");
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
    }

    #[test]
    fn an_away_answer_to_one_read_while_the_remote_is_reachable_skips_that_object() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let first = anna.import_piece("first", "one");
        let second = anna.import_piece("second", "two");
        anna.publish_by_hand(&shared, first, 1);
        anna.publish_by_hand(&shared, second, 1);
        let remote = ben.remote();
        remote.fail(Op::ReadFile, 1, Fault::Away);

        let report = done(sync_with(&ben.collection, &ben.clock, &remote, false));

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Pull);
        assert_eq!(report.listings.len(), 1);
    }

    #[test]
    fn an_away_answer_to_one_read_while_the_remote_is_gone_ends_the_run() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("first", "one");
        anna.publish_by_hand(&shared, id, 1);
        let remote = ben.remote();
        remote.fail(Op::ReadFile, 1, Fault::Away);
        remote.fail(Op::Reachability, 2, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, false).expect_err("the remote is gone");

        assert_eq!(error.kind(), ErrorKind::RemoteAway);
        assert!(state(&ben).get("last_sync").is_none());
        assert_eq!(remote.count(Op::Finish), 1);
    }

    #[test]
    fn a_dry_run_plans_the_pull_and_changes_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        let remote = ben.remote();

        let synced = sync_with(&ben.collection, &ben.clock, &remote, true).expect("plans");

        let Synced::Planned { plan, .. } = synced else {
            panic!("expected a plan");
        };
        let described: Vec<String> = plan.actions.iter().map(PlanAction::describe).collect();
        assert_eq!(described.len(), 2, "{described:?}");
        assert_eq!(described[0], format!("pull version 1 of {id}"));
        assert!(described[1].starts_with("fetch "), "{described:?}");
        assert!(ben.store().objects().expect("lists").is_empty());
        assert!(!ben.collection.remote_state_path().exists());
        assert_eq!(remote.count(Op::Clock), 0);
        // Only the cleanup's look at the metadata reads files.
        assert!(
            remote
                .calls()
                .iter()
                .filter(|call| call.op == Op::ReadFile)
                .all(|call| call.name.starts_with("meta.")),
        );
    }

    #[test]
    fn an_away_pull_while_the_remote_is_gone_ends_the_run() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Away);
        remote.fail(Op::Reachability, 2, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, false).expect_err("the remote is gone");

        assert_eq!(error.kind(), ErrorKind::RemoteAway);
        assert!(state(&ben).get("last_sync").is_none());
    }

    #[test]
    fn an_away_pull_while_the_remote_is_reachable_is_a_warning() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Away);
        remote.fail(Op::PullMetadata, 2, Fault::Away);

        let report = done(sync_with(&ben.collection, &ben.clock, &remote, false));

        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("cannot read the metadata"))
        );
    }

    #[test]
    fn an_away_pull_that_is_away_again_while_the_remote_is_gone_ends_the_run() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Away);
        remote.fail(Op::PullMetadata, 2, Fault::Away);
        remote.fail(Op::Reachability, 3, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, false).expect_err("the remote is gone");

        assert_eq!(error.kind(), ErrorKind::RemoteAway);
        assert_eq!(remote.count(Op::PullMetadata), 2);
    }

    #[test]
    fn a_dry_run_whose_metadata_read_fails_twice_plans_no_commit_and_notes_the_failure() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Io);
        remote.fail(Op::PullMetadata, 2, Fault::Io);

        let synced = sync_with(&anna.collection, &anna.clock, &remote, true).expect("plans");

        let Synced::Planned { plan, notes, .. } = synced else {
            panic!("expected a plan");
        };
        let described: Vec<String> = plan.actions.iter().map(PlanAction::describe).collect();
        assert!(described.is_empty(), "{described:?}");
        assert!(
            notes
                .iter()
                .any(|note| note.message.contains("cannot read the metadata")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_dry_run_whose_metadata_read_finds_the_remote_gone_fails_like_the_real_run() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Away);
        remote.fail(Op::Reachability, 2, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, true).expect_err("the remote is gone");

        assert_eq!(error.kind(), ErrorKind::RemoteAway);
    }

    #[test]
    fn a_dry_run_asks_a_failed_metadata_read_once_more() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Io);

        let synced = sync_with(&ben.collection, &ben.clock, &remote, true).expect("plans");

        let Synced::Planned { plan, notes, .. } = synced else {
            panic!("expected a plan");
        };
        assert!(
            plan.actions
                .iter()
                .any(|action| action.describe() == format!("pull version 1 of {id}"))
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn an_away_pull_that_succeeds_when_asked_again_is_no_warning() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        let remote = ben.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Away);

        let report = done(sync_with(&ben.collection, &ben.clock, &remote, false));

        assert_eq!(report.pulled.len(), 1);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(remote.count(Op::PullMetadata), 2);
    }

    #[test]
    fn the_errors_a_caller_acts_on_keep_their_kind() {
        let too_new = SyncError::Meta(MetaError::FormatTooNew { found: 9 });
        assert_eq!(too_new.kind(), ErrorKind::FormatTooNew);
        let no_remote = SyncError::Remote(RemoteError::NoRemote);
        assert_eq!(no_remote.kind(), ErrorKind::NoRemote);
        let store = SyncError::Store(StoreError::NoPending);
        assert_eq!(store.kind(), ErrorKind::Other);
        let io = SyncError::Io {
            action: "write",
            path: "x".into(),
            source: std::io::Error::from_raw_os_error(5),
        };
        assert_eq!(io.kind(), ErrorKind::Other);
        assert!(matches!(
            public(no_remote),
            Error::Remote(RemoteError::NoRemote)
        ));
        assert!(matches!(public(io), Error::Sync(SyncError::Io { .. })));
    }

    #[test]
    fn an_away_remote_after_eight_days_with_pending_work_is_overdue() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        ben.import_piece("notes", "hello");
        fs::write(
            ben.collection.remote_state_path(),
            r#"{"last_reachable": "2026-10-27T10:00:00Z"}"#,
        )
        .expect("state written");
        let remote = ben.remote();
        remote.fail(Op::Reachability, 1, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, false).expect_err("the remote is away");

        assert!(
            matches!(
                error,
                Error::Remote(RemoteError::Away { overdue: true, .. })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn an_away_remote_after_one_day_is_not_overdue() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        ben.import_piece("notes", "hello");
        fs::write(
            ben.collection.remote_state_path(),
            r#"{"last_reachable": "2026-11-03T10:00:00Z"}"#,
        )
        .expect("state written");
        let remote = ben.remote();
        remote.fail(Op::Reachability, 1, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, true).expect_err("the remote is away");

        assert!(
            matches!(
                error,
                Error::Remote(RemoteError::Away { overdue: false, .. })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_status_that_cannot_be_computed_counts_as_pending_work() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        fs::write(
            ben.collection.remote_state_path(),
            r#"{"last_reachable": "2026-10-27T10:00:00Z"}"#,
        )
        .expect("state written");
        let objects = ben.collection.objects_dir();
        fs::remove_dir_all(&objects).expect("removed");
        fs::write(&objects, b"a file where the objects directory belongs").expect("written");
        let remote = ben.remote();
        remote.fail(Op::Reachability, 1, Fault::Away);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, true).expect_err("the remote is away");

        assert!(
            matches!(
                error,
                Error::Remote(RemoteError::Away { overdue: true, .. })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_state_file_that_cannot_be_read_ends_the_run() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        fs::create_dir(ben.collection.remote_state_path()).expect("a directory in its place");

        let remote = ben.remote();
        let error = sync_with(&ben.collection, &ben.clock, &remote, false)
            .expect_err("the state cannot be recorded");

        assert!(
            matches!(error, Error::Sync(SyncError::Away(_))),
            "{error:?}"
        );
        assert_eq!(remote.count(Op::BeginPhase), 0, "the run ends at the probe");
    }

    #[test]
    fn a_probe_that_fails_for_another_reason_is_the_error_of_the_run() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let remote = ben.remote();
        remote.fail(Op::Reachability, 1, Fault::Io);

        let error =
            sync_with(&ben.collection, &ben.clock, &remote, false).expect_err("the probe fails");

        assert!(
            matches!(error, Error::Remote(RemoteError::Io { .. })),
            "{error:?}"
        );
        assert!(!ben.collection.remote_state_path().exists());
    }

    #[test]
    fn an_object_is_skipped_once_and_the_report_hands_out_its_warnings() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let id = crate::remote::testing::object(2);
        let mut report = SyncReport {
            warnings: vec![Warning::new("a repair did something")],
            ..SyncReport::default()
        };

        skip(
            &ben.collection,
            &mut report,
            id,
            SkipReason::Pull,
            "first".into(),
        );
        skip(
            &ben.collection,
            &mut report,
            id,
            SkipReason::Error,
            "second".into(),
        );

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].message, "first");
        assert_eq!(report.skipped[0].path, None);
        assert_eq!(report.warnings(), [Warning::new("a repair did something")]);
    }

    #[test]
    fn a_dry_run_after_a_sync_has_nothing_to_plan() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        done(sync_with(&ben.collection, &ben.clock, &ben.remote(), false));

        let synced = sync_with(&ben.collection, &ben.clock, &ben.remote(), true).expect("plans");

        let Synced::Planned { plan, .. } = synced else {
            panic!("expected a plan");
        };
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn two_items_run_at_the_same_time_on_two_workers() {
        let gate = crate::remote::testing::Gate::new();
        let results = run_bounded(2, vec![1, 2], |item| {
            gate.arrive(2);
            item * 10
        });
        assert_eq!(results, vec![10, 20]);
    }

    #[test]
    fn no_more_than_the_workers_run_at_once_and_results_keep_their_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let gate = crate::remote::testing::Gate::new();
        let results = run_bounded(2, (0..6).collect(), |item| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            // Two workers reach the gate together; the rest pass it.
            gate.arrive(2);
            running.fetch_sub(1, Ordering::SeqCst);
            item
        });
        assert_eq!(results, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn no_items_or_no_workers_still_finish() {
        assert_eq!(
            run_bounded(4, Vec::<u8>::new(), |item| item),
            Vec::<u8>::new()
        );
        assert_eq!(run_bounded(0, vec![7], |item| item), vec![7]);
    }

    /// Every file below `root` with its content, by path.
    fn snapshot(root: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
        fn walk(
            base: &std::path::Path,
            dir: &std::path::Path,
            found: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
        ) {
            for entry in fs::read_dir(dir).expect("lists") {
                let path = entry.expect("entry").path();
                let name = path
                    .strip_prefix(base)
                    .expect("below")
                    .display()
                    .to_string();
                if path.is_dir() {
                    found.insert(name, None);
                    walk(base, &path, found);
                } else {
                    found.insert(name, Some(fs::read(&path).expect("reads")));
                }
            }
        }
        let mut found = std::collections::BTreeMap::new();
        walk(root, root, &mut found);
        found
    }

    fn planned(synced: Result<Synced, Error>) -> Vec<String> {
        match synced.expect("plans") {
            Synced::Planned { plan, .. } => plan.actions.iter().map(PlanAction::describe).collect(),
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    #[test]
    fn a_dry_run_of_a_pending_archive_plans_the_uploads_the_commit_and_the_leftovers_and_writes_nothing()
     {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let kept = anna.import_piece("kept", "one");
        anna.sync();
        let id = anna.import_piece("notes", "hello");
        let dir = shared.objects().join(kept.to_string());
        fs::write(
            dir.join(".0123456789abcdef-x.tar.zst.anna.part"),
            b"partial",
        )
        .expect("planted");
        let local_before = snapshot(anna.collection.root());
        let remote_before = snapshot(shared.root.path());
        let remote = anna.remote();

        let described = planned(sync_with(&anna.collection, &anna.clock, &remote, true));

        assert_eq!(described.len(), 4, "{described:?}");
        assert!(described[0].starts_with("upload "), "{described:?}");
        assert!(described[0].ends_with(&format!("of {id}")), "{described:?}");
        assert!(described[1].starts_with("upload "), "{described:?}");
        assert_eq!(described[2], format!("commit the change to {id}"));
        // The machine's own temp file without a claim of its own goes at once.
        assert_eq!(
            described[3],
            format!("remove temp file .0123456789abcdef-x.tar.zst.anna.part of {kept}")
        );
        assert_eq!(snapshot(anna.collection.root()), local_before);
        assert_eq!(snapshot(shared.root.path()), remote_before);
        for op in [
            Op::Clock,
            Op::PutPayload,
            Op::WriteTemp,
            Op::CreateExclusive,
            Op::RenameIntoPlace,
            Op::Delete,
            Op::DeletePartial,
            Op::Flush,
        ] {
            assert_eq!(remote.count(op), 0, "{op:?}");
        }
    }

    #[test]
    fn a_dry_run_names_a_change_that_is_a_committed_version_and_a_conflict() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.sync();
        ben.sync();
        ben.record_history(id, HistoryAction::Unearth);
        fs::write(shared.objects().join(id.to_string()).join("meta.2"), {
            let mut document = crate::meta::parse(&shared.read(id, "meta.1")).expect("parses");
            document.history.push(crate::meta::HistoryEntry::new(
                anna.clock.now(),
                "anna",
                HistoryAction::Sync,
            ));
            crate::meta::to_bytes(&document).expect("serialises")
        })
        .expect("a version from another machine");

        let conflict = planned(sync_with(&ben.collection, &ben.clock, &ben.remote(), true));

        assert_eq!(conflict[0], format!("pull version 2 of {id}"));
        assert_eq!(
            conflict[1],
            format!("skip {id}, its change conflicts with the newest version")
        );
        assert!(
            !conflict.iter().any(|action| action.starts_with("upload ")),
            "no upload for a conflict: {conflict:?}"
        );
        assert!(
            conflict
                .iter()
                .any(|action| action.ends_with("(age unknown in a dry run)")),
            "{conflict:?}"
        );

        // A change that is the committed version 1 needs only recording.
        let settled = {
            let bytes = fs::read(ben.store().meta_path(id, 1)).expect("version 1");
            fs::write(ben.collection.meta_pending_path(id), bytes)
                .expect("pending equals version 1");
            fs::remove_file(shared.objects().join(id.to_string()).join("meta.2")).expect("removed");
            planned(sync_with(&ben.collection, &ben.clock, &ben.remote(), true))
        };
        assert_eq!(settled[0], format!("record version 1 of {id} as committed"));
    }

    #[test]
    fn a_dry_run_has_nothing_to_say_about_a_change_that_cannot_be_read() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let id = anna.import_piece("notes", "hello");
        fs::write(anna.collection.meta_pending_path(id), b"not metadata").expect("damaged");

        let described = planned(sync_with(
            &anna.collection,
            &anna.clock,
            &anna.remote(),
            true,
        ));

        assert!(described.is_empty(), "{described:?}");
    }
}
