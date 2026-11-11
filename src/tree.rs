// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The result of walking a piece.
//!
//! A walk records every entry it meets with a single `lstat`, never
//! following a link. Everything that reads a piece afterwards (idle check,
//! packing, verification, change detection) works from this record.

use crate::markers::RESTORED_FILE;
use crate::relpath::{Name, RelPath};
use rustix::fs::FileType;
use std::collections::HashSet;

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
            let restored =
                entry.class == Class::Control && entry.path.as_bytes() == RESTORED_FILE.as_bytes();
            entry.class != Class::Ignored && !restored
        })
    }

    /// What the version of this piece does not contain, as a person reads
    /// it: the ignored entries that are not inside an ignored folder (a
    /// folder counts once, with a trailing `/`) and the `.antiq-restored`
    /// marker of the top, sorted. The entries inside an ignored folder are
    /// left out with it and not named; every other entry is packed or named
    /// here.
    pub fn left_out(&self) -> Vec<String> {
        let mut left = Vec::new();
        // Entries are ordered by path bytes, so a name such as `cache.log`
        // can sit between `cache` and `cache/x`. Every ignored folder is
        // therefore remembered, and an entry is inside one when its parent
        // is such a folder: everything below an ignored folder is ignored
        // with it.
        let mut ignored_folders: HashSet<&[u8]> = HashSet::new();
        for entry in &self.entries {
            if entry.class == Class::Ignored {
                let path = entry.path.as_bytes();
                let inside = path
                    .iter()
                    .rposition(|byte| *byte == b'/')
                    .is_some_and(|slash| ignored_folders.contains(&path[..slash]));
                if !inside {
                    let suffix = if entry.kind == EntryKind::Directory {
                        "/"
                    } else {
                        ""
                    };
                    left.push(format!("{}{suffix}", entry.path));
                }
                if entry.kind == EntryKind::Directory {
                    ignored_folders.insert(path);
                }
            } else if entry.class == Class::Control
                && entry.path.as_bytes() == RESTORED_FILE.as_bytes()
            {
                left.push(RESTORED_FILE.to_string());
            }
        }
        left.sort();
        left
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

    fn classed(path: &str, kind: EntryKind, class: Class) -> TreeEntry {
        TreeEntry {
            class,
            ..entry(path, kind)
        }
    }

    fn tree_of(entries: Vec<TreeEntry>) -> PieceTree {
        PieceTree::new(
            RelPath::parse("projects/a").expect("valid path"),
            entries,
            vec![],
        )
    }

    #[test]
    fn an_ignored_folder_is_left_out_once_without_its_content() {
        let tree = tree_of(vec![
            classed("cache", EntryKind::Directory, Class::Ignored),
            classed("cache/deep", EntryKind::Directory, Class::Ignored),
            classed("cache/deep/x", EntryKind::File, Class::Ignored),
        ]);
        assert_eq!(tree.left_out(), ["cache/"]);
    }

    #[test]
    fn an_ignored_file_is_left_out_by_its_path() {
        let tree = tree_of(vec![
            entry("sub", EntryKind::Directory),
            classed("sub/debug.log", EntryKind::File, Class::Ignored),
        ]);
        assert_eq!(tree.left_out(), ["sub/debug.log"]);
    }

    #[test]
    fn a_name_that_sorts_between_a_folder_and_its_content_does_not_bring_the_content_back() {
        let tree = tree_of(vec![
            classed("cache", EntryKind::Directory, Class::Ignored),
            classed("cache.log", EntryKind::File, Class::Ignored),
            classed("cache/x", EntryKind::File, Class::Ignored),
            classed("build", EntryKind::Directory, Class::Ignored),
            classed("build-cache", EntryKind::Directory, Class::Ignored),
            classed("build-cache/y", EntryKind::File, Class::Ignored),
            classed("build/x", EntryKind::File, Class::Ignored),
        ]);
        assert_eq!(
            tree.left_out(),
            ["build-cache/", "build/", "cache.log", "cache/"]
        );
    }

    #[test]
    fn the_restored_marker_of_the_top_is_left_out() {
        let tree = tree_of(vec![classed(
            ".antiq-restored",
            EntryKind::File,
            Class::Control,
        )]);
        assert_eq!(tree.left_out(), [".antiq-restored"]);
    }

    #[test]
    fn the_pin_of_the_top_is_packed_and_not_left_out() {
        let tree = tree_of(vec![classed(".antiq-pin", EntryKind::File, Class::Control)]);
        assert_eq!(tree.left_out(), Vec::<String>::new());
        assert_eq!(tree.packed_entries().count(), 1);
    }

    #[test]
    fn a_restored_marker_below_the_top_is_left_out_only_when_a_rule_ignores_it() {
        let counted = tree_of(vec![
            entry("sub", EntryKind::Directory),
            classed("sub/.antiq-restored", EntryKind::File, Class::Counted),
        ]);
        let ignored = tree_of(vec![
            entry("sub", EntryKind::Directory),
            classed("sub/.antiq-restored", EntryKind::File, Class::Ignored),
        ]);
        assert_eq!(counted.left_out(), Vec::<String>::new());
        assert_eq!(ignored.left_out(), ["sub/.antiq-restored"]);
    }

    #[test]
    fn the_left_out_entries_come_sorted() {
        let tree = tree_of(vec![
            classed(".antiq-restored", EntryKind::File, Class::Control),
            classed("z.log", EntryKind::File, Class::Ignored),
            classed("a.log", EntryKind::File, Class::Ignored),
        ]);
        assert_eq!(tree.left_out(), [".antiq-restored", "a.log", "z.log"]);
    }

    #[test]
    fn a_piece_without_ignored_entries_leaves_nothing_out() {
        let tree = tree_of(vec![entry("a", EntryKind::File)]);
        assert_eq!(tree.left_out(), Vec::<String>::new());
    }
}
