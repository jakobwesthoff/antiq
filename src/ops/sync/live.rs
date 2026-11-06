// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live packages during sync.

use super::{
    MintedPiece, SkipReason, SyncAction, SyncContext, SyncError, SyncReport, UnbackedPiece,
    UnbackedReason, skip,
};
use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::device::{self, FsKind};
use crate::fsx::tree::TreeRoot;
use crate::meta::{FORMAT, HistoryAction, HistoryEntry, Lifecycle, Metadata};
use crate::ops::archive::new::id_time_of;
use crate::ops::status::{self, Flag, Piece, PieceKind, StatusRequest, last_name};
use crate::plan::Plan;
use crate::relpath::RelPath;
use crate::remote::RemoteState;
use crate::report::Warning;
use crate::select::Rules;
use crate::store::Store;
use crate::tracking::{self, EntryState, TrackingEntry, VersionTakenOut};
use crate::tree::Problem;
use crate::walk;
use jiff::Timestamp;
use std::path::Path;

// =========================================================
// The pieces a run looks at
// =========================================================

/// What `status` asks the world while `sync` picks its pieces: the real
/// filesystem and no remote. The pull has read the remote already, and an
/// away remote changes nothing about which folders are new.
struct OwnRunProbes;

impl status::Probes for OwnRunProbes {
    fn remote_state(&self, _: &Collection) -> RemoteState {
        RemoteState::None
    }

    fn walk_hooks(&self) -> &dyn walk::Hooks {
        &walk::RealFs
    }

    fn fs_kind(&self, root: &Path) -> Result<FsKind, FsxError> {
        device::fs_kind(root)
    }

    fn away_warning(
        &self,
        _: &Collection,
        _: RemoteState,
        _: bool,
        _: Timestamp,
    ) -> Option<Warning> {
        None
    }
}

/// The pieces `status` shows, in path order.
fn pieces(
    collection: &Collection,
    clock: &dyn Clock,
    own_lock: bool,
) -> Result<Vec<Piece>, SyncError> {
    Ok(status::status_with(&StatusRequest {
        collection,
        clock,
        probes: &OwnRunProbes,
        own_lock,
    })?
    .pieces)
}

// =========================================================
// Refresh: IDs for new pieces
// =========================================================

