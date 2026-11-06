// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live packages during sync.

use super::commit::{self, Class, Versions};
use super::{
    MintedPiece, RemovedLive, SkipReason, SyncAction, SyncContext, SyncError, SyncReport,
    UnbackedPiece, UnbackedReason, ends_run, path_of, skip,
};
use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::device::{self, FsKind};
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::meta::{
    self, Checksum, FORMAT, FileName, HistoryAction, HistoryEntry, Lifecycle, Metadata, Payload,
    PayloadKind,
};
use crate::ops::archive::apparent_size;
use crate::ops::archive::new::id_time_of;
use crate::ops::status::{self, Flag, Piece, PieceKind, StatusRequest, last_name};
use crate::pack::PackageFormat;
use crate::pack::live::{
    LiveError, LiveHooks, LiveOutcome, LivePackage, LiveSettings, LiveSource, lease_for,
    manifest_of, pack_live_with,
};
use crate::pack::manifest::Manifest;
use crate::plan::Plan;
use crate::relpath::{Name, RelPath, name_part};
use crate::remote::RemoteState;
use crate::report::Warning;
use crate::select::Rules;
use crate::store::{Current, Put, Source, Store};
use crate::tracking::{self, EntryState, TrackingEntry, VersionTakenOut};
use crate::tree::{PieceTree, Problem};
use crate::walk::{self, WalkError};
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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
// Refresh: IDs for new pieces and live packages
// =========================================================

/// Where the pieces of the collection are walked from.
struct Walking {
    root: TreeRoot,
    rules: Rules,
    junk: Vec<Name>,
}

impl Walking {
    fn open(collection: &Collection) -> Result<Walking, SyncError> {
        let config = &collection.config().collection;
        let root = TreeRoot::open(collection.root()).map_err(|source| SyncError::Io {
            action: "open",
            path: collection.root().display().to_string(),
            source,
        })?;
        let rules = Rules::new(&config.patterns, &config.exclude)
            .and_then(|rules| rules.bind(&root))
            .map_err(|error| SyncError::Failed(error.to_string()))?;
        // `status` has warned about the junk list already.
        let junk = status::junk_names(&config.junk, &mut Vec::new());
        Ok(Walking { root, rules, junk })
    }

    fn walk(&self, path: &RelPath) -> Result<PieceTree, WalkError> {
        walk::walk_piece(&self.root, path, &self.junk, &self.rules)
    }
}

/// The real world: nothing changes behind the packer's back.
struct NoHooks;

impl LiveHooks for NoHooks {}

/// Why backing up one piece stopped.
enum Stopped {
    /// The run cannot go on, as with a shortage of file descriptors.
    Run(SyncError),
    /// This piece is left for the next run.
    Piece(SyncError),
}

impl<E: Into<SyncError>> From<E> for Stopped {
    fn from(error: E) -> Stopped {
        Stopped::Piece(error.into())
    }
}

/// Gives the new pieces their ID and brings the live packages in line with
/// the folders. Runs after the pull and before the uploads: what it
/// records is committed and uploaded by the phases that follow.
pub(super) fn refresh(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    pull_failed: bool,
) -> Result<Captured, SyncError> {
    refresh_with(ctx, report, pull_failed, &NoHooks)
}

/// [`refresh`] with the points a test can change the world at.
pub(super) fn refresh_with(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    pull_failed: bool,
    hooks: &dyn LiveHooks,
) -> Result<Captured, SyncError> {
    back_up_pieces(ctx, report, pull_failed, hooks)?;
    if !pull_failed {
        strip_archives(ctx, report)?;
    }
    capture(ctx)
}

/// Mints and packs the pieces that are in their folders.
fn back_up_pieces(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    pull_failed: bool,
    hooks: &dyn LiveHooks,
) -> Result<(), SyncError> {
    let pieces: Vec<Piece> = pieces(ctx.collection, ctx.clock, true)?
        .into_iter()
        .filter(|piece| {
            matches!(
                piece.state,
                PieceKind::New | PieceKind::Live | PieceKind::Unearthed
            )
        })
        .collect();
    if pieces.is_empty() {
        return Ok(());
    }
    let walking = Walking::open(ctx.collection)?;
    let backup = Backup {
        ctx,
        walking: &walking,
        hooks,
        pull_failed,
    };

    for piece in pieces {
        let tree = match walking.walk(&piece.path) {
            Ok(tree) => tree,
            // A run without file descriptors cannot read the next piece
            // either.
            Err(error) => return Err(SyncError::Failed(error.to_string())),
        };
        if let Some(problem) = tree.problems().first() {
            let (reason, message) = unbacked_by(&piece.path, problem);
            report.unbacked.push(UnbackedPiece {
                id: piece.id,
                path: piece.path,
                reason,
                message,
            });
            continue;
        }
        let id = match piece.id {
            Some(id) => id,
            None => match mint(ctx, report, &piece.path, &tree) {
                Some(id) => id,
                None => continue,
            },
        };
        // A decision or a diverged folder is for the user to settle.
        if piece
            .flags
            .iter()
            .any(|flag| matches!(flag, Flag::Diverged | Flag::NeedsDecision))
        {
            continue;
        }
        match backup.piece(report, id, &piece.path, tree) {
            Ok(()) => {}
            Err(Stopped::Run(error)) => return Err(error),
            Err(Stopped::Piece(error)) => report.unbacked.push(UnbackedPiece {
                id: Some(id),
                path: piece.path,
                reason: UnbackedReason::Error,
                message: error.to_string(),
            }),
        }
    }
    Ok(())
}

/// Gives the new piece at `path` its ID and records the live metadata. A
/// failed record leaves the piece, and the object with it, for the next
/// run.
fn mint(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    path: &RelPath,
    tree: &PieceTree,
) -> Option<ObjectId> {
    let found = id_time_of(tree, ctx.clock.now());
    report.warnings.extend(found.warning.clone());
    let id = ctx.ids.mint(found.time);
    let metadata = Metadata {
        format: FORMAT,
        id,
        collection: ctx.collection.id(),
        path: path.clone(),
        name: last_name(path),
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
        Ok(()) => {
            report.minted.push(MintedPiece {
                id,
                path: path.clone(),
            });
            Some(id)
        }
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
                path: path.clone(),
                reason: UnbackedReason::Error,
                message,
            });
            None
        }
    }
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

/// The live payload of this machine with the highest version.
fn own_live<'m>(metadata: &'m Metadata, machine: &str) -> Option<&'m Payload> {
    metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Live && payload.machine == machine)
        .max_by_key(|payload| payload.version)
}

/// Whether the live package of `metadata` no longer holds what the walk
/// `tree` sees. A piece is unchanged when the manifest on disk was written
/// for the package the metadata names and the walk differs from it in
/// nothing. A manifest that is missing or cannot be read counts as
/// different, so the next package is made again. A piece without a live
/// package is not outdated.
fn outdated(collection: &Collection, machine: &str, metadata: &Metadata, tree: &PieceTree) -> bool {
    let Some(fresh) = manifest_of(tree, &collection.config().collection.live_exclude) else {
        return false;
    };
    let recorded = Manifest::read_recorded(&collection.manifest_path(metadata.id))
        .ok()
        .flatten();
    let named = own_live(metadata, machine).map(|payload| payload.checksum.value.as_str());
    let unchanged = match (&recorded, named) {
        (Some(recorded), Some(named)) => {
            recorded.package.as_deref() == Some(named) && recorded.manifest.diff(&fresh).is_empty()
        }
        _ => false,
    };
    !unchanged
}

/// What backing up the pieces of one run works with.
struct Backup<'a> {
    ctx: &'a SyncContext<'a>,
    walking: &'a Walking,
    hooks: &'a dyn LiveHooks,
    pull_failed: bool,
}

