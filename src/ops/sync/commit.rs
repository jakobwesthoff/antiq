// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Committing metadata versions.
//!
//! Every object with a recorded change (`meta.pending`) is classified once
//! per run, after the pull: the change may be a version that is committed
//! already, it may conflict with a version of another machine, or it is
//! ready to be committed.

use super::upload::{self, UploadFile, Uploaded};
use super::{
    CommittedVersion, SkipReason, SyncContext, SyncError, SyncReport, UploadedFile, conflict,
    ends_run, pull, run_bounded, skip,
};
use crate::id::ObjectId;
use crate::meta::{self, Metadata};
use crate::pack::hashing::hex;
use crate::remote::commit::{self as protocol, Outcome, Request, Settings};
use crate::store::Store;
use crate::tracking::{self, DecisionKind, VersionTakenOut};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::num::NonZeroU32;

/// What a recorded change is.
#[derive(Debug)]
pub(super) enum Class {
    /// The change is the committed version `number` and only needs to be
    /// recorded as such.
    Settle(u32),
    /// The change does not build on the newest version of the remote, or a
    /// decision about it is open. `lower` is the version of this machine's
    /// own change that reached the remote below the newest one.
    Conflict { lower: Option<u32> },
    Commit {
        /// The highest committed version the change was made on; 0 for a
        /// new object.
        based_on: u32,
        /// Where the history entries the change adds start.
        first_new: usize,
        pending: Box<Metadata>,
        base: Option<Box<Metadata>>,
    },
}

fn io(action: &'static str, path: &std::path::Path) -> impl FnOnce(std::io::Error) -> SyncError {
    let path = path.display().to_string();
    move |source| SyncError::Io {
        action,
        path,
        source,
    }
}

/// The objects that have a recorded change.
pub(super) fn pending_objects(ctx: &SyncContext<'_>) -> Result<Vec<ObjectId>, SyncError> {
    Ok(Store::new(ctx.collection)
        .objects()?
        .into_iter()
        .filter(|id| ctx.collection.meta_pending_path(*id).is_file())
        .collect())
}

/// The temp names of the uploads that wait, by object: every object that
/// has a recorded change keeps the temp files of its files, whatever the
/// run did with it, since a later run continues them. An object whose
/// metadata cannot be read keeps nothing.
pub(super) fn pending_temp_names(
    ctx: &SyncContext<'_>,
) -> Result<BTreeSet<(ObjectId, String)>, SyncError> {
    let store = Store::new(ctx.collection);
    let mut keep = BTreeSet::new();
    for id in pending_objects(ctx)? {
        let pending_path = ctx.collection.meta_pending_path(id);
        let Ok(pending) = fs::read(&pending_path)
            .map_err(|_| ())
            .and_then(|bytes| meta::parse(&bytes).map_err(|_| ()))
        else {
            continue;
        };
        let base = match store.committed_versions(id)?.last() {
            Some(&number) => fs::read(store.meta_path(id, number))
                .ok()
                .and_then(|bytes| meta::parse(&bytes).ok()),
            None => None,
        };
        for file in
            upload::files_to_upload(ctx.collection, ctx.machine, id, &pending, base.as_ref())?
        {
            keep.insert((id, file.temp));
        }
    }
    Ok(keep)
}

/// Says which of the three an object's recorded change is.
pub(super) fn classify(ctx: &SyncContext<'_>, id: ObjectId) -> Result<Class, SyncError> {
    let store = Store::new(ctx.collection);
    let pending_path = ctx.collection.meta_pending_path(id);
    let pending_bytes = fs::read(&pending_path).map_err(io("read", &pending_path))?;
    let pending = meta::parse(&pending_bytes)?;

    let versions = store.committed_versions(id)?;
    let top = match versions.last() {
        Some(&number) => {
            let path = store.meta_path(id, number);
            let bytes = fs::read(&path).map_err(io("read", &path))?;
            Some((number, meta::parse(&bytes)?))
        }
        None => None,
    };

    // A pending change that equals a committed version was committed by an
    // earlier run that ended before the local record. It is settled when
    // everything above it builds on it.
    for &number in versions.iter().rev() {
        let path = store.meta_path(id, number);
        if fs::read(&path).map_err(io("read", &path))? != pending_bytes {
            continue;
        }
        let (highest, newest) = top.as_ref().expect("a version exists");
        if number == *highest || newest.history_added_over(&pending).is_some() {
            return Ok(Class::Settle(number));
        }
        return Ok(Class::Conflict {
            lower: Some(number),
        });
    }

    let decided = tracking::read(ctx.collection, id)?.is_some_and(|entry| {
        entry
            .pending_decisions
            .iter()
            .any(|decision| decision.kind == DecisionKind::VersionConflict)
    });
    if decided {
        return Ok(Class::Conflict { lower: None });
    }
    Ok(match top {
        None => Class::Commit {
            based_on: 0,
            first_new: 0,
            pending: Box::new(pending),
            base: None,
        },
        Some((number, base)) => match pending.history_added_over(&base) {
            Some(first_new) => Class::Commit {
                based_on: number,
                first_new,
                pending: Box::new(pending),
                base: Some(Box::new(base)),
            },
            None => Class::Conflict { lower: None },
        },
    })
}

