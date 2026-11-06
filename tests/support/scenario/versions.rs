// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stored versions written by hand, damage to them, ageing of pieces and
//! folders outside the collection.
//!
//! A version is packed here with the `tar`, `zstd` and `sha2` crates and
//! its listing is written as text, following the spec's "Store layout" and
//! "Listing format". antiq's own packing and listing code is not used, so
//! a scenario that reads such a version checks antiq against the spec and
//! not against itself.

use super::collection::{Clock, Collection, PieceBuilder, StoredFiles, restored_line};
use crate::support::tree::Tree;
use filetime::FileTime;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The age in days of the files inside a written version.
const FILE_AGE_DAYS: i64 = 60;

/// The longest file name the spec allows in a store folder.
const LONGEST_NAME: usize = 200;

/// The three files of one stored version.
pub struct VersionFiles {
    pub archive: PathBuf,
    pub checksum: PathBuf,
    pub listing: PathBuf,
}

/// A folder outside any collection, for `import`. Its content is built like
/// a piece's.
pub struct Source {
    tree: Tree,
}

impl Source {
    pub fn new(clock: &Clock, build: impl FnOnce(&mut PieceBuilder)) -> Source {
        let mut tree = Tree::new();
        let mut piece = PieceBuilder::default();
        build(&mut piece);
        piece.write(clock, &mut tree, "source");
        Source { tree }
    }

    /// The folder to import.
    pub fn path(&self) -> PathBuf {
        self.tree.path("source")
    }
}