impl Backup<'_> {
    /// Brings the live package of one piece with an ID up to date.
    ///
    /// Only a piece that this collection owns is backed up, from metadata
    /// this machine has seen the committed state of: the recorded change
    /// is judged first, and one that is not ready to be committed is left
    /// to the commit phase.
    fn piece(
        &self,
        report: &mut SyncReport,
        id: ObjectId,
        path: &RelPath,
        tree: PieceTree,
    ) -> Result<(), Stopped> {
        let ctx = self.ctx;
        // Nothing may be built on metadata the pull could not read
        // completely.
        if self.pull_failed || report.skipped.iter().any(|skipped| skipped.id == id) {
            return Ok(());
        }
        let store = Store::new(ctx.collection);
        let Some(current) = store.current(id)? else {
            return Ok(());
        };
        let Some(entry) = tracking::read(ctx.collection, id)? else {
            return Ok(());
        };
        let metadata = &current.metadata;
        if metadata.collection != ctx.collection.id() || metadata.lifecycle != Lifecycle::Live {
            return Ok(());
        }
        let base = match current.source {
            Source::Pending => match commit::classify(ctx.collection, id, &Versions::new())? {
                Class::Commit { base, .. } => base.map(|base| *base),
                Class::Settle(_) | Class::Conflict { .. } => return Ok(()),
            },
            Source::Committed(number) => {
                let built_on = entry.version_taken_out == VersionTakenOut::Committed(number)
                    || entry.committed_versions.contains(&number);
                if !built_on {
                    return Ok(());
                }
                Some(metadata.clone())
            }
        };

        let config = &ctx.collection.config().collection;
        if !config.backup_live || manifest_of(&tree, &config.live_exclude).is_none() {
            return self.no_longer_refreshed(entry, metadata);
        }
        if !outdated(ctx.collection, ctx.machine, metadata, &tree) {
            self.clear_stale(entry)?;
            return Ok(());
        }
        self.pack(report, current, base, entry, path, tree)
    }

    /// A package that nothing refreshes any more is marked stale from the
    /// first run that notices, so that `status` says how old it is. It is
    /// never deleted for that reason.
    fn no_longer_refreshed(
        &self,
        mut entry: TrackingEntry,
        metadata: &Metadata,
    ) -> Result<(), Stopped> {
        if own_live(metadata, self.ctx.machine).is_some() && entry.stale_backup_since.is_none() {
            entry.stale_backup_since = Some(self.ctx.clock.now());
            tracking::write(
                self.ctx.collection,
                self.ctx.lock,
                self.ctx.clock,
                &mut entry,
            )?;
        }
        Ok(())
    }

    fn clear_stale(&self, mut entry: TrackingEntry) -> Result<(), Stopped> {
        if entry.stale_backup_since.is_some() {
            entry.stale_backup_since = None;
            tracking::write(
                self.ctx.collection,
                self.ctx.lock,
                self.ctx.clock,
                &mut entry,
            )?;
        }
        Ok(())
    }

    /// Packs the piece and records the package in the metadata. The manifest
    /// written beside it is that of the tree the package was made from,
    /// which differs from the first walk after a restart.
    fn pack(
        &self,
        report: &mut SyncReport,
        current: Current,
        base: Option<Metadata>,
        entry: TrackingEntry,
        path: &RelPath,
        tree: PieceTree,
    ) -> Result<(), Stopped> {
        let ctx = self.ctx;
        let collection = ctx.collection;
        let config = &collection.config().collection;
        let id = current.metadata.id;
        let tmp = collection.tmp_dir().join(format!("{id}.live.tar.zst"));
        // A leftover of a run that ended in the middle is ours: the lock
        // is held.
        remove_quietly(&tmp);

        let lease = lease_for(ctx.pool, apparent_size(&tree));
        report.warnings.extend(lease.warning().cloned());
        let outcome = pack_live_with(
            &LiveSource {
                collection: &self.walking.root,
                top: path,
                junk: &self.walking.junk,
                rules: &self.walking.rules,
                exclude: &config.live_exclude,
            },
            tree,
            &LiveSettings {
                workers: u32::try_from(lease.workers()).expect("a worker count fits 32 bits"),
                retries: config.live_retries,
            },
            &tmp,
            self.hooks,
        );
        drop(lease);

        let package = match outcome {
            Ok(LiveOutcome::Packed(package)) => package,
            Ok(LiveOutcome::NoPackage) => {
                return self.no_longer_refreshed(entry, &current.metadata);
            }
            Ok(LiveOutcome::Blocked(problems)) => {
                let problem = problems.first().expect("a blocked piece has a problem");
                let (reason, message) = unbacked_by(path, problem);
                report.unbacked.push(UnbackedPiece {
                    id: Some(id),
                    path: path.clone(),
                    reason,
                    message,
                });
                return Ok(());
            }
            Ok(LiveOutcome::Stale { changed, .. }) => {
                report.unbacked.push(UnbackedPiece {
                    id: Some(id),
                    path: path.clone(),
                    reason: UnbackedReason::Stale,
                    message: format!(
                        "`{}` kept changing while it was read",
                        path.concat(&changed)
                    ),
                });
                let mut entry = entry;
                if entry.stale_backup_since.is_none() {
                    entry.stale_backup_since = Some(ctx.clock.now());
                    tracking::write(collection, ctx.lock, ctx.clock, &mut entry)?;
                }
                return Ok(());
            }
            Err(LiveError::Walk(error)) => {
                return Err(Stopped::Run(SyncError::Failed(error.to_string())));
            }
            Err(error) => return Err(SyncError::Failed(error.to_string()).into()),
        };
        report.warnings.extend(
            package
                .notes
                .iter()
                .map(|note| Warning::new(note.to_string())),
        );

        let manifest_path = collection.manifest_path(id);
        let named = own_live(&current.metadata, ctx.machine);
        if named.is_some_and(|payload| payload.checksum.value == package.sha256) {
            // Only the stat data of the files moved: the package that
            // would be uploaded is the one that is there.
            remove_quietly(&tmp);
            package
                .manifest
                .write_recorded(&manifest_path, &package.sha256)?;
            return self.clear_stale(entry);
        }

        let store = Store::new(collection);
        let file = FileName::new(format!(
            "{}-{}.tar.zst",
            &package.sha256[..16],
            name_part(&last_name(path))
        ))
        .expect("a hash prefix and a name part make a plain file name");
        match store.put_file(ctx.lock, id, &file, &tmp, &package.sha256) {
            Ok(Put::Stored) => {}
            Ok(Put::AlreadyPresent) => remove_quietly(&tmp),
            Err(error) => {
                remove_quietly(&tmp);
                return Err(error.into());
            }
        }

        let replaced = current.source == Source::Pending;
        let dropped: Vec<FileName> = current
            .metadata
            .payloads
            .iter()
            .filter(|payload| payload.kind == PayloadKind::Live && payload.machine == ctx.machine)
            .map(|payload| payload.file.clone())
            .collect();
        let metadata = self.with_package(&current, base.as_ref(), &package, file.clone());
        store.write_pending(ctx.lock, &metadata)?;
        if replaced {
            let dropped: Vec<FileName> = dropped.into_iter().filter(|old| *old != file).collect();
            remove_unshared(&store, id, &dropped, base.as_ref());
        }
        package
            .manifest
            .write_recorded(&manifest_path, &package.sha256)?;
        self.clear_stale(entry)
    }

    /// The metadata with the new package in the place of this machine's
    /// live package. The version follows the newest live package any
    /// committed version or other machine has.
    fn with_package(
        &self,
        current: &Current,
        base: Option<&Metadata>,
        package: &LivePackage,
        file: FileName,
    ) -> Metadata {
        let ctx = self.ctx;
        let mut metadata = current.metadata.clone();
        metadata.payloads.retain(|payload| {
            !(payload.kind == PayloadKind::Live && payload.machine == ctx.machine)
        });
        let newest = base
            .map(|base| base.payloads.iter())
            .into_iter()
            .flatten()
            .chain(&metadata.payloads)
            .filter(|payload| payload.kind == PayloadKind::Live)
            .map(|payload| payload.version)
            .max()
            .unwrap_or(0);
        let mut extra = serde_json::Map::new();
        if !package.left_out.is_empty() {
            extra.insert(
                "left_out".into(),
                serde_json::to_value(&package.left_out).expect("paths serialise"),
            );
        }
        metadata.payloads.push(Payload {
            kind: PayloadKind::Live,
            version: newest + 1,
            file,
            package_format: PackageFormat::TAR_ZSTD_1.as_str().to_string(),
            size: package.size,
            checksum: Checksum::sha256(package.sha256.clone()),
            listing: None,
            machine: ctx.machine.to_string(),
            created: whole_seconds(ctx.clock.now()),
            verified: None,
            superseded: false,
            encoder: Some(package.encoder.clone()),
            extra,
        });
        // A recorded change that is already there carries its history
        // entry.
        if current.source != Source::Pending {
            metadata.history.push(HistoryEntry::new(
                ctx.clock.now(),
                ctx.machine,
                HistoryAction::Sync,
            ));
        }
        metadata
    }
}

/// Removes the local files `dropped` that the version `base` does not name.
/// A package that only a replaced recorded change named was never committed
/// or uploaded.
fn remove_unshared(store: &Store<'_>, id: ObjectId, dropped: &[FileName], base: Option<&Metadata>) {
    for old in dropped {
        let shared = base
            .iter()
            .flat_map(|base| &base.payloads)
            .any(|payload| payload.file == *old);
        if !shared {
            remove_quietly(&store.file_path(id, old));
        }
    }
}