/// Gives the new pieces their ID and brings the live packages of the
/// remote in line with what the pulled metadata names. Runs after the pull
/// and before the uploads.
pub(super) fn refresh(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<(), SyncError> {
    let new: Vec<RelPath> = pieces(ctx.collection, ctx.clock, true)?
        .into_iter()
        .filter(|piece| piece.state == PieceKind::New && piece.id.is_none())
        .map(|piece| piece.path)
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let config = &ctx.collection.config().collection;
    let root = TreeRoot::open(ctx.collection.root()).map_err(|source| SyncError::Io {
        action: "open",
        path: ctx.collection.root().display().to_string(),
        source,
    })?;
    let rules = Rules::new(&config.patterns, &config.exclude)
        .and_then(|rules| rules.bind(&root))
        .map_err(|error| SyncError::Failed(error.to_string()))?;
    // `status` has warned about the junk list already.
    let junk = status::junk_names(&config.junk, &mut Vec::new());

    for path in new {
        let tree = match walk::walk_piece(&root, &path, &junk, &rules) {
            Ok(tree) => tree,
            // A run without file descriptors cannot read the next piece
            // either.
            Err(error) => return Err(SyncError::Failed(error.to_string())),
        };
        if let Some(problem) = tree.problems().first() {
            let (reason, message) = unbacked_by(&path, problem);
            report.unbacked.push(UnbackedPiece {
                id: None,
                path,
                reason,
                message,
            });
            continue;
        }
        let found = id_time_of(&tree, ctx.clock.now());
        report.warnings.extend(found.warning.clone());
        let id = ctx.ids.mint(found.time);
        let metadata = Metadata {
            format: FORMAT,
            id,
            collection: ctx.collection.id(),
            path: path.clone(),
            name: last_name(&path),
            created: whole_seconds(found.time),
            created_source: found.source,
            lifecycle: Lifecycle::Live,
            payloads: Vec::new(),
            history: vec![HistoryEntry::new(
                ctx.clock.now(),
                ctx.machine,
                HistoryAction::Sync,
            )],
            extra: serde_json::Map::new(),
        };
        match record_new(ctx, &metadata) {
            Ok(()) => report.minted.push(MintedPiece { id, path }),
            Err(error) => {
                // The commit must not settle an object whose entry is
                // missing. The next run's repairs write the entry.
                let message = error.to_string();
                skip(
                    ctx.collection,
                    report,
                    id,
                    SkipReason::Error,
                    message.clone(),
                );
                report.unbacked.push(UnbackedPiece {
                    id: Some(id),
                    path,
                    reason: UnbackedReason::Error,
                    message,
                });
            }
        }
    }
    Ok(())
}

/// Writes the metadata of a new ID, then its entry. The entry comes second
/// because a crash between the two is closed by the start-of-run repairs,
/// which write the missing entry of live metadata this collection owns;
/// the other order would leave an entry the metadata cannot be found by.
fn record_new(ctx: &SyncContext<'_>, metadata: &Metadata) -> Result<(), SyncError> {
    Store::new(ctx.collection).write_pending(ctx.lock, metadata)?;
    let mut entry = TrackingEntry::new(
        metadata.id,
        metadata.path.clone(),
        EntryState::Live,
        VersionTakenOut::Pending,
        ctx.clock.now(),
    );
    tracking::write(ctx.collection, ctx.lock, ctx.clock, &mut entry)?;
    Ok(())
}

/// The reason the first walk problem of `piece` gives, and its text.
fn unbacked_by(piece: &RelPath, problem: &Problem) -> (UnbackedReason, String) {
    match problem {
        Problem::Unreadable { first } => (
            UnbackedReason::Unreadable,
            format!("`{}` cannot be read", piece.concat(first)),
        ),
        Problem::MountPoint { other } => (
            UnbackedReason::MountPoint,
            format!("another filesystem is mounted at `{}`", piece.concat(other)),
        ),
        Problem::Excluded { path } => (
            UnbackedReason::Excluded,
            format!("`{}` is on the exclude list", piece.concat(path)),
        ),
        Problem::NestedCollection { root } => (
            UnbackedReason::NestedCollection,
            format!("`{}` is a collection of its own", piece.concat(root)),
        ),
    }
}

/// What a dry run would do in [`refresh`]. It reads and writes nothing.
/// A new piece with a problem the walk reports gets no ID, so it is left
/// out.
pub(super) fn plan(
    collection: &Collection,
    clock: &dyn Clock,
    plan: &mut Plan<SyncAction>,
) -> Result<(), SyncError> {
    const KEEPS_OUT: [Flag; 4] = [
        Flag::Unreadable,
        Flag::MountPoint,
        Flag::NestedCollection,
        Flag::Excluded,
    ];
    for piece in pieces(collection, clock, false)? {
        if piece.state == PieceKind::New
            && piece.id.is_none()
            && !piece.flags.iter().any(|flag| KEEPS_OUT.contains(flag))
        {
            plan.push(SyncAction::MintId { path: piece.path });
        }
    }
    Ok(())
}

/// Deletes the live packages on the remote whose archive is committed.
/// Runs after the commits.
// TODO(I32): replace this stub with the deletion of committed live packages.
pub(super) fn delete_committed(_ctx: &SyncContext<'_>) -> Result<(), SyncError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{Machine, Shared};
    use super::super::{SyncAction, UnbackedReason};
    use super::unbacked_by;
    use crate::meta::{HistoryAction, Lifecycle};
    use crate::relpath::RelPath;
    use crate::tracking::{EntryState, VersionTakenOut};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    /// The one object a machine's store holds.
    fn only_object(machine: &Machine) -> crate::id::ObjectId {
        let objects = machine.store().objects().expect("lists");
        assert_eq!(objects.len(), 1, "{objects:?}");
        objects[0]
    }

    #[test]
    fn a_new_piece_gets_an_id_and_live_metadata() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");

        let report = anna.sync();

        let id = only_object(&anna);
        assert_eq!(report.minted.len(), 1, "{report:?}");
        assert_eq!(
            (report.minted[0].id, &report.minted[0].path),
            (id, &path("proj"))
        );
        let current = anna.store().current(id).expect("reads").expect("exists");
        let metadata = current.metadata;
        assert_eq!(metadata.lifecycle, Lifecycle::Live);
        assert_eq!(metadata.collection, anna.collection.id());
        assert_eq!(metadata.path, path("proj"));
        assert!(metadata.payloads.is_empty(), "{:?}", metadata.payloads);
        let actions: Vec<&str> = metadata.history.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(actions, vec![HistoryAction::Sync.as_str()]);
        assert_eq!(current.source, crate::store::Source::Committed(1));
        assert_eq!(
            report
                .committed
                .iter()
                .map(|c| (c.id, c.version))
                .collect::<Vec<_>>(),
            vec![(id, 1)]
        );
        let entry = anna.entry(id);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Committed(1));
        assert!(shared.names(id).contains(&"meta.1".to_string()));
    }

    #[test]
    fn with_backup_live_false_it_still_gets_an_id_without_a_payload() {
        let shared = Shared::new();
        let anna = Machine::with_settings(&shared, "anna", "backup_live = false");
        anna.folder("proj", "a.txt", "hello");

        let report = anna.sync();

        let id = only_object(&anna);
        assert_eq!(report.minted.len(), 1, "{report:?}");
        let metadata = anna
            .store()
            .current(id)
            .expect("reads")
            .expect("exists")
            .metadata;
        assert!(metadata.payloads.is_empty());
        assert!(shared.names(id).contains(&"meta.1".to_string()));
    }

    #[test]
    fn a_piece_the_live_exclude_covers_still_gets_an_id() {
        let shared = Shared::new();
        let anna = Machine::with_settings(&shared, "anna", "live_exclude = [\"proj\"]");
        anna.folder("proj", "a.txt", "hello");

        let report = anna.sync();

        assert_eq!(report.minted.len(), 1, "{report:?}");
        assert!(
            shared
                .names(only_object(&anna))
                .contains(&"meta.1".to_string())
        );
    }

    #[test]
    fn a_pinned_piece_gets_an_id() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        fs::write(folder.join(".antiq-pin"), "").expect("the pin is written");

        let report = anna.sync();

        assert_eq!(report.minted.len(), 1, "{report:?}");
    }

    #[test]
    fn a_piece_that_has_its_id_gets_no_second_one() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);

        let report = anna.sync();

        assert!(report.minted.is_empty(), "{report:?}");
        assert_eq!(only_object(&anna), id);
    }

    #[test]
    fn a_crash_between_the_metadata_and_the_entry_does_not_mint_twice() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        let tracking = anna.collection.tracking_dir();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::File::create(tracking.join("probe")).is_err(),
            "the precondition: the tracking directory refuses writes"
        );

        let first = anna.sync();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o755)).expect("writable");

        let minted = only_object(&anna);
        assert!(first.committed.is_empty(), "{first:?}");
        assert!(
            shared.names(minted).is_empty(),
            "nothing is committed without an entry"
        );
        assert!(
            first
                .unbacked
                .iter()
                .any(|piece| piece.reason == UnbackedReason::Error),
            "{first:?}"
        );
        let second = anna.sync();

        assert!(second.minted.is_empty(), "{second:?}");
        assert_eq!(only_object(&anna), minted);
        assert_eq!(anna.entry(minted).state, EntryState::Live);
    }

    #[test]
    fn an_unreadable_piece_gets_no_id_and_is_skipped() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        fs::create_dir(folder.join("locked")).expect("made");
        fs::set_permissions(folder.join("locked"), fs::Permissions::from_mode(0o000))
            .expect("locked");
        assert!(
            fs::read_dir(folder.join("locked")).is_err(),
            "the precondition: the directory cannot be read"
        );

        let report = anna.sync();
        fs::set_permissions(folder.join("locked"), fs::Permissions::from_mode(0o755))
            .expect("unlocked");

        assert!(anna.store().objects().expect("lists").is_empty());
        assert!(report.minted.is_empty());
        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::Unreadable);
        assert_eq!(report.unbacked[0].path, path("proj"));
        assert_eq!(report.unbacked[0].id, None);
        assert!(crate::report::CommandReport::partial(&report));
    }

    #[test]
    fn a_piece_with_an_excluded_path_gets_no_id_and_does_not_make_the_run_partial() {
        let shared = Shared::new();
        let anna = Machine::with_settings(&shared, "anna", "exclude = [\"proj/skip\"]");
        let folder = anna.folder("proj", "a.txt", "hello");
        fs::create_dir(folder.join("skip")).expect("made");

        let report = anna.sync();

        assert!(anna.store().objects().expect("lists").is_empty());
        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::Excluded);
        assert!(!crate::report::CommandReport::partial(&report));
    }

    #[test]
    fn a_piece_with_a_collection_inside_gets_no_id() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        fs::create_dir_all(folder.join("inner/.antiq")).expect("made");

        let report = anna.sync();

        assert!(anna.store().objects().expect("lists").is_empty());
        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::NestedCollection);
        assert!(!crate::report::CommandReport::partial(&report));
    }

    #[test]
    fn a_mount_point_in_a_piece_is_named_as_such() {
        let problem = crate::tree::Problem::MountPoint { other: path("mnt") };

        let (reason, message) = unbacked_by(&path("proj"), &problem);

        assert_eq!(reason, UnbackedReason::MountPoint);
        assert!(message.contains("proj/mnt"), "{message}");
    }

    #[test]
    fn a_dry_run_lists_mint_id_and_writes_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");

        let actions = anna.plan();

        assert!(
            actions.contains(&SyncAction::MintId { path: path("proj") }),
            "{actions:?}"
        );
        assert!(anna.store().objects().expect("lists").is_empty());
        assert!(shared.names(crate::remote::testing::object(1)).is_empty());
    }

    #[test]
    fn a_dry_run_leaves_a_piece_with_a_walk_problem_out() {
        let shared = Shared::new();
        let anna = Machine::with_settings(&shared, "anna", "exclude = [\"proj/skip\"]");
        let folder = anna.folder("proj", "a.txt", "hello");
        fs::create_dir(folder.join("skip")).expect("made");

        let actions = anna.plan();

        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, SyncAction::MintId { .. })),
            "{actions:?}"
        );
    }
}
