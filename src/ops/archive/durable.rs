// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proof that an archive is on disk before its folder is touched.
//!
//! Only [`seal`] makes a [`DurableArchive`], and it needs a verified
//! package, a payload and a tracking entry that says `removing` for that
//! payload. The functions that move or delete a folder take the token, so
//! the order of spec "States" cannot be skipped by a caller.

use crate::id::ObjectId;
use crate::listing::ListingEntry;
use crate::lock::RunLock;
use crate::meta::{FileName, Payload};
use crate::pack::verify::LstatRecord;
use crate::pack::{Packed, Verified};
use crate::relpath::RelPath;
use crate::tracking::{EntryState, TrackingEntry};

/// An archive whose payload and listing are stored, whose metadata is
/// recorded and whose tracking entry says `removing` for the payload.
///
/// ```compile_fail,E0451
/// // The fields are private: a caller that could fill them in could claim
/// // an archive that was never stored and go on to delete the folder.
/// use antiq::ops::archive::durable::DurableArchive;
/// use antiq::relpath::RelPath;
///
/// fn forge(id: antiq::id::ObjectId, file: antiq::meta::FileName) -> DurableArchive {
///     DurableArchive {
///         id,
///         path: RelPath::root(),
///         payload_file: file,
///         payload_version: 1,
///         record: Vec::new(),
///     }
/// }
/// ```
#[derive(Debug)]
pub struct DurableArchive {
    id: ObjectId,
    path: RelPath,
    payload_file: FileName,
    payload_version: u32,
    record: Vec<LstatRecord>,
}

impl DurableArchive {
    pub fn id(&self) -> ObjectId {
        self.id
    }

    /// The path of the folder the archive was made from.
    pub fn path(&self) -> &RelPath {
        &self.path
    }

    pub fn payload_file(&self) -> &FileName {
        &self.payload_file
    }

    pub fn payload_version(&self) -> u32 {
        self.payload_version
    }

    /// What the folder looked like when the package was verified against
    /// it: the baseline a later walk compares with.
    pub fn record(&self) -> &[LstatRecord] {
        &self.record
    }
}

/// A package verified against the folder, whose listing is the one that
/// is stored.
pub(super) struct Matched(Verified);

#[derive(Debug, thiserror::Error)]
#[error("the listing of the verified package differs from {against}")]
pub(super) struct ListingMismatch {
    against: &'static str,
}

impl Matched {
    /// The verify must find what the packing wrote.
    pub(super) fn of_packing(
        verified: Verified,
        packed: &Packed,
    ) -> Result<Matched, ListingMismatch> {
        if verified.listing() == packed.listing.as_slice() {
            Ok(Matched(verified))
        } else {
            Err(ListingMismatch {
                against: "the one made while packing",
            })
        }
    }

    /// The verify must find what an earlier run stored.
    pub(super) fn of_stored(
        verified: Verified,
        stored: &[ListingEntry],
    ) -> Result<Matched, ListingMismatch> {
        if verified.listing() == stored {
            Ok(Matched(verified))
        } else {
            Err(ListingMismatch {
                against: "the stored one",
            })
        }
    }

