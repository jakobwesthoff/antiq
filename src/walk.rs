// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lstat walk of a piece.

use crate::fsx::tree::{DirEntry, TreeRoot};
use crate::relpath::{Name, RelPath};
use crate::select::Rules;
use crate::tree::{EntryKind, PieceTree, Problem, TreeEntry};
use rustix::io::Errno;
use std::io;

/// Errors of the piece walk.
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("out of file descriptors while reading {path} in {piece}")]
    OutOfDescriptors {
        piece: RelPath,
        path: RelPath,
        #[source]
        source: io::Error,
    },
}

impl crate::error::HasKind for WalkError {}

/// Points where the walk and the selection can be made to see what an
/// unprivileged test cannot create: a second device, a descriptor
/// shortage.
pub(crate) trait Hooks {
    /// The device to use for `path`, relative to the piece's top, in place
    /// of the `reported` one.
    fn device(&self, _path: &RelPath, reported: u64) -> u64 {
        reported
    }

    /// Whether the entry at `path`, relative to the piece's top, is the
    /// root of a mount, in place of the `reported` answer.
    fn mount_root(&self, _path: &RelPath, reported: bool) -> bool {
        reported
    }

    /// An error to raise instead of making the call of kind `call` that
    /// works on `path`.
    fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error>;

    /// The kind to use for the entry at `path` in place of the `reported`
    /// one, which a listing may leave as `Unknown`.
    fn listed_kind(&self, _path: &RelPath, reported: EntryKind) -> EntryKind {
        reported
    }

    /// How many directory levels below its start a walk or a selection
    /// enters.
    fn max_depth(&self) -> usize {
        MAX_DEPTH
    }
}

/// Each level a walk is inside holds one directory open. The cap leaves
/// room for many walks at once under the limit of open files.
pub(crate) const MAX_DEPTH: usize = 512;

/// The kind of filesystem call a [`Hooks::fault`] can replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Call {
    /// `lstat` of the entry.
    Stat,
    /// Opening the directory.
    Open,
    /// Listing the directory.
    List,
}

/// The real filesystem: no device is replaced, no call fails.
pub(crate) struct RealFs;

impl Hooks for RealFs {
    fn fault(&self, _path: &RelPath, _call: Call) -> Option<io::Error> {
        None
    }
}

/// Whether the process, or the system, has no file descriptor left. That
/// is a problem of the run, not of the entry that happened to need one.
pub(crate) fn is_out_of_descriptors(error: &io::Error) -> bool {
    let code = error.raw_os_error();
    code == Some(Errno::MFILE.raw_os_error()) || code == Some(Errno::NFILE.raw_os_error())
}

/// Walks the piece whose top directory is `top` below `collection`.
///
/// Entries come back relative to the top, with the top itself as the
/// entry with the root path. Nothing is followed: every entry is read with
/// `lstat`, and a directory is entered only while it stays on the device of
/// the top directory and is no mount root. Files named in `junk` are left out; a directory
/// with a junk name is walked like any other. An entry that is on the
/// exclude list of `rules` is reported and, if a directory, not entered.
/// A directory more than [`MAX_DEPTH`] levels below the top is not entered
/// and makes the piece unreadable.
pub fn walk_piece(
    collection: &TreeRoot,
    top: &RelPath,
    junk: &[Name],
    rules: &Rules,
) -> Result<PieceTree, WalkError> {
    walk_piece_with(collection, top, junk, rules, &RealFs)
}

