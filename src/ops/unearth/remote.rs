// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unearthing from the remote.
//!
//! An archive that only the remote holds is fetched into `tmp/` together
//! with its listing, checked there, and unpacked from there. It goes into
//! the store only once the extracted tree has passed the strict
//! verification, so a refusal leaves the store as it was.

use super::local::{check_archive, is_out_of_space};
use super::{
    Planned, Shortfall, UnearthError, UnearthPlan, UnearthRequest, UnearthSource, io_error,
    plan_from,
};
use crate::clock::Clock;
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::space::available_space;
use crate::id::ObjectId;
use crate::index;
use crate::listing::{self, ListingEntry, ListingType};
use crate::lock::RunLock;
use crate::meta::{self, FileName, Metadata};
use crate::ops::status::{has_pending_remote_work, status};
use crate::pack::hashing::HashingReader;
use crate::pack::live::check_stream;
use crate::remote::away::{self, with_overdue};
use crate::remote::{KnownVersions, MetaFile, Reachability, Remote, RemoteError};
use crate::report::Warning;
use crate::store::{Current, Put, Source, Store, Unreadable};
use jiff::Timestamp;
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Files in `tmp/` that go when the unearth is over, whichever way it
/// ends. A file the store took is gone from there already.
#[derive(Default)]
struct Parts(Vec<PathBuf>);

impl Drop for Parts {
    fn drop(&mut self) {
        for part in &self.0 {
            let _ = std::fs::remove_file(part);
        }
    }
}

/// Where the listing the unearth compares with came from.
#[derive(Clone, Copy)]
enum ListingOrigin {
    Store,
    Remote,
}

/// The archive an unearth reads, with the listing of it, and what goes
/// into the store once the tree is verified.
pub(crate) struct Arrived {
    pub(crate) archive: PathBuf,
    /// The bytes the extraction of the archive needs below `staging/`. A
    /// fetched archive sits in `tmp/` already, so its own size is not in
    /// here. A live package has no listing; its size is a lower bound.
    pub(crate) room: u64,
    /// The members the listing names and where it came from. A live
    /// package has none; the strict verification compares the tree with
    /// the package itself.
    listing: Option<(Vec<ListingEntry>, ListingOrigin)>,
    /// Fetched files, by their final names.
    fetched: Vec<(FileName, PathBuf)>,
    _parts: Parts,
}

/// What the store took from an unearth.
#[derive(Default)]
pub(crate) struct Stored(Vec<FileName>);

impl Stored {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Removes the files this run put, if the unearth did not happen.
    pub(crate) fn undo(&self, collection: &Collection, plan: &UnearthPlan) {
        let store = Store::new(collection);
        for name in &self.0 {
            let _ = std::fs::remove_file(store.file_path(plan.id, name));
        }
    }
}

impl Arrived {
    /// An archive the store holds.
    pub(crate) fn held(
        collection: &Collection,
        plan: &UnearthPlan,
        archive: PathBuf,
    ) -> Result<Arrived, UnearthError> {
        Ok(Arrived {
            archive,
            room: plan.needed,
            listing: Some((held_listing(collection, plan)?, ListingOrigin::Store)),
            fetched: Vec::new(),
            _parts: Parts::default(),
        })
    }

    /// Compares the listing with the members the verified tree has.
    pub(crate) fn check_listing(
        &self,
        plan: &UnearthPlan,
        members: &[ListingEntry],
    ) -> Result<(), UnearthError> {
        let Some((listing, origin)) = &self.listing else {
            return Ok(());
        };
        let by_path = |entries: &[ListingEntry]| {
            let mut sorted = entries.to_vec();
            sorted.sort_by(|a, b| a.path.cmp(&b.path));
            sorted
        };
        if by_path(listing) == by_path(members) {
            return Ok(());
        }
        let whose = match origin {
            ListingOrigin::Store => "held on this machine",
            ListingOrigin::Remote => "fetched from the remote",
        };
        Err(UnearthError::ArchiveDiffers {
            file: plan.payload.file.as_str().to_string(),
            what: format!("its listing, {whose}, does not match the archive"),
        })
    }

    /// Puts what was fetched into the store.
    pub(crate) fn store(
        &self,
        collection: &Collection,
        lock: &RunLock,
        plan: &UnearthPlan,
    ) -> Result<Stored, UnearthError> {
        let store = Store::new(collection);
        let mut stored = Stored::default();
        for (name, file) in &self.fetched {
            let put = sha256_of(file)
                .and_then(|sha256| Ok(store.put_file(lock, plan.id, name, file, &sha256)?));
            match put {
                Ok(Put::Stored) => stored.0.push(name.clone()),
                Ok(Put::AlreadyPresent) => {}
                Err(error) => {
                    stored.undo(collection, plan);
                    return Err(error);
                }
            }
        }
        Ok(stored)
    }

    /// Refreshes the index after a listing came into the store. A failure
    /// is a warning: the index is derived and the next run rebuilds it.
    pub(crate) fn update_index(
        collection: &Collection,
        lock: &RunLock,
        stored: &Stored,
    ) -> Option<crate::report::Warning> {
        if stored.is_empty() {
            return None;
        }
        index::update(collection, lock)
            .err()
            .map(|error| crate::report::Warning::new(format!("cannot update the index: {error}")))
    }
}

fn held_listing(
    collection: &Collection,
    plan: &UnearthPlan,
) -> Result<Vec<ListingEntry>, UnearthError> {
    let name = plan
        .payload
        .listing
        .as_ref()
        .expect("planning refuses an archive without a listing");
    let file = Store::new(collection).file_path(plan.id, name);
    Ok(listing::read_all(
        File::open(&file).map_err(io_error("open", &file))?,
    )?)
}

fn sha256_of(path: &Path) -> Result<String, UnearthError> {
    let file = File::open(path).map_err(io_error("open", path))?;
    let mut reader = HashingReader::new(file);
    std::io::copy(&mut reader, &mut std::io::sink()).map_err(io_error("read", path))?;
    Ok(reader.sha256())
}

/// What fetching an archive came to.
pub(crate) enum Fetch {
    Arrived(Arrived),
    /// The staging volume has less room than archive and listing need,
    /// or filled up while a file was copied.
    NoSpace {
        needed: u64,
        available: u64,
    },
}

