// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Objects and tracking entries written straight into a collection, for
//! tests of commands that read them.

use antiq::id::ObjectId;
use antiq::listing::{self, ListingEntry, ListingType};
use antiq::meta::{
    Checksum, CreatedSource, FileName, Lifecycle, Metadata, Payload, PayloadKind, to_bytes,
};
use antiq::relpath::{Name, RelPath};
use antiq::tracking::{EntryState, TrackingEntry, VersionTakenOut};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// A fixed ID per number, so that a test can name an object.
pub fn id(number: u8) -> ObjectId {
    ObjectId::from_ulid(ulid::Ulid::from_parts(
        1_600_000_000_000 + u64::from(number),
        u128::from(number),
    ))
}

pub fn payload(kind: PayloadKind, version: u32, size: u64) -> Payload {
    Payload {
        kind,
        version,
        file: FileName::new(format!("{version:016x}-payload.tar.zst")).expect("a payload name"),
        package_format: "tar+zstd/1".into(),
        size,
        checksum: Checksum::sha256("00"),
        listing: None,
        machine: "laptop".into(),
        created: "2026-01-01T00:00:00Z".parse().expect("a time"),
        verified: None,
        superseded: false,
        encoder: None,
        extra: serde_json::Map::new(),
    }
}

/// An archive payload of object `number` whose listing, stored next to
/// the metadata, has a file at each of `members`.
pub fn archive_with_members(
    root: &Path,
    number: u8,
    version: u32,
    size: u64,
    members: &[&str],
) -> Payload {
    let sha256 = format!("{:064x}", u128::from(number) * 1_000 + u128::from(version));
    let listing = listing::file_name(&sha256).expect("a digest of 64 hex digits");
    let entries: Vec<ListingEntry> = members
        .iter()
        .map(|member| ListingEntry {
            path: RelPath::parse(member).expect("a valid member path"),
            kind: ListingType::File,
            size: 1,
            mtime: 0,
            mode: 0o644,
            link: None,
            sha256: Some("0".repeat(64)),
        })
        .collect();
    let directory = root.join(".antiq/objects").join(id(number).to_string());
    fs::create_dir_all(&directory).expect("a directory");
    let file = fs::File::create(directory.join(listing.as_str())).expect("the listing file");
    listing::write(&entries, file).expect("the listing is written");
    let mut payload = payload(PayloadKind::Archive, version, size);
    payload.checksum = Checksum::sha256(&sha256);
    payload.listing = Some(listing);
    payload
}

/// The metadata of object `number` at `at`, owned by the collection
/// `owner`.
pub fn metadata(
    number: u8,
    at: &str,
    lifecycle: Lifecycle,
    owner: &str,
    payloads: Vec<Payload>,
) -> Metadata {
    let path = RelPath::parse(at).expect("a valid path");
    let name =
        Name::from_bytes(path.components().last().expect("a name").to_vec()).expect("a valid name");
    Metadata {
        format: 1,
        id: id(number),
        collection: owner.parse().expect("an ID"),
        path,
        name,
        created: "2026-01-01T00:00:00Z".parse().expect("a time"),
        created_source: CreatedSource::Birth,
        lifecycle,
        payloads,
        history: Vec::new(),
        extra: serde_json::Map::new(),
    }
}

/// Writes `meta.1` of `known` below the collection at `root`.
pub fn put_meta(root: &Path, known: &Metadata) {
    let directory = root.join(".antiq/objects").join(known.id.to_string());
    fs::create_dir_all(&directory).expect("a directory");
    fs::write(
        directory.join("meta.1"),
        to_bytes(known).expect("metadata serialises"),
    )
    .expect("a file");
}

pub fn entry(number: u8, at: &str, state: EntryState) -> TrackingEntry {
    TrackingEntry::new(
        id(number),
        RelPath::parse(at).expect("a valid path"),
        state,
        VersionTakenOut::Committed(1),
        "2026-01-01T00:00:00Z".parse().expect("a time"),
    )
}

/// Holds the run lock of the collection at `root` in this process.
pub fn hold_lock(root: &Path) -> antiq::lock::RunLock {
    antiq::lock::acquire(
        &root.join(".antiq/lock"),
        "imperius",
        &antiq::clock::SystemClock,
    )
    .expect("the lock is free")
}

/// Every path below `root` with its content, directories as `None`, to
/// compare before and after a command.
pub fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(base: &Path, dir: &Path, seen: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for item in fs::read_dir(dir).expect("lists") {
            let path = item.expect("an item").path();
            let name = path
                .strip_prefix(base)
                .expect("below")
                .display()
                .to_string();
            if path.is_dir() {
                seen.insert(name, None);
                walk(base, &path, seen);
            } else {
                seen.insert(name, Some(fs::read(&path).expect("reads")));
            }
        }
    }
    let mut seen = BTreeMap::new();
    walk(root, root, &mut seen);
    seen
}

/// Writes the tracking entry of `entry` below the collection at `root`.
pub fn put_entry(root: &Path, entry: &TrackingEntry) {
    fs::write(
        root.join(".antiq/tracking")
            .join(format!("{}.json", entry.id)),
        serde_json::to_vec(entry).expect("an entry serialises"),
    )
    .expect("a file");
}