/// The part of a store file name that comes from the piece path: every
/// character outside `[A-Za-z0-9._]` becomes `-`, and repeated `-` collapse.
fn store_name(piece: &str) -> String {
    let mut name = String::new();
    for character in piece.chars() {
        let kept = character.is_ascii_alphanumeric() || character == '.' || character == '_';
        let next = if kept { character } else { '-' };
        if next == '-' && name.ends_with('-') {
            continue;
        }
        name.push(next);
    }
    name
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn utc(seconds: i64) -> String {
    jiff::Timestamp::from_second(seconds)
        .expect("the time is representable")
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// `2026-09-03T101500` as `2026-09-03T10:15:00Z`; a counter is dropped.
fn stamp_time(stamp: &str) -> String {
    let (date, rest) = stamp.split_once('T').expect("a stamp holds a T");
    let time = &rest[..6];
    format!("{date}T{}:{}:{}Z", &time[0..2], &time[2..4], &time[4..6])
}

impl Collection {
    /// Writes the three files of a version of `piece` into its store
    /// folder. `files` are `(path, content)` pairs below the piece; the
    /// directories above them are implied.
    pub fn add_version(&self, piece: &str, stamp: &str, files: &StoredFiles) {
        let names = self.version_files(piece, stamp);
        let archive_name = names
            .archive
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .to_string();
        assert!(
            archive_name.len() + ".sha256".len() <= LONGEST_NAME,
            "{archive_name} is too long: write a shorter piece path"
        );
        let clock = *self.clock();
        let file_time = clock.days_ago(FILE_AGE_DAYS);
        let dir_time = clock.dir_time();
        let top = piece.rsplit('/').next().expect("a piece has a name");

        // Entries in listing order: sorted by path, a directory with its
        // trailing slash.
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for (path, _) in files {
            let mut parts: Vec<&str> = path.split('/').collect();
            parts.pop();
            for end in 1..=parts.len() {
                dirs.insert(format!("{}/", parts[..end].join("/")));
            }
        }
        let mut entries: Vec<(String, Option<&Vec<u8>>)> =
            dirs.into_iter().map(|dir| (dir, None)).collect();
        entries.extend(
            files
                .iter()
                .map(|(path, content)| (path.clone(), Some(content))),
        );
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // The archive: one top-level directory named after the folder.
        let mut builder = tar::Builder::new(Vec::new());
        let mut append = |path: &str, content: Option<&Vec<u8>>| {
            let mut header = tar::Header::new_gnu();
            match content {
                Some(content) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(0o644);
                    header.set_mtime(file_time as u64);
                    header.set_size(content.len() as u64);
                    builder
                        .append_data(&mut header, path, &content[..])
                        .expect("the member is appended");
                }
                None => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(0o755);
                    header.set_mtime(dir_time as u64);
                    header.set_size(0);
                    builder
                        .append_data(&mut header, path, std::io::empty())
                        .expect("the member is appended");
                }
            }
        };
        append(top, None);
        for (path, content) in &entries {
            append(&format!("{top}/{}", path.trim_end_matches('/')), *content);
        }
        let tar = builder.into_inner().expect("the archive is finished");
        let archive = zstd::stream::encode_all(&tar[..], 3).expect("the archive compresses");
        let archive_hash = hex(&Sha256::digest(&archive));

        // The listing: header, then each entry with a checksum line after a
        // file.
        let regular: Vec<&Vec<u8>> = files.iter().map(|(_, content)| content).collect();
        let bytes: usize = regular.iter().map(|content| content.len()).sum();
        let mut text = String::new();
        let header = |text: &mut String, key: &str, value: &str| {
            text.push_str(&format!(
                "# {key:<14}{value}
"
            ));
        };
        text.push_str("# antiq listing 1\n");
        header(&mut text, "piece", piece);
        header(&mut text, "archived", &stamp_time(stamp));
        header(&mut text, "archive", &archive_name);
        header(&mut text, "archive-bytes", &archive.len().to_string());
        header(&mut text, "files", &regular.len().to_string());
        header(&mut text, "bytes", &bytes.to_string());
        text.push_str("#\n");
        for (path, content) in &entries {
            match content {
                None => text.push_str(&format!(
                    "# {:<4}  {:04o}  {}  {:>16}  {path}\n",
                    "dir",
                    0o755,
                    utc(dir_time),
                    "-"
                )),
                Some(content) => text.push_str(&format!(
                    "# {:<4}  {:04o}  {}  {:>16}  {path}\n{}  {path}\n",
                    "file",
                    0o644,
                    utc(file_time),
                    content.len(),
                    hex(&Sha256::digest(content))
                )),
            }
        }
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).expect("the encoder starts");
        encoder
            .include_checksum(true)
            .expect("the checksum is switched on");
        encoder
            .write_all(text.as_bytes())
            .expect("the listing is written");
        let listing = encoder.finish().expect("the listing is finished");

        fs::create_dir_all(names.archive.parent().expect("a store folder"))
            .expect("the store folder is created");
        fs::write(&names.archive, &archive).expect("the archive is written");
        fs::write(&names.checksum, format!("{archive_hash}  {archive_name}\n"))
            .expect("the checksum file is written");
        fs::write(&names.listing, listing).expect("the listing is written");
    }

    /// The paths the three files of a version have, whether they exist or
    /// not.
    pub fn version_files(&self, piece: &str, stamp: &str) -> VersionFiles {
        let folder = self.path(&format!(".antiq/pieces/{piece}"));
        let stem = format!("{}--{stamp}", store_name(piece));
        VersionFiles {
            archive: folder.join(format!("{stem}.tar.zst")),
            checksum: folder.join(format!("{stem}.tar.zst.sha256")),
            listing: folder.join(format!("{stem}.sha256.zst")),
        }
    }

    /// Cuts the archive in half: its size no longer matches its listing,
    /// nor its checksum.
    pub fn damage_truncate_archive(&self, piece: &str, stamp: &str) {
        let path = self.version_files(piece, stamp).archive;
        let bytes = fs::read(&path).expect("the archive exists");
        fs::write(&path, &bytes[..bytes.len() / 2]).expect("the archive is cut");
    }

    pub fn damage_remove_listing(&self, piece: &str, stamp: &str) {
        fs::remove_file(self.version_files(piece, stamp).listing).expect("the listing exists");
    }

    /// Replaces the checksum file with a line that is not a checksum line
    /// for the archive.
    pub fn damage_corrupt_checksum(&self, piece: &str, stamp: &str) {
        fs::write(
            self.version_files(piece, stamp).checksum,
            "not a checksum line\n",
        )
        .expect("the checksum file is written");
    }

    /// Removes the archive and keeps its sidecars: an incomplete version.
    pub fn damage_remove_archive(&self, piece: &str, stamp: &str) {
        fs::remove_file(self.version_files(piece, stamp).archive).expect("the archive exists");
    }

    /// Makes the piece `days` old: its regular files get that age, and an
    /// `.antiq-restored` marker is rewritten with that time and the version
    /// it names.
    pub fn set_age(&self, piece: &str, days: i64) {
        let time = FileTime::from_unix_time(self.clock().days_ago(days), 0);
        let root = self.path(piece);
        let marker = root.join(".antiq-restored");
        if let Ok(text) = fs::read_to_string(&marker) {
            let version = text.trim_end().split_once(" version ").map(|(_, v)| v);
            fs::write(&marker, restored_line(self.clock(), days, version))
                .expect("the marker is rewritten");
        }
        fn walk(dir: &Path, time: FileTime) {
            for item in fs::read_dir(dir).expect("the directory lists") {
                let path = item.expect("an entry").path();
                let metadata = fs::symlink_metadata(&path).expect("the entry has metadata");
                if metadata.is_dir() {
                    walk(&path, time);
                } else if metadata.is_file() {
                    filetime::set_file_mtime(&path, time).expect("the time can be set");
                }
            }
        }
        walk(&root, time);
    }
}
