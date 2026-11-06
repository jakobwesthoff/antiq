// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: a stored version can be checked with `shasum`, `tar` and the
//! listing alone, without antiq.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::outsider::{
    listing_check, shasum_check, store_files, tar_extract, tar_members,
};
use crate::support::scenario::world::{Setup, both_modes};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// The paths below `dir`, directories with a trailing slash, as a listing
/// writes them.
fn tree_paths(dir: &Path) -> BTreeSet<String> {
    fn walk(base: &Path, dir: &Path, found: &mut BTreeSet<String>) {
        for item in fs::read_dir(dir).expect("the directory lists") {
            let path = item.expect("an entry").path();
            let mut name = path
                .strip_prefix(base)
                .expect("below the base")
                .display()
                .to_string();
            if fs::symlink_metadata(&path).expect("metadata").is_dir() {
                name.push('/');
                found.insert(name);
                walk(base, &path, found);
            } else {
                found.insert(name);
            }
        }
    }
    let mut found = BTreeSet::new();
    walk(dir, dir, &mut found);
    found
}

/// The paths of the entry lines of a decoded listing: the part of a line
/// after its fixed fields, up to a tab that starts a link target.
fn listing_paths(listing: &str) -> BTreeSet<String> {
    listing
        .lines()
        .filter(|line| {
            ["# dir ", "# file ", "# link ", "# hard "]
                .iter()
                .any(|kind| line.starts_with(kind))
        })
        .map(|line| {
            let path = line.get(54..).expect("an entry line has a path at byte 54");
            path.split('\t').next().expect("a path").to_string()
        })
        .collect()
}

#[test]
fn a_stored_version_checks_out_with_shasum_tar_and_its_own_listing() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("projects/scanner", |piece| {
                    piece
                        .age_days(40)
                        .file("src/main.rs", "fn main() {}\n")
                        .hard_link("src/main.rs", "src/main-copy.rs")
                        .symlink("src/latest", "main.rs")
                        .file("docs/guide/intro.md", "# Intro\n");
                })
                .finish();
            Setup::new(collection)
        },
        |world| {
            let collection = world.collection();
            world.run(&["archive"]).expect_code(0);

            let versions = store_files(collection, "projects/scanner");
            assert_eq!(versions.len(), 1);
            let archive = versions[0].archive.as_ref().expect("the archive is stored");
            let checksum = versions[0]
                .checksum
                .as_ref()
                .expect("the checksum file is stored");
            let listing = versions[0].listing.as_ref().expect("the listing is stored");

            // The checksum file vouches for the archive, `tar` unpacks it
            // and the listing's checksum lines hold for the unpacked files.
            shasum_check(checksum.parent().expect("a store folder"), checksum);
            let scratch = tempfile::tempdir().expect("a scratch directory");
            let unpacked = scratch.path().join("unpacked");
            tar_extract(archive, &unpacked);
            let top = unpacked.join("scanner");
            listing_check(listing, &top, &scratch.path().join("listing"));

            // The listing names the same entries as the unpacked tree and
            // the archive's members below the top-level directory.
            let decoded = zstd::decode_all(fs::File::open(listing).expect("the listing opens"))
                .expect("the listing is zstd");
            let decoded = String::from_utf8(decoded).expect("the listing is text");
            let listed = listing_paths(&decoded);
            assert_eq!(listed, tree_paths(&top));
            let members: BTreeSet<String> = tar_members(archive)
                .into_iter()
                .filter_map(|member| member.strip_prefix("scanner/").map(str::to_string))
                .filter(|member| !member.is_empty())
                .collect();
            assert_eq!(listed, members);
        },
    );
}
