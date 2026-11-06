// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The remote part of `reconcile`: look at the remote, pull the metadata
//! this machine lacks, mark the pieces whose archive is gone from the
//! remote, remove leftovers and name the versions other machines wrote.

use super::{
    ChangedElsewhere, Opener, PieceRef, ReconcileAction, ReconcileError, RemoteReport,
    UnreadableVersion,
};
use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::Error;
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::meta;
use crate::ops::status::{current_archive, has_pending_remote_work, is_pending_upload, status};
use crate::relpath::{Name, temp_name};
use crate::remote::cleanup::{self, Cleanup, Mode, Settings};
use crate::remote::{Reachability, Remote, RemoteError, away};
use crate::report::Warning;
use crate::store::{Current, Source, Store, StoreError};
use crate::tombstone;
use crate::tracking::{self, TrackingEntry, VersionTakenOut};
use jiff::SignedDuration;
use std::collections::BTreeSet;

/// What the remote part found and did.
#[derive(Default)]
pub(super) struct Part {
    pub remote: Option<RemoteReport>,
    pub marked: Vec<PieceRef>,
    pub unmarked: Vec<PieceRef>,
    pub changed_elsewhere: Vec<ChangedElsewhere>,
    pub unreadable_versions: Vec<UnreadableVersion>,
    pub warnings: Vec<Warning>,
    /// What a dry run lists instead of doing.
    pub actions: Vec<ReconcileAction>,
    /// The remote answered and the marks are as the remote says.
    pub reached: bool,
    /// Why the part stopped. The local part stands.
    pub stop: Option<Error>,
}

/// Runs the remote part. A real run holds `lock`; a dry run (`lock` is
/// `None`) reads the remote and writes nothing.
pub(super) fn run(
    collection: &Collection,
    clock: &dyn Clock,
    opener: Opener,
    lock: Option<&RunLock>,
) -> Part {
    let mut part = Part::default();
    let remote = match opener(collection) {
        Ok(remote) => remote,
        Err(RemoteError::NoRemote) => return part,
        // Whatever stops the remote from opening, the run cannot use it.
        Err(error) => {
            part.stop = Some(RemoteError::Broken(error.to_string()).into());
            return part;
        }
    };
    if let Err(error) = reach(collection, clock, &*remote, lock, &mut part) {
        part.stop = Some(error);
    }
    remote.finish();
    part
}

fn overdue(collection: &Collection, clock: &dyn Clock, error: RemoteError) -> Error {
    away::with_overdue(
        Err::<(), Error>(error.into()),
        collection,
        || {
            status(collection, clock)
                .map(|report| has_pending_remote_work(&report.pieces))
                .unwrap_or(true)
        },
        clock.now(),
    )
    .expect_err("an error stays an error")
}

