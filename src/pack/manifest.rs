// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Manifests of live packages.
//!
//! A manifest records what the walk saw of a piece when its live package
//! was built. A later walk that differs from it means the package is out
//! of date. The comparison needs no content hash: any change of a file moves
//! at least one of the recorded times, and a replaced file has another inode.

use crate::fsx::FsxError;
use crate::fsx::atomic::write_atomic;
use crate::relpath::RelPath;
use crate::tree::{EntryKind, TreeEntry};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// The version of the manifest file. A file with another version is not read.
const FORMAT: u32 = 1;

/// The kinds of entry a live package holds. Fifos, sockets and devices are
/// not packed, so they are not in the manifest either: their coming and
/// going must not cause a new package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestKind {
    #[serde(rename = "file")]
    File,
    #[serde(rename = "dir")]
    Directory,
    #[serde(rename = "symlink")]
    Symlink,
}

/// What the walk recorded of one entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: RelPath,
    #[serde(rename = "type")]
    pub kind: ManifestKind,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub inode: u64,
}

/// The recorded state of a piece: every packed entry, sorted by path bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<ManifestEntry>,
}

/// One recorded property of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Kind,
    Size,
    Mtime,
    Ctime,
    Inode,
}

/// An entry present in both manifests with other values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub path: RelPath,
    pub fields: Vec<Field>,
}

/// How a new manifest differs from the recorded one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDiff {
    /// In the new manifest only.
    pub added: Vec<RelPath>,
    /// In the recorded manifest only.
    pub removed: Vec<RelPath>,
    pub changed: Vec<Changed>,
}

impl ManifestDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Errors of reading and writing a manifest file.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("cannot read the manifest {path}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("the manifest {path} is damaged: {reason}")]
    Damaged { path: String, reason: String },
    #[error("cannot write the manifest")]
    Write(#[from] FsxError),
}

impl crate::error::HasKind for ManifestError {}

impl Manifest {
    /// The manifest of walk entries. Entries that are not packed are left out.
    pub fn from_entries(entries: &[TreeEntry]) -> Manifest {
        let mut kept: Vec<ManifestEntry> = entries
            .iter()
            .filter_map(|entry| {
                let kind = match entry.kind {
                    EntryKind::File => ManifestKind::File,
                    EntryKind::Directory => ManifestKind::Directory,
                    EntryKind::Symlink => ManifestKind::Symlink,
                    _ => return None,
                };
                Some(ManifestEntry {
                    path: entry.path.clone(),
                    kind,
                    size: entry.size,
                    mtime_ns: entry.mtime_ns,
                    ctime_ns: entry.ctime_ns,
                    inode: entry.inode,
                })
            })
            .collect();
        // The merge in `diff` needs the order whatever order the caller used.
        kept.sort_by(|a, b| a.path.cmp(&b.path));
        Manifest { entries: kept }
    }

    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// How `new` differs from this manifest.
    pub fn diff(&self, new: &Manifest) -> ManifestDiff {
        let mut diff = ManifestDiff::default();
        let mut old = self.entries.iter().peekable();
        let mut fresh = new.entries.iter().peekable();
        loop {
            match (old.peek(), fresh.peek()) {
                (None, None) => break,
                (Some(gone), None) => {
                    diff.removed.push(gone.path.clone());
                    old.next();
                }
                (None, Some(added)) => {
                    diff.added.push(added.path.clone());
                    fresh.next();
                }
                (Some(before), Some(after)) => match before.path.cmp(&after.path) {
                    std::cmp::Ordering::Less => {
                        diff.removed.push(before.path.clone());
                        old.next();
                    }
                    std::cmp::Ordering::Greater => {
                        diff.added.push(after.path.clone());
                        fresh.next();
                    }
                    std::cmp::Ordering::Equal => {
                        let fields = changed_fields(before, after);
                        if !fields.is_empty() {
                            diff.changed.push(Changed {
                                path: before.path.clone(),
                                fields,
                            });
                        }
                        old.next();
                        fresh.next();
                    }
                },
            }
        }
        diff
    }

    /// Reads the manifest at `path`. A file that is not there gives `None`.
    ///
    /// A file that cannot be understood is an error. The manifest is only
    /// a record of what the last package was built from, so a caller can
    /// answer such an error like a changed tree and pack again.
    pub fn read(path: &Path) -> Result<Option<Manifest>, ManifestError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ManifestError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let damaged = |reason: String| ManifestError::Damaged {
            path: path.display().to_string(),
            reason,
        };
        // The format is checked before the entries are parsed, since the
        // entries of another format may look different.
        let file: StoredFile =
            serde_json::from_slice(&bytes).map_err(|error| damaged(error.to_string()))?;
        if file.format != FORMAT {
            return Err(damaged(format!("it has format {}", file.format)));
        }
        let entries: Vec<ManifestEntry> =
            serde_json::from_value(file.entries).map_err(|error| damaged(error.to_string()))?;
        // `diff` merges two sorted lists; an unsorted or repeating list
        // would give a wrong answer instead of an error.
        if entries.windows(2).any(|pair| pair[0].path >= pair[1].path) {
            return Err(damaged(
                "its entries are not in strictly increasing path order".to_string(),
            ));
        }
        Ok(Some(Manifest { entries }))
    }

    /// Replaces the manifest at `path`.
    pub fn write(&self, path: &Path) -> Result<(), ManifestError> {
        let text = serde_json::to_vec(&WrittenFile {
            format: FORMAT,
            entries: &self.entries,
        })
        .expect("a manifest of plain numbers and strings serialises");
        write_atomic(path, &text)?;
        Ok(())
    }
}