/// Removes a file this run made or replaced. One that cannot be removed
/// is picked up by cleanup.
fn remove_quietly(path: &Path) {
    let _ = fs::remove_file(path);
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
/// out. A piece whose live package is out of date is listed with the
/// commit that records the new one, unless a recorded change has that
/// commit listed already.
pub(super) fn plan(
    collection: &Collection,
    clock: &dyn Clock,
    machine: &str,
    plan: &mut Plan<SyncAction>,
) -> Result<(), SyncError> {
    const KEEPS_OUT: [Flag; 4] = [
        Flag::Unreadable,
        Flag::MountPoint,
        Flag::NestedCollection,
        Flag::Excluded,
    ];
    let walking = Walking::open(collection)?;
    let store = Store::new(collection);
    for piece in pieces(collection, clock, false)? {
        if piece.flags.iter().any(|flag| KEEPS_OUT.contains(flag)) {
            continue;
        }
        let Some(id) = piece.id else {
            if piece.state == PieceKind::New {
                plan.push(SyncAction::MintId { path: piece.path });
            }
            continue;
        };
        if !matches!(piece.state, PieceKind::Live | PieceKind::Unearthed)
            || !collection.config().collection.backup_live
            || piece.flags.contains(&Flag::Diverged)
            || piece.flags.contains(&Flag::NeedsDecision)
        {
            continue;
        }
        let Ok(Some(current)) = store.current(id) else {
            continue;
        };
        if current.metadata.collection != collection.id()
            || current.metadata.lifecycle != Lifecycle::Live
        {
            continue;
        }
        let Ok(tree) = walking.walk(&piece.path) else {
            continue;
        };
        if !tree.problems().is_empty() || !outdated(collection, machine, &current.metadata, &tree) {
            continue;
        }
        plan.push(SyncAction::PackLive {
            id,
            path: piece.path,
        });
        if current.source != Source::Pending {
            plan.push(SyncAction::Commit { id });
        }
    }
    plan_removals(collection, machine, plan)
}

/// The live packages a real run would delete, and the commit that a
/// committed archive still naming its package needs for it. An object whose
/// state cannot be read is left out; the real run reports it.
fn plan_removals(
    collection: &Collection,
    machine: &str,
    plan: &mut Plan<SyncAction>,
) -> Result<(), SyncError> {
    let store = Store::new(collection);
    for id in store.objects()? {
        let Ok(Some(current)) = store.current(id) else {
            continue;
        };
        let metadata = &current.metadata;
        if metadata.collection != collection.id() {
            continue;
        }
        let mut after = metadata.clone();
        if after.lifecycle == Lifecycle::Archived {
            after.payloads.retain(|payload| {
                !(payload.kind == PayloadKind::Live && payload.machine == machine)
            });
        }
        if names_live(&after) {
            continue;
        }
        // The package the newest committed version names goes with the
        // change that no longer names a live package.
        let Some(&highest) = store.committed_versions(id).unwrap_or_default().last() else {
            continue;
        };
        let Some(base) = read_version(&store, id, highest) else {
            continue;
        };
        let files = live_files(&base, machine);
        if files.is_empty() || (current.source != Source::Pending && after == *metadata) {
            continue;
        }
        for file in files {
            plan.push(SyncAction::RemoveLive {
                id,
                file: file.as_str().to_string(),
            });
        }
        if current.source != Source::Pending {
            plan.push(SyncAction::Commit { id });
        }
    }
    Ok(())
}

// =========================================================
// Archived objects drop their live package
// =========================================================

/// The files of the live payloads `machine` made.
fn live_files(metadata: &Metadata, machine: &str) -> Vec<FileName> {
    metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Live && payload.machine == machine)
        .map(|payload| payload.file.clone())
        .collect()
}

/// Whether `metadata` names a live package of any machine.
fn names_live(metadata: &Metadata) -> bool {
    metadata
        .payloads
        .iter()
        .any(|payload| payload.kind == PayloadKind::Live)
}

/// Takes the live payloads of this machine out of every archived object of
/// this collection that still names them. The archive command records the
/// archive with the live payload still in place, so that the package stays
/// on the remote until the archive is committed. Runs over every object in
/// the store, not over the pieces in their folders, since an archived piece
/// has none.
fn strip_archives(ctx: &SyncContext<'_>, report: &mut SyncReport) -> Result<(), SyncError> {
    let store = Store::new(ctx.collection);
    for id in store.objects()? {
        if report.skipped.iter().any(|skipped| skipped.id == id) {
            continue;
        }
        if let Err(error) = strip(ctx, &store, id) {
            let message = error.to_string();
            match path_of(ctx.collection, id) {
                Some(path) => report.unbacked.push(UnbackedPiece {
                    id: Some(id),
                    path,
                    reason: UnbackedReason::Error,
                    message,
                }),
                None => report.warnings.push(Warning::new(format!(
                    "the live package of {id} was not dropped from its archive: {message}"
                ))),
            }
        }
    }
    Ok(())
}

fn strip(ctx: &SyncContext<'_>, store: &Store<'_>, id: ObjectId) -> Result<(), SyncError> {
    let Some(current) = store.current(id)? else {
        return Ok(());
    };
    let metadata = &current.metadata;
    if metadata.collection != ctx.collection.id()
        || metadata.lifecycle != Lifecycle::Archived
        || live_files(metadata, ctx.machine).is_empty()
    {
        return Ok(());
    }
    let base = match current.source {
        Source::Pending => match commit::classify(ctx.collection, id, &Versions::new())? {
            Class::Commit { base, .. } => base.map(|base| *base),
            Class::Settle(_) | Class::Conflict { .. } => return Ok(()),
        },
        Source::Committed(number) => {
            let Some(entry) = tracking::read(ctx.collection, id)? else {
                return Ok(());
            };
            let built_on = entry.version_taken_out == VersionTakenOut::Committed(number)
                || entry.committed_versions.contains(&number);
            if !built_on {
                return Ok(());
            }
            Some(metadata.clone())
        }
    };
    let dropped = live_files(metadata, ctx.machine);
    let mut stripped = metadata.clone();
    stripped
        .payloads
        .retain(|payload| !(payload.kind == PayloadKind::Live && payload.machine == ctx.machine));
    if current.source != Source::Pending {
        stripped.history.push(HistoryEntry::new(
            ctx.clock.now(),
            ctx.machine,
            HistoryAction::Sync,
        ));
    }
    store.write_pending(ctx.lock, &stripped)?;
    remove_unshared(store, id, &dropped, base.as_ref());
    Ok(())
}

/// The live packages of this machine that the newest committed version of
/// an object with a recorded change names, with that version's number.
#[derive(Debug, Default)]
pub(super) struct Captured {
    held: BTreeMap<ObjectId, (u32, Vec<FileName>)>,
}

/// Notes the live packages that a commit of this run may leave without a
/// name, before the commit phase runs.
fn capture(ctx: &SyncContext<'_>) -> Result<Captured, SyncError> {
    let store = Store::new(ctx.collection);
    let mut captured = Captured::default();
    for id in commit::pending_objects(ctx.collection)? {
        let Some(&highest) = store.committed_versions(id)?.last() else {
            continue;
        };
        let Some(base) = read_version(&store, id, highest) else {
            continue;
        };
        let files = live_files(&base, ctx.machine);
        if !files.is_empty() {
            captured.held.insert(id, (highest, files));
        }
    }
    Ok(captured)
}

/// Deletes the live packages on the remote whose archive is committed.
/// Runs after the commits.
///
/// A package goes only when the version committed in this run and every
/// version above the one it was built on name no live package at all, and
/// only for an object committed in this run. A conflict, a refused commit
/// or a failed upload leaves the package where it is. A package that is
/// replaced by a newer one stays for cleanup.
pub(super) fn delete_committed(
    ctx: &SyncContext<'_>,
    report: &mut SyncReport,
    captured: Captured,
) -> Result<(), SyncError> {
    let store = Store::new(ctx.collection);
    let committed: BTreeSet<ObjectId> = report.committed.iter().map(|c| c.id).collect();
    for id in committed {
        let Some((highest, files)) = captured.held.get(&id) else {
            continue;
        };
        if !names_no_live_above(&store, id, *highest)? {
            continue;
        }
        let mut all_gone = true;
        for file in files {
            match ctx.remote.delete(id, file.as_str()) {
                Ok(()) => {
                    remove_quietly(&store.file_path(id, file));
                    report.removed_live.push(RemovedLive {
                        id,
                        file: file.as_str().to_string(),
                    });
                }
                Err(error) if ends_run(ctx.remote, &error) => return Err(error.into()),
                Err(error) => {
                    all_gone = false;
                    report.warnings.push(Warning::new(format!(
                        "the live package {} of {id} could not be deleted, cleanup will remove it: {error}",
                        file.as_str()
                    )));
                }
            }
        }
        if all_gone {
            remove_quietly(&ctx.collection.manifest_path(id));
        }
    }
    Ok(())
}

/// Whether the newest committed version of `id` and every committed version
/// above `built_on` name no live package.
fn names_no_live_above(store: &Store<'_>, id: ObjectId, built_on: u32) -> Result<bool, SyncError> {
    let versions = store.committed_versions(id)?;
    let newest = versions.last().copied().unwrap_or(0);
    // A version that cannot be read may name a package, so it keeps
    // the package.
    let names_one = versions
        .into_iter()
        .filter(|number| *number > built_on || *number == newest)
        .any(|number| read_version(store, id, number).is_none_or(|version| names_live(&version)));
    Ok(!names_one)
}

