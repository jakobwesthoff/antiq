// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The version fixture of `support::versions`, seen from a binary test: the
//! versions it writes are read back with the library, and its damage
//! helpers change the file they name and no other.

mod support;

use antiq::archive::check::{ChecksumFile, file_sha256};
use antiq::listing;
use antiq::relpath::RelPath;
use antiq::versions::names::{VersionFile, VersionFileNames};
use antiq::versions::stamp::Stamp;
use antiq::versions::store::is_being_stored;
use std::ffi::OsString;
use std::fs;
use support::scenario::collection::{Clock, Collection};
use support::tree::Tree;
use support::versions::{Store, StoredVersion};

const STAMP: &str = "2026-11-04T090211";

fn source() -> Tree {
    let mut tree = Tree::new();
    tree.file("notes/a.txt", "alpha");
    tree.file("notes/sub/b.txt", "beta");
    tree
}

fn stored() -> (Store, StoredVersion) {
    let mut store = Store::new();
    let version = store.add_version(&source(), "notes", STAMP);
    (store, version)
}

fn bytes(version: &StoredVersion, kind: VersionFile) -> Vec<u8> {
    fs::read(version.path(kind)).expect("the file exists")
}

fn all_bytes(version: &StoredVersion) -> Vec<Vec<u8>> {
    VersionFile::ALL
        .into_iter()
        .map(|kind| fs::read(version.path(kind)).unwrap_or_default())
        .collect()
}

#[test]
fn a_version_has_three_files_named_by_the_store_rules() {
    let (store, version) = stored();
    let scan = store.scan();
    assert_eq!(scan.pieces.len(), 1);
    let piece = &scan.pieces[0];
    assert_eq!(piece.path.to_string(), "notes");
    assert_eq!(piece.versions.len(), 1);
    let found = &piece.versions[0];
    let stamp: Stamp = STAMP.parse().expect("a stamp");
    let names = VersionFileNames::new(&RelPath::parse("notes").expect("a piece"), &stamp);
    assert_eq!(found.stamp, stamp);
    assert_eq!(found.archives, [OsString::from(&names.archive)]);
    assert_eq!(found.listings, [OsString::from(&names.listing)]);
    assert_eq!(found.checksums, [OsString::from(&names.checksum)]);
    assert_eq!(version.names, names);
    assert_eq!(version.stamp, stamp);
}

#[test]
fn the_listing_reads_in_full_and_matches_the_archive() {
    let (_store, version) = stored();
    let file = fs::File::open(version.path(VersionFile::Listing)).expect("the listing exists");
    let read = listing::read_full(file).expect("the listing reads in full");
    let archive_size = fs::metadata(version.path(VersionFile::Archive))
        .expect("the archive exists")
        .len();
    assert_eq!(read.header.archive_bytes, archive_size);
    assert_eq!(read.header.archive, version.names.archive);
    assert_eq!(read.header.piece.to_string(), "notes");
    assert_eq!(read.files, 2);

    let checksum = ChecksumFile::parse(&bytes(&version, VersionFile::Checksum))
        .expect("the checksum file parses");
    assert_eq!(checksum.archive(), version.names.archive);
    let archive_hash = file_sha256(&version.path(VersionFile::Archive)).expect("the archive reads");
    assert_eq!(checksum.sha256(), archive_hash);
}

#[test]
fn removing_truncating_and_overwriting_change_exactly_their_file() {
    let (_store, version) = stored();
    let before = all_bytes(&version);

    version.truncate(VersionFile::Archive, 5);
    let after = all_bytes(&version);
    assert_eq!(after[0], before[0][..5]);
    assert_eq!(&after[1..], &before[1..]);

    version.overwrite(VersionFile::Listing, b"not a listing");
    let after = all_bytes(&version);
    assert_eq!(after[1], b"not a listing");
    assert_eq!(after[2], before[2]);

    version.remove(VersionFile::Checksum);
    assert!(!version.path(VersionFile::Checksum).exists());
    assert_eq!(all_bytes(&version)[1], b"not a listing");
}

#[test]
fn a_checksum_file_with_two_lines_is_refused_by_the_parser_and_nothing_else_changes() {
    let (_store, version) = stored();
    let before = all_bytes(&version);
    version.checksum_with_two_lines();
    let after = all_bytes(&version);
    assert_eq!((&after[0], &after[1]), (&before[0], &before[1]));
    assert!(
        after[2].starts_with(&before[2]) && after[2].len() > before[2].len(),
        "the original line is kept and another follows"
    );
    ChecksumFile::parse(&after[2]).expect_err("two lines are not a checksum file");
}

#[test]
fn a_twin_doubles_one_slot_and_leaves_the_files_alone() {
    let (store, version) = stored();
    let before = all_bytes(&version);
    let twin = version.add_twin(VersionFile::Listing, "other-name");
    assert_eq!(all_bytes(&version), before);
    assert!(twin.is_file());
    let scan = store.scan();
    let found = &scan.pieces[0].versions[0];
    assert!(found.is_doubled());
    assert_eq!(found.listings.len(), 2);
    assert_eq!(found.archives.len(), 1);
    assert_eq!(found.checksums.len(), 1);
}

#[test]
fn a_staged_archive_makes_the_version_being_stored() {
    let (store, version) = stored();
    let piece = RelPath::parse("notes").expect("a piece");
    let staged =
        || is_being_stored(&store.tmp_pack_dir(), &piece, &version.stamp).expect("the check runs");
    assert!(!staged());
    let copy = version.stage_archive(&store);
    assert_eq!(
        fs::read(copy).expect("the staged copy exists"),
        bytes(&version, VersionFile::Archive)
    );
    assert!(staged());
}

#[test]
fn the_file_names_equal_the_ones_the_scenario_harness_writes() {
    let (_store, version) = stored();
    let collection = Collection::build(&Clock::now())
        .version("notes", STAMP, &[("a.txt", "alpha")])
        .finish();
    let by_hand = collection.version_files("notes", STAMP);
    for (kind, path) in [
        (VersionFile::Archive, by_hand.archive),
        (VersionFile::Listing, by_hand.listing),
        (VersionFile::Checksum, by_hand.checksum),
    ] {
        assert_eq!(
            path.file_name().expect("a file name").to_string_lossy(),
            version.names.of(kind)
        );
    }
}