/// Records that the recorded change of the object is the committed version
/// `number`: the tracking entry first, then the file, so that a stop in
/// between leaves a `meta.pending` equal to the committed version, which
/// the next run settles again.
pub(super) fn settle(ctx: &SyncContext<'_>, id: ObjectId, number: u32) -> Result<(), SyncError> {
    let pending_path = ctx.collection.meta_pending_path(id);
    let bytes = fs::read(&pending_path).map_err(io("read", &pending_path))?;
    let sha256 = hex(&Sha256::digest(&bytes));
    if let Some(mut entry) = tracking::read(ctx.collection, id)? {
        let mut changed = false;
        if entry.version_taken_out == VersionTakenOut::Pending {
            entry.version_taken_out = VersionTakenOut::Committed(number);
            changed = true;
        }
        if !entry.committed_versions.contains(&number) {
            entry.committed_versions.push(number);
            changed = true;
        }
        // A newer version that builds on this one leaves nothing to decide.
        if entry
            .pending_decisions
            .iter()
            .any(|decision| decision.kind == DecisionKind::VersionConflict)
        {
            entry.resolve_decision(DecisionKind::VersionConflict);
            changed = true;
        }
        if changed {
            tracking::write(ctx.collection, ctx.lock, ctx.clock, &mut entry)?;
        }
    }
    let number = NonZeroU32::new(number).expect("a version number is above zero");
    Store::new(ctx.collection).promote_pending(ctx.lock, id, number, &sha256)?;
    Ok(())
}

/// How the commit of one object ended.
#[derive(Debug, PartialEq, Eq)]
enum Finished {
    Committed(u32),
    /// A later version names the files of this one; the second pull judges
    /// it.
    Landed,
    Skipped(SkipReason, String),
}

/// Commits the recorded change of one object.
fn commit_one(
    ctx: &SyncContext<'_>,
    id: ObjectId,
    based_on: u32,
    first_new: usize,
    files: &[UploadFile],
) -> Result<Finished, SyncError> {
    let store = Store::new(ctx.collection);
    let pending_path = ctx.collection.meta_pending_path(id);
    let bytes = fs::read(&pending_path).map_err(io("read", &pending_path))?;
    let mut pending = meta::parse(&bytes)?;
    // Entries are read as the commit time. The stamped document is
    // recorded first, so that the bytes committed are the bytes of
    // `meta.pending`.
    pending.stamp_committed(first_new, ctx.clock.now());
    store.write_pending(ctx.lock, &pending)?;
    let bytes = fs::read(&pending_path).map_err(io("read", &pending_path))?;

    let uploads: Vec<_> = files.iter().map(|file| file.as_upload(id)).collect();
    let grace_hours = ctx.collection.config().collection.grace_hours;
    let outcome = protocol::commit(
        ctx.remote,
        &Settings::new(ctx.machine, grace_hours),
        ctx.lock,
        &Request {
            id,
            based_on,
            metadata: &bytes,
            uploads: &uploads,
            put_already: true,
        },
    )?;
    Ok(match outcome {
        Outcome::Committed { number } => {
            settle(ctx, id, number.get())?;
            Finished::Committed(number.get())
        }
        Outcome::Landed { .. } => Finished::Landed,
        Outcome::Overtaken { number, higher } => {
            conflict::record(ctx, id, Some(number.get()))?;
            Finished::Skipped(
                SkipReason::Conflict,
                format!(
                    "its change is version {number}, and version {higher} is on the remote; \
                     it waits for a decision"
                ),
            )
        }
        Outcome::Contested { number, claim } => Finished::Skipped(
            SkipReason::Contested,
            format!(
                "another machine holds the claim for version {claim} above version {number}; \
                 the next sync reads the remote again"
            ),
        ),
        Outcome::Refused(refusal) => Finished::Skipped(
            SkipReason::Refused,
            format!("the commit was refused: {refusal:?}"),
        ),
    })
}

/// Leaves an object alone after a failed step, or ends the run when the
/// failure is the remote's being gone.
fn fail(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    id: ObjectId,
    error: SyncError,
) -> Result<(), SyncError> {
    if let SyncError::Remote(remote) = &error
        && ends_run(ctx.remote, remote)
    {
        return Err(error);
    }
    skip(
        ctx.collection,
        report,
        id,
        SkipReason::Error,
        error.to_string(),
    );
    Ok(())
}