/// Copies a file of the object into `tmp/`. A volume that fills up while
/// the file is copied, or has no room for the next one, is `Ok(false)`.
fn copy_from_remote(
    remote: &dyn Remote,
    plan: &UnearthPlan,
    name: &FileName,
    part: &Path,
) -> Result<bool, UnearthError> {
    match remote.get_payload(plan.id, name.as_str(), part) {
        Ok(_) => Ok(true),
        Err(RemoteError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            Err(UnearthError::NotOnRemote {
                file: name.as_str().to_string(),
                path: plan.target.clone(),
            })
        }
        Err(error) if is_out_of_space(&error) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Fetches the archive of `plan`, and its listing when the store has
/// none. The listing comes first, so that the room for both is known
/// before the archive moves.
pub(crate) fn fetch(
    collection: &Collection,
    plan: &UnearthPlan,
    remote: &dyn Remote,
    free_space: &dyn Fn(&Path) -> Result<u64, FsxError>,
) -> Result<Fetch, UnearthError> {
    let id = plan.id;
    let staging_dir = collection.staging_dir();
    let no_room = |needed| -> Result<Fetch, UnearthError> {
        let available = free_space(&staging_dir).unwrap_or(0);
        Ok(Fetch::NoSpace { needed, available })
    };
    remote.begin_phase()?;
    let mut parts = Parts::default();
    let mut fetched = Vec::new();

    let listing_name = plan
        .payload
        .listing
        .as_ref()
        .expect("planning refuses an archive without a listing");
    let held = Store::new(collection).file_path(id, listing_name);
    let (entries, origin) = if held.exists() {
        (held_listing(collection, plan)?, ListingOrigin::Store)
    } else {
        let part = collection
            .tmp_dir()
            .join(format!("unearth-{id}.listing.part"));
        parts.0.push(part.clone());
        if !copy_from_remote(remote, plan, listing_name, &part)? {
            return no_room(plan.needed);
        }
        let entries = listing::read_all(File::open(&part).map_err(io_error("open", &part))?)?;
        fetched.push((listing_name.clone(), part));
        (entries, ListingOrigin::Remote)
    };

    let extracted = extracted_size(&entries);
    let needed = plan.payload.size + extracted;
    let available = free_space(&staging_dir)?;
    if available < needed {
        return Ok(Fetch::NoSpace { needed, available });
    }

    let file = &plan.payload.file;
    let part = collection
        .tmp_dir()
        .join(format!("unearth-{id}.{}.part", file.as_str()));
    parts.0.push(part.clone());
    if !copy_from_remote(remote, plan, file, &part)? {
        return no_room(needed);
    }
    check_archive(plan, &part)?;
    let archive = part.clone();
    fetched.push((file.clone(), part));
    Ok(Fetch::Arrived(Arrived {
        archive,
        room: extracted,
        listing: Some((entries, origin)),
        fetched,
        _parts: parts,
    }))
}

/// Fetches the live package of `plan` into `tmp/` and checks it: the file
/// is the one the metadata describes and its zstd stream is whole.
pub(crate) fn fetch_live(
    collection: &Collection,
    plan: &UnearthPlan,
    remote: &dyn Remote,
    free_space: &dyn Fn(&Path) -> Result<u64, FsxError>,
) -> Result<Fetch, UnearthError> {
    let staging_dir = collection.staging_dir();
    // The package and its extraction are on the volume together, so the
    // plan counts both.
    let needed = plan.needed;
    remote.begin_phase()?;
    let available = free_space(&staging_dir)?;
    if available < needed {
        return Ok(Fetch::NoSpace { needed, available });
    }

    let file = &plan.payload.file;
    let part = collection
        .tmp_dir()
        .join(format!("unearth-{}.{}.part", plan.id, file.as_str()));
    let mut parts = Parts::default();
    parts.0.push(part.clone());
    if !copy_from_remote(remote, plan, file, &part)? {
        let available = free_space(&staging_dir).unwrap_or(0);
        return Ok(Fetch::NoSpace { needed, available });
    }
    check_archive(plan, &part)?;
    check_stream(&part)?;
    Ok(Fetch::Arrived(Arrived {
        archive: part,
        room: plan.payload.size,
        listing: None,
        fetched: Vec::new(),
        _parts: parts,
    }))
}

// =========================================================
// Refreshing the metadata before planning
// =========================================================

/// What a run found out about the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// No remote is attached.
    Absent,
    Reachable,
    Away(String),
    Broken(String),
}

/// The remote as a run sees it before it plans: whether it can be used and
/// the metadata versions of the piece that it holds and this machine lacks.
///
/// Dropping it ends the run's use of the remote, whichever way the run
/// ends.
pub struct Refreshed {
    remote: Option<Box<dyn Remote>>,
    standing: Standing,
    /// The versions of the piece that were newer than this machine's
    /// highest committed one, lowest first. A real run has stored them.
    pulled: Vec<MetaFile>,
    /// Nothing is stored and nothing is recorded.
    dry_run: bool,
}

impl Drop for Refreshed {
    fn drop(&mut self) {
        if let Some(remote) = &self.remote {
            remote.finish();
        }
    }
}

/// Looks at the remote of `collection` and pulls the metadata versions of
/// `id` that this machine lacks. A real run, which holds `lock`, stores
/// them and records whether the remote was reachable; a dry run (`lock`
/// is `None`) does neither.
///
/// A remote that is away or broken is no error here, since a plan from
/// local data does not need it; [`Refreshed::check_usable`] says whether
/// the plan does.
pub fn refresh(
    collection: &Collection,
    lock: Option<&RunLock>,
    id: ObjectId,
    now: Timestamp,
) -> Result<Refreshed, UnearthError> {
    refresh_with(crate::remote::open(collection), collection, lock, id, now)
}

pub(crate) fn refresh_with(
    opened: Result<Box<dyn Remote>, RemoteError>,
    collection: &Collection,
    lock: Option<&RunLock>,
    id: ObjectId,
    now: Timestamp,
) -> Result<Refreshed, UnearthError> {
    let mut refreshed = Refreshed {
        remote: None,
        standing: Standing::Absent,
        pulled: Vec::new(),
        dry_run: lock.is_none(),
    };
    let remote = match opened {
        Ok(remote) => remote,
        Err(RemoteError::NoRemote) => return Ok(refreshed),
        Err(error) => {
            refreshed.standing = Standing::Broken(error.to_string());
            return Ok(refreshed);
        }
    };
    refreshed.standing = match remote.reachability() {
        Ok(assessment) => match assessment.reachability {
            Reachability::Reachable => Standing::Reachable,
            Reachability::Away => Standing::Away(assessment.reason),
            Reachability::Broken => Standing::Broken(assessment.reason),
        },
        Err(RemoteError::Away { message, .. }) => Standing::Away(message),
        Err(RemoteError::Broken(reason)) => Standing::Broken(reason),
        Err(error) => {
            refreshed.remote = Some(remote);
            return Err(error.into());
        }
    };
    refreshed.remote = Some(remote);

    if let Some(lock) = lock {
        match &refreshed.standing {
            Standing::Reachable => away::record_reachable(collection, lock, now)?,
            Standing::Away(_) => away::record_away(collection, lock, now)?,
            Standing::Absent | Standing::Broken(_) => {}
        }
    }
    if refreshed.standing != Standing::Reachable {
        return Ok(refreshed);
    }

    let store = Store::new(collection);
    let mut known = KnownVersions::new();
    for object in store.objects()? {
        if let Some(highest) = store.committed_versions(object)?.last() {
            known.insert(object, *highest);
        }
    }
    let remote = refreshed
        .remote
        .as_deref()
        .expect("the remote was just kept");
    remote.begin_phase()?;
    // Other objects' versions come with the same call and are left for
    // `sync`: an unearth changes this piece only.
    let mut pulled: Vec<MetaFile> = remote
        .pull_metadata(&known)?
        .into_iter()
        .filter(|file| file.id == id)
        .collect();
    pulled.sort_by_key(|file| file.number);
    if let Some(lock) = lock {
        for file in &pulled {
            store.store_committed(lock, id, file.number, &file.bytes)?;
        }
    }
    refreshed.pulled = pulled;
    Ok(refreshed)
}

impl Refreshed {
    pub fn standing(&self) -> &Standing {
        &self.standing
    }

    /// The remote, when the run may use it.
    pub fn remote(&self) -> Option<&dyn Remote> {
        if self.standing == Standing::Reachable {
            self.remote.as_deref()
        } else {
            None
        }
    }

    /// Plans the unearth against the metadata the run uses: the store's,
    /// which a real run has brought up to date, or for a dry run the store's
    /// together with what was pulled.
    pub fn plan(
        &self,
        collection: &Collection,
        request: &UnearthRequest,
    ) -> Result<Planned, UnearthError> {
        let current = self.current(collection, request.id)?;
        let planned = plan_from(collection, request, current, &available_space)?;
        self.count_remote_listing(collection, planned)
    }

    /// The metadata of the piece with the pulled versions merged in that
    /// the store does not hold yet; `None` when the store alone decides.
    fn current(
        &self,
        collection: &Collection,
        id: ObjectId,
    ) -> Result<Option<Current>, UnearthError> {
        let store = Store::new(collection);
        let held = store.committed_versions(id)?.last().copied().unwrap_or(0);
        let mut unreadable = Vec::new();
        let mut readable = None;
        for file in self
            .pulled
            .iter()
            .rev()
            .filter(|file| file.number.get() > held)
        {
            match read_pulled(id, file) {
                Ok(metadata) => {
                    readable = Some((file.number.get(), metadata));
                    break;
                }
                Err(version) => unreadable.push(version),
            }
        }
        if unreadable.is_empty() && readable.is_none() {
            return Ok(None);
        }
        Ok(match (store.current(id)?, readable) {
            // A recorded change stays the metadata in use.
            (Some(stored), Some(_)) if stored.source == Source::Pending => Some(Current {
                unreadable,
                ..stored
            }),
            (_, Some((number, metadata))) => Some(Current {
                metadata,
                source: Source::Committed(number),
                unreadable,
            }),
            (Some(mut stored), None) => {
                unreadable.append(&mut stored.unreadable);
                Some(Current {
                    unreadable,
                    ..stored
                })
            }
            // The reference resolved to an object this machine has metadata
            // of, so a store without any cannot meet unreadable pulled
            // versions here.
            (None, None) => None,
        })
    }

    /// A dry run does not fetch the listing of an archive the store lacks
    /// the listing of; it reads it from the remote to count the room the
    /// extraction needs.
    fn count_remote_listing(
        &self,
        collection: &Collection,
        planned: Planned,
    ) -> Result<Planned, UnearthError> {
        let Planned::Unearth(mut plan) = planned else {
            return Ok(planned);
        };
        let Some(remote) = self.remote().filter(|_| self.dry_run) else {
            return Ok(Planned::Unearth(plan));
        };
        // Only an archive has a listing; the room for a live package is
        // known from the metadata.
        if plan.source != UnearthSource::RemoteArchive {
            return Ok(Planned::Unearth(plan));
        }
        let name = plan
            .payload
            .listing
            .as_ref()
            .expect("planning refuses an archive without a listing");
        let held = Store::new(collection).file_path(plan.id, name);
        if held.exists() {
            return Ok(Planned::Unearth(plan));
        }
        let bytes = remote.read_file(plan.id, name.as_str())?;
        plan.needed += extracted_size(&listing::read_all(bytes.as_slice())?);
        let available = available_space(&collection.staging_dir())?;
        plan.shortfall = (available < plan.needed).then_some(Shortfall {
            needed: plan.needed,
            available,
        });
        Ok(Planned::Unearth(plan))
    }

    /// Fails when the plan needs a remote the run cannot use.
    pub fn check_usable(&self, planned: &Planned) -> Result<(), UnearthError> {
        let needs_remote = matches!(
            planned,
            Planned::Unearth(plan) if plan.source != UnearthSource::LocalArchive
        );
        if !needs_remote {
            return Ok(());
        }
        Err(match &self.standing {
            Standing::Reachable => return Ok(()),
            Standing::Absent => RemoteError::NoRemote,
            Standing::Away(message) => RemoteError::Away {
                message: message.clone(),
                overdue: false,
            },
            Standing::Broken(reason) => RemoteError::Broken(reason.clone()),
        }
        .into())
    }

    /// What the run says about the remote: that it is not used, and that
    /// it brought a newer version of the piece.
    pub fn warnings(&self, collection: &Collection) -> Vec<Warning> {
        let mut notes = Vec::new();
        match &self.standing {
            Standing::Away(_) => notes.push(Warning::new(
                "the remote is away; unearthing from local data",
            )),
            Standing::Broken(reason) => notes.push(Warning::new(format!(
                "the remote is broken ({reason}); unearthing from local data, its metadata was not pulled"
            ))),
            Standing::Absent | Standing::Reachable => {}
        }
        if let Some(note) = self.newer_version_note(collection) {
            notes.push(note);
        }
        notes
    }

    fn newer_version_note(&self, collection: &Collection) -> Option<Warning> {
        let newest = self.pulled.last()?;
        let current = self
            .current(collection, newest.id)
            .ok()
            .flatten()
            .or_else(|| Store::new(collection).current(newest.id).ok().flatten())?;
        let path = &current.metadata.path;
        Some(Warning::new(match current.source {
            Source::Committed(number) => {
                format!("the remote has metadata version {number} of `{path}`, which this run uses")
            }
            Source::Pending => format!(
                "the remote has metadata version {} of `{path}`; this machine's recorded change is used and `sync` applies it on top",
                newest.number
            ),
        }))
    }
}

fn read_pulled(id: ObjectId, file: &MetaFile) -> Result<Metadata, Unreadable> {
    let unreadable = |reason: String| Unreadable {
        number: file.number.get(),
        format: meta::read_format(&file.bytes).ok(),
        reason,
    };
    let metadata = meta::parse(&file.bytes).map_err(|error| unreadable(error.to_string()))?;
    if metadata.id != id {
        return Err(unreadable(format!("it belongs to {}", metadata.id)));
    }
    Ok(metadata)
}

/// The bytes the files of a listing take once extracted.
fn extracted_size(entries: &[ListingEntry]) -> u64 {
    entries
        .iter()
        .filter(|member| member.kind == ListingType::File)
        .map(|member| member.size)
        .sum()
}

/// Refreshes from the remote and plans: the two steps a run takes before
/// it changes anything. The run goes ahead from local data when the plan
/// needs no remote, and fails by how the remote stands when it does.
pub fn prepare(
    collection: &Collection,
    lock: Option<&RunLock>,
    request: &UnearthRequest,
    now: Timestamp,
) -> Result<(Refreshed, Planned), UnearthError> {
    let refreshed = refresh(collection, lock, request.id, now)?;
    let planned = refreshed.plan(collection, request)?;
    refreshed.check_usable(&planned)?;
    Ok((refreshed, planned))
}

/// Gives an away error the `overdue` mark when it is due: the remote has
/// been out of reach for longer than `away_warn_days` while remote work
/// is pending. A `status` that fails counts as pending work, so that an
/// error never hides an overdue remote.
pub fn overdue<T>(
    collection: &Collection,
    result: Result<T, UnearthError>,
    clock: &dyn Clock,
) -> Result<T, crate::error::Error> {
    with_overdue(
        result.map_err(UnearthError::into_error),
        collection,
        || {
            status(collection, clock)
                .map(|report| has_pending_remote_work(&report.pieces))
                .unwrap_or(true)
        },
        clock.now(),
    )
}

#[cfg(test)]
pub(in crate::ops::unearth) mod tests {
    use super::*;
    use crate::error::HasKind;
    use crate::fsx::space::available_space;
    use crate::fsx::tree::TreeRoot;
    use crate::fsx::{FsxError, RestoreModes};
    use crate::id::ObjectId;
    use crate::ops::unearth::local::{self, Environment};
    use crate::ops::unearth::tests::{FILES, Fixture, ID, id, path};
    use crate::ops::unearth::{Skipped, UnearthReport, UnearthSource};
    use crate::pack::extract::Refusal;
    use crate::pack::{PackError, Packager, TarZstd, TargetRules};
    use crate::relpath::RelPath;
    use crate::remote::testing::{Fault, FaultRemote, Op, path_remote, remote_root};
    use crate::remote::{
        Assessment, Created, KnownVersions, MetaFile, Placement, RemoteError, RemoteFile, Transfer,
        Upload,
    };
    use crate::store::Store;
    use crate::testing::package::Packaged;
    use crate::testing::package::{RawMember, compress, on_disk, raw_tar};
    use crate::tracking::EntryState;
    use jiff::Timestamp;
    use sha2::Digest;
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// A remote root and the remote on it, holding the archive and listing
    /// of an archived piece that this machine no longer has.
    struct Scenario {
        fx: Fixture,
        root: tempfile::TempDir,
        remote: FaultRemote,
        payload: FileName,
        listing: FileName,
    }

    /// The archive of a tar written by hand.
    fn from_tar(members: &[RawMember<'_>]) -> Packaged {
        let bytes = compress(&raw_tar(members));
        let (_dir, file) = on_disk(&bytes);
        let listing = TarZstd.list(&file).unwrap_or_default();
        Packaged {
            packed: crate::pack::Packed {
                size: bytes.len() as u64,
                sha256: crate::pack::hashing::hex(&sha2::Sha256::digest(&bytes)),
                listing,
                notes: Vec::new(),
            },
            bytes,
        }
    }

    /// The room a volume of `base` bytes has left while `tmp/` holds files:
    /// what a fetch has already put there is gone from it.
    pub(in crate::ops::unearth) fn after_tmp(
        tmp: std::path::PathBuf,
        base: u64,
    ) -> impl Fn(&std::path::Path) -> Result<u64, FsxError> {
        move |_| {
            let held: u64 = fs::read_dir(&tmp)
                .expect("lists")
                .map(|entry| entry.expect("an entry").metadata().expect("stats").len())
                .sum();
            Ok(base - held)
        }
    }

    impl Scenario {
        /// `keep_listing` leaves the listing in the local store.
        fn new(keep_listing: bool) -> Scenario {
            let mut fx = Fixture::new();
            fx.archived(ID, "projects/a", FILES);
            Scenario::moved(fx, keep_listing)
        }

        fn of(packaged: &Packaged) -> Scenario {
            let mut fx = Fixture::new();
            fx.archive_package(ID, "projects/a", packaged);
            Scenario::moved(fx, false)
        }

        fn moved(fx: Fixture, keep_listing: bool) -> Scenario {
            let current = Store::new(&fx.collection)
                .current(id(ID))
                .expect("reads")
                .expect("metadata");
            let payload = current.metadata.payloads[0].file.clone();
            let listing = current.metadata.payloads[0]
                .listing
                .clone()
                .expect("a listing");
            let root = remote_root();
            let there = root.path().join("objects").join(ID);
            fs::create_dir_all(&there).expect("the object directory");
            let here = fx.collection.object_dir(id(ID));
            fs::copy(here.join("meta.1"), there.join("meta.1")).expect("metadata");
            fs::copy(here.join(payload.as_str()), there.join(payload.as_str())).expect("payload");
            fs::copy(here.join(listing.as_str()), there.join(listing.as_str())).expect("listing");
            fs::remove_file(here.join(payload.as_str())).expect("the archive is not held");
            if !keep_listing {
                fs::remove_file(here.join(listing.as_str())).expect("the listing is not held");
            }
            let mut entry = fx.entry(ID);
            entry.local_payloads.clear();
            fx.write_entry(&mut entry);
            let remote = FaultRemote::new(path_remote(root.path(), "imperius"));
            Scenario {
                fx,
                root,
                remote,
                payload,
                listing,
            }
        }

        fn on_remote(&self, name: &FileName) -> std::path::PathBuf {
            self.root
                .path()
                .join("objects")
                .join(ID)
                .join(name.as_str())
        }

        fn change_on_remote(&self, name: &FileName, change: impl FnOnce(&mut Vec<u8>)) {
            let file = self.on_remote(name);
            let mut bytes = fs::read(&file).expect("reads");
            change(&mut bytes);
            fs::write(&file, bytes).expect("written");
        }

        fn environment<'a>(
            free_space: &'a dyn Fn(&std::path::Path) -> Result<u64, FsxError>,
            rename: local::Rename<'a>,
            probe: &'a dyn Fn(&TreeRoot) -> std::io::Result<TargetRules>,
        ) -> Environment<'a> {
            Environment {
                packager: &TarZstd,
                probe,
                free_space,
                rename,
            }
        }

        fn unearth_through(
            &self,
            remote: &dyn Remote,
            env: &Environment<'_>,
        ) -> Result<UnearthReport, UnearthError> {
            let plan = self.fx.planned(&self.fx.request(ID));
            assert_eq!(plan.source(), UnearthSource::RemoteArchive);
            local::unearth_archive(
                &self.fx.collection,
                &plan,
                &self.fx.lock,
                &self.fx.clock,
                env,
                Vec::new(),
                Some(remote),
            )
        }

        fn unearth(&self) -> Result<UnearthReport, UnearthError> {
            self.unearth_through(
                &self.remote,
                &Scenario::environment(
                    &available_space,
                    &crate::fsx::moves::move_dir,
                    &TargetRules::probe,
                ),
            )
        }

        fn held(&self, name: &FileName) -> bool {
            Store::new(&self.fx.collection)
                .file_path(id(ID), name)
                .exists()
        }

        fn tmp_is_empty(&self) -> bool {
            fs::read_dir(self.fx.collection.tmp_dir())
                .expect("lists")
                .count()
                == 0
        }

        /// Nothing of the fetch is left: no part file, and the store holds
        /// neither the archive nor the listing.
        fn nothing_is_left(&self) -> bool {
            self.tmp_is_empty() && !self.held(&self.payload) && !self.held(&self.listing)
        }
    }

    #[test]
    fn a_fetched_archive_is_unearthed_and_stays_local() {
        let scenario = Scenario::new(false);

        let report = scenario.unearth().expect("the unearth works");

        assert!(report.unearthed, "{report:?}");
        assert_eq!(
            fs::read_to_string(scenario.fx.at("projects/a/notes.txt")).expect("reads"),
            "hello"
        );
        assert!(scenario.held(&scenario.payload));
        assert!(scenario.held(&scenario.listing));
        let entry = scenario.fx.entry(ID);
        assert_eq!(entry.local_payloads.len(), 1);
        assert_eq!(entry.local_payloads[0].file, scenario.payload);
        assert!(scenario.tmp_is_empty());
    }

    #[test]
    fn the_stored_files_are_the_remotes_and_the_payload_record_is_not_touched() {
        let scenario = Scenario::new(false);

        scenario.unearth().expect("the unearth works");

        let store = Store::new(&scenario.fx.collection);
        for name in [&scenario.payload, &scenario.listing] {
            assert_eq!(
                fs::read(store.file_path(id(ID), name)).expect("reads"),
                fs::read(scenario.on_remote(name)).expect("reads"),
            );
        }
        let current = store.current(id(ID)).expect("reads").expect("metadata");
        assert_eq!(current.metadata.payloads[0].verified, None);
    }

    #[test]
    fn a_listing_the_store_holds_is_not_fetched() {
        let scenario = Scenario::new(true);

        scenario.unearth().expect("the unearth works");

        let fetched: Vec<String> = scenario
            .remote
            .calls()
            .into_iter()
            .filter(|call| call.op == Op::GetPayload)
            .map(|call| call.name)
            .collect();
        assert_eq!(fetched, [scenario.payload.as_str().to_string()]);
    }

    #[test]
    fn the_phase_begins_before_the_first_fetch() {
        let scenario = Scenario::new(false);

        scenario.unearth().expect("the unearth works");

        let calls = scenario.remote.calls();
        assert_eq!(calls[0].op, Op::BeginPhase);
        assert_eq!(scenario.remote.count(Op::BeginPhase), 1);
    }

    #[test]
    fn an_archive_of_another_size_than_the_metadata_says_is_refused() {
        let scenario = Scenario::new(false);
        scenario.change_on_remote(&scenario.payload, |bytes| bytes.push(0));

        let error = scenario.unearth().expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. } if what.contains("bytes")),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
    }

    #[test]
    fn an_archive_of_the_same_size_and_other_bytes_is_refused() {
        let scenario = Scenario::new(false);
        scenario.change_on_remote(&scenario.payload, |bytes| {
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0xff;
        });

        let error = scenario.unearth().expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. } if what.contains("SHA-256")),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
    }

    #[test]
    fn an_archive_with_an_oversized_record_is_refused_before_anything_is_read() {
        let big = vec![b'x'; 70_000];
        let scenario = Scenario::of(&from_tar(&[
            RawMember {
                kind: b'x',
                data: &big,
                ..RawMember::file(b"a/PaxHeader", b"")
            },
            RawMember {
                mode: 0o755,
                ..RawMember::dir(b"a/")
            },
            RawMember::file(b"a/f", b"1"),
        ]));

        let error = scenario.unearth().expect_err("refused");

        assert!(
            matches!(
                &error,
                UnearthError::Pack(PackError::Refused {
                    refusal: Refusal::OversizedRecord,
                    ..
                })
            ),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
        assert!(!scenario.fx.at("projects/a").exists());
    }

    fn other_listing() -> Vec<u8> {
        let other = from_tar(&[
            RawMember::dir(b"a/"),
            RawMember::file(b"a/other", b"different"),
        ]);
        let mut bytes = Vec::new();
        listing::write(other.packed.listing.iter(), &mut bytes).expect("a listing");
        bytes
    }

    #[test]
    fn a_fetched_listing_of_another_archive_is_refused() {
        let scenario = Scenario::new(false);
        scenario.change_on_remote(&scenario.listing, |bytes| *bytes = other_listing());

        let error = scenario.unearth().expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. }
                if what.contains("listing") && what.contains("remote")),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
        assert!(!scenario.fx.at("projects/a").exists());
    }

    #[test]
    fn a_held_listing_of_another_archive_is_refused() {
        let scenario = Scenario::new(true);
        let held = Store::new(&scenario.fx.collection).file_path(id(ID), &scenario.listing);
        fs::write(&held, other_listing()).expect("written");

        let error = scenario.unearth().expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. }
                if what.contains("listing") && what.contains("this machine")),
            "{error}"
        );
        assert!(!scenario.held(&scenario.payload));
        assert!(scenario.tmp_is_empty());
    }

    #[test]
    fn a_listing_that_is_not_zstd_is_refused() {
        let scenario = Scenario::new(false);
        scenario.change_on_remote(&scenario.listing, |bytes| *bytes = b"not zstd".to_vec());

        let error = scenario.unearth().expect_err("refused");

        assert!(matches!(error, UnearthError::Listing(_)), "{error}");
        assert!(scenario.nothing_is_left());
    }

    #[test]
    fn a_volume_with_room_for_archive_and_extraction_unearths_a_held_listing_piece() {
        let scenario = Scenario::new(true);
        let plan = scenario.fx.planned(&scenario.fx.request(ID));
        let room = after_tmp(scenario.fx.collection.tmp_dir(), plan.needed);
        let env = Scenario::environment(&room, &crate::fsx::moves::move_dir, &TargetRules::probe);

        let report = scenario
            .unearth_through(&scenario.remote, &env)
            .expect("unearths");

        assert!(report.unearthed, "{report:?}");
    }

    #[test]
    fn a_volume_one_byte_short_of_archive_and_extraction_skips_before_the_archive_moves() {
        let scenario = Scenario::new(true);
        let plan = scenario.fx.planned(&scenario.fx.request(ID));
        let room = after_tmp(scenario.fx.collection.tmp_dir(), plan.needed - 1);
        let env = Scenario::environment(&room, &crate::fsx::moves::move_dir, &TargetRules::probe);

        let report = scenario
            .unearth_through(&scenario.remote, &env)
            .expect("skips");

        assert!(!report.unearthed);
        assert_eq!(scenario.remote.count(Op::GetPayload), 0);
        assert!(scenario.tmp_is_empty());
        assert!(!scenario.held(&scenario.payload));
    }

    #[test]
    fn a_fetched_listing_counts_in_the_room_without_a_panic_or_a_skip() {
        let scenario = Scenario::new(false);
        let tmp = scenario.fx.collection.tmp_dir();
        let listing_size = fs::metadata(scenario.on_remote(&scenario.listing))
            .expect("the listing")
            .len();
        let archive_size = fs::metadata(scenario.on_remote(&scenario.payload))
            .expect("the archive")
            .len();
        let listed: u64 = FILES.iter().map(|(_, content)| content.len() as u64).sum();
        let room = after_tmp(tmp, listing_size + archive_size + listed);
        let env = Scenario::environment(&room, &crate::fsx::moves::move_dir, &TargetRules::probe);

        let report = scenario
            .unearth_through(&scenario.remote, &env)
            .expect("unearths");

        assert!(report.unearthed, "{report:?}");
    }

    #[test]
    fn a_volume_too_small_for_archive_and_listing_skips_before_the_archive_moves() {
        let scenario = Scenario::new(false);
        let plan = scenario.fx.planned(&scenario.fx.request(ID));
        let room = |_: &std::path::Path| Ok(plan.needed + 1);
        let env = Scenario::environment(&room, &crate::fsx::moves::move_dir, &TargetRules::probe);

        let report = scenario
            .unearth_through(&scenario.remote, &env)
            .expect("skips");

        assert!(!report.unearthed);
        let Some(Skipped::NoSpace { needed, .. }) = report.skipped else {
            panic!("expected a skip, got {report:?}");
        };
        assert_eq!(needed, plan.needed + 17);
        let fetched: Vec<String> = scenario
            .remote
            .calls()
            .into_iter()
            .filter(|call| call.op == Op::GetPayload)
            .map(|call| call.name)
            .collect();
        assert_eq!(fetched, [scenario.listing.as_str().to_string()]);
        assert!(scenario.nothing_is_left());
        assert_eq!(scenario.fx.entry(ID).state, EntryState::Gone);
    }

    /// A remote whose copy of one file fails with a full volume.
    pub(in crate::ops::unearth) struct Full<'a> {
        pub(in crate::ops::unearth) inner: &'a FaultRemote,
        pub(in crate::ops::unearth) file: String,
    }

    impl Remote for Full<'_> {
        fn reachability(&self) -> Result<Assessment, RemoteError> {
            self.inner.reachability()
        }
        fn begin_phase(&self) -> Result<(), RemoteError> {
            self.inner.begin_phase()
        }
        fn clock(&self) -> Result<Timestamp, RemoteError> {
            self.inner.clock()
        }
        fn pull_metadata(&self, known: &KnownVersions) -> Result<Vec<MetaFile>, RemoteError> {
            self.inner.pull_metadata(known)
        }
        fn list_objects(&self) -> Result<Vec<ObjectId>, RemoteError> {
            self.inner.list_objects()
        }
        fn list_dir(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
            self.inner.list_dir(id)
        }
        fn list_partial(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
            self.inner.list_partial(id)
        }
        fn read_file(&self, id: ObjectId, name: &str) -> Result<Vec<u8>, RemoteError> {
            self.inner.read_file(id, name)
        }
        fn get_payload(
            &self,
            id: ObjectId,
            name: &str,
            to: &Path,
        ) -> Result<Transfer, RemoteError> {
            if name == self.file {
                fs::write(to, b"half").expect("a part file");
                return Err(RemoteError::Io {
                    action: "write",
                    path: to.display().to_string(),
                    source: std::io::Error::from_raw_os_error(libc::ENOSPC),
                });
            }
            self.inner.get_payload(id, name, to)
        }
        fn create_exclusive(&self, id: ObjectId, name: &str) -> Result<Created, RemoteError> {
            self.inner.create_exclusive(id, name)
        }
        fn write_temp(&self, id: ObjectId, temp: &str, bytes: &[u8]) -> Result<(), RemoteError> {
            self.inner.write_temp(id, temp, bytes)
        }
        fn rename_into_place(
            &self,
            id: ObjectId,
            temp: &str,
            name: &str,
        ) -> Result<Placement, RemoteError> {
            self.inner.rename_into_place(id, temp, name)
        }
        fn put_payload(&self, upload: &Upload<'_>) -> Result<Transfer, RemoteError> {
            self.inner.put_payload(upload)
        }
        fn delete(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
            self.inner.delete(id, name)
        }
        fn delete_partial(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
            self.inner.delete_partial(id, name)
        }
        fn checksum(&self, id: ObjectId, name: &str) -> Result<String, RemoteError> {
            self.inner.checksum(id, name)
        }
        fn flush(&self) -> Result<(), RemoteError> {
            self.inner.flush()
        }
        fn finish(&self) {
            self.inner.finish()
        }
    }

    fn skipped_for_want_of_room(of: impl Fn(&Scenario) -> String) {
        let scenario = Scenario::new(false);
        let full = Full {
            inner: &scenario.remote,
            file: of(&scenario),
        };
        let env = Scenario::environment(
            &available_space,
            &crate::fsx::moves::move_dir,
            &TargetRules::probe,
        );

        let report = scenario.unearth_through(&full, &env).expect("skips");

        assert!(!report.unearthed);
        assert!(
            matches!(report.skipped, Some(Skipped::NoSpace { .. })),
            "{report:?}"
        );
        assert!(scenario.nothing_is_left());
        assert_eq!(scenario.fx.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_full_volume_while_the_listing_is_fetched_skips_the_piece() {
        skipped_for_want_of_room(|scenario| scenario.listing.as_str().to_string());
    }

    #[test]
    fn a_full_volume_while_the_archive_is_fetched_skips_the_piece() {
        skipped_for_want_of_room(|scenario| scenario.payload.as_str().to_string());
    }

    #[test]
    fn a_remote_that_lost_the_archive_says_so() {
        let scenario = Scenario::new(false);
        fs::remove_file(scenario.on_remote(&scenario.payload)).expect("gone");

        let error = scenario.unearth().expect_err("fails");

        assert!(
            matches!(&error, UnearthError::NotOnRemote { file, path }
                if file == scenario.payload.as_str() && *path == super::super::tests::path("projects/a")),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
    }

    #[test]
    fn a_remote_that_lost_the_listing_says_so() {
        let scenario = Scenario::new(false);
        fs::remove_file(scenario.on_remote(&scenario.listing)).expect("gone");

        let error = scenario.unearth().expect_err("fails");

        assert!(
            matches!(&error, UnearthError::NotOnRemote { file, .. } if file == scenario.listing.as_str()),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
    }

    #[test]
    fn another_failure_of_the_remote_is_passed_on() {
        let scenario = Scenario::new(false);
        scenario.remote.fail(Op::GetPayload, 2, Fault::Io);

        let error = scenario.unearth().expect_err("fails");

        assert!(
            matches!(error, UnearthError::Remote(RemoteError::Io { .. })),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
    }

    #[test]
    fn names_the_volume_cannot_hold_leave_the_store_as_it_was() {
        let scenario = Scenario::of(&from_tar(&[
            RawMember::dir(b"a/"),
            RawMember::file(b"a/Foo", b"1"),
            RawMember::file(b"a/foo", b"2"),
        ]));
        let probe = |_: &TreeRoot| Ok(TargetRules::macos());
        let env = Scenario::environment(&available_space, &crate::fsx::moves::move_dir, &probe);

        let error = scenario
            .unearth_through(&scenario.remote, &env)
            .expect_err("refused");

        assert!(
            matches!(error, UnearthError::Pack(PackError::UnholdableNames(_))),
            "{error}"
        );
        assert!(scenario.nothing_is_left());
    }

    fn failed_rename() -> FsxError {
        FsxError::CrossesDevices {
            from: "a".into(),
            to: "b".into(),
        }
    }

    #[test]
    fn a_rename_that_fails_takes_back_the_entry_and_the_files_this_run_stored() {
        let scenario = Scenario::new(false);
        let rename = |_: &TreeRoot, _: &RelPath, _: &TreeRoot, _: &RelPath| Err(failed_rename());
        let env = Scenario::environment(&available_space, &rename, &TargetRules::probe);

        let error = scenario
            .unearth_through(&scenario.remote, &env)
            .expect_err("fails");

        assert!(matches!(error, UnearthError::Fsx(_)), "{error}");
        assert!(scenario.nothing_is_left());
        let entry = scenario.fx.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert!(entry.local_payloads.is_empty());
    }

    #[test]
    fn files_stay_when_the_previous_entry_cannot_be_written_back() {
        let scenario = Scenario::new(false);
        let tracking = scenario.fx.collection.tracking_dir();
        let _restore = RestoreModes(vec![tracking.clone()]);
        let rename = |_: &TreeRoot, _: &RelPath, _: &TreeRoot, _: &RelPath| {
            fs::set_permissions(&tracking, fs::Permissions::from_mode(0o500)).expect("chmod");
            assert!(
                fs::File::create(tracking.join("probe")).is_err(),
                "the tracking directory must refuse writes; tests do not run as root"
            );
            Err(failed_rename())
        };
        let env = Scenario::environment(&available_space, &rename, &TargetRules::probe);

        let error = scenario
            .unearth_through(&scenario.remote, &env)
            .expect_err("fails");

        assert!(matches!(error, UnearthError::Fsx(_)), "{error}");
        assert!(scenario.held(&scenario.payload));
        assert!(scenario.held(&scenario.listing));
        assert_eq!(scenario.fx.entry(ID).state, EntryState::Unearthing);
        assert!(scenario.tmp_is_empty());
    }

    #[test]
    fn a_remote_that_is_away_keeps_its_kind() {
        let scenario = Scenario::new(false);
        scenario.remote.fail(Op::BeginPhase, 1, Fault::Away);

        let error = scenario.unearth().expect_err("fails");

        assert_eq!(error.kind(), crate::error::ErrorKind::RemoteAway);
        assert!(scenario.nothing_is_left());
    }

    #[test]
    fn a_file_another_run_put_in_place_in_the_meantime_stays_when_the_unearth_fails() {
        let scenario = Scenario::new(false);
        let there = scenario.on_remote(&scenario.payload);
        let here = Store::new(&scenario.fx.collection).file_path(id(ID), &scenario.payload);
        scenario.remote.after(Op::GetPayload, 2, move |_| {
            fs::copy(&there, &here).expect("the same bytes appear");
        });
        let rename = |_: &TreeRoot, _: &RelPath, _: &TreeRoot, _: &RelPath| Err(failed_rename());
        let env = Scenario::environment(&available_space, &rename, &TargetRules::probe);

        scenario
            .unearth_through(&scenario.remote, &env)
            .expect_err("fails");

        assert!(scenario.held(&scenario.payload), "it was not this run's");
        assert!(!scenario.held(&scenario.listing));
    }

    #[test]
    fn a_name_taken_with_other_bytes_takes_back_what_was_put_before_it() {
        let scenario = Scenario::new(false);
        let here = Store::new(&scenario.fx.collection).file_path(id(ID), &scenario.payload);
        scenario.remote.after(Op::GetPayload, 2, move |_| {
            fs::write(&here, b"other bytes").expect("another file appears");
        });

        let error = scenario.unearth().expect_err("fails");

        assert!(matches!(error, UnearthError::Store(_)), "{error}");
        assert!(!scenario.held(&scenario.listing));
        assert!(scenario.tmp_is_empty());
        assert!(!scenario.fx.at("projects/a").exists());
    }

    #[test]
    fn a_listing_that_vanished_after_planning_stops_a_local_unearth() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let plan = fx.planned(&fx.request(ID));
        let listing = plan.payload.listing.clone().expect("a listing");
        fs::remove_file(Store::new(&fx.collection).file_path(id(ID), &listing)).expect("gone");
        let env = Scenario::environment(
            &available_space,
            &crate::fsx::moves::move_dir,
            &TargetRules::probe,
        );

        let error = local::unearth_archive(
            &fx.collection,
            &plan,
            &fx.lock,
            &fx.clock,
            &env,
            Vec::new(),
            None,
        )
        .expect_err("fails");

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
    }

    #[test]
    fn a_held_listing_that_is_not_zstd_stops_the_unearth() {
        let scenario = Scenario::new(true);
        let held = Store::new(&scenario.fx.collection).file_path(id(ID), &scenario.listing);
        let plan = scenario.fx.planned(&scenario.fx.request(ID));
        fs::write(&held, b"not zstd").expect("written");
        let env = Scenario::environment(
            &available_space,
            &crate::fsx::moves::move_dir,
            &TargetRules::probe,
        );

        let error = local::unearth_archive(
            &scenario.fx.collection,
            &plan,
            &scenario.fx.lock,
            &scenario.fx.clock,
            &env,
            Vec::new(),
            Some(&scenario.remote),
        )
        .expect_err("fails");

        assert!(matches!(error, UnearthError::Listing(_)), "{error}");
    }

    #[test]
    fn an_index_that_cannot_be_updated_is_a_warning() {
        let scenario = Scenario::new(false);
        let index = scenario.fx.collection.index_dir();
        fs::create_dir_all(&index).expect("the index directory");
        let _restore = RestoreModes(vec![index.clone()]);
        fs::set_permissions(&index, fs::Permissions::from_mode(0o500)).expect("chmod");
        assert!(
            fs::File::create(index.join("probe")).is_err(),
            "the index directory must refuse writes; tests do not run as root"
        );

        let report = scenario.unearth().expect("the unearth works");

        assert!(report.unearthed);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("cannot update the index")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn the_order_of_the_calls_is_listing_then_archive() {
        let scenario = Scenario::new(false);
        let seen = RefCell::new(Vec::new());
        scenario.unearth().expect("the unearth works");
        seen.borrow_mut().extend(scenario.remote.calls());

        let names: Vec<String> = seen
            .borrow()
            .iter()
            .filter(|call| call.op == Op::GetPayload)
            .map(|call| call.name.clone())
            .collect();
        assert_eq!(
            names,
            [
                scenario.listing.as_str().to_string(),
                scenario.payload.as_str().to_string()
            ]
        );
        let _ = path("projects/a");
    }

    // =====================================================
    // Refreshing before the plan
    // =====================================================

    use crate::error::{Error, ErrorKind};
    use crate::ops::unearth::Planned;
    use crate::store::Source;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    impl Scenario {
        /// The remote holds a version 2 of the piece that moved it.
        fn newer_on_remote(&self) {
            let there = self.root.path().join("objects").join(ID);
            let mut document: serde_json::Value =
                serde_json::from_slice(&fs::read(there.join("meta.1")).expect("reads"))
                    .expect("json");
            document["path"] = serde_json::json!("projects/b");
            document["name"] = serde_json::json!("b");
            fs::write(
                there.join("meta.2"),
                serde_json::to_vec(&document).expect("json"),
            )
            .expect("written");
        }

        /// A change recorded here that no sync has committed: remote work
        /// is waiting.
        fn record_pending_change(&self) {
            let object_dir = self.fx.collection.object_dir(id(ID));
            fs::copy(
                object_dir.join("meta.1"),
                self.fx.collection.meta_pending_path(id(ID)),
            )
            .expect("copied");
        }

        fn state_file(&self) -> serde_json::Value {
            let bytes = fs::read(self.fx.collection.remote_state_path()).expect("the state file");
            serde_json::from_slice(&bytes).expect("json")
        }

        fn refresh(&self, with_lock: bool) -> Result<Refreshed, UnearthError> {
            let lock = with_lock.then_some(&self.fx.lock);
            let remote = FaultRemote::new(path_remote(self.root.path(), "imperius"));
            refresh_with(
                Ok(Box::new(remote)),
                &self.fx.collection,
                lock,
                id(ID),
                self.fx.clock.now(),
            )
        }
    }

    fn refuse_away(scenario: &Scenario) -> Refreshed {
        let remote = FaultRemote::new(path_remote(scenario.root.path(), "imperius"));
        remote.fail(Op::Reachability, 1, Fault::Away);
        refresh_with(
            Ok(Box::new(remote)),
            &scenario.fx.collection,
            Some(&scenario.fx.lock),
            id(ID),
            scenario.fx.clock.now(),
        )
        .expect("an away remote is a standing, not an error")
    }

    fn needs_remote(scenario: &Scenario) -> Planned {
        scenario
            .fx
            .plan(&scenario.fx.request(ID))
            .expect("planned from local data")
    }

    #[test]
    fn a_newer_version_of_the_remote_is_stored_before_planning() {
        let scenario = Scenario::new(true);
        scenario.newer_on_remote();

        let refreshed = scenario.refresh(true).expect("refreshed");

        let current = Store::new(&scenario.fx.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata");
        assert_eq!(current.source, Source::Committed(2));
        let planned = refreshed
            .plan(&scenario.fx.collection, &scenario.fx.request(ID))
            .expect("planned");
        assert!(
            matches!(&planned, Planned::Unearth(plan) if plan.target() == &path("projects/b")),
            "{planned:?}"
        );
        assert_eq!(
            refreshed.warnings(&scenario.fx.collection)[0].message,
            "the remote has metadata version 2 of `projects/b`, which this run uses"
        );
    }

    #[test]
    fn a_dry_run_plans_against_the_newer_version_and_stores_nothing() {
        let scenario = Scenario::new(true);
        scenario.newer_on_remote();

        let refreshed = scenario.refresh(false).expect("refreshed");

        let store = Store::new(&scenario.fx.collection);
        assert_eq!(store.committed_versions(id(ID)).expect("lists"), [1]);
        let planned = refreshed
            .plan(&scenario.fx.collection, &scenario.fx.request(ID))
            .expect("planned");
        assert!(
            matches!(&planned, Planned::Unearth(plan) if plan.target() == &path("projects/b")),
            "{planned:?}"
        );
        assert!(!scenario.fx.collection.remote_state_path().exists());
    }

    #[test]
    fn a_recorded_change_wins_over_a_newer_remote_version_and_the_note_says_so() {
        let scenario = Scenario::new(true);
        scenario.newer_on_remote();
        let object_dir = scenario.fx.collection.object_dir(id(ID));
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(object_dir.join("meta.1")).expect("reads"))
                .expect("json");
        document["path"] = serde_json::json!("projects/c");
        document["name"] = serde_json::json!("c");
        fs::write(
            scenario.fx.collection.meta_pending_path(id(ID)),
            serde_json::to_vec(&document).expect("json"),
        )
        .expect("written");

        let refreshed = scenario.refresh(false).expect("refreshed");

        let planned = refreshed
            .plan(&scenario.fx.collection, &scenario.fx.request(ID))
            .expect("planned");
        assert!(
            matches!(&planned, Planned::Unearth(plan) if plan.target() == &path("projects/c")),
            "{planned:?}"
        );
        assert_eq!(
            refreshed.warnings(&scenario.fx.collection)[0].message,
            "the remote has metadata version 2 of `projects/c`; this machine's recorded change \
             is used and `sync` applies it on top"
        );
    }

    #[test]
    fn a_remote_without_news_adds_no_note() {
        let scenario = Scenario::new(true);

        let refreshed = scenario.refresh(true).expect("refreshed");

        assert_eq!(refreshed.standing(), &Standing::Reachable);
        assert!(refreshed.warnings(&scenario.fx.collection).is_empty());
    }

    #[test]
    fn only_the_metadata_of_the_piece_is_stored() {
        let scenario = Scenario::new(true);
        let other = "01D39ZY06FGSCTVN4T2V9PKHFZ";
        let there = scenario.root.path().join("objects").join(other);
        fs::create_dir_all(&there).expect("dir");
        fs::write(there.join("meta.1"), b"{}").expect("written");

        scenario.refresh(true).expect("refreshed");

        let store = Store::new(&scenario.fx.collection);
        assert!(
            store
                .committed_versions(id(other))
                .expect("lists")
                .is_empty()
        );
    }

    #[test]
    fn a_reachable_remote_is_recorded_by_a_real_run_only() {
        let scenario = Scenario::new(true);
        scenario.refresh(false).expect("refreshed");
        assert!(!scenario.fx.collection.remote_state_path().exists());

        scenario.refresh(true).expect("refreshed");

        assert_eq!(
            scenario.state_file()["last_reachable"],
            "2026-11-04T12:00:00Z"
        );
    }

    #[test]
    fn an_away_remote_is_a_standing_and_recorded() {
        let scenario = Scenario::new(true);

        let refreshed = refuse_away(&scenario);

        assert!(matches!(refreshed.standing(), Standing::Away(_)));
        assert!(refreshed.remote().is_none());
        assert_eq!(
            scenario.state_file()["away_first_seen"],
            "2026-11-04T12:00:00Z"
        );
        let notes = refreshed.warnings(&scenario.fx.collection);
        assert!(
            notes[0]
                .message
                .starts_with("the remote is away; unearthing from local data"),
            "{notes:?}"
        );
    }

    #[test]
    fn a_remote_that_cannot_be_opened_is_broken_and_nothing_is_read() {
        let scenario = Scenario::new(true);

        let refreshed = refresh_with(
            Err(RemoteError::Unsupported("an ssh remote".into())),
            &scenario.fx.collection,
            Some(&scenario.fx.lock),
            id(ID),
            scenario.fx.clock.now(),
        )
        .expect("a broken remote is a standing");

        assert_eq!(
            refreshed.standing(),
            &Standing::Broken("an ssh remote is not supported yet".into())
        );
        assert_eq!(
            refreshed.warnings(&scenario.fx.collection)[0].message,
            "the remote is broken (an ssh remote is not supported yet); unearthing from local \
             data, its metadata was not pulled"
        );
        assert!(!scenario.fx.collection.remote_state_path().exists());
    }

    #[test]
    fn a_broken_assessment_is_a_standing() {
        let scenario = Scenario::new(true);
        let remote = FaultRemote::new(path_remote(scenario.root.path(), "imperius"));
        remote.fail(Op::Reachability, 1, Fault::Broken);

        let refreshed = refresh_with(
            Ok(Box::new(remote)),
            &scenario.fx.collection,
            None,
            id(ID),
            scenario.fx.clock.now(),
        )
        .expect("a standing");

        assert!(matches!(refreshed.standing(), Standing::Broken(_)));
    }

    #[test]
    fn no_remote_is_a_standing_without_a_note() {
        let scenario = Scenario::new(true);

        let refreshed = refresh_with(
            Err(RemoteError::NoRemote),
            &scenario.fx.collection,
            None,
            id(ID),
            scenario.fx.clock.now(),
        )
        .expect("a standing");

        assert_eq!(refreshed.standing(), &Standing::Absent);
        assert!(refreshed.warnings(&scenario.fx.collection).is_empty());
    }

    #[test]
    fn a_failing_pull_is_an_error() {
        let scenario = Scenario::new(true);
        let remote = FaultRemote::new(path_remote(scenario.root.path(), "imperius"));
        remote.fail(Op::PullMetadata, 1, Fault::Io);

        let error = refresh_with(
            Ok(Box::new(remote)),
            &scenario.fx.collection,
            None,
            id(ID),
            scenario.fx.clock.now(),
        )
        .err()
        .expect("fails");

        assert!(matches!(error, UnearthError::Remote(_)), "{error}");
    }

    #[test]
    fn a_state_file_that_cannot_be_read_fails_before_any_pull() {
        let scenario = Scenario::new(true);
        scenario.newer_on_remote();
        fs::create_dir(scenario.fx.collection.remote_state_path()).expect("a directory");

        let error = scenario.refresh(true).err().expect("fails");

        assert!(matches!(error, UnearthError::RemoteState(_)), "{error}");
        let store = Store::new(&scenario.fx.collection);
        assert_eq!(store.committed_versions(id(ID)).expect("lists"), [1]);
    }

    #[test]
    fn a_plan_that_needs_the_remote_fails_by_how_the_remote_stands() {
        let scenario = Scenario::new(true);
        let planned = {
            fs::remove_file(
                Store::new(&scenario.fx.collection).file_path(id(ID), &scenario.payload),
            )
            .ok();
            needs_remote(&scenario)
        };
        let away = refuse_away(&scenario);
        let broken = refresh_with(
            Err(RemoteError::Unsupported("x".into())),
            &scenario.fx.collection,
            None,
            id(ID),
            scenario.fx.clock.now(),
        )
        .expect("a standing");
        let absent = refresh_with(
            Err(RemoteError::NoRemote),
            &scenario.fx.collection,
            None,
            id(ID),
            scenario.fx.clock.now(),
        )
        .expect("a standing");

        let kinds: Vec<ErrorKind> = [&away, &broken, &absent]
            .iter()
            .map(|refreshed| {
                refreshed
                    .check_usable(&planned)
                    .expect_err("refused")
                    .kind()
            })
            .collect();

        assert_eq!(
            kinds,
            [
                ErrorKind::RemoteAway,
                ErrorKind::RemoteBroken,
                ErrorKind::NoRemote
            ]
        );
    }

    #[test]
    fn a_plan_from_a_local_archive_goes_ahead_whatever_the_remote_does() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let planned = fx.plan(&fx.request(ID)).expect("planned");
        let refreshed = refresh_with(
            Err(RemoteError::NoRemote),
            &fx.collection,
            None,
            id(ID),
            fx.clock.now(),
        )
        .expect("a standing");

        refreshed.check_usable(&planned).expect("usable");
    }

    #[test]
    fn a_unearthed_piece_needs_no_remote() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.unearthed(ID, "projects/a");
        let planned = fx.plan(&fx.request(ID)).expect("planned");
        let refreshed = refresh_with(
            Err(RemoteError::NoRemote),
            &fx.collection,
            None,
            id(ID),
            fx.clock.now(),
        )
        .expect("a standing");

        refreshed.check_usable(&planned).expect("usable");
    }

    #[test]
    fn the_remote_is_finished_once_whichever_way_the_run_ends() {
        let scenario = Scenario::new(true);
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        let remote = FaultRemote::new(path_remote(scenario.root.path(), "imperius"));
        let flag = Arc::clone(&first);
        remote.before(Op::Finish, 1, move |_| flag.store(true, Ordering::SeqCst));
        let flag = Arc::clone(&second);
        remote.before(Op::Finish, 2, move |_| flag.store(true, Ordering::SeqCst));
        let refreshed = refresh_with(
            Ok(Box::new(remote)),
            &scenario.fx.collection,
            None,
            id(ID),
            scenario.fx.clock.now(),
        )
        .expect("refreshed");
        assert!(!first.load(Ordering::SeqCst), "not before the run is over");

        drop(refreshed);

        assert!(first.load(Ordering::SeqCst));
        assert!(!second.load(Ordering::SeqCst));
    }

    #[test]
    fn a_dry_run_counts_the_listing_of_the_remote_in_the_room() {
        let scenario = Scenario::new(false);
        let dry = scenario.refresh(false).expect("refreshed");
        let real = scenario.refresh(true).expect("refreshed");
        let needed = |refreshed: &Refreshed| match refreshed
            .plan(&scenario.fx.collection, &scenario.fx.request(ID))
            .expect("planned")
        {
            Planned::Unearth(plan) => plan.needed,
            other => panic!("{other:?}"),
        };

        assert_eq!(needed(&dry), needed(&real) + 17);
    }

    fn away_remote_error() -> UnearthError {
        UnearthError::Remote(RemoteError::Away {
            message: "no route".into(),
            overdue: false,
        })
    }

    fn overdue_flag(result: Result<(), Error>) -> bool {
        match result.expect_err("an error") {
            Error::Remote(RemoteError::Away { overdue, .. }) => overdue,
            other => panic!("{other}"),
        }
    }

    #[test]
    fn an_away_error_is_overdue_after_eight_days_with_pending_work() {
        let scenario = Scenario::new(true);
        scenario.record_pending_change();
        fs::write(
            scenario.fx.collection.remote_state_path(),
            r#"{"last_reachable": "2026-10-27T12:00:00Z"}"#,
        )
        .expect("written");

        let result = overdue(
            &scenario.fx.collection,
            Err(away_remote_error()),
            &scenario.fx.clock,
        );

        assert!(overdue_flag(result));
    }

    #[test]
    fn an_away_error_is_not_overdue_after_one_day() {
        let scenario = Scenario::new(true);
        fs::write(
            scenario.fx.collection.remote_state_path(),
            r#"{"last_reachable": "2026-11-03T12:00:00Z"}"#,
        )
        .expect("written");

        let result = overdue(
            &scenario.fx.collection,
            Err(away_remote_error()),
            &scenario.fx.clock,
        );

        assert!(!overdue_flag(result));
    }

    #[test]
    fn an_away_error_is_not_overdue_without_pending_work() {
        let scenario = Scenario::new(true);
        fs::write(
            scenario.fx.collection.remote_state_path(),
            r#"{"last_reachable": "2026-10-27T12:00:00Z"}"#,
        )
        .expect("written");

        let result = overdue(
            &scenario.fx.collection,
            Err(away_remote_error()),
            &scenario.fx.clock,
        );

        assert!(!overdue_flag(result));
    }

    #[test]
    fn a_status_that_fails_counts_as_pending_work() {
        let scenario = Scenario::new(true);
        fs::write(
            scenario.fx.collection.remote_state_path(),
            r#"{"last_reachable": "2026-10-27T12:00:00Z"}"#,
        )
        .expect("written");
        let object_dir = scenario.fx.collection.object_dir(id(ID));
        fs::write(object_dir.join("meta.2"), b"not json").expect("written");
        let broken = scenario.fx.collection.objects_dir();
        let _restore = RestoreModes(vec![broken.clone()]);
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(&broken).is_err(),
            "the directory must refuse listing; tests do not run as root"
        );

        let result = overdue(
            &scenario.fx.collection,
            Err(away_remote_error()),
            &scenario.fx.clock,
        );

        assert!(overdue_flag(result));
    }

    #[test]
    fn other_errors_pass_through_overdue_with_their_kind() {
        let scenario = Scenario::new(true);

        let error = overdue::<()>(
            &scenario.fx.collection,
            Err(UnearthError::NoArchive {
                path: path("projects/a"),
            }),
            &scenario.fx.clock,
        )
        .expect_err("an error");

        assert!(
            matches!(error, Error::Unearth(UnearthError::NoArchive { .. })),
            "{error}"
        );
    }

    impl Scenario {
        fn version_on_remote(&self, number: u32, bytes: &[u8]) {
            let there = self.root.path().join("objects").join(ID);
            fs::write(there.join(format!("meta.{number}")), bytes).expect("written");
        }
    }

    fn dry_run_refusal(scenario: &Scenario) -> UnearthError {
        scenario
            .refresh(false)
            .expect("refreshed")
            .plan(&scenario.fx.collection, &scenario.fx.request(ID))
            .expect_err("refused")
    }

    #[test]
    fn a_newer_version_that_cannot_be_read_refuses_the_dry_run() {
        let scenario = Scenario::new(true);
        scenario.version_on_remote(2, b"not json");

        let error = dry_run_refusal(&scenario);

        assert!(
            matches!(&error, UnearthError::UnreadableVersions { versions, .. } if versions.starts_with("2 (")),
            "{error}"
        );
    }

    #[test]
    fn an_unreadable_version_above_a_readable_one_refuses_the_dry_run() {
        let scenario = Scenario::new(true);
        scenario.newer_on_remote();
        scenario.version_on_remote(3, b"not json");

        let error = dry_run_refusal(&scenario);

        assert!(
            matches!(&error, UnearthError::UnreadableVersions { versions, .. } if versions.starts_with("3 (")),
            "{error}"
        );
    }

    #[test]
    fn a_newer_version_of_another_object_is_unreadable_for_this_one() {
        let scenario = Scenario::new(true);
        let there = scenario.root.path().join("objects").join(ID);
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(there.join("meta.1")).expect("reads")).expect("json");
        document["id"] = serde_json::json!("01D39ZY06FGSCTVN4T2V9PKHFZ");
        scenario.version_on_remote(2, &serde_json::to_vec(&document).expect("json"));

        let error = dry_run_refusal(&scenario);

        assert!(
            matches!(&error, UnearthError::UnreadableVersions { versions, .. } if versions.contains("it belongs to 01D39ZY06FGSCTVN4T2V9PKHFZ")),
            "{error}"
        );
    }

    #[test]
    fn a_reachability_check_that_fails_otherwise_is_an_error_and_the_remote_is_finished() {
        let scenario = Scenario::new(true);
        let finished = Arc::new(AtomicBool::new(false));
        let remote = FaultRemote::new(path_remote(scenario.root.path(), "imperius"));
        remote.fail(Op::Reachability, 1, Fault::Io);
        let flag = Arc::clone(&finished);
        remote.before(Op::Finish, 1, move |_| flag.store(true, Ordering::SeqCst));

        let error = refresh_with(
            Ok(Box::new(remote)),
            &scenario.fx.collection,
            None,
            id(ID),
            scenario.fx.clock.now(),
        )
        .err()
        .expect("fails");

        assert!(matches!(error, UnearthError::Remote(_)), "{error}");
        assert!(finished.load(Ordering::SeqCst));
    }

    #[test]
    fn nothing_is_pulled_from_a_remote_that_is_away() {
        let scenario = Scenario::new(true);
        scenario.newer_on_remote();

        refuse_away(&scenario);

        let store = Store::new(&scenario.fx.collection);
        assert_eq!(store.committed_versions(id(ID)).expect("lists"), [1]);
    }

    #[test]
    fn the_unreadable_versions_of_the_store_are_listed_after_the_pulled_ones() {
        let scenario = Scenario::new(true);
        let here = scenario.fx.collection.object_dir(id(ID));
        fs::write(here.join("meta.2"), b"not json").expect("written");
        scenario.version_on_remote(3, b"not json");

        let error = dry_run_refusal(&scenario);

        assert!(
            matches!(&error, UnearthError::UnreadableVersions { versions, .. } if versions.starts_with("3 (") && versions.contains(", 2 (")),
            "{error}"
        );
    }

    #[test]
    fn a_fork_needs_the_remote_as_much_as_a_fetch() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.add_live(ID, 2);
        fx.make_foreign(ID);
        let planned = fx.plan(&fx.request(ID)).expect("planned");
        assert!(
            matches!(&planned, Planned::Unearth(plan) if plan.source == UnearthSource::Fork),
            "{planned:?}"
        );
        let refreshed = refresh_with(
            Err(RemoteError::NoRemote),
            &fx.collection,
            None,
            id(ID),
            fx.clock.now(),
        )
        .expect("a standing");

        let error = refreshed.check_usable(&planned).expect_err("refused");

        assert_eq!(error.kind(), ErrorKind::NoRemote);
    }

    #[test]
    fn a_restore_from_a_live_package_needs_the_remote_as_much_as_a_fetch() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.edit_meta(ID, |document| {
            document["lifecycle"] = serde_json::json!("live")
        });
        fx.live_only(ID);
        let planned = fx.plan(&fx.request(ID)).expect("planned");
        assert!(
            matches!(&planned, Planned::Unearth(plan) if plan.source == UnearthSource::LivePackage),
            "{planned:?}"
        );
        let refreshed = refresh_with(
            Err(RemoteError::NoRemote),
            &fx.collection,
            None,
            id(ID),
            fx.clock.now(),
        )
        .expect("a standing");

        let error = refreshed.check_usable(&planned).expect_err("refused");

        assert_eq!(error.kind(), ErrorKind::NoRemote);
    }
}