    pub(super) fn listing(&self) -> &[ListingEntry] {
        self.0.listing()
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum NotDurable {
    #[error("the tracking entry of {0} does not say removing")]
    NotRemoving(ObjectId),
    #[error("the tracking entry of {0} does not name the payload")]
    OtherPayload(ObjectId),
}

/// Turns the verified package, its payload and the `removing` entry into
/// the token. The run lock is the proof that nothing else changes the
/// collection meanwhile.
pub(super) fn seal(
    _lock: &RunLock,
    matched: Matched,
    payload: &Payload,
    entry: &TrackingEntry,
) -> Result<DurableArchive, NotDurable> {
    if entry.state != EntryState::Removing {
        return Err(NotDurable::NotRemoving(entry.id));
    }
    if entry.removing_payload.as_ref() != Some(&payload.file) {
        return Err(NotDurable::OtherPayload(entry.id));
    }
    Ok(DurableArchive {
        id: entry.id,
        path: entry.path.clone(),
        payload_file: payload.file.clone(),
        payload_version: payload.version,
        record: matched.0.record().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::fsx::tree::TreeRoot;
    use crate::meta::{Checksum, PayloadKind};
    use crate::pack::{Packager, TarZstd, WalkRules};
    use crate::testing::package::{on_disk, pack_piece};
    use crate::testing::tree::Tree;
    use crate::tracking::VersionTakenOut;
    use jiff::Timestamp;

    struct Sealed {
        _tree: Tree,
        _package: tempfile::TempDir,
        packed: Packed,
        verified: Option<Verified>,
        lock: RunLock,
        _lock_dir: tempfile::TempDir,
    }

    fn sealed() -> Sealed {
        let mut tree = Tree::new();
        tree.file("proj/a.txt", "a");
        let packaged = pack_piece(&tree, "proj");
        let (package, file) = on_disk(&packaged.bytes);
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let verified = TarZstd
            .verify_strict(&file, &root, &WalkRules::default())
            .expect("the package equals the folder");
        let lock_dir = tempfile::tempdir().expect("a directory");
        let lock = crate::lock::acquire(
            &lock_dir.path().join("lock"),
            "laptop",
            &FixedClock::new("2026-11-01T12:00:00Z".parse().expect("a time")),
        )
        .expect("the lock is free");
        Sealed {
            _tree: tree,
            _package: package,
            packed: packaged.packed,
            verified: Some(verified),
            lock,
            _lock_dir: lock_dir,
        }
    }

    fn payload() -> Payload {
        Payload {
            kind: PayloadKind::Archive,
            version: 3,
            file: FileName::new("0123456789abcdef-proj.tar.zst").expect("valid"),
            package_format: "tar+zstd/1".into(),
            size: 5,
            checksum: Checksum::sha256("00"),
            listing: None,
            machine: "laptop".into(),
            created: "2026-11-01T12:00:00Z".parse().expect("a time"),
            verified: None,
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        }
    }

    fn entry(state: EntryState, removing: Option<FileName>) -> TrackingEntry {
        let mut entry = TrackingEntry::new(
            ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_000, 1)),
            RelPath::parse("proj").expect("valid"),
            state,
            VersionTakenOut::Pending,
            "2026-11-01T12:00:00Z".parse::<Timestamp>().expect("a time"),
        );
        entry.removing_payload = removing;
        entry
    }

    #[test]
    fn the_token_carries_the_id_the_payload_and_the_record_of_the_verify() {
        let mut world = sealed();
        let verified = world.verified.take().expect("verified");
        let record = verified.record().to_vec();
        let matched = Matched::of_packing(verified, &world.packed).expect("the listings agree");
        let payload = payload();
        let token = seal(
            &world.lock,
            matched,
            &payload,
            &entry(EntryState::Removing, Some(payload.file.clone())),
        )
        .expect("a removing entry for the payload");
        assert_eq!(token.id(), entry(EntryState::Removing, None).id);
        assert_eq!(token.path().to_string(), "proj");
        assert_eq!(token.payload_file(), &payload.file);
        assert_eq!(token.payload_version(), 3);
        assert_eq!(token.record(), record.as_slice());
    }

    #[test]
    fn an_entry_that_is_not_removing_does_not_seal() {
        let mut world = sealed();
        let matched = Matched::of_packing(world.verified.take().expect("verified"), &world.packed)
            .expect("the listings agree");
        let payload = payload();
        let result = seal(
            &world.lock,
            matched,
            &payload,
            &entry(EntryState::Live, Some(payload.file.clone())),
        );
        assert!(
            matches!(result, Err(NotDurable::NotRemoving(_))),
            "{result:?}"
        );
    }

    #[test]
    fn an_entry_naming_another_payload_does_not_seal() {
        let mut world = sealed();
        let matched = Matched::of_packing(world.verified.take().expect("verified"), &world.packed)
            .expect("the listings agree");
        let other = FileName::new("fedcba9876543210-proj.tar.zst").expect("valid");
        let result = seal(
            &world.lock,
            matched,
            &payload(),
            &entry(EntryState::Removing, Some(other)),
        );
        assert!(
            matches!(result, Err(NotDurable::OtherPayload(_))),
            "{result:?}"
        );
    }

    #[test]
    fn an_entry_naming_no_payload_does_not_seal() {
        let mut world = sealed();
        let matched = Matched::of_packing(world.verified.take().expect("verified"), &world.packed)
            .expect("the listings agree");
        let result = seal(
            &world.lock,
            matched,
            &payload(),
            &entry(EntryState::Removing, None),
        );
        assert!(
            matches!(result, Err(NotDurable::OtherPayload(_))),
            "{result:?}"
        );
    }

    #[test]
    fn a_listing_other_than_the_packed_one_does_not_match() {
        let mut world = sealed();
        let mut other = world.packed.listing.clone();
        other.pop();
        let packed = Packed {
            listing: other,
            ..world.packed
        };
        let result = Matched::of_packing(world.verified.take().expect("verified"), &packed);
        assert!(result.is_err());
    }

    #[test]
    fn a_listing_other_than_the_stored_one_does_not_match() {
        let mut world = sealed();
        let result = Matched::of_stored(world.verified.take().expect("verified"), &[]);
        assert!(result.is_err());
    }

    #[test]
    fn the_listing_of_a_match_is_the_verified_one() {
        let mut world = sealed();
        let verified = world.verified.take().expect("verified");
        let expected = verified.listing().to_vec();
        let matched = Matched::of_packing(verified, &world.packed).expect("the listings agree");
        assert_eq!(matched.listing(), expected.as_slice());
    }
}