pub(crate) fn walk_piece_with(
    collection: &TreeRoot,
    top: &RelPath,
    junk: &[Name],
    rules: &Rules,
    hooks: &dyn Hooks,
) -> Result<PieceTree, WalkError> {
    let mut walker = Walker {
        piece: top,
        junk,
        rules,
        hooks,
        top_device: 0,
        entries: Vec::new(),
        mount_points: Vec::new(),
        excluded: Vec::new(),
        first_failure: None,
    };
    let root = RelPath::root();

    // The piece's top and the collection root are read in one call, so
    // that a failure of either marks the piece, not the collection.
    let read_top = walker.attempt(&root, Call::Stat, || {
        let top_entry = collection.lstat(top)?;
        let collection_device = collection.lstat(&RelPath::root())?.device;
        Ok((top_entry, collection_device))
    })?;
    if let Some((mut top_entry, collection_device)) = read_top {
        top_entry.path = root.clone();
        walker.top_device = hooks.device(&root, top_entry.device);
        // A top directory on another device, or the root of a mount on the
        // collection's device, is a mount point of its own, although
        // nothing inside it differs from the top.
        let top_is_mount_root = hooks.mount_root(&root, top_entry.mount_root);
        if walker.top_device != collection_device || top_is_mount_root {
            walker.mount_points.push(root.clone());
        }
        let is_directory = top_entry.kind == EntryKind::Directory;
        walker.entries.push(top_entry);
        if !is_directory {
            let not_a_directory = io::Error::from_raw_os_error(Errno::NOTDIR.raw_os_error());
            walker.note_failure(&root, not_a_directory)?;
        } else if let Some(directory) = walker.attempt(&root, Call::Open, || collection.sub(top))? {
            // One directory is open per level, so the levels are kept in
            // a list of their own and not on the call stack: a deep tree
            // costs no deep recursion, and the cap on the depth keeps the
            // open directories few.
            let mut levels = Vec::new();
            levels.extend(walker.open_level(directory, root)?);
            while !levels.is_empty() {
                walker.step(&mut levels)?;
            }
        }
    }

    let mut problems = Vec::new();
    if let Some(first) = walker.first_failure.take() {
        problems.push(Problem::Unreadable { first });
    }
    walker.mount_points.sort();
    problems.extend(
        walker
            .mount_points
            .into_iter()
            .map(|other| Problem::MountPoint { other }),
    );
    walker.excluded.sort();
    problems.extend(
        walker
            .excluded
            .into_iter()
            .map(|path| Problem::Excluded { path }),
    );
    Ok(PieceTree::new(top.clone(), walker.entries, problems))
}

/// A directory being walked: what is left of its listing.
struct Level {
    directory: TreeRoot,
    at: RelPath,
    children: std::vec::IntoIter<DirEntry>,
}

struct Walker<'a> {
    piece: &'a RelPath,
    junk: &'a [Name],
    rules: &'a Rules,
    hooks: &'a dyn Hooks,
    /// The device every entry of the piece has to be on, after the hook.
    top_device: u64,
    entries: Vec<TreeEntry>,
    mount_points: Vec<RelPath>,
    excluded: Vec<RelPath>,
    /// The smallest path, in entry order, that could not be read.
    first_failure: Option<RelPath>,
}

