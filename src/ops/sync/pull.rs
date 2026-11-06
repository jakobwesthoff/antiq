// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pulling metadata and the listings it names.

use super::commit::Versions;
use super::{
    PulledVersion, SkipReason, SyncAction, SyncContext, SyncError, SyncReport, ends_run, skip,
};
use crate::collection::Collection;
use crate::id::ObjectId;
use crate::index;
use crate::listing;
use crate::meta::{self, FileName, Metadata, PayloadKind};
use crate::pack::hashing::hex;
use crate::plan::Plan;
use crate::remote::{KnownVersions, MetaFile, Remote, RemoteError};
use crate::report::Warning;
use crate::store::Store;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

/// How the read of the remote's metadata ended when it failed.
pub(super) enum PullFailure {
    /// The remote is gone, which ends the run.
    RunEnds(RemoteError),
    /// The read failed twice and the remote is reachable.
    Failed(RemoteError),
}

/// Asks the remote for the versions this machine lacks. A failed read is
/// asked once more, since the second read often passes where a share was
/// only slow to answer. A failure that says the remote is gone is not.
pub(super) fn pull_files(
    remote: &dyn Remote,
    known: &KnownVersions,
) -> Result<Vec<MetaFile>, PullFailure> {
    let first = match remote.pull_metadata(known) {
        Ok(files) => return Ok(files),
        Err(error) => error,
    };
    if ends_run(remote, &first) {
        return Err(PullFailure::RunEnds(first));
    }
    remote.pull_metadata(known).map_err(|error| {
        if ends_run(remote, &error) {
            PullFailure::RunEnds(error)
        } else {
            PullFailure::Failed(error)
        }
    })
}

/// Pulls the metadata the remote has beyond this machine's, fetches the
/// listings the current metadata names and updates the member cache.
/// Returns whether the remote's metadata could not be read, in which case
/// nothing that depends on what other machines committed may be committed.
pub(super) fn run(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<bool, SyncError> {
    let known = crate::tombstone::known_versions(ctx.collection)?;
    let files = match pull_files(ctx.remote, &known) {
        Ok(files) => files,
        Err(PullFailure::RunEnds(error)) => return Err(error.into()),
        Err(PullFailure::Failed(error)) => {
            report.warnings.push(Warning::new(format!(
                "cannot read the metadata on the remote: {error}"
            )));
            return Ok(true);
        }
    };
    store_pulled(ctx, files, report);
    fetch_listings_and_index(ctx, report)?;
    Ok(false)
}

/// Fetches the missing listings and brings the member cache up to date.
/// A change recorded as committed stops hiding the newest version, so the
/// commit phase runs this again for the archives that version names.
pub(super) fn fetch_listings_and_index(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
) -> Result<(), SyncError> {
    fetch_listings(ctx, report)?;
    index::update(ctx.collection, ctx.lock)?;
    Ok(())
}

/// Stores the pulled versions. A version whose number is stored with other
/// bytes, or that cannot be stored, leaves its object alone for this run:
/// a committed version never changes, so one of the two copies is wrong.
pub(super) fn store_pulled(ctx: &SyncContext<'_>, files: Vec<MetaFile>, report: &mut SyncReport) {
    let store = Store::new(ctx.collection);
    let mut blocked: BTreeSet<ObjectId> = BTreeSet::new();
    for file in files {
        if blocked.contains(&file.id) {
            continue;
        }
        match store.store_committed(ctx.lock, file.id, file.number, &file.bytes) {
            Ok(()) => report.pulled.push(PulledVersion {
                id: file.id,
                version: file.number.get(),
            }),
            Err(error) => {
                blocked.insert(file.id);
                skip(
                    ctx.collection,
                    report,
                    file.id,
                    SkipReason::Pull,
                    error.to_string(),
                );
            }
        }
    }
}

/// The listings of the archives `metadata` names that are not stored here.
pub(super) fn missing_listings(
    collection: &Collection,
    id: ObjectId,
    metadata: &Metadata,
) -> Vec<FileName> {
    metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Archive)
        .filter_map(|payload| payload.listing.clone())
        .filter(|name| !collection.object_dir(id).join(name.as_str()).exists())
        .collect()
}

