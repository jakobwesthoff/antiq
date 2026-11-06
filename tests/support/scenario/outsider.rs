// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checks of the store that use no antiq code.

use super::collection::Collection;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The files of one version found in a store folder; a file that is not
/// there is `None`.
#[derive(Debug, PartialEq, Eq)]
pub struct StoredVersion {
    pub stamp: String,
    pub archive: Option<PathBuf>,
    pub checksum: Option<PathBuf>,
    pub listing: Option<PathBuf>,
}

/// Runs `shasum -a 256 -c <file>` in `dir`, which is what the spec gives a
/// person to check a stored version by hand, and fails when it does not
/// exit 0.
pub fn shasum_check(dir: &Path, file: &Path) {
    shasum_check_with("shasum", dir, file);
}

/// `shasum_check` with another program, to show the message of a missing
/// tool.
pub fn shasum_check_with(program: &str, dir: &Path, file: &Path) {
    // A tool that is not installed must fail the test with its name, not
    // pass as a check that found nothing wrong.
    Command::new(program)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {program}, which the check needs: {error}"));
    let output = Command::new(program)
        .args(["-a", "256", "-c"])
        .arg(file)
        .current_dir(dir)
        .output()
        .expect("the program ran a moment ago");
    assert!(
        output.status.success(),
        "{program} -c failed in {}:\n{}{}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The member paths of a `.tar.zst`, sorted, read with the `zstd` and `tar`
/// crates.
pub fn tar_members(archive: &Path) -> Vec<String> {
    let file = fs::File::open(archive).expect("the archive opens");
    let decoded = zstd::stream::Decoder::new(file).expect("the archive is zstd");
    let mut members: Vec<String> = tar::Archive::new(decoded)
        .entries()
        .expect("the archive lists")
        .map(|entry| {
            entry
                .expect("an entry")
                .path()
                .expect("a path")
                .display()
                .to_string()
        })
        .collect();
    members.sort();
    members
}

/// Unpacks `archive` into the new directory `dest` with `tar --zstd -xpf`,
/// the command the spec gives for the hand check. `-p` keeps the stored
/// modes whatever the umask of the test run.
pub fn tar_extract(archive: &Path, dest: &Path) {
    tar_extract_with("tar", archive, dest);
}

/// `tar_extract` with another program, to show the message of a missing
/// tool.
pub fn tar_extract_with(program: &str, archive: &Path, dest: &Path) {
    // A tool that is not installed must fail the test with its name, not
    // pass as a check that found nothing wrong.
    Command::new(program)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {program}, which the check needs: {error}"));
    fs::create_dir_all(dest).expect("the destination is created");
    let output = Command::new(program)
        .args(["--zstd", "-xpf"])
        .arg(archive)
        .current_dir(dest)
        .output()
        .expect("the program ran a moment ago");
    assert!(
        output.status.success(),
        "{program} failed on {}:\n{}{}",
        archive.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Checks the unpacked tree `dir` against the checksum lines of a
/// `.sha256.zst` listing with `shasum -a 256 -c`. The listing is
/// decompressed into `scratch`, outside the tree, so the check sees no
/// extra file.
pub fn listing_check(listing: &Path, dir: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).expect("the scratch directory is created");
    let decoded = scratch.join("listing.sha256");
    let mut decoder =
        zstd::stream::Decoder::new(fs::File::open(listing).expect("the listing opens"))
            .expect("the listing is zstd");
    let mut out = fs::File::create(&decoded).expect("the decoded listing is created");
    std::io::copy(&mut decoder, &mut out).expect("the listing decodes");
    shasum_check(dir, &decoded);
}

/// The order of versions: by time, then by counter as a number, no counter
/// counting as 1.
fn stamp_key(stamp: &str) -> (String, u64) {
    match stamp.split_at(17) {
        (time, "") => (time.to_string(), 1),
        (time, counter) => (
            time.to_string(),
            counter
                .trim_start_matches('-')
                .parse()
                .expect("a counter is a number"),
        ),
    }
}

/// The versions in the piece's store folder, found by the suffixes of the
/// file names, in version order. A piece without a store folder has none.
pub fn store_files(collection: &Collection, piece: &str) -> Vec<StoredVersion> {
    let folder = collection.path(&format!(".antiq/pieces/{piece}"));
    let Ok(items) = fs::read_dir(&folder) else {
        return Vec::new();
    };
    let mut found: BTreeMap<(String, u64), StoredVersion> = BTreeMap::new();
    for item in items {
        let path = item.expect("an entry").path();
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .to_string();
        let Some((_, rest)) = name.rsplit_once("--") else {
            continue;
        };
        for suffix in [".tar.zst.sha256", ".tar.zst", ".sha256.zst"] {
            let Some(stamp) = rest.strip_suffix(suffix) else {
                continue;
            };
            let version = found
                .entry(stamp_key(stamp))
                .or_insert_with(|| StoredVersion {
                    stamp: stamp.to_string(),
                    archive: None,
                    checksum: None,
                    listing: None,
                });
            match suffix {
                ".tar.zst" => version.archive = Some(path),
                ".tar.zst.sha256" => version.checksum = Some(path),
                _ => version.listing = Some(path),
            }
            break;
        }
    }
    found.into_values().collect()
}

impl Collection {
    /// The stamps of the piece's versions in version order. A version
    /// exists when its archive does.
    pub fn versions(&self, piece: &str) -> Vec<String> {
        store_files(self, piece)
            .into_iter()
            .filter(|version| version.archive.is_some())
            .map(|version| version.stamp)
            .collect()
    }
}
