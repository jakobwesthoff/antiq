// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lstat walk of a piece.

use crate::filesystem::tree::{DirEntry, TreeRoot};
use crate::markers::RESTORED_FILE;
use crate::pin::PIN_FILE;
use crate::relpath::{Name, RelPath};
use crate::rules::{IGNORE_FILE, PieceRules, RulesError, Scope, TIMELESS_FILE, parse_rule_file};
use crate::select::Rules;
use crate::shown::Shown;
use crate::tree::{Class, EntryKind, PieceTree, Problem, TreeEntry};
use rustix::io::Errno;
use std::io::{self, Read};

/// Errors of the piece walk.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum WalkError {
    #[error("out of file descriptors while reading {path} in {piece}", piece = Shown(piece), path = Shown(path))]
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

    /// Whether the directory at `prefix`, relative to the collection and
    /// above the piece's top, is the root of a mount, in place of the
    /// `reported` answer.
    fn prefix_mount_root(&self, _prefix: &RelPath, reported: bool) -> bool {
        reported
    }

    /// An error to raise instead of making the call of kind `call` that
    /// works on `path`.
    fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error>;

    /// The owner to use for the entry at `path`, relative to the piece's
    /// top, in place of the `reported` one. A test cannot create a file
    /// that belongs to someone else.
    fn owner(&self, _path: &RelPath, reported: u32) -> u32 {
        reported
    }

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
    /// Reading a rule file.
    Read,
}

/// The real filesystem: no device is replaced, no call fails.
pub(crate) struct RealFs;

impl Hooks for RealFs {
    fn fault(&self, _path: &RelPath, _call: Call) -> Option<io::Error> {
        None
    }
}

/// The hooks for the walk of a folder that stays where it is, as `import`
/// and its verify walk it: the folder being the root of a mount does not
/// make it a mount point, since nothing moves it. Mounts below it still do.
pub(crate) struct KeptFolder<'a>(pub(crate) &'a dyn Hooks);

impl Hooks for KeptFolder<'_> {
    fn device(&self, path: &RelPath, reported: u64) -> u64 {
        self.0.device(path, reported)
    }

    fn mount_root(&self, path: &RelPath, reported: bool) -> bool {
        !path.is_root() && self.0.mount_root(path, reported)
    }

    fn prefix_mount_root(&self, prefix: &RelPath, reported: bool) -> bool {
        self.0.prefix_mount_root(prefix, reported)
    }

    fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
        self.0.fault(path, call)
    }

    fn owner(&self, path: &RelPath, reported: u32) -> u32 {
        self.0.owner(path, reported)
    }

    fn listed_kind(&self, path: &RelPath, reported: EntryKind) -> EntryKind {
        self.0.listed_kind(path, reported)
    }

    fn max_depth(&self) -> usize {
        self.0.max_depth()
    }
}