/// Fetches the listing of every archive whose listing is not stored here.
fn fetch_listings(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<(), SyncError> {
    let store = Store::new(ctx.collection);
    for id in store.objects()? {
        // An object whose metadata cannot be read stays visible in
        // `status`; there is no listing to name for it.
        let Ok(Some(current)) = store.current(id) else {
            continue;
        };
        for name in missing_listings(ctx.collection, id, &current.metadata) {
            match fetch_listing(ctx, id, &name) {
                Ok(()) => {
                    if !report.listings.contains(&id) {
                        report.listings.push(id);
                    }
                }
                Err(SyncError::Remote(error)) if ends_run(ctx.remote, &error) => {
                    return Err(error.into());
                }
                Err(error) => {
                    skip(
                        ctx.collection,
                        report,
                        id,
                        SkipReason::Pull,
                        error.to_string(),
                    );
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Reads the listing from the remote, checks that it parses as a whole and
/// stores it under its name.
fn fetch_listing(ctx: &SyncContext<'_>, id: ObjectId, name: &FileName) -> Result<(), SyncError> {
    let bytes = ctx.remote.read_file(id, name.as_str())?;
    listing::read_all(&bytes[..])?;

    let tmp_dir = ctx.collection.tmp_dir();
    fs::create_dir_all(&tmp_dir).map_err(|source| SyncError::Io {
        action: "create",
        path: tmp_dir.display().to_string(),
        source,
    })?;
    let tmp = tmp_dir.join(format!("sync-{id}-{}.part", name.as_str()));
    fs::write(&tmp, &bytes).map_err(|source| SyncError::Io {
        action: "write",
        path: tmp.display().to_string(),
        source,
    })?;
    let stored = Store::new(ctx.collection).put_file(
        ctx.lock,
        id,
        name,
        &tmp,
        &hex(&Sha256::digest(&bytes)),
    );
    // A stored file was moved away from `tmp`; any other outcome leaves it.
    let _ = fs::remove_file(&tmp);
    stored?;
    Ok(())
}

/// The pull part of a dry run: the versions that would be pulled and the
/// listings that would be fetched, judged on the versions as they would be
/// stored.
///
/// A read of the remote's metadata that fails twice is a note and nothing
/// is planned for the metadata, as the real run pulls nothing and commits
/// nothing then; the result is `None`.
pub(super) fn plan_pull(
    collection: &Collection,
    remote: &dyn Remote,
    plan: &mut Plan<SyncAction>,
    notes: &mut Vec<Warning>,
) -> Result<Option<BTreeMap<ObjectId, Versions>>, SyncError> {
    let store = Store::new(collection);
    let files = match pull_files(remote, &crate::tombstone::known_versions(collection)?) {
        Ok(files) => files,
        Err(PullFailure::RunEnds(error)) => return Err(error.into()),
        Err(PullFailure::Failed(error)) => {
            notes.push(Warning::new(format!(
                "cannot read the metadata on the remote: {error}"
            )));
            return Ok(None);
        }
    };
    let mut would_pull: BTreeMap<ObjectId, Versions> = BTreeMap::new();
    for file in &files {
        would_pull
            .entry(file.id)
            .or_default()
            .insert(file.number.get(), file.bytes.clone());
    }
    let mut highest: BTreeMap<ObjectId, &MetaFile> = BTreeMap::new();
    for file in &files {
        plan.push(SyncAction::PullMetadata {
            id: file.id,
            version: file.number.get(),
        });
        if highest
            .get(&file.id)
            .is_none_or(|seen| seen.number < file.number)
        {
            highest.insert(file.id, file);
        }
    }

    let mut ids: BTreeSet<ObjectId> = store.objects()?.into_iter().collect();
    ids.extend(highest.keys().copied());
    for id in ids {
        let pending = collection.meta_pending_path(id).exists();
        let pulled = highest
            .get(&id)
            .filter(|_| !pending)
            .and_then(|file| meta::parse(&file.bytes).ok());
        let metadata = match pulled {
            Some(metadata) => metadata,
            None => match store.current(id) {
                Ok(Some(current)) => current.metadata,
                _ => continue,
            },
        };
        for name in missing_listings(collection, id, &metadata) {
            plan.push(SyncAction::FetchListing {
                id,
                file: name.as_str().to_string(),
            });
        }
    }
    Ok(Some(would_pull))
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{Machine, Shared};
    use super::*;
    use crate::lock;
    use crate::remote::testing::object;
    use std::num::NonZeroU32;

    #[test]
    fn a_version_stored_with_other_bytes_leaves_its_object_alone() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let lock = lock::acquire(&ben.collection.lock_path(), "ben", &ben.clock).expect("free");
        let remote = ben.remote();
        let ctx = SyncContext {
            collection: &ben.collection,
            remote: &remote,
            lock: &lock,
            clock: &ben.clock,
            machine: "ben",
            ids: &ben.ids,
            pool: &ben.pool,
            jobs: 1,
        };
        let id = object(3);
        let one = NonZeroU32::new(1).expect("nonzero");
        let two = NonZeroU32::new(2).expect("nonzero");
        let file = |number, bytes: &[u8]| MetaFile {
            id,
            number,
            bytes: bytes.to_vec(),
        };
        let mut first = SyncReport::default();
        store_pulled(&ctx, vec![file(one, b"{\"a\":1}")], &mut first);
        assert_eq!(first.pulled.len(), 1);

        let mut again = SyncReport::default();
        store_pulled(
            &ctx,
            vec![file(one, b"{\"a\":2}"), file(two, b"{\"a\":3}")],
            &mut again,
        );

        assert!(again.pulled.is_empty(), "version 2 is not stored either");
        assert_eq!(again.skipped.len(), 1);
        assert_eq!(again.skipped[0].reason, SkipReason::Pull);
        assert!(again.skipped[0].message.contains("never changes"));
        assert_eq!(ben.store().committed_versions(id).expect("lists"), vec![1]);
    }

    fn two_machines_with_a_listing() -> (Shared, Machine, ObjectId) {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let ben = Machine::new(&shared, "ben");
        let id = anna.import_piece("notes", "hello");
        anna.publish_by_hand(&shared, id, 1);
        (shared, ben, id)
    }

    #[test]
    fn a_temp_directory_that_cannot_be_made_skips_the_object() {
        let (_shared, ben, id) = two_machines_with_a_listing();
        fs::remove_dir(ben.collection.tmp_dir()).expect("removed");
        fs::write(ben.collection.tmp_dir(), b"a file").expect("written");

        let report =
            match super::super::sync_with(&ben.collection, &ben.clock, &ben.remote(), false)
                .expect("runs")
            {
                super::super::Synced::Done(report) => report,
                other => panic!("{other:?}"),
            };

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].id, id);
        assert!(report.skipped[0].message.contains("cannot create"));
    }

    #[test]
    fn a_temp_file_that_cannot_be_written_skips_the_object() {
        use std::os::unix::fs::PermissionsExt;
        let (_shared, ben, id) = two_machines_with_a_listing();
        let tmp = ben.collection.tmp_dir();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::write(tmp.join("probe"), b"x").is_err(),
            "the test runs as a user the permissions bind"
        );

        let report =
            match super::super::sync_with(&ben.collection, &ben.clock, &ben.remote(), false)
                .expect("runs")
            {
                super::super::Synced::Done(report) => report,
                other => panic!("{other:?}"),
            };
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).expect("restored");

        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].id, id);
        assert!(report.skipped[0].message.contains("cannot write"));
    }

    #[test]
    fn a_dry_run_leaves_out_an_object_whose_metadata_cannot_be_read() {
        let shared = Shared::new();
        let ben = Machine::new(&shared, "ben");
        let id = object(7);
        let dir = shared.objects().join(id.to_string());
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("meta.1"), br#"{"format": 99}"#).expect("written");
        super::super::sync_with(&ben.collection, &ben.clock, &ben.remote(), false).expect("pulls");

        let plan = match super::super::sync_with(&ben.collection, &ben.clock, &ben.remote(), true)
            .expect("plans")
        {
            super::super::Synced::Planned { plan, .. } => plan,
            other => panic!("{other:?}"),
        };

        assert!(plan.is_empty());
    }

    /// Records the pruning of `id` up to `version` on `ben`.
    fn prune(ben: &Machine, id: ObjectId, version: u32) {
        let lock = lock::acquire(&ben.collection.lock_path(), "ben", &ben.clock).expect("free");
        let mut tombstones = crate::tombstone::Tombstones::default();
        tombstones.record(id, version);
        tombstones.write(&ben.collection, &lock).expect("written");
    }

    fn pulled_by_a_real_run(ben: &Machine) -> Vec<(ObjectId, u32)> {
        match super::super::sync_with(&ben.collection, &ben.clock, &ben.remote(), false)
            .expect("runs")
        {
            super::super::Synced::Done(report) => report
                .pulled
                .iter()
                .map(|pulled| (pulled.id, pulled.version))
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_pruned_object_is_not_pulled_back() {
        let (_shared, ben, id) = two_machines_with_a_listing();
        prune(&ben, id, 1);

        assert_eq!(pulled_by_a_real_run(&ben), vec![]);
        assert!(
            ben.store()
                .committed_versions(id)
                .expect("lists")
                .is_empty()
        );
    }

    #[test]
    fn a_newer_version_lifts_the_record_of_a_pruned_object() {
        let (shared, ben, id) = two_machines_with_a_listing();
        let there = shared.objects().join(id.to_string());
        fs::copy(there.join("meta.1"), there.join("meta.2")).expect("a newer version");
        prune(&ben, id, 1);

        assert_eq!(pulled_by_a_real_run(&ben), vec![(id, 2)]);
    }

    #[test]
    fn a_dry_run_does_not_plan_to_pull_a_pruned_object_back() {
        let (_shared, ben, id) = two_machines_with_a_listing();
        prune(&ben, id, 1);

        let plan = match super::super::sync_with(&ben.collection, &ben.clock, &ben.remote(), true)
            .expect("plans")
        {
            super::super::Synced::Planned { plan, .. } => plan,
            other => panic!("{other:?}"),
        };

        assert!(plan.is_empty(), "{:?}", plan.dry_run());
    }
}