/// The committed version `number` of `id`, when it reads.
fn read_version(store: &Store<'_>, id: ObjectId, number: u32) -> Option<Metadata> {
    let bytes = fs::read(store.meta_path(id, number)).ok()?;
    meta::parse(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{Machine, Shared};
    use super::super::{SyncAction, UnbackedReason};
    use super::unbacked_by;
    use crate::clock::Clock;
    use crate::id::ObjectId;
    use crate::meta::{HistoryAction, Lifecycle, Payload, PayloadKind};
    use crate::pack::manifest::Manifest;
    use crate::relpath::RelPath;
    use crate::remote::testing::{Fault, Op};
    use crate::tracking::{EntryState, VersionTakenOut};
    use sha2::{Digest, Sha256};
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
        assert_eq!(metadata.payloads.len(), 1, "{:?}", metadata.payloads);
        assert_eq!(metadata.payloads[0].kind, PayloadKind::Live);
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

    // =====================================================
    // Backing up live pieces
    // =====================================================

    /// The live payloads the current metadata of `id` names.
    fn live_payloads(machine: &Machine, id: ObjectId) -> Vec<Payload> {
        machine
            .store()
            .current(id)
            .expect("reads")
            .expect("exists")
            .metadata
            .payloads
            .into_iter()
            .filter(|payload| payload.kind == PayloadKind::Live)
            .collect()
    }

    fn sha256_of(bytes: &[u8]) -> String {
        crate::pack::hashing::hex(&Sha256::digest(bytes))
    }

    /// Changes `file` so that its size differs from every earlier content.
    fn change(folder: &std::path::Path, file: &str, content: &str) {
        fs::write(folder.join(file), content).expect("the file is written");
    }

    #[test]
    fn a_live_piece_gets_a_package_and_it_is_uploaded_and_committed() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");

        let report = anna.sync();

        let id = only_object(&anna);
        let payloads = live_payloads(&anna, id);
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        let payload = &payloads[0];
        assert_eq!(payload.version, 1);
        assert_eq!(payload.machine, "anna");
        assert_eq!(payload.listing, None);
        assert!(payload.encoder.is_some());
        let on_remote = shared.read(id, payload.file.as_str());
        assert_eq!(payload.checksum.value, sha256_of(&on_remote));
        assert_eq!(payload.size, on_remote.len() as u64);
        assert_eq!(
            report
                .uploaded
                .iter()
                .map(|u| u.file.as_str())
                .collect::<Vec<_>>(),
            vec![payload.file.as_str()]
        );
        let recorded = Manifest::read_recorded(&anna.collection.manifest_path(id))
            .expect("reads")
            .expect("the manifest is there");
        assert_eq!(
            recorded.package.as_deref(),
            Some(payload.checksum.value.as_str())
        );
        assert_eq!(anna.entry(id).stale_backup_since, None);
    }

    #[test]
    fn an_unchanged_piece_transfers_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let manifest = fs::read(anna.collection.manifest_path(id)).expect("the manifest");
        let remote = anna.remote();

        let report = anna.sync_with(&remote);

        assert!(report.uploaded.is_empty(), "{report:?}");
        assert!(report.committed.is_empty(), "{report:?}");
        assert_eq!(remote.count(Op::PutPayload), 0);
        assert_eq!(anna.store().committed_versions(id).expect("lists"), vec![1]);
        assert!(!anna.collection.meta_pending_path(id).exists());
        assert_eq!(
            fs::read(anna.collection.manifest_path(id)).expect("the manifest"),
            manifest
        );
    }

    #[test]
    fn a_changed_piece_uploads_a_new_package_and_the_old_one_stays_until_cleanup() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let first = live_payloads(&anna, id).remove(0);
        change(&folder, "a.txt", "hello again");

        let report = anna.sync();

        let payloads = live_payloads(&anna, id);
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert_eq!(payloads[0].version, 2);
        assert_ne!(payloads[0].file, first.file);
        assert_eq!(
            report
                .committed
                .iter()
                .map(|c| c.version)
                .collect::<Vec<_>>(),
            vec![2]
        );
        let names = shared.names(id);
        assert!(
            names.contains(&first.file.as_str().to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&payloads[0].file.as_str().to_string()),
            "{names:?}"
        );
        let current = anna.store().current(id).expect("reads").expect("exists");
        let actions: Vec<&str> = current
            .metadata
            .history
            .iter()
            .map(|e| e.action.as_str())
            .collect();
        assert_eq!(actions, vec!["sync", "sync"]);
    }

    #[test]
    fn a_touched_file_with_the_same_content_only_replaces_the_manifest() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let before = fs::read(anna.collection.manifest_path(id)).expect("the manifest");
        // The tar header holds the modification time, so restoring it
        // leaves the package bytes alone while the ctime moves.
        let mtime = filetime::FileTime::from_last_modification_time(
            &fs::metadata(folder.join("a.txt")).expect("stat"),
        );
        change(&folder, "a.txt", "hello");
        filetime::set_file_mtime(folder.join("a.txt"), mtime).expect("mtime is restored");
        let remote = anna.remote();

        let report = anna.sync_with(&remote);

        assert!(report.uploaded.is_empty(), "{report:?}");
        assert!(report.committed.is_empty(), "{report:?}");
        assert_eq!(remote.count(Op::PutPayload), 0);
        assert_eq!(anna.store().committed_versions(id).expect("lists"), vec![1]);
        let after = fs::read(anna.collection.manifest_path(id)).expect("the manifest");
        assert_ne!(after, before, "the manifest records the new ctime");
    }

    #[test]
    fn a_manifest_that_cannot_be_read_counts_as_changed() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let good = fs::read(anna.collection.manifest_path(id)).expect("the manifest");
        fs::write(anna.collection.manifest_path(id), "not json").expect("damaged");

        let report = anna.sync();

        assert!(
            report.uploaded.is_empty(),
            "the package is the same: {report:?}"
        );
        assert_eq!(
            fs::read(anna.collection.manifest_path(id)).expect("the manifest"),
            good
        );
    }

    #[test]
    fn a_missing_manifest_is_made_again_without_a_new_version() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let good = fs::read(anna.collection.manifest_path(id)).expect("the manifest");
        fs::remove_file(anna.collection.manifest_path(id)).expect("removed");

        let report = anna.sync();

        assert!(report.committed.is_empty(), "{report:?}");
        assert_eq!(
            fs::read(anna.collection.manifest_path(id)).expect("the manifest"),
            good
        );
    }

    #[test]
    fn left_out_entries_are_recorded_in_the_payload() {
        let shared = Shared::new();
        let anna = Machine::with_settings(&shared, "anna", "live_exclude = [\"proj/cache\"]");
        let folder = anna.folder("proj", "a.txt", "hello");
        fs::create_dir(folder.join("cache")).expect("made");
        fs::write(folder.join("cache/big.bin"), "x").expect("written");

        anna.sync();

        let id = only_object(&anna);
        let payload = live_payloads(&anna, id).remove(0);
        assert_eq!(
            payload.extra.get("left_out"),
            Some(&serde_json::json!(["proj/cache"]))
        );
    }

    #[test]
    fn a_payload_that_left_nothing_out_records_no_left_out() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");

        anna.sync();

        let payload = live_payloads(&anna, only_object(&anna)).remove(0);
        assert!(payload.extra.get("left_out").is_none());
    }

    #[test]
    fn a_second_refresh_over_a_pending_package_replaces_it_and_removes_the_file() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        let remote = anna.remote();
        remote.fail(Op::PutPayload, 1, Fault::Io);
        anna.sync_with(&remote);
        let id = only_object(&anna);
        let first = live_payloads(&anna, id).remove(0);
        assert!(anna.collection.meta_pending_path(id).exists());
        change(&folder, "a.txt", "hello again");

        anna.sync();

        let second = live_payloads(&anna, id).remove(0);
        assert_ne!(second.file, first.file);
        assert_eq!(second.version, 1, "the first package never was committed");
        let local = anna.collection.object_dir(id);
        assert!(!local.join(first.file.as_str()).exists());
        assert!(local.join(second.file.as_str()).exists());
        assert!(shared.names(id).contains(&second.file.as_str().to_string()));
    }

    #[test]
    fn a_dry_run_lists_pack_live_for_a_changed_piece_and_writes_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");

        let actions = anna.plan();

        assert!(
            actions.contains(&SyncAction::PackLive {
                id,
                path: path("proj")
            }),
            "{actions:?}"
        );
        assert!(actions.contains(&SyncAction::Commit { id }), "{actions:?}");
        assert!(!anna.collection.meta_pending_path(id).exists());
        assert_eq!(anna.store().committed_versions(id).expect("lists"), vec![1]);
    }

    #[test]
    fn a_dry_run_lists_nothing_for_an_unchanged_piece() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();

        let actions = anna.plan();

        assert!(actions.is_empty(), "{actions:?}");
    }

    // =====================================================
    // Guards, stale packages and packages nothing refreshes
    // =====================================================

    /// Runs `work` with the context `sync` has after its pull: the run lock
    /// held, the remote of the machine.
    fn under_lock<R>(
        machine: &Machine,
        work: impl FnOnce(&crate::ops::sync::SyncContext<'_>) -> R,
    ) -> R {
        let lock = crate::lock::acquire(
            &machine.collection.lock_path(),
            machine.name,
            &machine.clock,
        )
        .expect("free");
        let remote = machine.remote();
        let ctx = crate::ops::sync::SyncContext {
            collection: &machine.collection,
            remote: &remote,
            lock: &lock,
            clock: &machine.clock,
            machine: machine.name,
            ids: &machine.ids,
            pool: &machine.pool,
            jobs: 1,
        };
        work(&ctx)
    }

    fn refresh_under_lock(
        machine: &Machine,
        pull_failed: bool,
        hooks: &dyn crate::pack::live::LiveHooks,
        report: &mut crate::ops::sync::SyncReport,
    ) {
        under_lock(machine, |ctx| {
            super::refresh_with(ctx, report, pull_failed, hooks).expect("the refresh runs");
        });
    }

    /// Backs up the piece `name` of `id` without the choice of pieces
    /// `refresh` makes before.
    fn back_up_piece(machine: &Machine, id: ObjectId, name: &str) {
        under_lock(machine, |ctx| {
            let walking = super::Walking::open(ctx.collection).expect("opens");
            let tree = walking.walk(&path(name)).expect("walks");
            let backup = super::Backup {
                ctx,
                walking: &walking,
                hooks: &super::NoHooks,
                pull_failed: false,
            };
            let stopped = backup
                .piece(&mut Default::default(), id, &path(name), tree)
                .is_err();
            assert!(!stopped, "the piece is left alone, not failed");
        });
    }

    /// Makes the world change in the middle of a pack: appends to a file
    /// after it was read, and lets the restart's walk meet a mount or a
    /// shortage of descriptors. The package is corrupted before its check
    /// when `corrupt` is set.
    #[derive(Default)]
    struct Scripted {
        touch: Option<std::path::PathBuf>,
        mount: Option<RelPath>,
        emfile: bool,
        corrupt: bool,
        /// Touches the file in the first attempt only, so the restart packs
        /// a tree that differs from the first walk.
        first_attempt_only: bool,
        touched: std::cell::Cell<bool>,
        walks: std::cell::Cell<u32>,
    }

    impl Scripted {
        fn racing(folder: &std::path::Path) -> Scripted {
            Scripted {
                touch: Some(folder.join("a.txt")),
                ..Scripted::default()
            }
        }
    }

    impl crate::pack::live::LiveHooks for Scripted {
        fn walk_hooks(&self) -> &dyn crate::walk::Hooks {
            self
        }

        fn after_walk(&self, _attempt: u32, _tree: &crate::tree::PieceTree) {
            self.walks.set(self.walks.get() + 1);
        }

        fn after_read(&self, _entry: &crate::tree::TreeEntry, attempt: u32) {
            if self.first_attempt_only && attempt > 1 {
                return;
            }
            if let Some(file) = &self.touch {
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(file)
                    .expect("the file opens");
                std::io::Write::write_all(&mut file, b"more").expect("written");
                self.touched.set(true);
            }
        }

        fn before_check(&self, package: &std::path::Path) {
            if self.corrupt {
                fs::write(package, b"junk").expect("the package is corrupted");
            }
        }
    }

    impl crate::walk::Hooks for Scripted {
        fn mount_root(&self, path: &RelPath, reported: bool) -> bool {
            reported || (self.touched.get() && self.mount.as_ref() == Some(path))
        }

        fn fault(&self, _path: &RelPath, _call: crate::walk::Call) -> Option<std::io::Error> {
            (self.emfile && self.touched.get())
                .then(|| std::io::Error::from_raw_os_error(libc::EMFILE))
        }
    }

    #[test]
    fn a_file_changing_while_it_is_read_ends_as_backup_stale_since() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let first = live_payloads(&anna, id).remove(0);
        let manifest = fs::read(anna.collection.manifest_path(id)).expect("the manifest");
        change(&folder, "a.txt", "hello again");
        let mut report = crate::ops::sync::SyncReport::default();

        refresh_under_lock(&anna, false, &Scripted::racing(&folder), &mut report);

        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::Stale);
        assert_eq!(report.unbacked[0].id, Some(id));
        assert!(
            report.unbacked[0].message.contains("proj/a.txt"),
            "{report:?}"
        );
        assert!(crate::report::CommandReport::partial(&report));
        assert_eq!(anna.entry(id).stale_backup_since, Some(anna.clock.now()));
        assert_eq!(live_payloads(&anna, id), vec![first]);
        assert!(!anna.collection.meta_pending_path(id).exists());
        assert_eq!(
            fs::read(anna.collection.manifest_path(id)).expect("the manifest"),
            manifest
        );
        assert!(
            !anna
                .collection
                .tmp_dir()
                .join(format!("{id}.live.tar.zst"))
                .exists(),
            "the half-made package is removed"
        );

        // A later run that finds the piece quiet packs it and takes the
        // mark away.
        anna.sync();
        assert_eq!(anna.entry(id).stale_backup_since, None);
        assert_eq!(live_payloads(&anna, id)[0].version, 2);
    }

    #[test]
    fn a_restarted_pack_records_the_manifest_of_the_tree_it_packed() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        let hooks = Scripted {
            first_attempt_only: true,
            ..Scripted::racing(&folder)
        };
        refresh_under_lock(&anna, false, &hooks, &mut Default::default());
        assert!(hooks.touched.get(), "the first attempt met a change");
        assert_eq!(hooks.walks.get(), 2, "the restart packed again");

        let again = Scripted::default();
        refresh_under_lock(&anna, false, &again, &mut Default::default());

        assert_eq!(again.walks.get(), 0, "the piece is packed a second time");
    }

    #[test]
    fn a_piece_that_stays_stale_keeps_the_time_it_was_first_marked() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let first = anna.clock.now();
        refresh_under_lock(
            &anna,
            false,
            &Scripted::racing(&folder),
            &mut Default::default(),
        );
        anna.clock.advance(jiff::SignedDuration::from_hours(1));

        refresh_under_lock(
            &anna,
            false,
            &Scripted::racing(&folder),
            &mut Default::default(),
        );

        assert_eq!(anna.entry(id).stale_backup_since, Some(first));
    }

    #[test]
    fn a_pull_that_failed_mints_but_builds_nothing_on_the_metadata() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        anna.folder("fresh", "b.txt", "new");
        let mut report = crate::ops::sync::SyncReport::default();

        refresh_under_lock(&anna, true, &super::NoHooks, &mut report);

        assert_eq!(report.minted.len(), 1, "{report:?}");
        assert_eq!(report.minted[0].path, path("fresh"));
        assert!(!anna.collection.meta_pending_path(id).exists());
        assert_eq!(live_payloads(&anna, id)[0].version, 1);
        let minted = report.minted[0].id;
        assert!(
            live_payloads(&anna, minted).is_empty(),
            "the new piece is not packed either"
        );
    }

    #[test]
    fn an_object_the_pull_left_alone_is_not_packed() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let mut report = crate::ops::sync::SyncReport::default();
        crate::ops::sync::skip(
            &anna.collection,
            &mut report,
            id,
            crate::ops::sync::SkipReason::Pull,
            "x".into(),
        );

        refresh_under_lock(&anna, false, &super::NoHooks, &mut report);

        assert!(!anna.collection.meta_pending_path(id).exists());
    }

    #[test]
    fn a_recorded_change_that_is_already_committed_is_settled_before_it_is_built_on() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        fs::copy(
            anna.store().meta_path(id, 1),
            anna.collection.meta_pending_path(id),
        )
        .expect("the committed version is recorded again");
        change(&folder, "a.txt", "hello again");

        let report = anna.sync();

        assert!(
            report.committed.iter().all(|c| c.version == 1),
            "{report:?}"
        );
        assert!(!anna.collection.meta_pending_path(id).exists());
        assert_eq!(live_payloads(&anna, id)[0].version, 1);
        anna.sync();
        assert_eq!(live_payloads(&anna, id)[0].version, 2);
    }

    #[test]
    fn a_recorded_change_that_conflicts_is_left_as_it_is() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let mut metadata = anna
            .store()
            .current(id)
            .expect("reads")
            .expect("exists")
            .metadata;
        metadata.history[0].machine = "somebody-else".into();
        let lock =
            crate::lock::acquire(&anna.collection.lock_path(), "anna", &anna.clock).expect("free");
        anna.store()
            .write_pending(&lock, &metadata)
            .expect("recorded");
        drop(lock);
        let recorded = fs::read(anna.collection.meta_pending_path(id)).expect("the change");
        change(&folder, "a.txt", "hello again");
        let mut report = crate::ops::sync::SyncReport::default();

        refresh_under_lock(&anna, false, &super::NoHooks, &mut report);

        assert_eq!(
            fs::read(anna.collection.meta_pending_path(id)).expect("the change"),
            recorded
        );
        assert!(report.unbacked.is_empty(), "{report:?}");
    }

    #[test]
    fn a_piece_without_a_tracking_entry_is_left_alone() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let failing = anna.remote();
        failing.fail(Op::PutPayload, 1, Fault::Io);
        anna.sync_with(&failing);
        let recorded = fs::read(anna.collection.meta_pending_path(id)).expect("the change");
        fs::remove_file(anna.collection.tracking_path(id)).expect("removed");
        change(&folder, "a.txt", "hello once more");

        back_up_piece(&anna, id, "proj");

        assert_eq!(
            fs::read(anna.collection.meta_pending_path(id)).expect("the change"),
            recorded
        );
    }

    #[test]
    fn a_piece_of_another_collection_is_left_alone() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let mut metadata = anna
            .store()
            .current(id)
            .expect("reads")
            .expect("exists")
            .metadata;
        metadata.collection = crate::remote::testing::object(9);
        metadata.history.push(crate::meta::HistoryEntry::new(
            anna.clock.now(),
            "anna",
            HistoryAction::Unearth,
        ));
        let lock =
            crate::lock::acquire(&anna.collection.lock_path(), "anna", &anna.clock).expect("free");
        anna.store()
            .write_pending(&lock, &metadata)
            .expect("recorded");
        drop(lock);
        let recorded = fs::read(anna.collection.meta_pending_path(id)).expect("the change");
        change(&folder, "a.txt", "hello again");

        back_up_piece(&anna, id, "proj");

        assert_eq!(
            fs::read(anna.collection.meta_pending_path(id)).expect("the change"),
            recorded
        );
    }

    #[test]
    fn an_archived_object_gets_no_live_package() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let mut metadata = anna
            .store()
            .current(id)
            .expect("reads")
            .expect("exists")
            .metadata;
        metadata.lifecycle = Lifecycle::Archived;
        metadata.history.push(crate::meta::HistoryEntry::new(
            anna.clock.now(),
            "anna",
            HistoryAction::Archive,
        ));
        let lock =
            crate::lock::acquire(&anna.collection.lock_path(), "anna", &anna.clock).expect("free");
        anna.store()
            .write_pending(&lock, &metadata)
            .expect("recorded");
        drop(lock);
        let recorded = fs::read(anna.collection.meta_pending_path(id)).expect("the change");
        change(&folder, "a.txt", "hello again");

        back_up_piece(&anna, id, "proj");

        assert_eq!(
            fs::read(anna.collection.meta_pending_path(id)).expect("the change"),
            recorded
        );
    }

    #[test]
    fn a_version_this_machine_did_not_take_out_is_not_built_on() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let mut entry = anna.entry(id);
        entry.version_taken_out = VersionTakenOut::Pending;
        entry.committed_versions.clear();
        let lock =
            crate::lock::acquire(&anna.collection.lock_path(), "anna", &anna.clock).expect("free");
        crate::tracking::write(&anna.collection, &lock, &anna.clock, &mut entry).expect("written");
        drop(lock);
        change(&folder, "a.txt", "hello again");

        back_up_piece(&anna, id, "proj");

        assert!(!anna.collection.meta_pending_path(id).exists());
    }

    #[test]
    fn turning_backup_live_off_marks_the_package_stale_and_keeps_it() {
        let shared = Shared::new();
        let mut anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        anna.set_setting("backup_live = false");
        change(&folder, "a.txt", "hello again");
        anna.clock.advance(jiff::SignedDuration::from_hours(2));
        let marked = anna.clock.now();

        let report = anna.sync();

        assert_eq!(anna.entry(id).stale_backup_since, Some(marked));
        assert_eq!(live_payloads(&anna, id), vec![package.clone()]);
        assert!(
            shared
                .names(id)
                .contains(&package.file.as_str().to_string())
        );
        assert!(report.unbacked.is_empty(), "{report:?}");

        // The mark keeps the time of the first run that noticed.
        anna.clock.advance(jiff::SignedDuration::from_hours(2));
        anna.sync();
        assert_eq!(anna.entry(id).stale_backup_since, Some(marked));
    }

    #[test]
    fn a_live_exclude_entry_that_covers_the_piece_marks_its_package_stale_and_keeps_it() {
        let shared = Shared::new();
        let mut anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        anna.set_setting("live_exclude = [\"proj\"]");
        anna.clock.advance(jiff::SignedDuration::from_hours(2));

        anna.sync();

        assert_eq!(anna.entry(id).stale_backup_since, Some(anna.clock.now()));
        assert_eq!(live_payloads(&anna, id), vec![package]);
    }

    #[test]
    fn a_piece_that_never_had_a_package_is_not_marked_stale_when_nothing_backs_it_up() {
        let shared = Shared::new();
        let anna = Machine::with_settings(&shared, "anna", "backup_live = false");
        anna.folder("proj", "a.txt", "hello");

        anna.sync();

        assert_eq!(anna.entry(only_object(&anna)).stale_backup_since, None);
    }

    #[test]
    fn a_piece_that_gets_backed_up_again_loses_its_stale_mark() {
        let shared = Shared::new();
        let mut anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        anna.set_setting("backup_live = false");
        anna.sync();
        assert!(anna.entry(id).stale_backup_since.is_some());
        let config = anna.collection.root().join(".antiq/config.toml");
        let text = fs::read_to_string(&config).expect("reads");
        fs::write(
            &config,
            text.replace("backup_live = false", "backup_live = true"),
        )
        .expect("written");
        anna.collection =
            crate::collection::Collection::find(anna.collection.root()).expect("a collection");

        anna.sync();

        assert_eq!(anna.entry(id).stale_backup_since, None);
    }

    #[test]
    fn a_known_piece_with_an_unreadable_file_is_reported_with_its_id() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
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

        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].id, Some(id));
        assert_eq!(report.unbacked[0].reason, UnbackedReason::Unreadable);
        assert_eq!(live_payloads(&anna, id)[0].version, 1);
    }

    #[test]
    fn a_dry_run_lists_nothing_for_a_piece_that_is_not_backed_up() {
        let shared = Shared::new();
        let mut anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        anna.set_setting("backup_live = false");
        change(&folder, "a.txt", "hello again");

        let actions = anna.plan();

        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn a_dry_run_takes_no_pool() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        change(&folder, "a.txt", "hello again");

        let synced = crate::ops::sync::sync_using(&crate::ops::sync::SyncRequest {
            collection: &anna.collection,
            clock: &anna.clock,
            remote: &anna.remote(),
            ids: &anna.ids,
            pool: None,
            dry_run: true,
        })
        .expect("a dry run needs no pool");

        assert!(matches!(synced, crate::ops::sync::Synced::Planned { .. }));
    }

    #[test]
    fn a_real_run_without_a_pool_fails_before_it_changes_anything() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");

        let error = crate::ops::sync::sync_using(&crate::ops::sync::SyncRequest {
            collection: &anna.collection,
            clock: &anna.clock,
            remote: &anna.remote(),
            ids: &anna.ids,
            pool: None,
            dry_run: false,
        })
        .expect_err("a real run needs workers");

        assert!(error.to_string().contains("pool"), "{error}");
        assert!(anna.store().objects().expect("lists").is_empty());
    }

    #[test]
    fn a_mount_that_shows_up_in_a_restart_blocks_the_piece() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        fs::create_dir(folder.join("sub")).expect("made");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let hooks = Scripted {
            mount: Some(path("sub")),
            ..Scripted::racing(&folder)
        };
        let mut report = crate::ops::sync::SyncReport::default();

        refresh_under_lock(&anna, false, &hooks, &mut report);

        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::MountPoint);
        assert_eq!(report.unbacked[0].id, Some(id));
        assert_eq!(live_payloads(&anna, id)[0].version, 1);
    }

    #[test]
    fn a_shortage_of_descriptors_in_a_restart_ends_the_run() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        change(&folder, "a.txt", "hello again");
        let hooks = Scripted {
            emfile: true,
            ..Scripted::racing(&folder)
        };

        let error = under_lock(&anna, |ctx| {
            super::refresh_with(ctx, &mut Default::default(), false, &hooks)
                .expect_err("the run ends")
        });

        assert!(error.to_string().contains("file descriptors"), "{error}");
    }

    #[test]
    fn a_package_that_fails_its_check_fails_the_piece_and_leaves_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let hooks = Scripted {
            corrupt: true,
            ..Scripted::default()
        };
        let mut report = crate::ops::sync::SyncReport::default();

        refresh_under_lock(&anna, false, &hooks, &mut report);

        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::Error);
        assert!(crate::report::CommandReport::partial(&report));
        assert!(!anna.collection.meta_pending_path(id).exists());
        assert!(
            !anna
                .collection
                .tmp_dir()
                .join(format!("{id}.live.tar.zst"))
                .exists()
        );
    }

    #[test]
    fn a_package_that_cannot_be_stored_fails_the_piece_and_leaves_no_temp_file() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let dir = anna.collection.object_dir(id);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::File::create(dir.join("probe")).is_err(),
            "the precondition: the object directory refuses writes"
        );

        let report = anna.sync();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("writable");

        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::Error);
        assert!(
            !anna
                .collection
                .tmp_dir()
                .join(format!("{id}.live.tar.zst"))
                .exists()
        );
        assert_eq!(live_payloads(&anna, id)[0].version, 1);
    }

    #[test]
    fn a_manifest_that_cannot_be_written_fails_the_piece_and_the_next_run_repairs_it() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let tracking = anna.collection.tracking_dir();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::File::create(tracking.join("probe")).is_err(),
            "the precondition: the tracking directory refuses writes"
        );

        let first = anna.sync();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o755)).expect("writable");

        assert!(
            first
                .unbacked
                .iter()
                .any(|piece| piece.reason == UnbackedReason::Error),
            "{first:?}"
        );
        // The first run committed the new version on the remote but could
        // not record that here, so the second one settles it and the third
        // finds the manifest outdated.
        anna.sync();
        let third = anna.sync();
        let recorded = Manifest::read_recorded(&anna.collection.manifest_path(id))
            .expect("reads")
            .expect("is there");
        assert_eq!(
            recorded.package.as_deref(),
            Some(live_payloads(&anna, id)[0].checksum.value.as_str())
        );
        assert!(third.unbacked.is_empty(), "{third:?}");
    }

    // =====================================================
    // Deleting live packages once their change is committed
    // =====================================================

    /// Records the change of `id` through `edit` as a command of this
    /// machine would, and removes the folder of the piece.
    fn record_by_hand(
        machine: &Machine,
        id: ObjectId,
        edit: impl FnOnce(&mut crate::meta::Metadata),
    ) {
        let mut metadata = machine
            .store()
            .current(id)
            .expect("reads")
            .expect("exists")
            .metadata;
        edit(&mut metadata);
        let lock = crate::lock::acquire(
            &machine.collection.lock_path(),
            machine.name,
            &machine.clock,
        )
        .expect("free");
        machine
            .store()
            .write_pending(&lock, &metadata)
            .expect("recorded");
        drop(lock);
        fs::remove_dir_all(machine.collection.root().join("proj")).expect("the folder goes");
    }

    /// The change `archive` records: the lifecycle `archived` with the live
    /// payload still named.
    fn archive_by_hand(machine: &Machine, id: ObjectId) {
        let now = machine.clock.now();
        let name = machine.name;
        record_by_hand(machine, id, |metadata| {
            metadata.lifecycle = Lifecycle::Archived;
            metadata.history.push(crate::meta::HistoryEntry::new(
                now,
                name,
                HistoryAction::Archive,
            ));
        });
    }

    fn live_on_remote(shared: &Shared, id: ObjectId, payload: &Payload) -> bool {
        shared
            .names(id)
            .contains(&payload.file.as_str().to_string())
    }

    #[test]
    fn the_live_package_of_an_archived_piece_is_deleted_only_after_the_commit() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        archive_by_hand(&anna, id);
        let failing = anna.remote();
        failing.fail(Op::CreateExclusive, 1, Fault::Io);

        let first = anna.sync_with(&failing);

        assert!(first.removed_live.is_empty(), "{first:?}");
        assert!(live_on_remote(&shared, id, &package), "the commit failed");

        let remote = anna.remote();
        let second = anna.sync_with(&remote);

        assert_eq!(
            second.removed_live,
            vec![crate::ops::sync::RemovedLive {
                id,
                file: package.file.as_str().to_string()
            }]
        );
        assert!(!live_on_remote(&shared, id, &package));
        assert!(
            !anna
                .collection
                .object_dir(id)
                .join(package.file.as_str())
                .exists()
        );
        assert!(!anna.collection.manifest_path(id).exists());
        let calls = remote.calls();
        let deleted = calls
            .iter()
            .position(|call| call.op == Op::Delete && call.name.contains(package.file.as_str()))
            .expect("the package is deleted");
        let committed = calls
            .iter()
            .rposition(|call| call.op == Op::RenameIntoPlace)
            .expect("the metadata is put in place");
        assert!(committed < deleted, "{calls:?}");
    }

    #[test]
    fn the_archive_strips_the_live_payload_from_its_recorded_change() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        archive_by_hand(&anna, id);
        let failing = anna.remote();
        failing.fail(Op::CreateExclusive, 1, Fault::Io);

        anna.sync_with(&failing);

        assert!(anna.collection.meta_pending_path(id).exists());
        assert!(live_payloads(&anna, id).is_empty());
    }

    #[test]
    fn a_conflicting_archive_deletes_nothing_and_keeps_its_live_payload() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        let now = anna.clock.now();
        record_by_hand(&anna, id, |metadata| {
            metadata.lifecycle = Lifecycle::Archived;
            metadata.history[0].machine = "somebody-else".into();
            metadata.history.push(crate::meta::HistoryEntry::new(
                now,
                "anna",
                HistoryAction::Archive,
            ));
        });

        let report = anna.sync();

        assert!(report.removed_live.is_empty(), "{report:?}");
        assert!(live_on_remote(&shared, id, &package));
        assert_eq!(live_payloads(&anna, id), vec![package]);
    }

    #[test]
    fn a_change_that_drops_the_live_payload_deletes_the_package_after_its_commit() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        let now = anna.clock.now();
        record_by_hand(&anna, id, |metadata| {
            metadata
                .payloads
                .retain(|payload| payload.kind != PayloadKind::Live);
            metadata.history.push(crate::meta::HistoryEntry::new(
                now,
                "anna",
                HistoryAction::Fork,
            ));
        });

        let report = anna.sync();

        assert_eq!(report.removed_live.len(), 1, "{report:?}");
        assert!(!live_on_remote(&shared, id, &package));
    }

    #[test]
    fn a_replaced_package_is_not_deleted_by_sync() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let first = live_payloads(&anna, id).remove(0);
        change(&folder, "a.txt", "hello again");

        let report = anna.sync();

        assert!(report.removed_live.is_empty(), "{report:?}");
        assert!(live_on_remote(&shared, id, &first));
    }

    #[test]
    fn a_delete_that_fails_is_a_warning_and_the_run_stays_complete() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        archive_by_hand(&anna, id);
        let remote = anna.remote();
        remote.fail(Op::Delete, 1, Fault::Io);

        let report = anna.sync_with(&remote);

        assert!(report.removed_live.is_empty(), "{report:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains(package.file.as_str())),
            "{report:?}"
        );
        assert!(
            !crate::report::CommandReport::partial(&report),
            "{report:?}"
        );
        assert_eq!(report.committed.len(), 1);
    }

    #[test]
    fn a_package_of_another_machine_is_never_deleted() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let now = anna.clock.now();
        let package = live_payloads(&anna, id).remove(0);
        // The other machine's package has to be uploadable for the commit
        // to go through.
        fs::copy(
            anna.collection.object_dir(id).join(package.file.as_str()),
            anna.collection
                .object_dir(id)
                .join("ffffffffffffffff-proj.tar.zst"),
        )
        .expect("copied");
        record_by_hand(&anna, id, |metadata| {
            let mut other = metadata.payloads[0].clone();
            other.machine = "ben".into();
            other.version = 2;
            other.file = crate::meta::FileName::new("ffffffffffffffff-proj.tar.zst")
                .expect("a payload name");
            metadata.payloads.push(other);
            metadata.lifecycle = Lifecycle::Archived;
            metadata.history.push(crate::meta::HistoryEntry::new(
                now,
                "anna",
                HistoryAction::Archive,
            ));
        });
        let remote = anna.remote();

        let report = anna.sync_with(&remote);

        assert_eq!(report.committed.len(), 1, "{report:?}");
        assert!(report.removed_live.is_empty(), "{report:?}");
        assert!(
            !remote
                .calls()
                .iter()
                .any(|call| call.op == Op::Delete && call.name.ends_with(".tar.zst")),
            "{:?}",
            remote.calls()
        );
        let payloads = live_payloads(&anna, id);
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert_eq!(payloads[0].machine, "ben");
    }

    #[test]
    fn a_committed_archive_that_still_names_a_live_package_is_stripped_in_a_new_version() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        archive_by_hand(&anna, id);
        anna.publish_by_hand(&shared, id, 2);
        // The first run records the version as committed.
        anna.sync();
        assert!(!anna.collection.meta_pending_path(id).exists());

        let report = anna.sync();

        assert_eq!(
            report
                .committed
                .iter()
                .map(|c| c.version)
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert!(live_payloads(&anna, id).is_empty());
        let current = anna.store().current(id).expect("reads").expect("exists");
        assert_eq!(
            current.metadata.history.last().map(|e| e.action.as_str()),
            Some(HistoryAction::Sync.as_str())
        );
        assert_eq!(report.removed_live.len(), 1, "{report:?}");
        assert!(!live_on_remote(&shared, id, &package));
    }

    #[test]
    fn a_dry_run_lists_remove_live_for_an_archive_and_for_a_dropped_payload() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        archive_by_hand(&anna, id);

        let actions = anna.plan();

        assert!(
            actions.contains(&SyncAction::RemoveLive {
                id,
                file: package.file.as_str().to_string()
            }),
            "{actions:?}"
        );
        assert!(
            shared
                .names(id)
                .contains(&package.file.as_str().to_string())
        );
    }

    #[test]
    fn a_dry_run_lists_remove_live_for_a_dropped_payload() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        let now = anna.clock.now();
        record_by_hand(&anna, id, |metadata| {
            metadata
                .payloads
                .retain(|payload| payload.kind != PayloadKind::Live);
            metadata.history.push(crate::meta::HistoryEntry::new(
                now,
                "anna",
                HistoryAction::Fork,
            ));
        });

        let actions = anna.plan();

        assert!(
            actions.contains(&SyncAction::RemoveLive {
                id,
                file: package.file.as_str().to_string()
            }),
            "{actions:?}"
        );
    }

    #[test]
    fn a_dry_run_lists_the_commit_for_a_committed_archive_that_names_a_live_package() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let package = live_payloads(&anna, id).remove(0);
        archive_by_hand(&anna, id);
        anna.publish_by_hand(&shared, id, 2);
        anna.sync();
        assert!(!anna.collection.meta_pending_path(id).exists());

        let actions = anna.plan();

        let file = package.file.as_str().to_string();
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action,
                    SyncAction::RemoveLive { .. } | SyncAction::Commit { .. }
                ))
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                SyncAction::RemoveLive { id, file },
                SyncAction::Commit { id }
            ]
        );
    }

    #[test]
    fn a_dry_run_lists_no_removal_for_a_live_piece_or_an_archive_of_another_machines_package() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        change(&folder, "a.txt", "hello again");

        let actions = anna.plan();

        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, SyncAction::RemoveLive { .. })),
            "{actions:?}"
        );
    }

    #[test]
    fn a_delete_that_finds_the_remote_gone_ends_the_run() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        archive_by_hand(&anna, id);
        let remote = anna.remote();
        remote.fail(Op::Delete, 1, Fault::Away);
        remote.fail(Op::Reachability, 2, Fault::Away);

        let error = anna.sync_request(&remote, false).expect_err("the run ends");

        assert_eq!(error.kind(), crate::error::ErrorKind::RemoteAway, "{error}");
    }

    // =====================================================
    // Branches the other tests do not reach
    // =====================================================

    fn write_entry(machine: &Machine, entry: &mut crate::tracking::TrackingEntry) {
        let lock = crate::lock::acquire(
            &machine.collection.lock_path(),
            machine.name,
            &machine.clock,
        )
        .expect("free");
        crate::tracking::write(&machine.collection, &lock, &machine.clock, entry).expect("written");
    }

    #[test]
    fn a_piece_with_a_decision_open_or_a_diverged_folder_is_not_packed() {
        for kind in [
            crate::tracking::DecisionKind::VersionConflict,
            crate::tracking::DecisionKind::DivergedFolder,
        ] {
            let shared = Shared::new();
            let anna = Machine::new(&shared, "anna");
            let folder = anna.folder("proj", "a.txt", "hello");
            anna.sync();
            let id = only_object(&anna);
            let mut entry = anna.entry(id);
            entry.decide(kind);
            write_entry(&anna, &mut entry);
            change(&folder, "a.txt", "hello again");

            let mut report = crate::ops::sync::SyncReport::default();
            refresh_under_lock(&anna, false, &super::NoHooks, &mut report);

            assert!(!anna.collection.meta_pending_path(id).exists(), "{kind:?}");
            assert_eq!(anna.plan().len(), 0, "{kind:?}");
        }
    }

    #[test]
    fn a_stale_mark_that_cannot_be_written_fails_the_piece() {
        let shared = Shared::new();
        let mut anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        anna.set_setting("backup_live = false");
        let tracking = anna.collection.tracking_dir();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::File::create(tracking.join("probe")).is_err(),
            "the precondition: the tracking directory refuses writes"
        );

        let report = anna.sync();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o755)).expect("writable");

        assert!(
            report
                .unbacked
                .iter()
                .any(|piece| piece.reason == UnbackedReason::Error),
            "{report:?}"
        );
    }

    #[test]
    fn a_stale_mark_that_cannot_be_removed_fails_the_piece() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let mut entry = anna.entry(id);
        entry.stale_backup_since = Some(anna.clock.now());
        write_entry(&anna, &mut entry);
        let tracking = anna.collection.tracking_dir();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::File::create(tracking.join("probe")).is_err(),
            "the precondition: the tracking directory refuses writes"
        );

        let report = anna.sync();
        fs::set_permissions(&tracking, fs::Permissions::from_mode(0o755)).expect("writable");

        assert!(
            report
                .unbacked
                .iter()
                .any(|piece| piece.reason == UnbackedReason::Error),
            "{report:?}"
        );
        assert!(anna.entry(id).stale_backup_since.is_some());
    }

    #[test]
    fn a_package_that_is_in_the_store_already_is_not_stored_again() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let folder = anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        change(&folder, "a.txt", "hello again");
        let failing = anna.remote();
        failing.fail(Op::PutPayload, 1, Fault::Io);
        anna.sync_with(&failing);
        let second = live_payloads(&anna, id).remove(0);
        // A run that ended after it stored the package and before it
        // recorded it leaves the package without a name.
        fs::remove_file(anna.collection.meta_pending_path(id)).expect("removed");

        anna.sync();

        assert_eq!(live_payloads(&anna, id)[0].file, second.file);
        assert!(
            !anna
                .collection
                .tmp_dir()
                .join(format!("{id}.live.tar.zst"))
                .exists(),
            "the duplicate is not left in tmp"
        );
    }

    #[test]
    fn an_object_without_metadata_is_left_alone() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let empty = crate::remote::testing::object(9);
        fs::create_dir(anna.collection.object_dir(empty)).expect("made");
        let mut report = crate::ops::sync::SyncReport::default();

        refresh_under_lock(&anna, false, &super::NoHooks, &mut report);
        back_up_piece(&anna, empty, "proj");

        assert!(report.unbacked.is_empty(), "{report:?}");
        assert!(anna.plan().is_empty());
    }

    /// A committed archive that still names the package and whose entry
    /// the edit changes.
    fn committed_archive_with(
        edit: impl FnOnce(&mut crate::tracking::TrackingEntry),
    ) -> (Shared, Machine, ObjectId) {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        archive_by_hand(&anna, id);
        anna.publish_by_hand(&shared, id, 2);
        anna.sync();
        let mut entry = anna.entry(id);
        edit(&mut entry);
        write_entry(&anna, &mut entry);
        (shared, anna, id)
    }

    #[test]
    fn a_committed_archive_without_an_entry_is_not_stripped() {
        let (_shared, anna, id) = committed_archive_with(|_| {});
        fs::remove_file(anna.collection.tracking_path(id)).expect("removed");

        let report = anna.sync();

        assert!(!anna.collection.meta_pending_path(id).exists());
        assert!(report.committed.is_empty(), "{report:?}");
    }

    #[test]
    fn a_committed_archive_this_machine_did_not_take_out_is_not_stripped() {
        let (_shared, anna, id) = committed_archive_with(|entry| {
            entry.version_taken_out = VersionTakenOut::Pending;
            entry.committed_versions.clear();
        });

        anna.sync();

        assert_eq!(
            anna.store().committed_versions(id).expect("lists"),
            vec![1, 2]
        );
    }

    #[test]
    fn an_archive_that_cannot_be_stripped_is_reported_with_its_path_and_keeps_its_package() {
        let (shared, anna, id) = committed_archive_with(|_| {});
        let dir = anna.collection.object_dir(id);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::File::create(dir.join("probe")).is_err(),
            "the precondition: the object directory refuses writes"
        );

        let report = anna.sync();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("writable");

        assert_eq!(report.unbacked.len(), 1, "{report:?}");
        assert_eq!(report.unbacked[0].reason, UnbackedReason::Error);
        assert_eq!(report.unbacked[0].path, path("proj"));
        assert!(report.removed_live.is_empty());
        assert!(shared.names(id).len() > 1);
    }

    #[test]
    fn a_version_that_cannot_be_read_keeps_the_package() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        let store = anna.store();
        assert!(
            !super::names_no_live_above(&store, id, 1).expect("lists"),
            "version 1 names the package"
        );

        fs::write(store.meta_path(id, 1), "junk").expect("damaged");

        assert!(!super::names_no_live_above(&store, id, 0).expect("lists"));
    }

    #[test]
    fn a_base_that_cannot_be_read_captures_and_plans_nothing() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        anna.folder("proj", "a.txt", "hello");
        anna.sync();
        let id = only_object(&anna);
        archive_by_hand(&anna, id);
        fs::write(anna.store().meta_path(id, 1), "junk").expect("damaged");

        let captured = under_lock(&anna, |ctx| super::capture(ctx).expect("captures"));
        let mut plan = crate::plan::Plan::new();
        super::plan_removals(&anna.collection, "anna", &mut plan).expect("plans");

        assert!(captured.held.is_empty());
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
    }
}
