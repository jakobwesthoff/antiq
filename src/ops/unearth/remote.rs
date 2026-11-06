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
use super::{UnearthError, UnearthPlan, io_error};
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::index;
use crate::listing::{self, ListingEntry, ListingType};
use crate::lock::RunLock;
use crate::meta::FileName;
use crate::pack::hashing::HashingReader;
use crate::remote::{Remote, RemoteError};
use crate::store::{Put, Store};
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
    listing: Vec<ListingEntry>,
    origin: ListingOrigin,
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
            listing: held_listing(collection, plan)?,
            origin: ListingOrigin::Store,
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
        let by_path = |entries: &[ListingEntry]| {
            let mut sorted = entries.to_vec();
            sorted.sort_by(|a, b| a.path.cmp(&b.path));
            sorted
        };
        if by_path(&self.listing) == by_path(members) {
            return Ok(());
        }
        let whose = match self.origin {
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

    let extracted: u64 = entries
        .iter()
        .filter(|member| member.kind == ListingType::File)
        .map(|member| member.size)
        .sum();
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
        listing: entries,
        origin,
        fetched,
        _parts: parts,
    }))
}

#[cfg(test)]
mod tests {
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
    struct Full<'a> {
        inner: &'a FaultRemote,
        file: String,
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
}