fn reach(
    collection: &Collection,
    clock: &dyn Clock,
    remote: &dyn Remote,
    lock: Option<&RunLock>,
    part: &mut Part,
) -> Result<(), Error> {
    let standing = match remote.reachability() {
        Ok(assessment) => match assessment.reachability {
            Reachability::Reachable => Ok(()),
            Reachability::Away => Err(RemoteError::Away {
                message: assessment.reason,
                overdue: false,
            }),
            Reachability::Broken => Err(RemoteError::Broken(assessment.reason)),
        },
        Err(error @ (RemoteError::Away { .. } | RemoteError::Broken(_))) => Err(error),
        Err(error) => return Err(error.into()),
    };
    let now = clock.now();
    if let Some(lock) = lock {
        match &standing {
            Ok(()) => {
                away::record_reachable(collection, lock, now).map_err(ReconcileError::Away)?
            }
            Err(RemoteError::Away { .. }) => {
                away::record_away(collection, lock, now).map_err(ReconcileError::Away)?
            }
            Err(_) => {}
        }
    }
    if let Err(error) = standing {
        return Err(overdue(collection, clock, error));
    }

    let store = Store::new(collection);
    let mut pulled = 0;
    if let Some(lock) = lock {
        remote.begin_phase()?;
        let known = tombstone::known_versions(collection)?;
        for file in remote.pull_metadata(&known)? {
            store.store_committed(lock, file.id, file.number, &file.bytes)?;
            pulled += 1;
        }
    }

    let tracked = tracking::read_all(collection)?;
    let listed: BTreeSet<ObjectId> = remote.list_objects()?.into_iter().collect();
    let mut changes = Vec::new();
    let machine = collection.machine_name().map_err(RemoteError::from)?;
    // Every object with a recorded change keeps the temp files of its
    // uploads, as `sync` does.
    let mut keep = crate::ops::sync::commit::pending_temp_names(collection, &machine)?;
    for entry in &tracked.entries {
        let current = match store.current(entry.id) {
            Ok(Some(current)) => current,
            Ok(None) => continue,
            Err(StoreError::NothingReadable { unreadable }) => {
                for version in unreadable {
                    unreadable_note(part, entry.id, version.number, &version.reason);
                }
                continue;
            }
            Err(error) => {
                part.warnings.push(Warning::new(format!(
                    "the metadata of {} cannot be read: {error}",
                    entry.id
                )));
                continue;
            }
        };
        for version in &current.unreadable {
            unreadable_note(part, entry.id, version.number, &version.reason);
        }
        if current.unreadable.is_empty() {
            changed_elsewhere(part, &store, entry, &current)?;
        }
        if is_pending_upload(Some(entry), &current)
            && let Some(archive) = current_archive(&current.metadata)
        {
            for file in std::iter::once(&archive.file).chain(archive.listing.iter()) {
                let name =
                    Name::from_bytes(file.as_str().as_bytes().to_vec()).map_err(Error::from)?;
                keep.insert((entry.id, temp_name(&name, &machine).to_string()));
            }
        }
        if let Some(missing) = archive_missing(remote, &listed, entry, &current)?
            && missing != entry.missing_on_remote
        {
            changes.push((entry.clone(), missing));
        }
    }

    let settings = Settings {
        machine: &machine,
        collection: collection.id(),
        grace: SignedDuration::from_hours(i64::from(collection.config().collection.grace_hours)),
        keep: &keep,
    };
    let mode = match lock {
        Some(lock) => Mode::Run {
            lock,
            now: remote.clock()?,
        },
        None => Mode::DryRun,
    };
    let cleaned = cleanup::plan(remote, &settings, &mode)?;

    for (entry, missing) in &changes {
        let piece = PieceRef {
            id: entry.id,
            path: entry.path.to_string(),
        };
        if *missing {
            part.marked.push(piece);
        } else {
            part.unmarked.push(piece);
        }
    }
    let Cleanup { steps, left_alone } = cleaned.clone();
    match (lock, mode) {
        (Some(lock), Mode::Run { now, .. }) => {
            let applied = cleanup::apply(remote, &settings, lock, now, &cleaned)?;
            for (mut entry, missing) in changes {
                entry.missing_on_remote = missing;
                tracking::write(collection, lock, clock, &mut entry)?;
            }
            remote.flush()?;
            part.reached = true;
            part.remote = Some(RemoteReport {
                pulled,
                removed: applied.removed,
                skipped: applied.skipped,
                left_alone,
            });
        }
        _ => {
            part.reached = true;
            part.actions
                .extend(part.marked.iter().cloned().map(ReconcileAction::from_mark));
            part.actions.extend(
                part.unmarked
                    .iter()
                    .cloned()
                    .map(ReconcileAction::from_unmark),
            );
            part.actions
                .extend(steps.into_iter().map(ReconcileAction::from_step));
        }
    }
    Ok(())
}

fn unreadable_note(part: &mut Part, id: ObjectId, version: u32, reason: &str) {
    part.warnings.push(Warning::new(format!(
        "version {version} of {id} cannot be read: {reason}"
    )));
    part.unreadable_versions.push(UnreadableVersion {
        id,
        version,
        reason: reason.to_string(),
    });
}

/// Whether the current archive of the piece is gone from the remote, for a
/// piece that depends on the remote for it. `None` when the piece does not.
fn archive_missing(
    remote: &dyn Remote,
    listed: &BTreeSet<ObjectId>,
    entry: &TrackingEntry,
    current: &Current,
) -> Result<Option<bool>, RemoteError> {
    if current.source == Source::Pending
        || entry
            .local_payloads
            .iter()
            .any(|held| held.kind == meta::PayloadKind::Archive)
    {
        return Ok(None);
    }
    let Some(archive) = current_archive(&current.metadata) else {
        return Ok(None);
    };
    if !listed.contains(&entry.id) {
        return Ok(Some(true));
    }
    let files = remote.list_dir(entry.id)?;
    Ok(Some(
        !files.iter().any(|file| file.name == archive.file.as_str()),
    ))
}