#[derive(Serialize)]
struct WrittenFile<'a> {
    format: u32,
    entries: &'a [ManifestEntry],
}

#[derive(Deserialize)]
struct StoredFile {
    format: u32,
    entries: serde_json::Value,
}

fn changed_fields(before: &ManifestEntry, after: &ManifestEntry) -> Vec<Field> {
    let mut fields = Vec::new();
    if before.kind != after.kind {
        fields.push(Field::Kind);
    }
    if before.size != after.size {
        fields.push(Field::Size);
    }
    if before.mtime_ns != after.mtime_ns {
        fields.push(Field::Mtime);
    }
    if before.ctime_ns != after.ctime_ns {
        fields.push(Field::Ctime);
    }
    if before.inode != after.inode {
        fields.push(Field::Inode);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind) -> TreeEntry {
        TreeEntry {
            path: RelPath::parse(path).expect("valid path"),
            kind,
            size: 3,
            allocated: 4096,
            mtime_ns: 1_700_000_000_123_456_789,
            ctime_ns: 1_700_000_001_000_000_001,
            inode: 77,
            device: 1,
            mode: 0o644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
        }
    }

    fn manifest_of(entries: &[TreeEntry]) -> Manifest {
        Manifest::from_entries(entries)
    }

    fn paths(list: &[RelPath]) -> Vec<String> {
        list.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn from_entries_keeps_directories_files_and_symlinks_and_drops_special_files() {
        let manifest = manifest_of(&[
            entry("", EntryKind::Directory),
            entry("dir", EntryKind::Directory),
            entry("dir/file", EntryKind::File),
            entry("link", EntryKind::Symlink),
            entry("pipe", EntryKind::Fifo),
            entry("sock", EntryKind::Socket),
        ]);
        let kept: Vec<(String, ManifestKind)> = manifest
            .entries()
            .iter()
            .map(|e| (e.path.to_string(), e.kind))
            .collect();
        assert_eq!(
            kept,
            [
                ("".to_string(), ManifestKind::Directory),
                ("dir".to_string(), ManifestKind::Directory),
                ("dir/file".to_string(), ManifestKind::File),
                ("link".to_string(), ManifestKind::Symlink),
            ]
        );
    }

    /// The manifest of one file, built from `file` after `change` ran on
    /// the entry the recorded manifest was built from.
    fn diff_after(change: impl FnOnce(&mut TreeEntry)) -> ManifestDiff {
        let before = entry("f", EntryKind::File);
        let mut after = before.clone();
        change(&mut after);
        manifest_of(&[before]).diff(&manifest_of(&[after]))
    }

    fn only_field(diff: &ManifestDiff, field: Field) {
        assert_eq!(
            diff.changed,
            [Changed {
                path: RelPath::parse("f").expect("valid path"),
                fields: vec![field],
            }]
        );
        assert!(diff.added.is_empty() && diff.removed.is_empty());
    }

    #[test]
    fn a_new_size_alone_is_reported() {
        only_field(&diff_after(|e| e.size += 1), Field::Size);
    }

    #[test]
    fn a_new_modification_time_alone_is_reported() {
        only_field(&diff_after(|e| e.mtime_ns += 1), Field::Mtime);
    }

    #[test]
    fn a_new_change_time_alone_is_reported() {
        only_field(&diff_after(|e| e.ctime_ns += 1), Field::Ctime);
    }

    #[test]
    fn a_new_inode_alone_is_reported() {
        only_field(&diff_after(|e| e.inode += 1), Field::Inode);
    }

    #[test]
    fn a_new_kind_alone_is_reported() {
        only_field(&diff_after(|e| e.kind = EntryKind::Symlink), Field::Kind);
    }

    #[test]
    fn a_new_directory_and_a_new_file_are_added() {
        let before = manifest_of(&[entry("", EntryKind::Directory)]);
        let after = manifest_of(&[
            entry("", EntryKind::Directory),
            entry("new", EntryKind::Directory),
            entry("new/file", EntryKind::File),
        ]);
        let diff = before.diff(&after);
        assert_eq!(paths(&diff.added), ["new", "new/file"]);
        assert!(diff.removed.is_empty() && diff.changed.is_empty());
    }

    #[test]
    fn a_missing_entry_is_removed() {
        let before = manifest_of(&[
            entry("", EntryKind::Directory),
            entry("gone", EntryKind::File),
        ]);
        let after = manifest_of(&[entry("", EntryKind::Directory)]);
        let diff = before.diff(&after);
        assert_eq!(paths(&diff.removed), ["gone"]);
        assert!(diff.added.is_empty() && diff.changed.is_empty());
    }

    fn file_in(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("01BX5ZZKBKACTAV9WEVGEMMVRY.manifest.json")
    }

    fn damaged(text: &str) -> String {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = file_in(&dir);
        std::fs::write(&path, text).expect("the file is written");
        match Manifest::read(&path) {
            Err(ManifestError::Damaged { reason, .. }) => reason,
            other => panic!("expected a damaged manifest, got {other:?}"),
        }
    }

    #[test]
    fn a_written_manifest_reads_back_with_every_value() {
        let mut odd = entry("a/b", EntryKind::File);
        odd.path = RelPath::from_bytes(b"a/b\xFF".to_vec()).expect("valid path");
        odd.mtime_ns = -1_500_000_000;
        let manifest = manifest_of(&[
            entry("", EntryKind::Directory),
            entry("a", EntryKind::Directory),
            odd,
            entry("l", EntryKind::Symlink),
        ]);
        let dir = tempfile::tempdir().expect("a temp dir");
        manifest.write(&file_in(&dir)).expect("the manifest writes");
        let back = Manifest::read(&file_in(&dir))
            .expect("the manifest reads")
            .expect("the file exists");
        assert_eq!(back, manifest);
    }

    #[test]
    fn the_file_holds_the_format_and_one_object_per_entry() {
        let manifest = manifest_of(&[entry("a", EntryKind::File)]);
        let dir = tempfile::tempdir().expect("a temp dir");
        manifest.write(&file_in(&dir)).expect("the manifest writes");
        let text = std::fs::read_to_string(file_in(&dir)).expect("the file exists");
        assert_eq!(
            text,
            concat!(
                r#"{"format":1,"entries":[{"path":"a","type":"file","size":3,"#,
                r#""mtime_ns":1700000000123456789,"ctime_ns":1700000001000000001,"#,
                r#""inode":77}]}"#
            )
        );
    }

    // A guard: a manifest that was never written is no error, only no
    // manifest.
    #[test]
    fn a_missing_manifest_is_none() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(
            Manifest::read(&file_in(&dir))
                .expect("a missing file is no error")
                .is_none()
        );
    }

    #[test]
    fn text_that_is_no_json_is_damaged() {
        assert!(damaged("not json").contains("expected"), "serde's reason");
    }

    #[test]
    fn a_truncated_manifest_is_damaged() {
        damaged(r#"{"format":1,"entries":[{"path":"a","type":"fi"#);
    }

    #[test]
    fn another_format_is_damaged() {
        let reason = damaged(r#"{"format":2,"entries":[]}"#);
        assert!(reason.contains("format 2"), "{reason}");
    }

    #[test]
    fn entries_out_of_order_are_damaged() {
        let reason = damaged(concat!(
            r#"{"format":1,"entries":["#,
            r#"{"path":"b","type":"file","size":0,"mtime_ns":0,"ctime_ns":0,"inode":1},"#,
            r#"{"path":"a","type":"file","size":0,"mtime_ns":0,"ctime_ns":0,"inode":2}]}"#
        ));
        assert!(reason.contains("order"), "{reason}");
    }

    #[test]
    fn a_path_listed_twice_is_damaged() {
        let reason = damaged(concat!(
            r#"{"format":1,"entries":["#,
            r#"{"path":"a","type":"file","size":0,"mtime_ns":0,"ctime_ns":0,"inode":1},"#,
            r#"{"path":"a","type":"file","size":0,"mtime_ns":0,"ctime_ns":0,"inode":2}]}"#
        ));
        assert!(reason.contains("order"), "{reason}");
    }

    #[test]
    fn a_manifest_that_cannot_be_read_names_its_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A directory in place of the file: reading it fails with an
        // error other than "not found".
        let error = Manifest::read(dir.path()).expect_err("a directory is no manifest");
        assert!(matches!(error, ManifestError::Read { .. }), "{error:?}");
        assert!(
            error
                .to_string()
                .contains(&dir.path().display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn a_manifest_in_a_missing_directory_fails_to_write() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("missing").join("m.json");
        let error = Manifest::default().write(&path).expect_err("no directory");
        assert!(matches!(error, ManifestError::Write(_)), "{error:?}");
    }

    #[test]
    fn an_entry_between_two_others_is_matched_to_its_own_path() {
        let around = |between: bool| {
            let mut entries = vec![
                entry("a", EntryKind::File),
                entry("c", EntryKind::File),
                entry("d", EntryKind::File),
            ];
            if between {
                entries.insert(1, entry("b", EntryKind::File));
            }
            manifest_of(&entries)
        };
        let added = around(false).diff(&around(true));
        assert_eq!(paths(&added.added), ["b"]);
        assert!(added.removed.is_empty() && added.changed.is_empty());
        let removed = around(true).diff(&around(false));
        assert_eq!(paths(&removed.removed), ["b"]);
        assert!(removed.added.is_empty() && removed.changed.is_empty());
    }

    // A guard against a diff that reports everything: it passes with
    // any correct implementation.
    #[test]
    fn equal_manifests_have_an_empty_diff() {
        let entries = [
            entry("", EntryKind::Directory),
            entry("a", EntryKind::File),
            entry("l", EntryKind::Symlink),
        ];
        assert!(
            manifest_of(&entries)
                .diff(&manifest_of(&entries))
                .is_empty()
        );
    }
}