impl Walker<'_> {
    /// Runs one filesystem call on behalf of `path`, unless the hook makes
    /// it fail. A failure other than a descriptor shortage is noted and
    /// gives `None`, so the walk can go on with the rest of the piece.
    fn attempt<T>(
        &mut self,
        path: &RelPath,
        kind: Call,
        call: impl FnOnce() -> io::Result<T>,
    ) -> Result<Option<T>, WalkError> {
        let result = match self.hooks.fault(path, kind) {
            Some(error) => Err(error),
            None => call(),
        };
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                self.note_failure(path, error)?;
                Ok(None)
            }
        }
    }

    /// An entry that cannot be read makes the piece unreadable, also when
    /// it has vanished since its directory was listed. Running out of
    /// descriptors says nothing about the entry and ends the run.
    fn note_failure(&mut self, path: &RelPath, error: io::Error) -> Result<(), WalkError> {
        if is_out_of_descriptors(&error) {
            return Err(WalkError::OutOfDescriptors {
                piece: self.piece.clone(),
                path: path.clone(),
                source: error,
            });
        }
        if self.first_failure.as_ref().is_none_or(|first| path < first) {
            self.first_failure = Some(path.clone());
        }
        Ok(())
    }

    /// Lists `directory`, which lies at `at` below the top.
    fn open_level(&mut self, directory: TreeRoot, at: RelPath) -> Result<Option<Level>, WalkError> {
        let Some(mut listing) =
            self.attempt(&at, Call::List, || directory.read_dir(&RelPath::root()))?
        else {
            return Ok(None);
        };
        listing.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(Level {
            directory,
            at,
            children: listing.into_iter(),
        }))
    }

    /// Records the next entry of the innermost directory and, if it is a
    /// directory on the top's device, opens it as the new innermost one.
    /// Each directory is opened from its parent, so no call ever passes a
    /// path longer than one name.
    fn step(&mut self, levels: &mut Vec<Level>) -> Result<(), WalkError> {
        let depth = levels.len();
        let level = levels
            .last_mut()
            .expect("the walk steps only while a level is open");
        let Some(listed) = level.children.next() else {
            levels.pop();
            return Ok(());
        };
        let child = level.at.join(&listed.name);
        let single = RelPath::from_bytes(listed.name.as_bytes().to_vec())
            .expect("a listed name is a valid path of one component");
        let Some(mut entry) =
            self.attempt(&child, Call::Stat, || level.directory.lstat(&single))?
        else {
            return Ok(());
        };
        if entry.kind != EntryKind::Directory && self.junk.contains(&listed.name) {
            return Ok(());
        }
        entry.path = child.clone();

        let foreign = self.hooks.device(&child, entry.device) != self.top_device
            || self.hooks.mount_root(&child, entry.mount_root);
        if foreign {
            self.mount_points.push(child.clone());
        }
        let excluded = self.rules.has_exclusions()
            && self
                .rules
                .excludes(&self.piece.concat(&child), entry.device, entry.inode);
        if excluded {
            self.excluded.push(child.clone());
        }
        let enter = entry.kind == EntryKind::Directory && !foreign && !excluded;
        self.entries.push(entry);
        if !enter {
            return Ok(());
        }
        if depth > self.hooks.max_depth() {
            let too_deep = io::Error::other("the directory is nested too deeply");
            return self.note_failure(&child, too_deep);
        }
        if let Some(inner) = self.attempt(&child, Call::Open, || level.directory.sub(&single))? {
            let opened = self.open_level(inner, child)?;
            levels.extend(opened);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::deep_path;
    use crate::testing::tree::Tree;
    use crate::tree::{EntryKind, Problem};
    use rustix::io::Errno;
    use std::fs;
    use std::io::Write;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn no_rules() -> Rules {
        Rules::new(&[], &[]).expect("no patterns are valid")
    }

    fn name(text: &str) -> Name {
        Name::from_bytes(text.as_bytes().to_vec()).expect("valid name")
    }

    fn errno(code: Errno) -> io::Error {
        io::Error::from_raw_os_error(code.raw_os_error())
    }

    /// Replaces devices and fails calls at chosen paths of the piece.
    #[derive(Default)]
    struct Staged {
        foreign: Vec<RelPath>,
        mount_roots: Vec<RelPath>,
        faults: Vec<(RelPath, Call, Errno)>,
        depth: Option<usize>,
    }

    impl Hooks for Staged {
        fn max_depth(&self) -> usize {
            self.depth.unwrap_or(MAX_DEPTH)
        }

        fn device(&self, path: &RelPath, reported: u64) -> u64 {
            if self.foreign.contains(path) {
                reported + 1
            } else {
                reported
            }
        }

        fn mount_root(&self, path: &RelPath, reported: bool) -> bool {
            reported || self.mount_roots.contains(path)
        }

        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            self.faults
                .iter()
                .find(|(at, kind, _)| at == path && *kind == call)
                .map(|(_, _, code)| errno(*code))
        }
    }

    fn walk(tree: &Tree, top: &str) -> PieceTree {
        walk_with(tree, top, &[], &RealFs).expect("the walk succeeds")
    }

    fn walk_with(
        tree: &Tree,
        top: &str,
        junk: &[Name],
        hooks: &dyn Hooks,
    ) -> Result<PieceTree, WalkError> {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        walk_piece_with(&root, &path(top), junk, &no_rules(), hooks)
    }

    fn paths(tree: &PieceTree) -> Vec<String> {
        tree.entries().iter().map(|e| e.path.to_string()).collect()
    }

    fn assert_cannot_read(directory: &std::path::Path) {
        assert!(
            fs::read_dir(directory).is_err(),
            "the test needs a user that mode 000 locks out"
        );
    }

    #[test]
    fn a_mixed_tree_is_recorded_with_lstat_and_nothing_is_followed() {
        let mut tree = Tree::new();
        tree.file("piece/a.txt", "hi");
        tree.file("piece/dir/inner", "x");
        tree.hard_link("piece/a.txt", "piece/dir/second");
        tree.symlink("piece/dangling", "nowhere");
        tree.file("outside/deep/f", "x");
        tree.symlink("piece/link", tree.path("outside"));
        tree.fifo("piece/queue");

        let walked = walk(&tree, "piece");

        assert_eq!(
            paths(&walked),
            [
                "",
                "a.txt",
                "dangling",
                "dir",
                "dir/inner",
                "dir/second",
                "link",
                "queue"
            ]
        );
        let kind_of = |p: &str| {
            walked
                .entries()
                .iter()
                .find(|e| e.path.to_string() == p)
                .unwrap_or_else(|| panic!("{p} is recorded"))
        };
        assert_eq!(kind_of("").kind, EntryKind::Directory);
        assert_eq!(kind_of("dangling").kind, EntryKind::Symlink);
        assert_eq!(
            kind_of("dangling").link_target.as_deref(),
            Some(&b"nowhere"[..])
        );
        assert_eq!(kind_of("link").kind, EntryKind::Symlink);
        assert_eq!(kind_of("queue").kind, EntryKind::Fifo);
        assert_eq!(kind_of("a.txt").nlink, 2);
        assert_eq!(kind_of("a.txt").size, 2);
        assert!(walked.problems().is_empty());
        assert_eq!(walked.top(), &path("piece"));
    }

    #[test]
    fn entries_come_out_in_path_byte_order() {
        let mut tree = Tree::new();
        tree.file("piece/dir/x", "x");
        tree.file("piece/dir-y", "y");
        let walked = walk(&tree, "piece");
        assert_eq!(paths(&walked), ["", "dir", "dir-y", "dir/x"]);
    }

    #[test]
    fn junk_files_are_left_out_and_junk_named_directories_are_not() {
        let mut tree = Tree::new();
        tree.file("piece/.DS_Store", "x");
        tree.file("piece/dir/.DS_Store", "x");
        tree.file("piece/dir/keep", "x");
        tree.file("piece/other/.DS_Store/inside", "x");
        let walked =
            walk_with(&tree, "piece", &[name(".DS_Store")], &RealFs).expect("the walk succeeds");
        assert_eq!(
            paths(&walked),
            [
                "",
                "dir",
                "dir/keep",
                "other",
                "other/.DS_Store",
                "other/.DS_Store/inside"
            ]
        );
    }

    #[test]
    fn an_unreadable_directory_makes_the_piece_unreadable_and_stays_an_entry() {
        let mut tree = Tree::new();
        tree.file("piece/readable", "x");
        let locked = tree.unreadable_dir("piece/b_locked");
        tree.unreadable_dir("piece/a_locked");
        assert_cannot_read(&locked);

        let walked = walk(&tree, "piece");

        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: path("a_locked")
            }]
        );
        assert_eq!(
            paths(&walked),
            ["", "a_locked", "b_locked", "readable"],
            "the walk goes on past the failure"
        );
    }

    #[test]
    fn the_first_failure_is_the_smallest_path_in_entry_order() {
        let mut tree = Tree::new();
        tree.file("piece/dir/x", "x");
        tree.file("piece/dir-y", "y");
        let hooks = Staged {
            faults: vec![
                (path("dir/x"), Call::Stat, Errno::NOENT),
                (path("dir-y"), Call::Stat, Errno::NOENT),
            ],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: path("dir-y")
            }]
        );
    }

    #[test]
    fn a_missing_top_is_unreadable_with_no_entries() {
        let tree = Tree::new();
        let walked = walk(&tree, "gone");
        assert!(walked.entries().is_empty());
        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: RelPath::root()
            }]
        );
    }

    #[test]
    fn a_top_that_cannot_be_opened_keeps_only_its_own_entry() {
        let mut tree = Tree::new();
        let locked = tree.unreadable_dir("piece");
        assert_cannot_read(&locked);
        let walked = walk(&tree, "piece");
        assert_eq!(paths(&walked), [""]);
        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: RelPath::root()
            }]
        );
    }

    #[test]
    fn a_symlink_at_the_top_is_not_walked() {
        let mut tree = Tree::new();
        tree.file("real/f", "x");
        tree.symlink("piece", tree.path("real"));
        let walked = walk(&tree, "piece");
        assert_eq!(paths(&walked), [""]);
        assert_eq!(walked.entries()[0].kind, EntryKind::Symlink);
        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: RelPath::root()
            }]
        );
    }

    #[test]
    fn a_directory_on_another_device_is_recorded_and_not_entered() {
        let mut tree = Tree::new();
        tree.file("piece/sub/mnt/inside", "x");
        tree.file("piece/sub/f", "x");
        let hooks = Staged {
            foreign: vec![path("sub/mnt")],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(paths(&walked), ["", "sub", "sub/f", "sub/mnt"]);
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: path("sub/mnt")
            }]
        );
    }

    #[test]
    fn a_mount_root_on_the_pieces_device_is_recorded_and_not_entered() {
        let mut tree = Tree::new();
        tree.file("piece/sub/mnt/inside", "x");
        tree.file("piece/sub/f", "x");
        let hooks = Staged {
            mount_roots: vec![path("sub/mnt")],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(paths(&walked), ["", "sub", "sub/f", "sub/mnt"]);
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: path("sub/mnt")
            }]
        );
    }

    #[test]
    fn a_file_that_is_a_mount_root_is_recorded_too() {
        let mut tree = Tree::new();
        tree.file("piece/bound", "x");
        let hooks = Staged {
            mount_roots: vec![path("bound")],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: path("bound")
            }]
        );
    }

    #[test]
    fn a_top_that_is_a_mount_root_counts_as_a_mount_point_at_its_own_path() {
        let mut tree = Tree::new();
        tree.file("piece/sub/f", "x");
        let hooks = Staged {
            mount_roots: vec![RelPath::root()],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(
            paths(&walked),
            ["", "sub", "sub/f"],
            "the content is walked"
        );
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: RelPath::root()
            }]
        );
    }

    #[test]
    fn a_top_on_another_device_that_is_also_a_mount_root_is_listed_once() {
        let mut tree = Tree::new();
        tree.file("piece/f", "x");
        let hooks = Staged {
            foreign: vec![RelPath::root(), path("f")],
            mount_roots: vec![RelPath::root()],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: RelPath::root()
            }]
        );
    }

    #[test]
    fn mount_points_are_listed_in_entry_order() {
        let mut tree = Tree::new();
        tree.file("piece/dir/x", "x");
        tree.file("piece/dir-y", "y");
        let hooks = Staged {
            foreign: vec![path("dir/x"), path("dir-y")],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(
            walked.problems(),
            [
                Problem::MountPoint {
                    other: path("dir-y")
                },
                Problem::MountPoint {
                    other: path("dir/x")
                }
            ]
        );
    }

    fn walk_excluding(tree: &Tree, exclude: &[&str]) -> PieceTree {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let exclude: Vec<RelPath> = exclude.iter().map(|p| path(p)).collect();
        let rules = Rules::new(&[], &exclude)
            .expect("no patterns are valid")
            .bind(&root)
            .expect("the exclude list binds");
        walk_piece(&root, &path("piece"), &[], &rules).expect("the walk succeeds")
    }

    #[test]
    fn an_existing_exclude_entry_inside_a_piece_is_reported_and_not_entered() {
        let mut tree = Tree::new();
        tree.file("piece/sub/keep/inner", "x");
        tree.file("piece/sub/f", "x");
        let walked = walk_excluding(&tree, &["piece/sub/keep"]);
        assert_eq!(paths(&walked), ["", "sub", "sub/f", "sub/keep"]);
        assert_eq!(
            walked.problems(),
            [Problem::Excluded {
                path: path("sub/keep")
            }]
        );
    }

    #[test]
    fn an_exclude_entry_that_does_not_exist_changes_nothing() {
        let mut tree = Tree::new();
        tree.file("piece/sub/f", "x");
        let walked = walk_excluding(&tree, &["piece/sub/never"]);
        assert!(walked.problems().is_empty());
    }

    #[test]
    fn an_exclude_entry_is_found_under_another_spelling_where_the_volume_allows_it() {
        let mut tree = Tree::new();
        tree.file("piece/sub/keep/inner", "x");
        let insensitive = fs::metadata(tree.path("PIECE")).is_ok();
        let walked = walk_excluding(&tree, &["PIECE/SUB/KEEP"]);
        let found = [Problem::Excluded {
            path: path("sub/keep"),
        }];
        assert_eq!(walked.problems(), &found[..usize::from(insensitive)]);
    }

    #[test]
    fn directories_below_the_depth_cap_make_the_piece_unreadable_and_are_not_entered() {
        let mut tree = Tree::new();
        tree.file("piece/a/b/c/d/f", "x");
        let hooks = Staged {
            depth: Some(2),
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(paths(&walked), ["", "a", "a/b", "a/b/c"]);
        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: path("a/b/c")
            }]
        );
    }

    #[test]
    fn the_default_depth_cap_is_512_levels() {
        assert_eq!(RealFs.max_depth(), 512);
    }

    #[test]
    fn a_file_on_another_device_is_a_mount_point_too() {
        let mut tree = Tree::new();
        tree.file("piece/sub/f", "x");
        let hooks = Staged {
            foreign: vec![path("sub/f")],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(paths(&walked), ["", "sub", "sub/f"]);
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: path("sub/f")
            }]
        );
    }

    #[test]
    fn a_top_on_another_device_counts_as_a_mount_point_at_its_own_path() {
        let mut tree = Tree::new();
        tree.file("piece/sub/f", "x");
        let hooks = Staged {
            foreign: vec![RelPath::root(), path("sub"), path("sub/f")],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
        assert_eq!(
            paths(&walked),
            ["", "sub", "sub/f"],
            "the content is walked"
        );
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: RelPath::root()
            }],
            "the children share the top's device"
        );
    }

    #[test]
    fn a_sparse_file_reports_an_allocated_size_below_its_size() {
        let mut tree = Tree::new();
        tree.sparse("piece/hole", 1 << 20);
        let walked = walk(&tree, "piece");
        let hole = &walked.entries()[1];
        assert_eq!(hole.size, 1 << 20);
        assert!(hole.allocated < hole.size, "{} allocated", hole.allocated);
    }

    #[test]
    fn a_tree_deeper_than_the_path_limit_is_walked_completely() {
        let mut tree = Tree::new();
        tree.dir("piece");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let deep = deep_path(25);
        let mut so_far = path("piece");
        for component in deep.components() {
            let component = Name::from_bytes(component.to_vec()).expect("valid name");
            so_far = so_far.join(&component);
            root.create_dir(&so_far, 0o755)
                .expect("the directory is created");
        }
        root.create_file(&so_far.join(&name("f")), 0o644)
            .expect("the file is created")
            .write_all(b"x")
            .expect("the file is written");

        let walked =
            walk_piece(&root, &path("piece"), &[], &no_rules()).expect("the walk succeeds");

        assert_eq!(walked.entries().len(), 1 + 25 + 1);
        let last = walked.entries().last().expect("entries exist");
        assert_eq!(last.path.to_string(), format!("{deep}/f"));
        assert_eq!(last.size, 1);
        assert!(walked.problems().is_empty());
    }

    #[test]
    fn running_out_of_descriptors_fails_the_run_wherever_it_happens() {
        let mut tree = Tree::new();
        tree.file("piece/sub/dir/f", "x");
        let places = [
            (path("sub/dir"), Call::Stat),
            (path("sub/dir"), Call::Open),
            (path("sub/dir"), Call::List),
            (RelPath::root(), Call::Stat),
            (RelPath::root(), Call::Open),
            (RelPath::root(), Call::List),
        ];
        for (at, call) in places {
            let hooks = Staged {
                faults: vec![(at.clone(), call, Errno::MFILE)],
                ..Staged::default()
            };
            let error = walk_with(&tree, "piece", &[], &hooks)
                .expect_err("a descriptor shortage fails the run");
            let WalkError::OutOfDescriptors {
                piece,
                path: failed,
                ..
            } = error;
            assert_eq!(piece, path("piece"));
            assert_eq!(failed, at);
        }
    }

    #[test]
    fn other_errors_on_an_entry_make_the_piece_unreadable() {
        let mut tree = Tree::new();
        tree.file("piece/sub/dir/f", "x");
        for call in [Call::Stat, Call::Open, Call::List] {
            for code in [Errno::ACCESS, Errno::NOENT] {
                let hooks = Staged {
                    faults: vec![(path("sub/dir"), call, code)],
                    ..Staged::default()
                };
                let walked = walk_with(&tree, "piece", &[], &hooks).expect("the walk succeeds");
                assert_eq!(
                    walked.problems(),
                    [Problem::Unreadable {
                        first: path("sub/dir")
                    }],
                    "{call:?} {code:?}"
                );
                assert_eq!(
                    paths(&walked).contains(&"sub/dir".to_string()),
                    call != Call::Stat,
                    "{call:?}: an entry that was stat'd stays recorded"
                );
            }
        }
    }

    #[test]
    fn only_a_descriptor_shortage_counts_as_out_of_descriptors() {
        assert!(is_out_of_descriptors(&errno(Errno::MFILE)));
        assert!(is_out_of_descriptors(&errno(Errno::NFILE)));
        assert!(!is_out_of_descriptors(&errno(Errno::ACCESS)));
        assert!(!is_out_of_descriptors(&io::Error::other("no code")));
    }
}
