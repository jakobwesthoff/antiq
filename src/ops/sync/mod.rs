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
use crate::id::ObjectId;
use crate::index::IndexError;
use crate::listing::ListingError;
use crate::lock::{Holder, RunLock};
use crate::meta::MetaError;
use crate::ops::status::{self, StatusError};
use crate::plan::{Plan, PlanAction};
use crate::relpath::RelPath;
use crate::remote::away::{self, AwayError};
use crate::remote::{Assessment, Reachability, Remote, RemoteError};
use crate::report::{self, CommandReport, Warning};
use crate::run::{self, Started};
use crate::store::{Store, StoreError};
use crate::tracking::TrackingError;
use serde::Serialize;

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
    Away(#[from] AwayError),
    #[error(transparent)]
    Status(#[from] StatusError),
    /// The remote failed. [`sync_with`] hands it on as `Error::Remote`, so
    /// that the away policy sees it.
    #[error(transparent)]
    Remote(#[from] RemoteError),
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

/// What a run of `sync` did.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub pulled: Vec<PulledVersion>,
    /// Objects whose listing was fetched.
    pub listings: Vec<ObjectId>,
    pub skipped: Vec<SkippedObject>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for SyncReport {
    fn partial(&self) -> bool {
        !self.skipped.is_empty()
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// One step of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SyncAction {
    PullMetadata { id: ObjectId, version: u32 },
    FetchListing { id: ObjectId, file: String },
}

impl PlanAction for SyncAction {
    fn describe(&self) -> String {
        match self {
            SyncAction::PullMetadata { id, version } => {
                format!("pull version {version} of {id}")
            }
            SyncAction::FetchListing { id, file } => format!("fetch {file} of {id}"),
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
                let ctx = SyncContext {
                    collection,
                    remote,
                    lock: guard.lock(),
                    clock,
                    machine: &machine,
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
                pull::plan_pull(collection, remote, &mut plan)?;
                Ok(Synced::Planned {
                    plan,
                    notes: report::setting_notes(collection),
                    holder: holder.clone(),
                })
            }
        }
    })();
    remote.finish();
    outcome.map_err(public)
}

/// The phases of a real run, in the order of the spec.
fn real_run(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<(), SyncError> {
    let _pull_failed = pull::run(ctx, report)?;
    live::refresh(ctx)?;
    live::delete_committed(ctx)?;
    tiers::apply(ctx)?;
    away::record_sync(ctx.collection, ctx.lock, ctx.clock.now())?;
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
        assert_eq!(remote.count(Op::ReadFile), 0);
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
        assert_eq!(remote.count(Op::ReadFile), 0);
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

        let report = done(sync_with(&ben.collection, &ben.clock, &remote, false));

        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.contains("cannot read the metadata"))
        );
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
}