/// Whether `problem` only matters when the folder is removed or moved: a
/// locked entry cannot be deleted, a locked parent cannot let the folder
/// go. A folder that stays is not held back by either.
pub(crate) fn only_blocks_removal(problem: &Problem) -> bool {
    matches!(
        problem,
        Problem::Locked { .. } | Problem::LockedParent { .. }
    )
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
/// the top directory and is no mount root. A top below a mount root of the
/// collection counts as a mount point at its own path. An entry that is on
/// the exclude list of `select` is reported and, if a directory, not entered. A
/// directory named `.antiq` marks a collection of its own: the directory
/// that holds it is reported, and the walk goes on.
/// A directory more than [`MAX_DEPTH`] levels below the top is not entered
/// and makes the piece unreadable.
///
/// The commands walk through [`walk_piece_with`], which takes the hooks that
/// only the library sees; this entry point is the one the integration
/// tests, a separate crate, can call.
pub fn walk_piece(
    collection: &TreeRoot,
    top: &RelPath,
    select: &Rules,
    rules: &PieceRules,
) -> Result<PieceTree, WalkError> {
    walk_piece_with(collection, top, select, rules, &RealFs)
}

pub(crate) fn walk_piece_with(
    collection: &TreeRoot,
    top: &RelPath,
    select: &Rules,
    rules: &PieceRules,
    hooks: &dyn Hooks,
) -> Result<PieceTree, WalkError> {
    let (ignore, timeless) = rules.scopes();
    let mut walker = Walker {
        piece: top,
        select,
        read_rule_files: rules.reads_files(),
        ignore,
        timeless,
        hooks,
        top_device: 0,
        entries: Vec::new(),
        mount_points: Vec::new(),
        excluded: Vec::new(),
        nested_collections: Vec::new(),
        rule_files: Vec::new(),
        marker_faults: Vec::new(),
        locked: Vec::new(),
        locked_parent: None,
        first_failure: None,
    };
    let root = RelPath::root();

    // The piece's top and the collection root are read in one call, so
    // that a failure of either marks the piece, not the collection.
    let read_top = walker.attempt(&root, Call::Stat, || {
        let top_entry = collection.lstat(top)?;
        let collection_device = collection.lstat(&RelPath::root())?.device;
        let mut below_a_mount_root = false;
        for prefix in directories_above(top) {
            let above = collection.lstat(&prefix)?;
            below_a_mount_root |= hooks.prefix_mount_root(&prefix, above.mount_root);
        }
        Ok((top_entry, collection_device, below_a_mount_root))
    })?;
    if let Some((mut top_entry, collection_device, below_a_mount_root)) = read_top {
        top_entry.path = root.clone();
        walker.top_device = hooks.device(&root, top_entry.device);
        // A top directory on another device, the root of a mount on the
        // collection's device, or a top below such a mount root is a mount
        // point of its own, although nothing inside it differs from the
        // top. A mount root above the top shares the device of everything
        // below it, so only the directories above show it.
        let top_is_mount_root = hooks.mount_root(&root, top_entry.mount_root);
        if walker.top_device != collection_device || top_is_mount_root || below_a_mount_root {
            walker.mount_points.push(root.clone());
        }
        let is_directory = top_entry.kind == EntryKind::Directory;
        walker.note_locked(&root, &top_entry, || collection.user_can_write(top))?;
        walker.entries.push(top_entry);
        if !top.is_root() {
            walker.check_parent(collection, top)?;
        }
        if !is_directory {
            let not_a_directory = io::Error::from_raw_os_error(Errno::NOTDIR.raw_os_error());
            walker.note_failure(&root, not_a_directory)?;
        } else if let Some(directory) = walker.attempt(&root, Call::Open, || collection.sub(top))? {
            // One directory is open per level, so the levels are kept in
            // a list of their own and not on the call stack: a deep tree
            // costs no deep recursion, and the cap on the depth keeps the
            // open directories few.
            let mut levels = Vec::new();
            levels.extend(walker.open_level(directory, root, false)?);
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
    walker.nested_collections.sort();
    problems.extend(
        walker
            .nested_collections
            .into_iter()
            .map(|root| Problem::NestedCollection { root }),
    );
    walker.locked.sort();
    problems.extend(
        walker
            .locked
            .into_iter()
            .map(|path| Problem::Locked { path }),
    );
    problems.extend(
        walker
            .locked_parent
            .into_iter()
            .map(|path| Problem::LockedParent { path }),
    );
    walker.rule_files.sort();
    problems.extend(
        walker
            .rule_files
            .into_iter()
            .map(|(path, line)| Problem::RuleFile { path, line }),
    );
    walker
        .marker_faults
        .sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    problems.extend(
        walker
            .marker_faults
            .into_iter()
            .map(|name| Problem::MarkerNotAFile { name }),
    );
    Ok(PieceTree::new(top.clone(), walker.entries, problems))
}

/// The directories between the collection root and `top`, the root
/// excluded and `top` itself too: `a` and `a/b` for `a/b/c`.
fn directories_above(top: &RelPath) -> Vec<RelPath> {
    let mut components: Vec<&[u8]> = top.components().collect();
    components.pop();
    let mut above = Vec::new();
    let mut prefix = RelPath::root();
    for component in components {
        let name = Name::from_bytes(component.to_vec()).expect("a component of a path is a name");
        prefix = prefix.join(&name);
        above.push(prefix.clone());
    }
    above
}

/// A directory being walked: what is left of its listing.
struct Level {
    directory: TreeRoot,
    at: RelPath,
    /// The directory is ignored, so its rule files do not count.
    ignored: bool,
    children: std::vec::IntoIter<DirEntry>,
}

struct Walker<'a> {
    piece: &'a RelPath,
    select: &'a Rules,
    /// Whether rule files are read.
    read_rule_files: bool,
    ignore: Scope,
    timeless: Scope,
    hooks: &'a dyn Hooks,
    /// The device every entry of the piece has to be on, after the hook.
    top_device: u64,
    entries: Vec<TreeEntry>,
    mount_points: Vec<RelPath>,
    excluded: Vec<RelPath>,
    /// The directories that hold a `.antiq` directory, the marker of a
    /// collection of its own.
    nested_collections: Vec<RelPath>,
    /// The rule files that cannot be used, with the line at fault.
    rule_files: Vec<(RelPath, Option<usize>)>,
    /// The marker names at the top that are not files.
    marker_faults: Vec<Name>,
    /// The entries that are locked.
    locked: Vec<RelPath>,
    /// The directory above the top, if it is locked.
    locked_parent: Option<RelPath>,
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
    fn open_level(
        &mut self,
        directory: TreeRoot,
        at: RelPath,
        ignored: bool,
    ) -> Result<Option<Level>, WalkError> {
        let Some(mut listing) =
            self.attempt(&at, Call::List, || directory.read_dir(&RelPath::root()))?
        else {
            return Ok(None);
        };
        listing.sort_by(|a, b| a.name.cmp(&b.name));
        // The rules of a directory have to be known before any of its
        // entries is judged. A directory that is ignored is deleted with
        // its piece whatever its rules say, so they are not even read.
        if self.read_rule_files && !ignored {
            for file_name in [IGNORE_FILE, TIMELESS_FILE] {
                if listing
                    .iter()
                    .any(|listed| listed.name.as_bytes() == file_name)
                {
                    self.read_rule_file(&directory, &at, file_name)?;
                }
            }
        }
        Ok(Some(Level {
            directory,
            at,
            ignored,
            children: listing.into_iter(),
        }))
    }

    /// Reads the rule file `file_name` of the directory at `at`, unless it
    /// is something other than a regular file, and adds its rules to the
    /// scope of its kind. A file that cannot be used refuses the piece.
    fn read_rule_file(
        &mut self,
        directory: &TreeRoot,
        at: &RelPath,
        file_name: &[u8],
    ) -> Result<(), WalkError> {
        let name = Name::from_bytes(file_name.to_vec()).expect("a rule file name is valid");
        let single = RelPath::from_bytes(file_name.to_vec()).expect("a rule file name is a path");
        let path = at.join(&name);
        let Some(stat) = self.attempt(&path, Call::Stat, || directory.lstat(&single))? else {
            return Ok(());
        };
        if stat.kind != EntryKind::File {
            return Ok(());
        }
        let read = || -> io::Result<Vec<u8>> {
            let mut content = Vec::new();
            directory.open_read(&single)?.read_to_end(&mut content)?;
            Ok(content)
        };
        let result = match self.hooks.fault(&path, Call::Read) {
            Some(error) => Err(error),
            None => read(),
        };
        let content = match result {
            Ok(content) => content,
            Err(error) if is_out_of_descriptors(&error) => {
                return Err(WalkError::OutOfDescriptors {
                    piece: self.piece.clone(),
                    path,
                    source: error,
                });
            }
            Err(_) => {
                self.rule_files.push((path, None));
                return Ok(());
            }
        };
        match parse_rule_file(content) {
            Ok(rules) if file_name == IGNORE_FILE => self.ignore.add_file(at, rules),
            Ok(rules) => self.timeless.add_file(at, rules),
            Err(error) => {
                // Line 0 stands for a failure of the rule set as a whole.
                let line = match error {
                    RulesError::BadLine { line, .. } => Some(line).filter(|line| *line > 0),
                    _ => None,
                };
                self.rule_files.push((path, line));
            }
        }
        Ok(())
    }

    /// Notes `entry` at `path` if it is locked: immutable or append-only,
    /// or a directory that another user owns and this one cannot write.
    /// Removing or moving it would fail halfway. A directory the user owns
    /// is never locked by its mode, since removal gives itself owner access.
    /// `writable` asks whether the user can write the directory.
    fn note_locked(
        &mut self,
        path: &RelPath,
        entry: &TreeEntry,
        writable: impl FnOnce() -> io::Result<bool>,
    ) -> Result<(), WalkError> {
        let mut locked = entry.flag_locked;
        let owner = self.hooks.owner(path, entry.uid);
        if !locked
            && entry.kind == EntryKind::Directory
            && owner != rustix::process::geteuid().as_raw()
        {
            locked = self
                .attempt(path, Call::Stat, writable)?
                .is_some_and(|writable| !writable);
        }
        if locked {
            self.locked.push(path.clone());
        }
        Ok(())
    }

    /// Notes the directory that holds the top if it is locked, whoever owns
    /// it: antiq changes nothing outside the piece, so a parent the user
    /// cannot write keeps the piece from being removed or moved.
    fn check_parent(&mut self, collection: &TreeRoot, top: &RelPath) -> Result<(), WalkError> {
        let parent = directories_above(top).pop().unwrap_or_else(RelPath::root);
        let check = || -> io::Result<bool> {
            let entry = collection.lstat(&parent)?;
            Ok(entry.flag_locked || !collection.user_can_write(&parent)?)
        };
        // The hook is asked about the parent's path in the collection: the
        // root path already stands for the top's own `lstat`.
        let result = match self.hooks.fault(&parent, Call::Stat) {
            Some(error) => Err(error),
            None => check(),
        };
        match result {
            Ok(false) => {}
            Ok(true) => self.locked_parent = Some(parent),
            Err(error) if is_out_of_descriptors(&error) => {
                return Err(WalkError::OutOfDescriptors {
                    piece: self.piece.clone(),
                    path: parent,
                    source: error,
                });
            }
            // A parent that cannot be checked cannot be shown to be
            // writable, and the piece in it cannot be removed or moved.
            Err(_) => self.locked_parent = Some(parent),
        }
        Ok(())
    }

    /// A marker name at the top of the piece that is not a file refuses the
    /// piece: it would be packed or deleted without anyone having read it.
    /// A symlink is a pin, but not a restored marker.
    fn check_marker_name(&mut self, name: &Name, kind: EntryKind) {
        let pin = name.as_bytes() == PIN_FILE.as_bytes();
        let restored = name.as_bytes() == RESTORED_FILE.as_bytes();
        let fits = match kind {
            EntryKind::File => true,
            EntryKind::Symlink => pin,
            _ => false,
        };
        if (pin || restored) && !fits {
            self.marker_faults.push(name.clone());
        }
    }

    /// What the rules and the control files make of the entry `listed` in
    /// the directory `level`.
    fn classify(&mut self, level: &Level, listed: &DirEntry, entry: &TreeEntry) -> Class {
        let is_dir = entry.kind == EntryKind::Directory;
        // Both scopes see every entry: a directory that matches has to be
        // remembered for what lies below it.
        let ignored = self.ignore.decide(&entry.path, is_dir);
        let timeless = self.timeless.decide(&entry.path, is_dir);
        let name = listed.name.as_bytes();
        let regular = entry.kind == EntryKind::File;
        let rule_file = self.read_rule_files
            && regular
            && !level.ignored
            && (name == IGNORE_FILE || name == TIMELESS_FILE);
        let marker = level.at.is_root()
            && ((regular && name == RESTORED_FILE.as_bytes())
                || (name == PIN_FILE.as_bytes()
                    && matches!(entry.kind, EntryKind::File | EntryKind::Symlink)));
        if rule_file || marker {
            Class::Control
        } else if ignored {
            Class::Ignored
        } else if timeless {
            Class::Timeless
        } else {
            Class::Counted
        }
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
        entry.path = child.clone();
        entry.class = self.classify(level, &listed, &entry);
        self.note_locked(&child, &entry, || level.directory.user_can_write(&single))?;
        // Compared without regard to case: on a volume that ignores case
        // `.ANTIQ` is the marker too, and on one that does not the worst
        // outcome is a refused piece, never a deleted collection.
        if entry.kind == EntryKind::Directory
            && listed.name.as_bytes().eq_ignore_ascii_case(b".antiq")
        {
            self.nested_collections.push(level.at.clone());
        }
        if level.at.is_root() {
            self.check_marker_name(&listed.name, entry.kind);
        }

        let foreign = self.hooks.device(&child, entry.device) != self.top_device
            || self.hooks.mount_root(&child, entry.mount_root);
        if foreign {
            self.mount_points.push(child.clone());
        }
        let excluded = self.select.has_exclusions()
            && self
                .select
                .excludes(&self.piece.concat(&child), entry.device, entry.inode);
        if excluded {
            self.excluded.push(child.clone());
        }
        let enter = entry.kind == EntryKind::Directory && !foreign && !excluded;
        let ignored = entry.class == Class::Ignored;
        self.entries.push(entry);
        if !enter {
            return Ok(());
        }
        if depth > self.hooks.max_depth() {
            let too_deep = io::Error::other("the directory is nested too deeply");
            return self.note_failure(&child, too_deep);
        }
        if let Some(inner) = self.attempt(&child, Call::Open, || level.directory.sub(&single))? {
            let opened = self.open_level(inner, child, ignored)?;
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
    use crate::tree::{Class, EntryKind, Problem};
    use rustix::io::Errno;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn no_selection() -> Rules {
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
        above_mount_roots: Vec<RelPath>,
        faults: Vec<(RelPath, Call, Errno)>,
        depth: Option<usize>,
        /// Entries that another user owns.
        foreign_owner: Vec<RelPath>,
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

        fn prefix_mount_root(&self, prefix: &RelPath, reported: bool) -> bool {
            reported || self.above_mount_roots.contains(prefix)
        }

        fn owner(&self, path: &RelPath, reported: u32) -> u32 {
            if self.foreign_owner.contains(path) {
                reported + 1
            } else {
                reported
            }
        }

        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            self.faults
                .iter()
                .find(|(at, kind, _)| at == path && *kind == call)
                .map(|(_, _, code)| errno(*code))
        }
    }

    fn walk(tree: &Tree, top: &str) -> PieceTree {
        walk_with(tree, top, &PieceRules::none(), &RealFs).expect("the walk succeeds")
    }

    fn walk_with(
        tree: &Tree,
        top: &str,
        rules: &PieceRules,
        hooks: &dyn Hooks,
    ) -> Result<PieceTree, WalkError> {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        walk_piece_with(&root, &path(top), &no_selection(), rules, hooks)
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

    fn walk_ruled(tree: &Tree, ignore: &[&str], timeless: &[&str]) -> PieceTree {
        walk_ruled_with(tree, ignore, timeless, &RealFs).expect("the walk succeeds")
    }

    fn walk_ruled_with(
        tree: &Tree,
        ignore: &[&str],
        timeless: &[&str],
        hooks: &dyn Hooks,
    ) -> Result<PieceTree, WalkError> {
        let lines = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        let rules = PieceRules::new(&lines(ignore), &lines(timeless)).expect("valid rules");
        walk_with(tree, "piece", &rules, hooks)
    }

    fn class_of(tree: &PieceTree, entry: &str) -> Class {
        tree.entries()
            .iter()
            .find(|candidate| candidate.path.to_string() == entry)
            .unwrap_or_else(|| panic!("{entry} is recorded"))
            .class
    }

    fn packed(tree: &PieceTree) -> Vec<String> {
        tree.packed_entries()
            .map(|entry| entry.path.to_string())
            .collect()
    }

    #[test]
    fn an_ignored_directory_stays_in_the_record_but_does_not_reach_the_archive() {
        let mut tree = Tree::new();
        tree.file("piece/node_modules/dep/index.js", "x");
        tree.file("piece/keep", "x");
        let walked = walk_ruled(&tree, &["node_modules/"], &[]);
        assert_eq!(class_of(&walked, "node_modules"), Class::Ignored);
        assert_eq!(
            class_of(&walked, "node_modules/dep/index.js"),
            Class::Ignored
        );
        assert_eq!(class_of(&walked, "keep"), Class::Counted);
        assert_eq!(class_of(&walked, ""), Class::Counted);
        assert_eq!(packed(&walked), ["", "keep"]);
        assert_eq!(walked.entries().len(), 5);
    }

    #[test]
    fn an_ignored_subtree_is_checked_for_what_refuses_a_piece() {
        let mut tree = Tree::new();
        tree.dir("piece/skip/nested/.antiq");
        tree.dir("piece/skip/mount");
        tree.dir("piece/skip/excluded");
        let locked = tree.unreadable_dir("piece/skip/unreadable");
        assert_cannot_read(&locked);
        let hooks = Staged {
            mount_roots: vec![path("skip/mount")],
            ..Staged::default()
        };
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let select = Rules::new(&[], &[path("piece/skip/excluded")])
            .expect("no patterns are valid")
            .bind(&root)
            .expect("the exclude list binds");
        let rules = PieceRules::new(&["skip/".to_string()], &[]).expect("valid rules");
        let walked =
            walk_piece_with(&root, &path("piece"), &select, &rules, &hooks).expect("walks");
        assert_eq!(
            walked.problems(),
            [
                Problem::Unreadable {
                    first: path("skip/unreadable")
                },
                Problem::MountPoint {
                    other: path("skip/mount")
                },
                Problem::Excluded {
                    path: path("skip/excluded")
                },
                Problem::NestedCollection {
                    root: path("skip/nested")
                },
            ]
        );
        assert_eq!(class_of(&walked, "skip"), Class::Ignored);
        assert_eq!(class_of(&walked, "skip/nested/.antiq"), Class::Ignored);
    }

    #[test]
    fn a_timeless_directory_is_still_packed() {
        let mut tree = Tree::new();
        tree.file("piece/.git/HEAD", "x");
        tree.file("piece/src/main.rs", "x");
        let walked = walk_ruled(&tree, &[], &[".git/"]);
        assert_eq!(class_of(&walked, ".git"), Class::Timeless);
        assert_eq!(class_of(&walked, ".git/HEAD"), Class::Timeless);
        assert_eq!(class_of(&walked, "src/main.rs"), Class::Counted);
        assert_eq!(packed(&walked).len(), 5);
    }

    #[test]
    fn a_rule_file_applies_below_its_directory_and_the_nearest_one_wins() {
        let mut tree = Tree::new();
        tree.file("piece/a/.antiqignore", "docs/*.log\n");
        tree.file("piece/a/docs/x.log", "x");
        tree.file("piece/a/b/docs/x.log", "x");
        tree.file("piece/sibling/docs/x.log", "x");
        tree.file("piece/.antiqignore", "*.tmp\n");
        tree.file("piece/a/b/.antiqignore", "!keep.tmp\n");
        tree.file("piece/a/b/keep.tmp", "x");
        tree.file("piece/a/drop.tmp", "x");
        let walked = walk_ruled(&tree, &["*.cfg"], &[]);
        assert_eq!(class_of(&walked, "a/docs/x.log"), Class::Ignored);
        assert_eq!(class_of(&walked, "a/b/docs/x.log"), Class::Counted);
        assert_eq!(class_of(&walked, "sibling/docs/x.log"), Class::Counted);
        assert_eq!(class_of(&walked, "a/b/keep.tmp"), Class::Counted);
        assert_eq!(class_of(&walked, "a/drop.tmp"), Class::Ignored);
    }

    #[test]
    fn a_negation_does_not_bring_back_what_an_ignored_directory_holds() {
        let mut tree = Tree::new();
        tree.file("piece/cache/.antiqignore", "!x\n");
        tree.file("piece/cache/x", "x");
        let walked = walk_ruled(&tree, &["cache/"], &[]);
        assert_eq!(class_of(&walked, "cache/x"), Class::Ignored);
    }

    #[test]
    fn ignored_wins_where_an_ignore_and_a_timeless_rule_match() {
        let mut tree = Tree::new();
        tree.file("piece/both.log", "x");
        tree.file("piece/only.log", "x");
        let walked = walk_ruled(&tree, &["both.*"], &["*.log"]);
        assert_eq!(class_of(&walked, "both.log"), Class::Ignored);
        assert_eq!(class_of(&walked, "only.log"), Class::Timeless);
    }

    #[test]
    fn a_name_in_the_default_ignore_list_matches_files_and_directories() {
        let mut tree = Tree::new();
        tree.file("piece/.DS_Store", "x");
        tree.file("piece/dir/.DS_Store", "x");
        tree.file("piece/other/.DS_Store/inside", "x");
        tree.file("piece/dir/keep", "x");
        let walked = walk_ruled(&tree, &[".DS_Store"], &[]);
        assert_eq!(packed(&walked), ["", "dir", "dir/keep", "other"]);
        assert_eq!(class_of(&walked, "other/.DS_Store/inside"), Class::Ignored);
    }

    #[test]
    fn rule_files_are_control_files_whatever_the_rules_say() {
        let mut tree = Tree::new();
        tree.file("piece/.antiqignore", "*\n");
        tree.file("piece/data", "x");
        let walked = walk_ruled(&tree, &[], &[]);
        assert_eq!(class_of(&walked, ".antiqignore"), Class::Control);
        assert_eq!(class_of(&walked, "data"), Class::Ignored);
        assert_eq!(packed(&walked), ["", ".antiqignore"]);

        let mut tree = Tree::new();
        tree.file("piece/sub/.antiq-timeless", "x\n");
        tree.file("piece/sub/.antiqignore", "x\n");
        let walked = walk_ruled(&tree, &[".antiq*"], &["sub/"]);
        assert_eq!(class_of(&walked, "sub/.antiq-timeless"), Class::Control);
        assert_eq!(class_of(&walked, "sub/.antiqignore"), Class::Control);
    }

    #[test]
    fn a_rule_file_inside_an_ignored_directory_is_ignored_and_not_read() {
        let mut tree = Tree::new();
        tree.file("piece/skip/.antiqignore", [0xff, 0xfe]);
        tree.file("piece/skip/.antiq-timeless", "bad\\\n");
        let unreadable = tree.file("piece/skip/inner/.antiqignore", "x");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
            .expect("the mode can be set");
        let walked = walk_ruled(&tree, &["skip/"], &[]);
        assert_eq!(class_of(&walked, "skip/.antiqignore"), Class::Ignored);
        assert_eq!(class_of(&walked, "skip/.antiq-timeless"), Class::Ignored);
        assert_eq!(walked.problems(), []);
    }

    #[test]
    fn what_is_not_a_regular_file_is_no_rule_file() {
        let mut tree = Tree::new();
        tree.file("outside", "*\n");
        tree.symlink("piece/.antiqignore", tree.path("outside"));
        tree.fifo("piece/sub/.antiq-timeless");
        tree.file("piece/data", "x");
        let walked = walk_ruled(&tree, &[], &[]);
        assert_eq!(class_of(&walked, ".antiqignore"), Class::Counted);
        assert_eq!(class_of(&walked, "sub/.antiq-timeless"), Class::Counted);
        assert_eq!(class_of(&walked, "data"), Class::Counted);
        assert_eq!(walked.problems(), []);
    }

    #[test]
    fn a_broken_rule_file_refuses_the_piece_naming_file_and_line() {
        let mut tree = Tree::new();
        tree.file("piece/a/.antiqignore", "fine\nbad\\\n");
        tree.file("piece/b/.antiq-timeless", "bad\\");
        tree.file("piece/c/.antiqignore", [0xff, 0xfe]);
        let hidden = tree.file("piece/d/.antiq-timeless", "x");
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000))
            .expect("the mode can be set");
        assert!(fs::read(&hidden).is_err(), "mode 000 locks the user out");
        let walked = walk_ruled(&tree, &[], &[]);
        assert_eq!(
            walked.problems(),
            [
                Problem::RuleFile {
                    path: path("a/.antiqignore"),
                    line: Some(2)
                },
                Problem::RuleFile {
                    path: path("b/.antiq-timeless"),
                    line: Some(1)
                },
                Problem::RuleFile {
                    path: path("c/.antiqignore"),
                    line: None
                },
                Problem::RuleFile {
                    path: path("d/.antiq-timeless"),
                    line: None
                },
            ]
        );
    }

    #[test]
    fn running_out_of_descriptors_while_reading_a_rule_file_ends_the_run() {
        let mut tree = Tree::new();
        tree.file("piece/.antiqignore", "x\n");
        let hooks = Staged {
            faults: vec![(path(".antiqignore"), Call::Read, Errno::MFILE)],
            ..Staged::default()
        };
        let error = walk_ruled_with(&tree, &[], &[], &hooks).expect_err("the run fails");
        assert!(matches!(
            error,
            WalkError::OutOfDescriptors { path: ref at, .. } if at == &path(".antiqignore")
        ));
    }

    #[test]
    fn a_read_error_other_than_a_shortage_makes_the_rule_file_a_problem() {
        let mut tree = Tree::new();
        tree.file("piece/.antiqignore", "x\n");
        let hooks = Staged {
            faults: vec![(path(".antiqignore"), Call::Read, Errno::IO)],
            ..Staged::default()
        };
        let walked = walk_ruled_with(&tree, &[], &[], &hooks).expect("the walk succeeds");
        assert_eq!(
            walked.problems(),
            [Problem::RuleFile {
                path: path(".antiqignore"),
                line: None
            }]
        );
    }

    #[test]
    fn markers_at_the_top_are_control_files_below_it_they_are_ordinary() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-restored", "x");
        tree.file("piece/.antiq-pin", "x");
        tree.file("piece/sub/.antiq-restored", "x");
        tree.file("piece/sub/.antiq-pin", "x");
        let walked = walk_ruled(&tree, &[".antiq-*"], &[]);
        assert_eq!(class_of(&walked, ".antiq-restored"), Class::Control);
        assert_eq!(class_of(&walked, ".antiq-pin"), Class::Control);
        assert_eq!(class_of(&walked, "sub/.antiq-restored"), Class::Ignored);
        assert_eq!(packed(&walked), ["", ".antiq-pin", "sub"]);
    }

    #[test]
    fn a_staged_tree_is_walked_with_no_rules_at_all() {
        let mut tree = Tree::new();
        tree.file("piece/.antiqignore", "*\n");
        tree.file("piece/.DS_Store", "x");
        tree.file("piece/data", "x");
        let walked = walk(&tree, "piece");
        assert_eq!(class_of(&walked, "data"), Class::Counted);
        assert_eq!(class_of(&walked, ".DS_Store"), Class::Counted);
        assert_eq!(class_of(&walked, ".antiqignore"), Class::Counted);
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
        assert_eq!(
            walked.problems(),
            [Problem::MountPoint {
                other: RelPath::root()
            }]
        );
    }

    fn marker_problem(text: &str) -> Problem {
        Problem::MarkerNotAFile { name: name(text) }
    }

    #[test]
    fn a_marker_name_at_the_top_that_is_not_a_file_refuses_the_piece() {
        let mut tree = Tree::new();
        tree.fifo("piece/.antiq-pin");
        tree.socket("piece/.antiq-restored");
        let walked = walk(&tree, "piece");
        assert_eq!(
            walked.problems(),
            [
                marker_problem(".antiq-pin"),
                marker_problem(".antiq-restored")
            ]
        );
    }

    #[test]
    fn a_directory_with_a_marker_name_is_refused_and_still_entered() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin/inside", "x");
        tree.dir("piece/.antiq-restored");
        let walked = walk(&tree, "piece");
        assert_eq!(
            walked.problems(),
            [
                marker_problem(".antiq-pin"),
                marker_problem(".antiq-restored")
            ]
        );
        assert!(paths(&walked).contains(&".antiq-pin/inside".to_string()));
    }

    #[test]
    fn a_symlink_is_a_pin_but_not_a_restored_marker() {
        let mut tree = Tree::new();
        tree.symlink("piece/.antiq-pin", "elsewhere");
        let walked = walk(&tree, "piece");
        assert_eq!(walked.problems(), []);
        assert_eq!(class_of(&walked, ".antiq-pin"), Class::Control);

        let mut tree = Tree::new();
        tree.symlink("piece/.antiq-restored", "elsewhere");
        let walked = walk(&tree, "piece");
        assert_eq!(walked.problems(), [marker_problem(".antiq-restored")]);
    }

    #[test]
    fn marker_names_below_the_top_are_ordinary_entries() {
        let mut tree = Tree::new();
        tree.fifo("piece/sub/.antiq-pin");
        tree.dir("piece/sub/.antiq-restored");
        let walked = walk(&tree, "piece");
        assert_eq!(walked.problems(), []);
        assert_eq!(class_of(&walked, "sub/.antiq-pin"), Class::Counted);
    }

    #[test]
    fn a_nested_collection_is_found_whatever_the_case_of_the_marker_directory() {
        let mut tree = Tree::new();
        tree.dir("piece/a/.ANTIQ");
        tree.dir("piece/b/.Antiq");
        let walked = walk(&tree, "piece");
        assert_eq!(
            walked.problems(),
            [
                Problem::NestedCollection { root: path("a") },
                Problem::NestedCollection { root: path("b") },
            ]
        );
    }

    /// Makes the directory read-only; the mode comes back when the guard
    /// drops.
    fn close(tree: &Tree, relative: &str) -> crate::filesystem::RestoreModes {
        let directory = tree.path(relative);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555))
            .expect("the mode is set");
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "root writes anywhere"
        );
        crate::filesystem::RestoreModes(vec![directory])
    }

    #[test]
    fn a_directory_someone_else_owns_and_the_user_cannot_write_is_locked() {
        let mut tree = Tree::new();
        tree.file("piece/theirs/f", "x");
        tree.file("piece/theirs-writable/f", "x");
        tree.file("piece/mine/f", "x");
        let _closed = close(&tree, "piece/theirs");
        let _closed_mine = close(&tree, "piece/mine");
        let hooks = Staged {
            foreign_owner: vec![path("theirs"), path("theirs-writable")],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("walks");
        assert_eq!(
            walked.problems(),
            [Problem::Locked {
                path: path("theirs")
            }]
        );
    }

    #[test]
    fn a_top_someone_else_owns_and_the_user_cannot_write_is_locked() {
        let mut tree = Tree::new();
        tree.file("piece/f", "x");
        let _closed = close(&tree, "piece");
        let hooks = Staged {
            foreign_owner: vec![RelPath::root()],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("walks");
        assert_eq!(
            walked.problems(),
            [Problem::Locked {
                path: RelPath::root()
            }]
        );
    }

    #[test]
    fn a_directory_the_user_owns_is_not_locked_whatever_its_mode() {
        let mut tree = Tree::new();
        tree.file("piece/mine/f", "x");
        let _closed = close(&tree, "piece/mine");
        let walked = walk(&tree, "piece");
        assert_eq!(walked.problems(), []);
        assert!(walked.entries().iter().all(|entry| !entry.flag_locked));
    }

    #[test]
    fn a_top_whose_parent_the_user_cannot_write_is_locked_by_its_parent() {
        let mut tree = Tree::new();
        tree.file("area/piece/f", "x");
        let _closed = close(&tree, "area");
        let walked = walk(&tree, "area/piece");
        assert_eq!(
            walked.problems(),
            [Problem::LockedParent { path: path("area") }]
        );
    }

    #[test]
    fn the_collection_root_is_the_parent_of_a_top_directly_below_it() {
        let mut tree = Tree::new();
        tree.file("piece/f", "x");
        let _closed = close(&tree, "");
        let walked = walk(&tree, "piece");
        assert_eq!(
            walked.problems(),
            [Problem::LockedParent {
                path: RelPath::root()
            }]
        );
    }

    #[test]
    fn a_walk_from_the_root_of_the_handle_has_no_parent_to_check() {
        let mut tree = Tree::new();
        tree.file("f", "x");
        let _closed = close(&tree, "");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let walked = walk_piece(
            &root,
            &RelPath::root(),
            &no_selection(),
            &PieceRules::none(),
        )
        .expect("walks");
        assert_eq!(walked.problems(), []);
    }

    #[test]
    fn problems_come_in_the_order_the_status_lines_are_printed() {
        let mut tree = Tree::new();
        tree.file("area/piece/a/.antiqignore", "bad\\\n");
        tree.dir("area/piece/b/.antiq");
        tree.dir("area/piece/c-theirs");
        tree.fifo("area/piece/.antiq-pin");
        let unreadable = tree.unreadable_dir("area/piece/d");
        assert_cannot_read(&unreadable);
        let _closed = close(&tree, "area/piece/c-theirs");
        let _parent = close(&tree, "area");
        let hooks = Staged {
            foreign_owner: vec![path("c-theirs")],
            ..Staged::default()
        };
        let walked = walk_with(
            &tree,
            "area/piece",
            &PieceRules::new(&[], &[]).expect("rules"),
            &hooks,
        )
        .expect("walks");
        let order: Vec<&str> = walked
            .problems()
            .iter()
            .map(|problem| match problem {
                Problem::Unreadable { .. } => "unreadable",
                Problem::MountPoint { .. } => "mount",
                Problem::Excluded { .. } => "excluded",
                Problem::NestedCollection { .. } => "nested",
                Problem::Locked { .. } => "locked",
                Problem::LockedParent { .. } => "locked parent",
                Problem::RuleFile { .. } => "rule file",
                Problem::MarkerNotAFile { .. } => "marker",
            })
            .collect();
        assert_eq!(
            order,
            [
                "unreadable",
                "nested",
                "locked",
                "locked parent",
                "rule file",
                "marker"
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_immutable_file_or_directory_is_locked_wherever_it_lies() {
        let mut tree = Tree::new();
        tree.file("piece/plain", "x");
        tree.file("piece/frozen", "x");
        tree.file("piece/dir/inner", "x");
        tree.file("piece/skip/deep/frozen", "x");
        tree.immutable("piece/frozen");
        tree.immutable("piece/dir");
        tree.immutable("piece/skip/deep/frozen");
        let walked = walk_ruled(&tree, &["skip/"], &[]);
        assert_eq!(
            walked.problems(),
            [
                Problem::Locked { path: path("dir") },
                Problem::Locked {
                    path: path("frozen")
                },
                Problem::Locked {
                    path: path("skip/deep/frozen")
                },
            ]
        );
        assert_eq!(class_of(&walked, "skip/deep/frozen"), Class::Ignored);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_immutable_top_is_locked_at_the_root_path() {
        let mut tree = Tree::new();
        tree.file("piece/f", "x");
        tree.immutable("piece");
        let walked = walk(&tree, "piece");
        assert_eq!(
            walked.problems(),
            [Problem::Locked {
                path: RelPath::root()
            }]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_immutable_parent_locks_the_top_whatever_its_owner() {
        let mut tree = Tree::new();
        tree.file("area/piece/f", "x");
        tree.immutable("area");
        let walked = walk(&tree, "area/piece");
        assert_eq!(
            walked.problems(),
            [Problem::LockedParent { path: path("area") }]
        );
    }

    #[test]
    fn a_collection_marker_inside_the_piece_is_reported_with_the_directory_holding_it() {
        let mut tree = Tree::new();
        tree.file("piece/sub/.antiq/config.toml", "x");
        tree.file("piece/sub/f", "x");
        let walked = walk(&tree, "piece");
        assert_eq!(
            walked.problems(),
            [Problem::NestedCollection { root: path("sub") }]
        );
        assert!(
            paths(&walked).contains(&"sub/.antiq/config.toml".to_string()),
            "the walk goes on into the marker"
        );
    }

    #[test]
    fn a_collection_marker_at_the_top_of_the_piece_names_the_top() {
        let mut tree = Tree::new();
        tree.dir("piece/.antiq");
        let walked = walk(&tree, "piece");
        assert_eq!(
            walked.problems(),
            [Problem::NestedCollection {
                root: RelPath::root()
            }]
        );
    }

    #[test]
    fn a_file_or_a_symlink_named_antiq_is_no_collection_marker() {
        let mut tree = Tree::new();
        tree.file("piece/a/.antiq", "x");
        tree.symlink("piece/b/.antiq", tree.path("piece"));
        tree.dir("piece/b");
        let walked = walk(&tree, "piece");
        assert_eq!(walked.problems(), []);
    }

    #[test]
    fn nested_collections_come_last_in_path_order() {
        let mut tree = Tree::new();
        tree.dir("piece/z/.antiq");
        tree.dir("piece/a/.antiq");
        tree.file("piece/mnt/f", "x");
        let hooks = Staged {
            foreign: vec![path("mnt")],
            ..Staged::default()
        };
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
        assert_eq!(
            walked.problems(),
            [
                Problem::MountPoint { other: path("mnt") },
                Problem::NestedCollection { root: path("a") },
                Problem::NestedCollection { root: path("z") },
            ]
        );
    }

    #[test]
    fn a_piece_below_a_mount_root_counts_as_a_mount_point_at_its_own_path() {
        let mut tree = Tree::new();
        tree.file("proj/deeper/piece/f", "x");
        for above in ["proj", "proj/deeper"] {
            let hooks = Staged {
                above_mount_roots: vec![path(above)],
                ..Staged::default()
            };
            let walked =
                walk_with(&tree, "proj/deeper/piece", &PieceRules::none(), &hooks).expect("walks");
            assert_eq!(
                walked.problems(),
                [Problem::MountPoint {
                    other: RelPath::root()
                }],
                "{above}"
            );
            assert_eq!(paths(&walked), ["", "f"], "the content is walked");
        }
    }

    #[test]
    fn directories_above_the_top_that_are_no_mount_roots_add_no_problem() {
        let mut tree = Tree::new();
        tree.file("proj/piece/f", "x");
        let walked = walk(&tree, "proj/piece");
        assert_eq!(walked.problems(), []);
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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
        walk_piece(&root, &path("piece"), &rules, &PieceRules::none()).expect("the walk succeeds")
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
    fn a_hard_link_of_an_excluded_file_is_not_excluded() {
        let mut tree = Tree::new();
        tree.file("piece/sub/keep.txt", "x");
        tree.hard_link("piece/sub/keep.txt", "piece/sub/linked.txt");
        let walked = walk_excluding(&tree, &["piece/sub/keep.txt"]);
        assert_eq!(
            walked.problems(),
            [Problem::Excluded {
                path: path("sub/keep.txt")
            }]
        );
        assert!(paths(&walked).contains(&"sub/linked.txt".to_string()));
    }

    #[test]
    fn directories_below_the_depth_cap_make_the_piece_unreadable_and_are_not_entered() {
        let mut tree = Tree::new();
        tree.file("piece/a/b/c/d/f", "x");
        let hooks = Staged {
            depth: Some(2),
            ..Staged::default()
        };
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
        assert_eq!(paths(&walked), ["", "a", "a/b", "a/b/c"]);
        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: path("a/b/c")
            }]
        );
    }

    #[test]
    fn the_depth_cap_applies_inside_an_ignored_directory() {
        let mut tree = Tree::new();
        tree.file("piece/skip/a/b/c/f", "x");
        let hooks = Staged {
            depth: Some(2),
            ..Staged::default()
        };
        let walked = walk_ruled_with(&tree, &["skip/"], &[], &hooks).expect("walks");
        assert_eq!(class_of(&walked, "skip/a/b"), Class::Ignored);
        assert_eq!(
            walked.problems(),
            [Problem::Unreadable {
                first: path("skip/a/b")
            }]
        );
    }

    #[test]
    fn a_broken_rule_file_below_a_timeless_directory_refuses_the_piece() {
        let mut tree = Tree::new();
        tree.file("piece/sub/.antiqignore", "bad\\\n");
        tree.file("piece/sub/data", "x");
        let walked = walk_ruled(&tree, &[], &["sub/"]);
        assert_eq!(class_of(&walked, "sub/data"), Class::Timeless);
        assert_eq!(
            walked.problems(),
            [Problem::RuleFile {
                path: path("sub/.antiqignore"),
                line: Some(1)
            }]
        );
    }

    #[test]
    fn a_marker_name_that_is_not_a_file_is_refused_although_a_rule_ignores_it() {
        let mut tree = Tree::new();
        tree.fifo("piece/.antiq-pin");
        tree.dir("piece/.antiq-restored");
        let walked = walk_ruled(&tree, &[".*"], &[]);
        assert_eq!(class_of(&walked, ".antiq-pin"), Class::Ignored);
        assert_eq!(
            walked.problems(),
            [
                marker_problem(".antiq-pin"),
                marker_problem(".antiq-restored")
            ]
        );
    }

    #[test]
    fn a_parent_that_cannot_be_checked_is_a_locked_parent_not_an_unreadable_top() {
        let mut tree = Tree::new();
        tree.file("area/piece/f", "x");
        let hooks = Staged {
            faults: vec![(path("area"), Call::Stat, Errno::IO)],
            ..Staged::default()
        };
        let walked = walk_with(&tree, "area/piece", &PieceRules::none(), &hooks).expect("walks");
        assert_eq!(
            walked.problems(),
            [Problem::LockedParent { path: path("area") }]
        );
    }

    #[test]
    fn running_out_of_descriptors_while_checking_the_parent_ends_the_run() {
        let mut tree = Tree::new();
        tree.file("area/piece/f", "x");
        let hooks = Staged {
            faults: vec![(path("area"), Call::Stat, Errno::MFILE)],
            ..Staged::default()
        };
        let error =
            walk_with(&tree, "area/piece", &PieceRules::none(), &hooks).expect_err("the run fails");
        assert!(matches!(error, WalkError::OutOfDescriptors { .. }));
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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
        let walked =
            walk_with(&tree, "piece", &PieceRules::none(), &hooks).expect("the walk succeeds");
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

        let walked = walk_piece(&root, &path("piece"), &no_selection(), &PieceRules::none())
            .expect("the walk succeeds");

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
            let error = walk_with(&tree, "piece", &PieceRules::none(), &hooks)
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
                let walked = walk_with(&tree, "piece", &PieceRules::none(), &hooks)
                    .expect("the walk succeeds");
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
    /// Answers every question of the walk with a fixed value.
    struct Fixed;

    impl Hooks for Fixed {
        fn device(&self, _path: &RelPath, _reported: u64) -> u64 {
            7
        }

        fn mount_root(&self, _path: &RelPath, _reported: bool) -> bool {
            true
        }

        fn prefix_mount_root(&self, _prefix: &RelPath, _reported: bool) -> bool {
            true
        }

        fn fault(&self, _path: &RelPath, _call: Call) -> Option<io::Error> {
            Some(io::Error::other("fixed"))
        }

        fn owner(&self, _path: &RelPath, _reported: u32) -> u32 {
            8
        }

        fn listed_kind(&self, _path: &RelPath, _reported: EntryKind) -> EntryKind {
            EntryKind::Socket
        }

        fn max_depth(&self) -> usize {
            9
        }
    }

    #[test]
    fn a_kept_folder_asks_the_hooks_below_it_and_answers_for_its_top_mount_itself() {
        let kept = KeptFolder(&Fixed);
        let top = RelPath::root();
        let below = RelPath::parse("sub").expect("a path");

        assert!(!kept.mount_root(&top, true));
        assert!(kept.mount_root(&below, false));
        assert_eq!(kept.device(&below, 1), 7);
        assert!(kept.prefix_mount_root(&below, false));
        assert!(kept.fault(&below, Call::Stat).is_some());
        assert_eq!(kept.owner(&below, 1), 8);
        assert_eq!(kept.listed_kind(&below, EntryKind::File), EntryKind::Socket);
        assert_eq!(kept.max_depth(), 9);
    }

    #[test]
    fn only_a_locked_entry_and_a_locked_parent_block_a_removal_alone() {
        let path = RelPath::parse("a").expect("a path");

        assert!(only_blocks_removal(&Problem::Locked { path: path.clone() }));
        assert!(only_blocks_removal(&Problem::LockedParent {
            path: path.clone()
        }));
        assert!(!only_blocks_removal(&Problem::MountPoint { other: path }));
    }
}