/// Notes a committed version this machine has not taken out: above the one
/// the entry took out and above every version it lists as committed.
fn changed_elsewhere(
    part: &mut Part,
    store: &Store,
    entry: &TrackingEntry,
    current: &Current,
) -> Result<(), Error> {
    let Some(&newest) = store.committed_versions(entry.id)?.last() else {
        return Ok(());
    };
    let taken = match entry.version_taken_out {
        VersionTakenOut::Committed(number) => number,
        VersionTakenOut::Pending => 0,
    };
    let listed = entry.committed_versions.iter().copied().max().unwrap_or(0);
    if newest <= taken || newest <= listed {
        return Ok(());
    }
    let metadata = match current.source {
        Source::Committed(number) if number == newest => current.metadata.clone(),
        // A recorded change is the current metadata; the committed
        // version is read on its own.
        _ => {
            let path = store.meta_path(entry.id, newest);
            let bytes = std::fs::read(&path).map_err(|source| crate::fsx::FsxError::Io {
                action: "read",
                path: path.display().to_string(),
                source,
            })?;
            meta::parse(&bytes)?
        }
    };
    let machine = metadata
        .history
        .last()
        .map_or_else(|| "unknown".to_string(), |last| last.machine.clone());
    part.changed_elsewhere.push(ChangedElsewhere {
        id: entry.id,
        path: entry.path.to_string(),
        version: newest,
        machine,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{ReconcileRequest, Reconciled, reconcile};
    use super::*;
    use crate::clock::FixedClock;
    use crate::relpath::RelPath;
    use crate::remote::testing::{
        COLLECTION_ID, Fault, FaultRemote, Op, metadata_bytes, object, path_remote, remote_root,
    };
    use crate::testing::tree::TempCollection;
    use crate::tracking::EntryState;
    use std::fs;

    struct World {
        temp: TempCollection,
        remote: tempfile::TempDir,
        collection: Collection,
        clock: FixedClock,
    }

    /// A collection with one archived piece whose archive the remote does
    /// not hold, so that a full run would mark it.
    fn world() -> World {
        let temp = TempCollection::minimal();
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n\n[machine]\nname = \"laptop\"\n"),
        )
        .expect("the config");
        let collection = Collection::find(temp.root()).expect("a collection");
        let remote = remote_root();
        let id = object(1);
        let bytes = metadata_bytes(
            id,
            COLLECTION_ID,
            "alpha",
            &[("0000000000000001-p.tar.zst", "0000000000000001.listing.zst")],
        );
        let local = collection.object_dir(id);
        fs::create_dir_all(&local).expect("dirs");
        fs::write(local.join("meta.1"), &bytes).expect("meta");
        let there = remote.path().join("objects").join(id.to_string());
        fs::create_dir_all(&there).expect("dirs");
        fs::write(there.join("meta.1"), &bytes).expect("meta");
        let mut entry = TrackingEntry::new(
            id,
            RelPath::parse("alpha").expect("a path"),
            EntryState::Gone,
            VersionTakenOut::Committed(1),
            "2026-11-04T12:00:00Z".parse().expect("a time"),
        );
        let clock = FixedClock::new("2026-11-04T12:00:00Z".parse().expect("a time"));
        let lock = crate::lock::acquire(&collection.lock_path(), "laptop", &clock).expect("free");
        tracking::write(&collection, &lock, &clock, &mut entry).expect("written");
        drop(lock);
        World {
            temp,
            remote,
            collection,
            clock,
        }
    }

    fn entry_bytes(world: &World) -> Vec<u8> {
        fs::read(world.collection.tracking_path(object(1))).expect("the entry")
    }

    fn reconcile_with(world: &World, opener: Opener) -> Result<Reconciled, Error> {
        let mut interaction = crate::interact::NonInteractive;
        reconcile(
            &world.collection,
            &world.clock,
            opener,
            crate::brake::BrakeInput {
                force: false,
                interaction: &mut interaction,
            },
            ReconcileRequest::default(),
        )
    }

    fn stop_of(reconciled: Reconciled) -> Error {
        match reconciled {
            Reconciled::Done(report) => report.stop.expect("the remote part stopped"),
            Reconciled::Planned { .. } => panic!("a real run was asked for"),
        }
    }

    #[test]
    fn a_remote_that_cannot_be_opened_is_broken_and_the_local_part_stands() {
        let world = world();
        let opener = |_: &Collection| -> Result<Box<dyn Remote>, RemoteError> {
            Err(RemoteError::Io {
                action: "open",
                path: "the remote".into(),
                source: std::io::Error::from_raw_os_error(5),
            })
        };

        let reconciled = reconcile_with(&world, &opener).expect("the local part runs");

        let Reconciled::Done(report) = reconciled else {
            panic!("a real run was asked for");
        };
        assert_eq!(report.index.written, 0);
        assert!(world.collection.index_dir().is_dir());
        let stop = report.stop.expect("the remote part stopped");
        assert_eq!(stop.kind(), crate::error::ErrorKind::RemoteBroken);
    }

    #[test]
    fn a_failing_listing_stops_the_remote_part_and_changes_nothing() {
        let world = world();
        let before = entry_bytes(&world);
        let root = world.remote.path().to_path_buf();
        let opener = move |_: &Collection| -> Result<Box<dyn Remote>, RemoteError> {
            let remote = FaultRemote::new(path_remote(&root, "laptop"));
            remote.fail(Op::ListObjects, 1, Fault::Io);
            Ok(Box::new(remote))
        };

        let stop = stop_of(reconcile_with(&world, &opener).expect("the local part runs"));

        assert_eq!(stop.kind(), crate::error::ErrorKind::Other);
        assert_eq!(entry_bytes(&world), before);
    }

    #[test]
    fn a_remote_that_fails_the_reachability_check_stops_the_part() {
        let world = world();
        let root = world.remote.path().to_path_buf();
        let opener = move |_: &Collection| -> Result<Box<dyn Remote>, RemoteError> {
            let remote = FaultRemote::new(path_remote(&root, "laptop"));
            remote.fail(Op::Reachability, 1, Fault::Io);
            Ok(Box::new(remote))
        };

        let stop = stop_of(reconcile_with(&world, &opener).expect("the local part runs"));

        assert_eq!(stop.kind(), crate::error::ErrorKind::Other);
    }

    #[test]
    fn a_remote_without_a_listing_failure_marks_the_piece() {
        let world = world();
        let root = world.remote.path().to_path_buf();
        let opener = move |_: &Collection| -> Result<Box<dyn Remote>, RemoteError> {
            Ok(Box::new(path_remote(&root, "laptop")))
        };

        let reconciled = reconcile_with(&world, &opener).expect("runs");

        let Reconciled::Done(report) = reconciled else {
            panic!("a real run was asked for");
        };
        assert!(report.stop.is_none());
        assert_eq!(report.marked.len(), 1);
        let _ = &world.temp;
    }

    fn remote_failing(world: &World, op: Op, fault: Fault) -> Error {
        let root = world.remote.path().to_path_buf();
        let opener = move |_: &Collection| -> Result<Box<dyn Remote>, RemoteError> {
            let remote = FaultRemote::new(path_remote(&root, "laptop"));
            remote.fail(op, 1, fault);
            Ok(Box::new(remote))
        };
        stop_of(reconcile_with(world, &opener).expect("the local part runs"))
    }

    #[test]
    fn a_remote_that_answers_away_ends_the_part_with_the_away_kind() {
        let world = world();
        let stop = remote_failing(&world, Op::Reachability, Fault::Away);
        assert_eq!(stop.kind(), crate::error::ErrorKind::RemoteAway);
    }

    #[test]
    fn a_remote_that_answers_broken_ends_the_part_with_the_broken_kind() {
        let world = world();
        let stop = remote_failing(&world, Op::Reachability, Fault::Broken);
        assert_eq!(stop.kind(), crate::error::ErrorKind::RemoteBroken);
    }

    #[test]
    fn an_entry_without_metadata_is_left_alone() {
        let world = world();
        fs::remove_dir_all(world.collection.object_dir(object(1))).expect("removed");
        fs::remove_dir_all(
            world
                .remote
                .path()
                .join("objects")
                .join(object(1).to_string()),
        )
        .expect("removed there as well");
        let root = world.remote.path().to_path_buf();
        let opener = move |_: &Collection| -> Result<Box<dyn Remote>, RemoteError> {
            Ok(Box::new(path_remote(&root, "laptop")))
        };

        let Reconciled::Done(report) = reconcile_with(&world, &opener).expect("runs") else {
            panic!("a real run was asked for");
        };

        assert!(report.marked.is_empty(), "{:?}", report.marked);
    }

    #[test]
    fn a_recorded_change_that_cannot_be_read_is_a_warning_and_the_run_goes_on() {
        let world = world();
        fs::write(world.collection.meta_pending_path(object(1)), b"{ not json").expect("written");
        let root = world.remote.path().to_path_buf();
        let opener = move |_: &Collection| -> Result<Box<dyn Remote>, RemoteError> {
            Ok(Box::new(path_remote(&root, "laptop")))
        };

        let Reconciled::Done(report) = reconcile_with(&world, &opener).expect("runs") else {
            panic!("a real run was asked for");
        };

        assert!(report.stop.is_none());
        assert!(
            report.warnings.iter().any(|warning| warning
                .message
                .starts_with(&format!("the metadata of {} cannot be read", object(1)))),
            "{:?}",
            report.warnings
        );
    }
}
