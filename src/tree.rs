// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The result of walking a piece.
//!
//! A walk records every entry it meets with a single `lstat`, never
//! following a link. Everything that reads a piece afterwards (idle check,
//! packing, verification, change detection) works from this record.

use crate::relpath::{Name, RelPath};
use rustix::fs::FileType;

/// What kind of file system object an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Fifo,
    Socket,
    CharDevice,
    BlockDevice,
    /// A mode that names no known kind.
    Unknown,
}

impl EntryKind {
    /// The kind a `st_mode` names.
    pub fn from_mode(mode: u32) -> EntryKind {
        match FileType::from_raw_mode(mode as rustix::fs::RawMode) {
            FileType::RegularFile => EntryKind::File,
            FileType::Directory => EntryKind::Directory,
            FileType::Symlink => EntryKind::Symlink,
            FileType::Fifo => EntryKind::Fifo,
            FileType::Socket => EntryKind::Socket,
            FileType::CharacterDevice => EntryKind::CharDevice,
            FileType::BlockDevice => EntryKind::BlockDevice,
            FileType::Unknown => EntryKind::Unknown,
        }
    }
}

/// What the rules and the control files make of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Packed and counted for the idle time.
    Counted,
    /// Packed, not counted for the idle time.
    Timeless,
    /// Neither packed nor counted: an ignore rule matches it, or the
    /// directory it lies in.
    Ignored,
    /// A control file of antiq: a rule file, or a marker at the top.
    Control,
}

/// One entry of a piece as `lstat` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Relative to the top directory of the piece; the top directory
    /// itself is the entry with the root path.
    pub path: RelPath,
    pub kind: EntryKind,
    /// Apparent size in bytes.
    pub size: u64,
    /// Bytes allocated on disk. Below `size` for a sparse file.
    pub allocated: u64,
    /// Nanoseconds since the epoch; negative before 1970.
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub inode: u64,
    pub device: u64,
    /// The whole `st_mode`: kind and permission bits.
    pub mode: u32,
    pub nlink: u64,
    /// What a symlink points to, as the bytes the filesystem holds.
    pub link_target: Option<Vec<u8>>,
    /// Nanoseconds since the epoch, where the filesystem records a birth
    /// time.
    pub birth_ns: Option<i128>,
    /// The entry is the root of a mounted filesystem. Unlike a different
    /// `device`, this also holds for a bind mount or a firmlink, which
    /// share the device of the directory they appear in.
    pub mount_root: bool,
    /// The owner of the entry.
    pub uid: u32,
    /// The entry is immutable or append-only, so it can be neither removed
    /// nor moved by its owner. Always false where the platform has no such
    /// flags.
    pub flag_locked: bool,
    /// Set by the walk; an entry that did not come from a walk is
    /// `Counted`.
    pub class: Class,
}

/// Something about a piece that the walk could not handle silently. Paths
/// are relative to the top directory of the piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The first entry that could not be read; the piece is unreadable.
    Unreadable { first: RelPath },
    /// Another device is mounted at this path inside the piece.
    MountPoint { other: RelPath },
    /// An entry of the collection's exclude list lies at this path inside
    /// the piece.
    Excluded { path: RelPath },
    /// A collection of its own lies inside the piece: the directory at
    /// this path holds a `.antiq` directory, the marker of a collection.
    NestedCollection { root: RelPath },
    /// The entry at this path is immutable or append-only, or a directory
    /// the user neither owns nor can write.
    Locked { path: RelPath },
    /// The directory that holds the top of the piece is locked, so the
    /// piece cannot be removed or moved. The path is relative to the
    /// collection.
    LockedParent { path: RelPath },
    /// The rule file at this path cannot be used. `line` is the line where
    /// a rule is at fault, and `None` for a file that cannot be read or is
    /// no text.
    RuleFile { path: RelPath, line: Option<usize> },
    /// The marker of this name lies at the top of the piece as something
    /// other than a file.
    MarkerNotAFile { name: Name },
}

/// A piece as the walk saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceTree {
    top: RelPath,
    entries: Vec<TreeEntry>,
    problems: Vec<Problem>,
}

impl PieceTree {
    /// `top` is the path of the piece's top directory in the collection.
    /// The entries are sorted by path bytes (`dir`, `dir-y`, `dir/x`), so
    /// that the top directory comes first and equal trees list equally.
    pub fn new(top: RelPath, mut entries: Vec<TreeEntry>, problems: Vec<Problem>) -> PieceTree {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        PieceTree {
            top,
            entries,
            problems,
        }
    }

    pub fn top(&self) -> &RelPath {
        &self.top
    }

    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    /// The entries that reach the archive: everything the rules do not
    /// ignore, and not the `.antiq-restored` marker of the top, which
    /// records the piece's history and is no part of its content.
    pub fn packed_entries(&self) -> impl Iterator<Item = &TreeEntry> {
        self.entries.iter().filter(|entry| {
            let restored = entry.class == Class::Control
                && entry.path.as_bytes() == crate::markers::RESTORED_FILE.as_bytes();
            entry.class != Class::Ignored && !restored
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind) -> TreeEntry {
        TreeEntry {
            path: RelPath::parse(path).expect("valid path"),
            kind,
            size: 0,
            allocated: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            inode: 0,
            device: 0,
            mode: 0o644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
            mount_root: false,
            uid: 0,
            flag_locked: false,
            class: Class::Counted,
        }
    }

    #[test]
    fn a_mode_names_the_kind_of_entry() {
        let modes = [
            (0o100644, EntryKind::File),
            (0o040755, EntryKind::Directory),
            (0o120777, EntryKind::Symlink),
            (0o010644, EntryKind::Fifo),
            (0o140755, EntryKind::Socket),
            (0o020644, EntryKind::CharDevice),
            (0o060644, EntryKind::BlockDevice),
            (0o000644, EntryKind::Unknown),
        ];
        for (mode, kind) in modes {
            assert_eq!(EntryKind::from_mode(mode), kind, "mode {mode:o}");
        }
    }

    #[test]
    fn entries_are_sorted_by_path_bytes_with_the_top_directory_first() {
        let tree = PieceTree::new(
            RelPath::parse("projects/a").expect("valid path"),
            vec![
                entry("dir/x", EntryKind::File),
                entry("dir-y", EntryKind::File),
                entry("dir", EntryKind::Directory),
                entry("", EntryKind::Directory),
            ],
            vec![],
        );
        let paths: Vec<String> = tree.entries().iter().map(|e| e.path.to_string()).collect();
        assert_eq!(paths, ["", "dir", "dir-y", "dir/x"]);
    }

    #[test]
    fn the_tree_keeps_its_top_and_its_problems() {
        let problems = vec![
            Problem::Unreadable {
                first: RelPath::parse("locked").expect("valid path"),
            },
            Problem::MountPoint {
                other: RelPath::parse("mnt").expect("valid path"),
            },
        ];
        let top = RelPath::parse("projects/a").expect("valid path");
        let tree = PieceTree::new(top.clone(), vec![], problems.clone());
        assert_eq!(tree.top(), &top);
        assert_eq!(tree.problems(), problems.as_slice());
        assert!(tree.entries().is_empty());
    }
}