/// Leaves every object with a recorded change alone for this run.
pub(super) fn skip_all(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    reason: SkipReason,
    message: &str,
) -> Result<(), SyncError> {
    for id in pending_objects(ctx)? {
        skip(ctx.collection, report, id, reason, message.to_string());
    }
    Ok(())
}

fn record_conflict(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    id: ObjectId,
    lower: Option<u32>,
) -> Result<(), SyncError> {
    match conflict::record(ctx, id, lower) {
        Ok(()) => skip(
            ctx.collection,
            report,
            id,
            SkipReason::Conflict,
            "its change conflicts with a newer version on the remote and waits for a decision"
                .to_string(),
        ),
        Err(error) => fail(ctx, report, id, error)?,
    }
    Ok(())
}

fn settle_reported(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    id: ObjectId,
    number: u32,
) -> Result<(), SyncError> {
    match settle(ctx, id, number) {
        Ok(()) => report.committed.push(CommittedVersion {
            id,
            version: number,
        }),
        Err(error) => fail(ctx, report, id, error)?,
    }
    Ok(())
}

/// Settles, uploads and commits every recorded change.
pub(super) fn run(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<(), SyncError> {
    let mut ready = Vec::new();
    for id in pending_objects(ctx)? {
        // An object the pull left alone is not committed on top of what
        // could not be taken from the remote.
        if report.skipped.iter().any(|skipped| skipped.id == id) {
            continue;
        }
        match classify(ctx, id) {
            Ok(Class::Settle(number)) => settle_reported(ctx, report, id, number)?,
            Ok(Class::Conflict { lower }) => record_conflict(ctx, report, id, lower)?,
            Ok(Class::Commit {
                based_on,
                first_new,
                pending,
                base,
            }) => {
                match upload::files_to_upload(
                    ctx.collection,
                    ctx.machine,
                    id,
                    &pending,
                    base.as_deref(),
                ) {
                    Ok(files) => ready.push((id, based_on, first_new, files)),
                    Err(error) => fail(ctx, report, id, error)?,
                }
            }
            Err(error) => fail(ctx, report, id, error)?,
        }
    }

    // The uploads of all objects are flushed once, before any commit names
    // them.
    let work: Vec<_> = ready
        .iter()
        .filter(|(_, _, _, files)| !files.is_empty())
        .map(|(id, _, _, files)| (*id, files.clone()))
        .collect();
    let mut failed = Vec::new();
    for Uploaded { id, written, error } in upload::run(ctx, work)? {
        for (file, size) in written {
            report.uploaded.push(UploadedFile { id, file, size });
        }
        if let Some(error) = error {
            failed.push(id);
            fail(ctx, report, id, error)?;
        }
    }

    let jobs: Vec<_> = ready
        .into_iter()
        .filter(|(id, _, _, _)| !failed.contains(id))
        .collect();
    let results = run_bounded(ctx.jobs, jobs, |(id, based_on, first_new, files)| {
        (id, commit_one(ctx, id, based_on, first_new, &files))
    });
    let mut landed = Vec::new();
    for (id, result) in results {
        match result {
            Ok(Finished::Committed(version)) => {
                report.committed.push(CommittedVersion { id, version })
            }
            Ok(Finished::Landed) => landed.push(id),
            Ok(Finished::Skipped(reason, message)) => {
                skip(ctx.collection, report, id, reason, message);
            }
            Err(error) => fail(ctx, report, id, error)?,
        }
    }
    settle_landed(ctx, report, landed)
}

/// Pulls once more and judges the objects whose retried commit found a
/// higher version: they are settled when that version builds on theirs and
/// are conflicts otherwise.
fn settle_landed(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    landed: Vec<ObjectId>,
) -> Result<(), SyncError> {
    if landed.is_empty() {
        return Ok(());
    }
    if pull::run(ctx, report)? {
        for id in landed {
            skip(
                ctx.collection,
                report,
                id,
                SkipReason::Pull,
                "the remote's metadata could not be read to judge the newer version".to_string(),
            );
        }
        return Ok(());
    }
    for id in landed {
        match classify(ctx, id) {
            Ok(Class::Settle(number)) => settle_reported(ctx, report, id, number)?,
            Ok(Class::Conflict { lower }) => record_conflict(ctx, report, id, lower)?,
            Ok(Class::Commit { .. }) => skip(
                ctx.collection,
                report,
                id,
                SkipReason::Error,
                "the version that was committed cannot be found".to_string(),
            ),
            Err(error) => fail(ctx, report, id, error)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{Machine, Shared};
    use super::*;
    use crate::clock::Clock;
    use crate::error::{Error, ErrorKind};
    use crate::meta::HistoryAction;
    use crate::ops::status::{self, Flag};
    use crate::ops::sync::sync_with;
    use crate::remote::RemoteError;
    use crate::remote::testing::{Fault, Op};
    use crate::report::CommandReport;
    use crate::tracking;

    fn anna_and_ben() -> (Shared, Machine, Machine) {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        (shared, anna, ben)
    }

    /// The names in the object's directory on this machine.
    fn local_names(machine: &Machine, id: ObjectId) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(machine.collection.object_dir(id))
            .expect("lists")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        names.sort();
        names
    }

    fn has_pending(machine: &Machine, id: ObjectId) -> bool {
        machine.collection.meta_pending_path(id).exists()
    }

    /// The local payload is never dropped by `sync`.
    fn assert_payload_kept(machine: &Machine, id: ObjectId) {
        assert!(
            local_names(machine, id)
                .iter()
                .any(|name| name.ends_with(".tar.zst")),
            "the payload is still stored here"
        );
    }

    #[test]
    fn an_imported_archive_is_uploaded_and_committed() {
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();

        let report = anna.sync_with(&remote);

        assert_eq!(report.committed, [CommittedVersion { id, version: 1 }]);
        assert_eq!(report.uploaded.len(), 2, "{:?}", report.uploaded);
        assert_eq!(
            remote.count(Op::PutPayload),
            2,
            "the commit does not put the files again"
        );
        assert!(!report.partial());
        assert!(!has_pending(&anna, id));
        let local = fs::read(anna.store().meta_path(id, 1)).expect("promoted");
        assert_eq!(local, shared.read(id, "meta.1"));
        let entry = anna.entry(id);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
        assert_eq!(entry.committed_versions, [1]);
        assert_payload_kept(&anna, id);
        // The upload is not confirmed until the tier phase verifies the
        // remote copy, which is what clears the flag.
        let shown = status::status(&anna.collection, &anna.clock).expect("status");
        assert!(shown.pieces[0].flags.contains(&Flag::PendingUpload));
    }

    #[test]
    fn a_change_to_the_metadata_alone_commits_without_an_upload() {
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        anna.sync();
        anna.record_history(id, HistoryAction::Sync);
        let remote = anna.remote();

        let report = anna.sync_with(&remote);

        assert_eq!(report.committed, [CommittedVersion { id, version: 2 }]);
        assert!(report.uploaded.is_empty());
        assert_eq!(remote.count(Op::PutPayload), 0);
        assert_eq!(remote.count(Op::Flush), 2, "only the commit's own flushes");
        assert!(shared.names(id).contains(&"meta.2".to_string()));
    }

    #[test]
    fn history_entries_get_the_commit_time() {
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        anna.sync();
        anna.record_history(id, HistoryAction::Sync);
        anna.clock.advance(jiff::SignedDuration::from_hours(23));
        anna.sync();
        let committed = crate::meta::parse(&shared.read(id, "meta.2")).expect("parses");
        let last = committed.history.last().expect("an entry");
        assert_eq!(last.time.to_string(), "2026-11-05T09:00:00Z");
        let first = &committed.history[0];
        assert_eq!(first.time.to_string(), "2026-11-04T10:00:00Z", "kept");
    }

    /// The metadata bytes another machine commits as version `number` of
    /// `id`: the current document with one more history entry.
    fn higher_version(machine: &Machine, shared: &Shared, id: ObjectId, from: u32) -> Vec<u8> {
        let mut document = crate::meta::parse(&shared.read(id, &format!("meta.{from}")))
            .expect("the version parses");
        document.history.push(crate::meta::HistoryEntry::new(
            machine.clock.now(),
            "ben",
            HistoryAction::Sync,
        ));
        crate::meta::to_bytes(&document).expect("serialises")
    }

    /// What `ben` does to commit `bytes` as version `number`.
    fn commit_as_ben(
        inner: &crate::remote::path::PathRemote,
        id: ObjectId,
        number: u32,
        bytes: &[u8],
    ) {
        use crate::remote::Remote;
        let temp = format!(".meta.{number}.ben.part");
        inner.write_temp(id, &temp, bytes).expect("writes");
        inner
            .rename_into_place(id, &temp, &format!("meta.{number}"))
            .expect("renames");
    }

    #[test]
    fn a_failed_upload_keeps_the_change_and_the_other_objects_commit() {
        let (shared, anna, _) = anna_and_ben();
        let first = anna.import_piece("first", "one");
        let second = anna.import_piece("second", "two");
        let (failing, fine) = if first < second {
            (first, second)
        } else {
            (second, first)
        };
        let remote = anna.remote();
        remote.fail(Op::PutPayload, 1, Fault::Io);

        let report = anna.sync_with(&remote);

        assert_eq!(
            report.committed,
            [CommittedVersion {
                id: fine,
                version: 1
            }]
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].id, failing);
        assert_eq!(report.skipped[0].reason, SkipReason::Error);
        assert!(report.partial());
        assert!(has_pending(&anna, failing));
        assert_payload_kept(&anna, failing);
        assert_eq!(
            remote.count(Op::CreateExclusive),
            1,
            "no claim for the failed object"
        );
        assert!(
            !shared
                .names(failing)
                .iter()
                .any(|name| name.starts_with("meta."))
        );
    }

    #[test]
    fn the_temp_names_of_uploads_that_wait_survive_the_cleanup() {
        use crate::remote::testing::Fault as F;
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::PutPayload, 1, F::Io);
        let now: jiff::Timestamp = "2026-11-10T00:00:00Z".parse().expect("a time");
        remote.set_clock(now);
        let payload = local_names(&anna, id)
            .into_iter()
            .find(|name| name.ends_with(".tar.zst"))
            .expect("a payload");
        let dir = shared.objects().join(id.to_string());
        fs::create_dir_all(&dir).expect("dir");
        let kept = format!(".{payload}.anna.part");
        for name in [kept.as_str(), ".0123456789abcdef-stray.tar.zst.anna.part"] {
            fs::write(dir.join(name), b"partial").expect("planted");
            let old = filetime::FileTime::from_unix_time(now.as_second() - 400 * 3600, 0);
            filetime::set_file_mtime(dir.join(name), old).expect("aged");
        }

        let report = anna.sync_with(&remote);

        let names = shared.names(id);
        assert!(names.contains(&kept), "{names:?}");
        assert!(
            !names.contains(&".0123456789abcdef-stray.tar.zst.anna.part".to_string()),
            "{names:?}"
        );
        assert_eq!(report.cleanup.removed, 1);
    }

    #[test]
    fn an_away_answer_to_a_put_while_the_remote_is_gone_ends_the_run() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::PutPayload, 1, Fault::Away);
        remote.fail(Op::Reachability, 2, Fault::Away);

        let error = sync_with(&anna.collection, &anna.clock, &remote, false)
            .expect_err("the remote is gone");

        assert_eq!(error.kind(), ErrorKind::RemoteAway);
        assert!(has_pending(&anna, id));
        assert!(
            !anna
                .collection
                .remote_state_path()
                .to_string_lossy()
                .is_empty()
        );
        let state = fs::read_to_string(anna.collection.remote_state_path()).expect("state");
        assert!(!state.contains("last_sync"), "{state}");
        assert_payload_kept(&anna, id);
    }

    #[test]
    fn an_away_answer_to_a_put_while_the_remote_is_reachable_skips_the_object() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::PutPayload, 1, Fault::Away);

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Error);
        assert!(has_pending(&anna, id));
    }

    #[test]
    fn a_flush_that_fails_after_the_uploads_leaves_the_objects_pending() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::Flush, 1, Fault::Io);

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert!(
            report.skipped[0].message.contains("durable"),
            "{:?}",
            report.skipped
        );
        assert!(has_pending(&anna, id));
        assert_eq!(remote.count(Op::CreateExclusive), 0);
    }

    #[test]
    fn a_stop_between_the_tracking_and_the_promotion_is_settled_by_the_next_run() {
        use std::os::unix::fs::PermissionsExt;
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let dir = anna.collection.object_dir(id);
        let locked = dir.clone();
        let remote = anna.remote();
        remote.after(Op::RenameIntoPlace, 1, move |_| {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("read-only");
        });

        let report = anna.sync_with(&remote);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("restored");

        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
        assert_eq!(report.skipped[0].reason, SkipReason::Error);
        assert!(has_pending(&anna, id), "the file could not be renamed");
        let entry = anna.entry(id);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
        assert_eq!(entry.committed_versions, [1]);

        let again = anna.sync();

        assert_eq!(again.committed, [CommittedVersion { id, version: 1 }]);
        assert!(!has_pending(&anna, id));
        assert_eq!(
            fs::read(anna.store().meta_path(id, 1)).expect("promoted"),
            shared.read(id, "meta.1")
        );
    }

    #[test]
    fn a_version_that_was_overtaken_is_a_conflict_and_keeps_everything() {
        let (shared, anna, _ben) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        let other = {
            // The bytes of a document of the same object that `ben` commits
            // as version 2, right after the rename of version 1.
            let mut document = crate::meta::parse(
                &fs::read(anna.collection.meta_pending_path(id)).expect("pending"),
            )
            .expect("parses");
            document.history[0].machine = "ben".to_string();
            crate::meta::to_bytes(&document).expect("serialises")
        };
        remote.after(Op::RenameIntoPlace, 1, move |inner| {
            commit_as_ben(inner, id, 2, &other);
        });

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Conflict);
        assert!(report.committed.is_empty());
        assert!(has_pending(&anna, id));
        assert_payload_kept(&anna, id);
        let entry = anna.entry(id);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
        assert_eq!(entry.committed_versions, [1]);
        assert_eq!(
            entry
                .pending_decisions
                .iter()
                .map(|d| d.kind)
                .collect::<Vec<_>>(),
            [DecisionKind::VersionConflict]
        );
        assert!(shared.names(id).contains(&"meta.2".to_string()));

        // The open decision keeps the next run from committing again.
        let again = anna.sync_with(&anna.remote());
        assert_eq!(again.skipped[0].reason, SkipReason::Conflict);
        assert_eq!(again.uploaded.len(), 0);
    }

    #[test]
    fn a_version_that_was_overtaken_by_one_built_on_it_is_settled_and_the_decision_goes() {
        let (_shared, anna, ben) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        let other = {
            let mut document = crate::meta::parse(
                &fs::read(anna.collection.meta_pending_path(id)).expect("pending"),
            )
            .expect("parses");
            document.history.push(crate::meta::HistoryEntry::new(
                ben.clock.now(),
                "ben",
                HistoryAction::Sync,
            ));
            crate::meta::to_bytes(&document).expect("serialises")
        };
        remote.after(Op::RenameIntoPlace, 1, move |inner| {
            commit_as_ben(inner, id, 2, &other);
        });
        let first = anna.sync_with(&remote);
        assert_eq!(first.skipped[0].reason, SkipReason::Conflict);

        let again = anna.sync();

        assert_eq!(again.committed, [CommittedVersion { id, version: 1 }]);
        assert!(!has_pending(&anna, id));
        let entry = anna.entry(id);
        assert!(
            entry.pending_decisions.is_empty(),
            "{:?}",
            entry.pending_decisions
        );
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
    }

    #[test]
    fn a_claim_above_the_version_makes_it_contested_and_the_next_run_settles_it() {
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.after(Op::RenameIntoPlace, 1, move |inner| {
            use crate::remote::Remote;
            inner.create_exclusive(id, "meta.2.claim").expect("a claim");
        });

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Contested);
        assert!(has_pending(&anna, id));
        let entry = anna.entry(id);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
        assert!(entry.pending_decisions.is_empty());
        assert_payload_kept(&anna, id);

        fs::remove_file(shared.objects().join(id.to_string()).join("meta.2.claim"))
            .expect("released");
        let again = anna.sync();

        assert_eq!(again.committed, [CommittedVersion { id, version: 1 }]);
        assert!(!has_pending(&anna, id));
    }

    #[test]
    fn a_retry_that_finds_a_higher_version_naming_its_files_is_settled_after_a_pull() {
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        let pending_path = anna.collection.meta_pending_path(id);
        let clock_now = anna.clock.now();
        let shared_dir = shared.objects();
        // Before the commit looks at the remote, an earlier attempt's version
        // and a version built on it are there.
        remote.before(Op::ListDir, 1, move |inner| {
            use crate::remote::Remote;
            let mut document =
                crate::meta::parse(&fs::read(&pending_path).expect("pending")).expect("parses");
            document.stamp_committed(0, clock_now);
            let ours = crate::meta::to_bytes(&document).expect("serialises");
            document.history.push(crate::meta::HistoryEntry::new(
                clock_now,
                "ben",
                HistoryAction::Sync,
            ));
            let theirs = crate::meta::to_bytes(&document).expect("serialises");
            let _ = &shared_dir;
            commit_as_ben(inner, id, 1, &ours);
            commit_as_ben(inner, id, 2, &theirs);
            let _ = inner.flush();
        });

        let report = anna.sync_with(&remote);

        assert_eq!(report.committed, [CommittedVersion { id, version: 1 }]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(!has_pending(&anna, id));
        assert!(
            anna.store().meta_path(id, 2).exists(),
            "the second pull stored version 2"
        );
    }

    #[test]
    fn a_retry_that_finds_a_higher_version_is_left_alone_when_the_second_pull_fails() {
        let (shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        let pending_path = anna.collection.meta_pending_path(id);
        let clock_now = anna.clock.now();
        let shared_dir = shared.objects();
        // Before the commit looks at the remote, an earlier attempt's version
        // and a version built on it are there.
        remote.before(Op::ListDir, 1, move |inner| {
            use crate::remote::Remote;
            let mut document =
                crate::meta::parse(&fs::read(&pending_path).expect("pending")).expect("parses");
            document.stamp_committed(0, clock_now);
            let ours = crate::meta::to_bytes(&document).expect("serialises");
            document.history.push(crate::meta::HistoryEntry::new(
                clock_now,
                "ben",
                HistoryAction::Sync,
            ));
            let theirs = crate::meta::to_bytes(&document).expect("serialises");
            let _ = &shared_dir;
            commit_as_ben(inner, id, 1, &ours);
            commit_as_ben(inner, id, 2, &theirs);
            let _ = inner.flush();
        });

        let report = anna.sync_with(&remote);

        assert_eq!(report.committed, [CommittedVersion { id, version: 1 }]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(!has_pending(&anna, id));
        assert!(
            anna.store().meta_path(id, 2).exists(),
            "the second pull stored version 2"
        );
    }

    #[test]
    fn a_commit_that_is_refused_keeps_the_change() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.before(Op::ListDir, 1, move |inner| {
            use crate::remote::Remote;
            inner
                .create_exclusive(id, "meta.1.claim")
                .expect("a foreign claim");
        });

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Refused);
        assert!(report.skipped[0].message.contains("ClaimOpen"));
        assert!(has_pending(&anna, id));
        assert_payload_kept(&anna, id);
    }

    #[test]
    fn a_version_that_appears_after_the_pull_is_refused_as_behind() {
        let (shared, anna, ben) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let other = {
            let mut document = crate::meta::parse(
                &fs::read(anna.collection.meta_pending_path(id)).expect("pending"),
            )
            .expect("parses");
            document.history.push(crate::meta::HistoryEntry::new(
                ben.clock.now(),
                "ben",
                HistoryAction::Sync,
            ));
            crate::meta::to_bytes(&document).expect("serialises")
        };
        let remote = anna.remote();
        remote.before(Op::ListDir, 1, move |inner| {
            commit_as_ben(inner, id, 1, &other);
        });

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped[0].reason, SkipReason::Refused);
        assert!(report.skipped[0].message.contains("Behind"));
        assert!(has_pending(&anna, id));
        assert_eq!(
            shared
                .names(id)
                .iter()
                .filter(|n| n.ends_with(".claim"))
                .count(),
            0
        );
    }

    #[test]
    fn a_change_that_does_not_build_on_the_newest_version_is_a_conflict_and_never_asks_for_a_claim()
    {
        let (shared, anna, ben) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        anna.sync();
        ben.sync();
        ben.record_history(id, HistoryAction::Unearth);
        let higher = higher_version(&anna, &shared, id, 1);
        let dir = shared.objects().join(id.to_string());
        fs::write(dir.join("meta.2"), higher).expect("a version from another machine");
        let remote = ben.remote();

        let report = ben.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Conflict);
        assert_eq!(remote.count(Op::CreateExclusive), 0);
        assert!(has_pending(&ben, id));
    }

    #[test]
    fn a_pull_that_failed_leaves_every_change_alone_and_the_cleanup_still_runs() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::PullMetadata, 1, Fault::Io);
        remote.fail(Op::PullMetadata, 2, Fault::Io);

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Pull);
        assert!(has_pending(&anna, id));
        assert_eq!(remote.count(Op::PutPayload), 0);
        assert_eq!(remote.count(Op::CreateExclusive), 0);
        assert!(remote.count(Op::ListObjects) >= 1, "the cleanup ran");
    }

    #[test]
    fn the_temp_names_of_a_conflicted_object_survive_the_cleanup_too() {
        let (shared, anna, ben) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        anna.sync();
        ben.sync();
        ben.record_history(id, HistoryAction::Unearth);
        let dir = shared.objects().join(id.to_string());
        fs::write(dir.join("meta.2"), higher_version(&anna, &shared, id, 1)).expect("a version");
        let remote = ben.remote();
        let now: jiff::Timestamp = "2026-11-10T00:00:00Z".parse().expect("a time");
        remote.set_clock(now);

        let pending =
            crate::meta::parse(&fs::read(ben.collection.meta_pending_path(id)).expect("pending"))
                .expect("parses");
        // The change adds a payload that is not on the remote.
        let mut with_new = pending.clone();
        let mut extra = with_new.payloads[0].clone();
        extra.file = crate::meta::FileName::new("ffffffffffffffff-new.tar.zst").expect("a name");
        extra.listing = None;
        with_new.payloads.push(extra);
        let lock =
            crate::lock::acquire(&ben.collection.lock_path(), "ben", &ben.clock).expect("free");
        ben.store()
            .write_pending(&lock, &with_new)
            .expect("recorded");
        drop(lock);
        fs::write(
            ben.collection
                .object_dir(id)
                .join("ffffffffffffffff-new.tar.zst"),
            b"data",
        )
        .expect("a payload");
        let kept = ".ffffffffffffffff-new.tar.zst.ben.part";
        fs::write(dir.join(kept), b"partial").expect("planted");
        let old = filetime::FileTime::from_unix_time(now.as_second() - 400 * 3600, 0);
        filetime::set_file_mtime(dir.join(kept), old).expect("aged");

        let report = ben.sync_with(&remote);

        assert_eq!(report.skipped[0].reason, SkipReason::Conflict);
        assert!(
            shared.names(id).contains(&kept.to_string()),
            "{:?}",
            shared.names(id)
        );
    }

    #[test]
    fn the_uploads_of_several_objects_run_at_most_jobs_at_a_time() {
        let shared = Shared::new();
        let anna = Machine::with_jobs(&shared, "anna", 2);
        for name in ["a", "b", "c", "d"] {
            anna.import_piece(name, name);
        }
        let remote = anna.remote();
        remote.gate(Op::PutPayload, 2);

        let report = anna.sync_with(&remote);

        assert_eq!(report.committed.len(), 4);
        assert_eq!(remote.peak_in_flight(Op::PutPayload), 2);
    }

    #[test]
    fn a_cleanup_that_fails_is_a_warning_and_makes_the_run_partial() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::Clock, 1, Fault::Io);

        let report = anna.sync_with(&remote);

        assert!(report.partial());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.message.starts_with("cleaning the remote failed"))
        );
        assert_eq!(report.committed.len(), 1, "the commit is not undone");
        assert!(!has_pending(&anna, id));
    }

    #[test]
    fn a_cleanup_that_finds_the_remote_gone_ends_the_run() {
        let (_shared, anna, _) = anna_and_ben();
        anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::Clock, 1, Fault::Away);
        remote.fail(Op::Reachability, 2, Fault::Away);

        let error = sync_with(&anna.collection, &anna.clock, &remote, false)
            .expect_err("the remote is gone");

        assert!(
            matches!(error, Error::Remote(RemoteError::Away { .. })),
            "{error:?}"
        );
        let state = fs::read_to_string(anna.collection.remote_state_path()).expect("state");
        assert!(!state.contains("last_sync"));
    }

    #[test]
    fn an_open_decision_keeps_a_change_from_being_uploaded_or_committed() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let mut entry = anna.entry(id);
        entry.decide(DecisionKind::VersionConflict);
        let lock =
            crate::lock::acquire(&anna.collection.lock_path(), "anna", &anna.clock).expect("free");
        tracking::write(&anna.collection, &lock, &anna.clock, &mut entry).expect("written");
        drop(lock);
        let remote = anna.remote();

        let report = anna.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Conflict);
        assert_eq!(remote.count(Op::PutPayload), 0);
        assert_eq!(remote.count(Op::CreateExclusive), 0);
        assert!(has_pending(&anna, id));
    }

    #[test]
    fn an_object_the_pull_left_alone_is_not_committed() {
        let (shared, anna, ben) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        anna.sync();
        ben.sync();
        ben.record_history(id, HistoryAction::Unearth);
        // The listing is gone locally and unusable on the remote.
        for dir in [
            ben.collection.object_dir(id),
            shared.objects().join(id.to_string()),
        ] {
            for entry in fs::read_dir(dir).expect("lists") {
                let path = entry.expect("entry").path();
                if path.to_string_lossy().ends_with(".listing.zst") {
                    if path.starts_with(ben.collection.object_dir(id)) {
                        fs::remove_file(&path).expect("removed");
                    } else {
                        fs::write(&path, b"not a listing").expect("overwritten");
                    }
                }
            }
        }
        let remote = ben.remote();

        let report = ben.sync_with(&remote);

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Pull);
        assert_eq!(remote.count(Op::CreateExclusive), 0);
        assert!(has_pending(&ben, id));
    }

    #[test]
    fn a_flush_after_the_uploads_that_finds_the_remote_gone_ends_the_run() {
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let remote = anna.remote();
        remote.fail(Op::Flush, 1, Fault::Away);
        remote.fail(Op::Reachability, 2, Fault::Away);

        let error = sync_with(&anna.collection, &anna.clock, &remote, false)
            .expect_err("the remote is gone");

        assert_eq!(error.kind(), ErrorKind::RemoteAway);
        assert!(has_pending(&anna, id));
    }

    #[test]
    fn a_recorded_change_that_cannot_be_read_skips_its_object_and_the_others_commit() {
        let (_shared, anna, _) = anna_and_ben();
        let broken = anna.import_piece("broken", "one");
        let fine = anna.import_piece("fine", "two");
        fs::write(anna.collection.meta_pending_path(broken), b"not metadata").expect("damaged");

        let report = anna.sync();

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].id, broken);
        assert_eq!(report.skipped[0].reason, SkipReason::Error);
        assert_eq!(report.committed.len(), 1);
        assert_eq!(report.committed[0].id, fine);
        assert_payload_kept(&anna, broken);
    }

    #[test]
    fn a_recorded_change_that_cannot_be_opened_skips_its_object() {
        use std::os::unix::fs::PermissionsExt;
        let (_shared, anna, _) = anna_and_ben();
        let id = anna.import_piece("notes", "hello");
        let pending = anna.collection.meta_pending_path(id);
        fs::set_permissions(&pending, fs::Permissions::from_mode(0o000)).expect("closed");
        assert!(
            fs::read(&pending).is_err(),
            "the test runs as a user the permissions bind"
        );

        let report = anna.sync();
        fs::set_permissions(&pending, fs::Permissions::from_mode(0o644)).expect("restored");

        assert_eq!(report.skipped.len(), 1);
        assert!(
            report.skipped[0].message.contains("cannot read"),
            "{:?}",
            report.skipped
        );
    }
}
