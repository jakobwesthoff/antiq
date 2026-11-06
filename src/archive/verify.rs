// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Verifying a package against a folder or a listing.

use super::hashing::HashingReader;
use super::read::read_package;
use super::tar_write::{DirectoryStack, listing_line};
use super::{PackError, Verified, WalkRules};
use crate::filesystem::tree::{TreeError, TreeRoot};
use crate::listing::{LinkTarget, ListingEntry, ListingType};
use crate::relpath::RelPath;
use crate::select::Rules;
use crate::tree::{EntryKind, Problem, TreeEntry};
use crate::walk::{Hooks, KeptFolder, MAX_DEPTH, RealFs, only_blocks_removal, walk_piece_with};
use rustix::io::Errno;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// What differs between a package or a listing and a folder at one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Type,
    Size,
    Mtime,
    Mode,
    Link,
    Content,
}

/// A way a folder differs from a package or a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Difference {
    /// The package or listing holds the path, the folder does not.
    Removed(RelPath),
    /// The folder holds the path, the package or listing does not.
    Added(RelPath),
    /// Both hold the path and these fields differ.
    Changed { path: RelPath, fields: Vec<Field> },
    /// The file was replaced, resized or touched while it was read.
    ChangedWhileRead(RelPath),
}

impl Difference {
    pub fn path(&self) -> &RelPath {
        match self {
            Difference::Removed(path)
            | Difference::Added(path)
            | Difference::ChangedWhileRead(path)
            | Difference::Changed { path, .. } => path,
        }
    }
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Difference::Removed(path) => write!(f, "`{path}` is gone from the folder"),
            Difference::Added(path) => write!(f, "`{path}` is new in the folder"),
            Difference::ChangedWhileRead(path) => write!(f, "`{path}` changed while it was read"),
            Difference::Changed { path, fields } => {
                let names: Vec<String> = fields.iter().map(|field| format!("{field:?}")).collect();
                write!(f, "`{path}` differs in {}", names.join(", "))
            }
        }
    }
}

/// The count of differences and the first of them, for an error message.
pub(super) fn describe(differences: &[Difference]) -> String {
    let count = differences.len();
    let noun = if count == 1 {
        "difference"
    } else {
        "differences"
    };
    differences
        .first()
        .map(|first| format!("{count} {noun}, the first: {first}"))
        .unwrap_or_default()
}

/// Like [`describe`], for the members of an archive compared with a
/// listing, where `Removed` is a listed path the archive lacks and `Added`
/// an archived path the listing lacks.
pub(super) fn describe_against_listing(differences: &[Difference]) -> String {
    let count = differences.len();
    let noun = if count == 1 {
        "difference"
    } else {
        "differences"
    };
    differences
        .first()
        .map(|first| {
            let first = match first {
                Difference::Removed(path) => format!("`{path}` is listed but not in the archive"),
                Difference::Added(path) => format!("`{path}` is in the archive but not listed"),
                other => other.to_string(),
            };
            format!("{count} {noun}, the first: {first}")
        })
        .unwrap_or_default()
}

/// What the walk's `lstat` said about one entry of the folder. The walk
/// that comes after the folder has been moved compares against these.
///
/// The top entry's `ctime_ns` changes whenever antiq moves the piece, as
/// the move adds owner write to the directory and restores its mode, so a
/// consumer compares the top's `kind`, `inode` and `mtime_ns` only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LstatRecord {
    /// Relative to the folder; the folder itself is the root path.
    pub path: RelPath,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub inode: u64,
}

impl LstatRecord {
    /// Whether `other` describes the same entry in the same state, ctime
    /// aside.
    fn same_but_for_ctime(&self, other: &LstatRecord) -> bool {
        (self.kind, self.size, self.mtime_ns, self.inode)
            == (other.kind, other.size, other.mtime_ns, other.inode)
    }

    fn of(entry: &TreeEntry) -> LstatRecord {
        LstatRecord {
            path: entry.path.clone(),
            kind: entry.kind,
            size: entry.size,
            mtime_ns: entry.mtime_ns,
            ctime_ns: entry.ctime_ns,
            inode: entry.inode,
        }
    }
}

/// The outcome of comparing a folder with a listing.
///
/// ```compile_fail
/// // The differences are only read: a caller that could edit them could
/// // hide a difference from the code that acts on them.
/// fn forge(mut compared: antiq::archive::verify::Compared) {
///     compared.differences.clear();
/// }
/// ```
#[derive(Debug)]
pub struct Compared {
    differences: Vec<Difference>,
    record: Vec<LstatRecord>,
}

impl Compared {
    /// In path order; empty when the folder equals the listing.
    pub fn differences(&self) -> &[Difference] {
        &self.differences
    }

    /// The `lstat` of every entry of the folder, in path order.
    pub fn record(&self) -> &[LstatRecord] {
        &self.record
    }
}

/// Compares `package` with the tree below `root`.
///
/// The package must be one antiq wrote in this run or one that passed the
/// structure check of `extract`, as it is read with `read_package`.
pub(crate) fn verify_strict(
    package: &Path,
    root: &TreeRoot,
    rules: &WalkRules,
    hooks: &dyn Hooks,
) -> Result<Verified, PackError> {
    verify_strict_observed(package, root, rules, hooks, &mut |_, _| {})
}

fn verify_strict_observed(
    package: &Path,
    root: &TreeRoot,
    rules: &WalkRules,
    hooks: &dyn Hooks,
    observe: &mut dyn FnMut(Stage, &RelPath),
) -> Result<Verified, PackError> {
    // The stream is read to its end first, so a damaged package never
    // reaches the folder.
    let package = read_package(package)?;
    let (folder, moved) = read_folder_granted(root, rules, hooks, observe)?;
    let mut differences = differences_from(&package.entries, &folder, rules.staged);
    differences.extend(moved.into_iter().map(Difference::ChangedWhileRead));
    // The container is the first member and has the top directory's mode
    // and time. A staging root keeps the mode its maker gave it until the
    // piece is moved into place, but gets the time from the package.
    let top = listing_line(&folder.top, ListingType::Dir, 0, None, None);
    let (field, wrong) = if rules.staged {
        (Field::Mtime, package.container.mtime != top.mtime)
    } else {
        (Field::Mode, package.container.mode != top.mode)
    };
    if wrong {
        differences.push(Difference::Changed {
            path: RelPath::root(),
            fields: vec![field],
        });
    }
    differences.sort_by(|a, b| a.path().cmp(b.path()));
    if differences.is_empty() {
        Ok(Verified {
            record: folder.record,
            listing: package.entries,
        })
    } else {
        Err(PackError::Differs(differences))
    }
}

/// Reads the folder; in a staging tree the directories whose mode keeps
/// the owner out are opened for the read and get their mode back. Returns
/// the folder and the directories that were not the same after their mode
/// came back.
fn read_folder_granted(
    root: &TreeRoot,
    rules: &WalkRules,
    hooks: &dyn Hooks,
    observe: &mut dyn FnMut(Stage, &RelPath),
) -> Result<(Folder, Vec<RelPath>), PackError> {
    let mut granted = Granted::new(root);
    if rules.staged {
        granted.grant();
    }
    let mut folder = read_folder(root, rules, hooks, observe, &granted.modes())?;
    let restored = granted.restore()?;
    // Restoring a mode changes the directory's ctime, so the record takes
    // the ctime from the directory as it is left. Anything else that
    // differs from what the walk recorded changed under the read.
    let mut moved = Vec::new();
    for (path, now) in restored {
        let index = folder
            .record
            .binary_search_by(|recorded| recorded.path.cmp(&path))
            .expect("the walk recorded every directory it read");
        let walked = &mut folder.record[index];
        match now.map(|now| LstatRecord::of(&now)) {
            Some(now) if now.same_but_for_ctime(walked) => walked.ctime_ns = now.ctime_ns,
            _ => moved.push(path),
        }
    }
    Ok((folder, moved))
}

/// Directories of a staging tree that were given owner read and search for
/// the duration of a read. The extraction's final pass sets the modes the
/// package stores, and a mode without owner access hides the directory's
/// children from everyone but root.
struct Granted<'a> {
    root: &'a TreeRoot,
    /// The directories whose mode was changed, in the order of the change,
    /// with the mode they had.
    changed: Vec<(RelPath, u32)>,
}

impl<'a> Granted<'a> {
    fn new(root: &'a TreeRoot) -> Granted<'a> {
        Granted {
            root,
            changed: Vec::new(),
        }
    }

    /// Gives every directory below the root that lacks owner read or
    /// search both, from the top down. A mode is set on the directory
    /// itself and never through a symlink.
    ///
    /// Whatever cannot be granted or listed stays as it is: the walk that
    /// follows meets it and reports the piece as unreadable.
    fn grant(&mut self) {
        let mut pending = vec![(RelPath::root(), 0usize)];
        while let Some((directory, depth)) = pending.pop() {
            if let Ok(entry) = self.root.lstat(&directory)
                && entry.kind == EntryKind::Directory
            {
                let granted = entry.mode | OWNER_READ_SEARCH;
                if granted != entry.mode && self.root.set_mode(&directory, granted).is_ok() {
                    self.changed.push((directory.clone(), entry.mode));
                }
                // The walk stops at this depth and reports the piece as
                // unreadable.
                if depth < MAX_DEPTH {
                    for child in self.root.read_dir(&directory).unwrap_or_default() {
                        if matches!(child.kind, EntryKind::Directory | EntryKind::Unknown) {
                            pending.push((directory.join(&child.name), depth + 1));
                        }
                    }
                }
            }
        }
    }

    /// The modes the granted directories had.
    fn modes(&self) -> HashMap<RelPath, u32> {
        self.changed.iter().cloned().collect()
    }

    /// Puts the modes back, children first, and returns the directories it
    /// touched, each with its `lstat` taken right after its mode is back,
    /// while the directories above it still allow that; `None` where that
    /// `lstat` failed. Every mode is tried; the first failure is returned.
    #[allow(clippy::type_complexity, reason = "one pair of a path and its lstat")]
    fn restore(&mut self) -> Result<Vec<(RelPath, Option<TreeEntry>)>, PackError> {
        let mut restored = Vec::new();
        let mut failure = None;
        while let Some((directory, mode)) = self.changed.pop() {
            match self.root.set_mode(&directory, mode) {
                Ok(()) => {
                    let now = self.root.lstat(&directory).ok();
                    restored.push((directory, now));
                }
                Err(source) => {
                    failure.get_or_insert(PackError::RestoreMode {
                        path: directory,
                        mode: mode & 0o7777,
                        source,
                    });
                }
            }
        }
        failure.map_or(Ok(restored), Err)
    }
}

impl Drop for Granted<'_> {
    /// A verify that fails or panics leaves the staging as it found it.
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Owner read and search, which listing a directory and reaching a name in
/// it need.
const OWNER_READ_SEARCH: u32 = 0o500;

/// Compares the tree below `root` with `listing`: path, type, size,
/// modification time, mode, link target and content hash. Symlink modes do
/// not count, and directory modification times only in a staging tree
/// (`rules.staged`), which also gets the treatment of locked directories
/// that [`verify_strict`] gives it.
pub fn compare_with_listing(
    listing: &[ListingEntry],
    root: &TreeRoot,
    rules: &WalkRules,
) -> Result<Compared, PackError> {
    compare_with_listing_observed(listing, root, rules, &mut |_, _| {})
}

/// The points in reading a file at which a test can change the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The walk has recorded the file; nothing has opened it yet.
    AfterWalk,
    /// The file has been read to its end; its final `fstat` is still to
    /// come.
    AfterRead,
}

fn compare_with_listing_observed(
    listing: &[ListingEntry],
    root: &TreeRoot,
    rules: &WalkRules,
    observe: &mut dyn FnMut(Stage, &RelPath),
) -> Result<Compared, PackError> {
    let (folder, moved) = read_folder_granted(root, rules, &RealFs, observe)?;
    let mut differences = differences_from(listing, &folder, rules.staged);
    differences.extend(moved.into_iter().map(Difference::ChangedWhileRead));
    differences.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(Compared {
        differences,
        record: folder.record,
    })
}

/// The differences between `members` and what was read of the folder,
/// the files that changed while read among them.
fn differences_from(
    members: &[ListingEntry],
    folder: &Folder,
    directory_times: bool,
) -> Vec<Difference> {
    let mut differences = compare(members, &folder.lines, &folder.changed, directory_times);
    differences.extend(
        folder
            .changed
            .iter()
            .cloned()
            .map(Difference::ChangedWhileRead),
    );
    differences
}

/// The folder as the package of it would describe it.
struct Folder {
    /// The top directory itself.
    top: TreeEntry,
    /// The listing lines of the entries the package would hold, in path
    /// order, the top directory not among them.
    lines: Vec<ListingEntry>,
    /// Files that were not the ones the walk recorded when they were read,
    /// and so have no line.
    changed: Vec<RelPath>,
    record: Vec<LstatRecord>,
}

/// Walks the folder the way packing does and reads every file it would
/// pack.
fn read_folder(
    root: &TreeRoot,
    rules: &WalkRules,
    hooks: &dyn Hooks,
    observe: &mut dyn FnMut(Stage, &RelPath),
    modes: &HashMap<RelPath, u32>,
) -> Result<Folder, PackError> {
    let no_rules = Rules::new(&[], &[]).expect("no patterns are valid");
    let kept = KeptFolder(hooks);
    let hooks: &dyn Hooks = if rules.kept { &kept } else { hooks };
    let walked = walk_piece_with(root, &RelPath::root(), &no_rules, &rules.rules, hooks)?;
    // Packing refuses a walk with a problem, so no package holds such a
    // folder.
    let blocking = |problem: &&Problem| !(rules.kept && only_blocks_removal(problem));
    if let Some(problem) = walked.problems().iter().find(blocking) {
        return Err(PackError::TreeHasProblems(problem.clone()));
    }
    let mut lines = Vec::new();
    let mut changed = Vec::new();
    let mut record = Vec::new();
    // The first path of each file with several names, and its hash when it
    // was read without changing.
    let mut first_link: HashMap<(u64, u64), (RelPath, Option<String>)> = HashMap::new();
    let mut directories = DirectoryStack::new(root);
    // Directories that were granted access are described by the mode they
    // had before.
    let entries: Vec<TreeEntry> = walked
        .packed_entries()
        .cloned()
        .map(|mut entry| {
            if let Some(mode) = modes.get(&entry.path) {
                entry.mode = *mode;
            }
            entry
        })
        .collect();
    for entry in &entries {
        record.push(LstatRecord::of(entry));
        match entry.kind {
            EntryKind::Directory if entry.path.is_root() => {}
            EntryKind::Directory => {
                lines.push(listing_line(entry, ListingType::Dir, 0, None, None))
            }
            EntryKind::Symlink => {
                let target = entry
                    .link_target
                    .clone()
                    .expect("lstat reads the target of every symlink it reports");
                let link = Some(LinkTarget::from_bytes(target));
                lines.push(listing_line(entry, ListingType::Symlink, 0, link, None));
            }
            EntryKind::File => {
                // Packing stores the first path in byte order of a file
                // with several names and links the others to it.
                if entry.nlink > 1 {
                    let key = (entry.device, entry.inode);
                    if let Some((first, sha256)) = first_link.get(&key) {
                        // A link to a file that changed while read has no
                        // hash to compare, so it is reported with its
                        // first path.
                        let Some(sha256) = sha256 else {
                            changed.push(entry.path.clone());
                            continue;
                        };
                        let link = Some(LinkTarget::from_bytes(first.as_bytes().to_vec()));
                        let sha256 = Some(sha256.clone());
                        lines.push(listing_line(entry, ListingType::Hardlink, 0, link, sha256));
                        continue;
                    }
                }
                let read = read_file(&mut directories, entry, observe)?;
                if entry.nlink > 1 {
                    let hash = match &read {
                        Read::Hashed(sha256) => Some(sha256.clone()),
                        Read::Changed => None,
                    };
                    first_link.insert((entry.device, entry.inode), (entry.path.clone(), hash));
                }
                match read {
                    Read::Hashed(sha256) => lines.push(listing_line(
                        entry,
                        ListingType::File,
                        entry.size,
                        None,
                        Some(sha256),
                    )),
                    Read::Changed => changed.push(entry.path.clone()),
                }
            }
            _ => {}
        }
    }
    let top = entries
        .first()
        .cloned()
        .expect("a walk without problems has its top entry");
    Ok(Folder {
        top,
        lines,
        changed,
        record,
    })
}

/// How reading a file went.
enum Read {
    Hashed(String),
    /// The file is no longer the one the walk recorded.
    Changed,
}

/// Reads the file `entry` names and hashes it, and checks that it was the
/// file the walk recorded from the first byte to the last: the handle
/// opened has the recorded inode, size, modification time and ctime, and
/// still has them once the file is read.
fn read_file(
    directories: &mut DirectoryStack<'_>,
    entry: &TreeEntry,
    observe: &mut dyn FnMut(Stage, &RelPath),
) -> Result<Read, PackError> {
    let read = |source| PackError::Read {
        path: entry.path.clone(),
        source,
    };
    observe(Stage::AfterWalk, &entry.path);
    let mut file = match directories.open_file(entry) {
        Ok(file) => file,
        Err(error) if is_replaced(&error) => return Ok(Read::Changed),
        Err(error) => return Err(read(error)),
    };
    if !is_recorded(&file.metadata().map_err(read)?, entry) {
        return Ok(Read::Changed);
    }
    let mut reader = HashingReader::new(&mut file);
    io::copy(&mut reader, &mut io::sink()).map_err(read)?;
    let (length, sha256) = (reader.bytes_read(), reader.sha256());
    observe(Stage::AfterRead, &entry.path);
    if length != entry.size || !is_recorded(&file.metadata().map_err(read)?, entry) {
        return Ok(Read::Changed);
    }
    Ok(Read::Hashed(sha256))
}

/// Whether opening failed because the entry is not what the walk saw:
/// gone, a symlink now, or another kind of file. A directory above it that
/// became a symlink gives `ELOOP` where the open follows nothing, and
/// `ENOTDIR` where the directory is opened as a path.
fn is_replaced(error: &io::Error) -> bool {
    let errno = error.raw_os_error();
    error.kind() == io::ErrorKind::NotFound
        || errno == Some(Errno::LOOP.raw_os_error())
        || errno == Some(Errno::NOTDIR.raw_os_error())
        || matches!(TreeError::of(error), Some(TreeError::NotRegular(_)))
}

/// Whether the open file is the entry the walk recorded.
fn is_recorded(file: &Metadata, entry: &TreeEntry) -> bool {
    let nanoseconds =
        |seconds: i64, nanos: i64| i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    file.ino() == entry.inode
        && file.len() == entry.size
        && nanoseconds(file.mtime(), file.mtime_nsec()) == entry.mtime_ns
        && nanoseconds(file.ctime(), file.ctime_nsec()) == entry.ctime_ns
}

/// The differences between two sets of lines, in path order. `left` is
/// the package or listing, `right` the folder.
pub(super) fn compare(
    left: &[ListingEntry],
    right: &[ListingEntry],
    skip: &[RelPath],
    directory_times: bool,
) -> Vec<Difference> {
    type Pair<'a> = (Option<&'a ListingEntry>, Option<&'a ListingEntry>);
    let mut pairs: BTreeMap<&RelPath, Pair<'_>> = BTreeMap::new();
    for line in left {
        pairs.entry(&line.path).or_default().0 = Some(line);
    }
    for line in right {
        pairs.entry(&line.path).or_default().1 = Some(line);
    }
    let mut differences = Vec::new();
    for (path, pair) in pairs {
        if skip.contains(path) {
            continue;
        }
        match pair {
            (Some(then), Some(now)) => {
                let fields = differing(then, now, directory_times);
                if !fields.is_empty() {
                    differences.push(Difference::Changed {
                        path: path.clone(),
                        fields,
                    });
                }
            }
            (Some(_), None) => differences.push(Difference::Removed(path.clone())),
            (None, _) => differences.push(Difference::Added(path.clone())),
        }
    }
    differences
}

/// The fields in which two lines of one path differ. A different type
/// stands for itself: the other fields have no meaning across types.
fn differing(then: &ListingEntry, now: &ListingEntry, directory_times: bool) -> Vec<Field> {
    if then.kind != now.kind {
        return vec![Field::Type];
    }
    let mut fields = Vec::new();
    if then.size != now.size {
        fields.push(Field::Size);
    }
    if then.mtime != now.mtime && (directory_times || then.kind != ListingType::Dir) {
        fields.push(Field::Mtime);
    }
    // Linux cannot set the mode of a symlink, so a mode compared here
    // would differ between the platforms for the same package.
    if then.mode != now.mode && then.kind != ListingType::Symlink {
        fields.push(Field::Mode);
    }
    if then.link != now.link {
        fields.push(Field::Link);
    }
    if then.sha256 != now.sha256 {
        fields.push(Field::Content);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::PieceRules;
    use crate::testing::package::{RawMember, compress, on_disk, pack_piece, raw_tar};
    use crate::testing::tree::Tree;
    use crate::tree::Problem;
    use filetime::FileTime;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    /// A piece `proj` and the listing its package has.
    fn packed(tree: &Tree) -> Vec<ListingEntry> {
        pack_piece(tree, "proj").packed.listing
    }

    fn compare_dir(dir: &Path, listing: &[ListingEntry]) -> Vec<Difference> {
        let root = TreeRoot::open(dir).expect("the folder opens");
        compare_with_listing(listing, &root, &WalkRules::default())
            .expect("the comparison runs")
            .differences()
            .to_vec()
    }

    fn compare(tree: &Tree, listing: &[ListingEntry]) -> Vec<Difference> {
        compare_dir(&tree.path("proj"), listing)
    }

    fn mixed() -> Tree {
        let mut tree = Tree::new();
        tree.file_with("proj/src/main.rs", "fn main() {}", 0o600, 1_000_000_000);
        tree.file_with("proj/notes.txt", "hello", 0o644, 1_000_000_001);
        tree.symlink("proj/src/link", "main.rs");
        tree.dir("proj/empty");
        tree
    }

    fn changed(text: &str, fields: &[Field]) -> Difference {
        Difference::Changed {
            path: path(text),
            fields: fields.to_vec(),
        }
    }

    #[test]
    fn a_folder_equals_the_listing_it_was_packed_to() {
        let tree = mixed();
        assert_eq!(compare(&tree, &packed(&tree)), []);
    }

    #[test]
    fn a_file_edited_to_the_same_size_and_time_differs_in_content() {
        let tree = mixed();
        let listing = packed(&tree);
        let file = tree.path("proj/notes.txt");
        std::fs::write(&file, "HELLO").expect("the file is written");
        filetime::set_file_mtime(&file, FileTime::from_unix_time(1_000_000_001, 0))
            .expect("the time is set");
        assert_eq!(
            compare(&tree, &listing),
            [changed("notes.txt", &[Field::Content])]
        );
    }

    #[test]
    fn a_deleted_file_is_removed() {
        let tree = mixed();
        let listing = packed(&tree);
        std::fs::remove_file(tree.path("proj/notes.txt")).expect("the file is removed");
        assert_eq!(
            compare(&tree, &listing),
            [Difference::Removed(path("notes.txt"))]
        );
    }

    #[test]
    fn a_new_file_is_added() {
        let mut tree = mixed();
        let listing = packed(&tree);
        tree.file("proj/src/new.rs", "x");
        assert_eq!(
            compare(&tree, &listing),
            [Difference::Added(path("src/new.rs"))]
        );
    }

    #[test]
    fn a_changed_modification_time_differs_in_mtime() {
        let tree = mixed();
        let listing = packed(&tree);
        filetime::set_file_mtime(
            tree.path("proj/notes.txt"),
            FileTime::from_unix_time(1_000_000_002, 0),
        )
        .expect("the time is set");
        assert_eq!(
            compare(&tree, &listing),
            [changed("notes.txt", &[Field::Mtime])]
        );
    }

    #[test]
    fn a_changed_mode_differs_in_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tree = mixed();
        let listing = packed(&tree);
        std::fs::set_permissions(
            tree.path("proj/notes.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("the mode is set");
        assert_eq!(
            compare(&tree, &listing),
            [changed("notes.txt", &[Field::Mode])]
        );
    }

    #[test]
    fn a_repointed_symlink_differs_in_link() {
        let tree = mixed();
        let listing = packed(&tree);
        std::fs::remove_file(tree.path("proj/src/link")).expect("the link is removed");
        std::os::unix::fs::symlink("other.rs", tree.path("proj/src/link")).expect("a new link");
        // The new link has a new modification time of its own.
        let fields = compare(&tree, &listing);
        assert!(
            matches!(&fields[..], [Difference::Changed { path, fields }]
                if path.to_string() == "src/link" && fields.contains(&Field::Link)),
            "{fields:?}"
        );
    }

    #[test]
    fn a_file_replaced_by_a_directory_differs_in_type_only() {
        let tree = mixed();
        let listing = packed(&tree);
        std::fs::remove_file(tree.path("proj/notes.txt")).expect("the file is removed");
        std::fs::create_dir(tree.path("proj/notes.txt")).expect("the directory is made");
        assert_eq!(
            compare(&tree, &listing),
            [changed("notes.txt", &[Field::Type])]
        );
    }

    #[test]
    fn a_grown_file_differs_in_size() {
        let tree = mixed();
        let listing = packed(&tree);
        let file = tree.path("proj/notes.txt");
        std::fs::write(&file, "hello world").expect("the file is written");
        filetime::set_file_mtime(&file, FileTime::from_unix_time(1_000_000_001, 0))
            .expect("the time is set");
        assert_eq!(
            compare(&tree, &listing),
            [changed("notes.txt", &[Field::Size, Field::Content])]
        );
    }

    #[test]
    fn a_symlink_mode_does_not_count() {
        let tree = mixed();
        let mut listing = packed(&tree);
        listing
            .iter_mut()
            .find(|line| line.path == path("src/link"))
            .expect("the line exists")
            .mode = 0o700;
        assert_eq!(compare(&tree, &listing), []);
    }

    #[test]
    fn a_newer_directory_time_does_not_count() {
        let tree = mixed();
        let listing = packed(&tree);
        filetime::set_file_mtime(
            tree.path("proj/src"),
            FileTime::from_unix_time(2_000_000_000, 0),
        )
        .expect("the time is set");
        assert_eq!(compare(&tree, &listing), []);
    }

    #[test]
    fn a_listing_line_without_a_hash_differs_in_content() {
        let tree = mixed();
        let mut listing = packed(&tree);
        listing
            .iter_mut()
            .find(|line| line.path == path("notes.txt"))
            .expect("the line exists")
            .sha256 = None;
        assert_eq!(
            compare(&tree, &listing),
            [changed("notes.txt", &[Field::Content])]
        );
    }

    #[test]
    fn a_renamed_folder_still_equals_its_listing() {
        let tree = mixed();
        let listing = packed(&tree);
        std::fs::rename(tree.path("proj"), tree.path("renamed")).expect("the folder is renamed");
        assert_eq!(compare_dir(&tree.path("renamed"), &listing), []);
    }

    #[test]
    fn a_folder_past_1024_bytes_of_path_equals_its_listing() {
        use crate::testing::deep_path;
        use std::io::Write;
        let tree = Tree::new();
        let collection = TreeRoot::open(tree.root()).expect("opens");
        collection
            .create_dir(&path("proj"), 0o755)
            .expect("the piece is made");
        let piece = collection.sub(&path("proj")).expect("opens");
        let deep = deep_path(6);
        for level in 1..=6 {
            let directory: Vec<&[u8]> = deep.components().take(level).collect();
            piece
                .create_dir(
                    &RelPath::from_bytes(directory.join(&b'/')).expect("valid"),
                    0o755,
                )
                .expect("the directory is made");
        }
        piece
            .create_file(&deep.concat(&path("data.txt")), 0o644)
            .expect("the file is made")
            .write_all(b"deep")
            .expect("written");
        let listing = packed(&tree);
        assert_eq!(listing.len(), 7);
        assert_eq!(compare(&tree, &listing), []);
    }

    fn ds_store_rules() -> WalkRules {
        WalkRules {
            rules: PieceRules::new(&[".DS_Store".to_string()], &[]).expect("valid rules"),
            staged: false,
            ..WalkRules::default()
        }
    }

    fn compare_with_rules(
        tree: &Tree,
        listing: &[ListingEntry],
        rules: &WalkRules,
    ) -> Vec<Difference> {
        let root = TreeRoot::open(&tree.path("proj")).expect("the folder opens");
        compare_with_listing(listing, &root, rules)
            .expect("the comparison runs")
            .differences()
            .to_vec()
    }

    #[test]
    fn ignored_entries_on_disk_do_not_count() {
        use crate::testing::package::pack_piece_with;
        let mut tree = mixed();
        let listing = pack_piece_with(&tree, "proj", &ds_store_rules().rules)
            .packed
            .listing;
        tree.file("proj/src/.DS_Store", "x");
        tree.file("proj/.DS_Store/inside", "x");
        assert_eq!(compare_with_rules(&tree, &listing, &ds_store_rules()), []);
    }

    #[test]
    fn an_entry_the_listing_holds_and_the_rules_ignore_is_missing() {
        let mut tree = mixed();
        tree.file("proj/src/.DS_Store", "x");
        let listing = packed(&tree);
        assert_eq!(
            compare_with_rules(&tree, &listing, &ds_store_rules()),
            [Difference::Removed(path("src/.DS_Store"))]
        );
    }

    #[test]
    fn hard_links_equal_the_listing_they_were_packed_to() {
        let mut tree = mixed();
        tree.hard_link("proj/notes.txt", "proj/src/notes-copy.txt");
        let listing = packed(&tree);
        assert_eq!(compare(&tree, &listing), []);
    }

    #[test]
    fn a_file_the_listing_holds_twice_but_now_is_a_hard_link_differs_in_type() {
        let mut tree = mixed();
        tree.file_with("proj/src/copy.txt", "hello", 0o644, 1_000_000_001);
        let listing = packed(&tree);
        std::fs::remove_file(tree.path("proj/src/copy.txt")).expect("removed");
        tree.hard_link("proj/notes.txt", "proj/src/copy.txt");
        assert_eq!(
            compare(&tree, &listing),
            [changed("src/copy.txt", &[Field::Type])]
        );
    }

    #[test]
    fn a_hard_link_the_listing_holds_but_now_is_a_copy_differs_in_type() {
        let mut tree = mixed();
        tree.hard_link("proj/notes.txt", "proj/src/copy.txt");
        let listing = packed(&tree);
        std::fs::remove_file(tree.path("proj/src/copy.txt")).expect("removed");
        tree.file_with("proj/src/copy.txt", "hello", 0o644, 1_000_000_001);
        assert_eq!(
            compare(&tree, &listing),
            [changed("src/copy.txt", &[Field::Type])]
        );
    }

    #[test]
    fn removing_the_first_name_of_a_hard_link_reports_both_names() {
        let mut tree = mixed();
        tree.hard_link("proj/notes.txt", "proj/src/copy.txt");
        let listing = packed(&tree);
        std::fs::remove_file(tree.path("proj/notes.txt")).expect("removed");
        assert_eq!(
            compare(&tree, &listing),
            [
                Difference::Removed(path("notes.txt")),
                changed("src/copy.txt", &[Field::Type])
            ]
        );
    }

    #[test]
    fn a_fifo_on_disk_is_not_added() {
        let mut tree = mixed();
        let listing = packed(&tree);
        tree.fifo("proj/queue");
        assert_eq!(compare(&tree, &listing), []);
    }

    #[test]
    fn a_fifo_where_the_listing_has_a_file_is_removed() {
        let mut tree = mixed();
        let listing = packed(&tree);
        std::fs::remove_file(tree.path("proj/notes.txt")).expect("removed");
        tree.fifo("proj/notes.txt");
        assert_eq!(
            compare(&tree, &listing),
            [Difference::Removed(path("notes.txt"))]
        );
    }

    #[test]
    fn a_folder_with_an_unreadable_directory_is_refused() {
        let mut tree = mixed();
        let listing = packed(&tree);
        let locked = tree.unreadable_dir("proj/locked");
        assert!(
            std::fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let error = compare_with_listing(&listing, &root, &WalkRules::default())
            .expect_err("a walk with a problem is refused");
        assert!(
            matches!(&error, PackError::TreeHasProblems(Problem::Unreadable { first })
                if first.to_string() == "locked"),
            "{error:?}"
        );
    }

    #[test]
    fn the_record_holds_every_entry_as_lstat_reports_it() {
        let mut tree = mixed();
        tree.fifo("proj/queue");
        let listing = packed(&tree);
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let compared = compare_with_listing(&listing, &root, &WalkRules::default())
            .expect("the comparison runs");
        let paths: Vec<String> = compared
            .record()
            .iter()
            .map(|r| r.path.to_string())
            .collect();
        assert_eq!(
            paths,
            [
                "",
                "empty",
                "notes.txt",
                "queue",
                "src",
                "src/link",
                "src/main.rs"
            ]
        );
        for recorded in compared.record() {
            let seen = root.lstat(&recorded.path).expect("the entry exists");
            assert_eq!(
                (
                    recorded.kind,
                    recorded.size,
                    recorded.mtime_ns,
                    recorded.ctime_ns,
                    recorded.inode
                ),
                (
                    seen.kind,
                    seen.size,
                    seen.mtime_ns,
                    seen.ctime_ns,
                    seen.inode
                ),
                "{}",
                recorded.path
            );
        }
    }

    #[test]
    fn a_comparison_lists_no_differences_for_an_equal_folder_and_the_differences_otherwise() {
        let tree = mixed();
        let listing = packed(&tree);
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let equal = compare_with_listing(&listing, &root, &WalkRules::default()).expect("runs");
        assert_eq!(equal.differences(), []);
        assert_eq!(equal.record().len(), 6);

        std::fs::remove_file(tree.path("proj/notes.txt")).expect("removed");
        let differing = compare_with_listing(&listing, &root, &WalkRules::default()).expect("runs");
        assert_eq!(
            differing.differences(),
            [Difference::Removed(path("notes.txt"))]
        );
    }

    /// Compares while `change` runs at the chosen stage for `path`.
    fn compare_changing(
        tree: &Tree,
        listing: &[ListingEntry],
        at: (Stage, &str),
        change: impl Fn(&Tree),
    ) -> Result<Compared, PackError> {
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let mut observe = |stage: Stage, seen: &RelPath| {
            if (stage, seen.to_string().as_str()) == at {
                change(tree);
            }
        };
        compare_with_listing_observed(listing, &root, &WalkRules::default(), &mut observe)
    }

    fn only_changed_while_read(compared: Compared, expected: &str) {
        assert_eq!(
            compared.differences(),
            [Difference::ChangedWhileRead(path(expected))]
        );
    }

    #[test]
    fn a_file_changed_between_the_walk_and_its_read_is_changed_while_read() {
        let tree = mixed();
        let listing = packed(&tree);
        let compared = compare_changing(&tree, &listing, (Stage::AfterWalk, "notes.txt"), |t| {
            std::fs::write(t.path("proj/notes.txt"), "hello again").expect("written");
        })
        .expect("runs");
        only_changed_while_read(compared, "notes.txt");
    }

    #[test]
    fn a_hard_link_whose_first_path_changed_while_read_is_reported_with_it() {
        let mut tree = mixed();
        tree.hard_link("proj/notes.txt", "proj/src/notes-copy.txt");
        let listing = packed(&tree);
        let compared = compare_changing(&tree, &listing, (Stage::AfterWalk, "notes.txt"), |t| {
            std::fs::write(t.path("proj/notes.txt"), "hello again").expect("written");
        })
        .expect("runs");
        assert_eq!(
            compared.differences(),
            [
                Difference::ChangedWhileRead(path("notes.txt")),
                Difference::ChangedWhileRead(path("src/notes-copy.txt")),
            ]
        );
    }

    #[test]
    fn a_file_changed_after_its_read_is_changed_while_read() {
        let tree = mixed();
        let listing = packed(&tree);
        let compared = compare_changing(&tree, &listing, (Stage::AfterRead, "notes.txt"), |t| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(t.path("proj/notes.txt"))
                .expect("opens");
            file.write_all(b"!").expect("written");
        })
        .expect("runs");
        only_changed_while_read(compared, "notes.txt");
    }

    #[test]
    fn differences_of_every_kind_come_in_path_order() {
        use std::os::unix::fs::PermissionsExt;
        let mut tree = mixed();
        tree.file_with("proj/a.txt", "a", 0o644, 1_000_000_001);
        let listing = packed(&tree);
        std::fs::set_permissions(
            tree.path("proj/a.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("the mode is set");
        tree.file("proj/zzz", "new");
        let compared = compare_changing(&tree, &listing, (Stage::AfterWalk, "notes.txt"), |t| {
            std::fs::write(t.path("proj/notes.txt"), "hello again").expect("written");
        })
        .expect("runs");
        assert_eq!(
            compared.differences(),
            [
                changed("a.txt", &[Field::Mode]),
                Difference::ChangedWhileRead(path("notes.txt")),
                Difference::Added(path("zzz")),
            ]
        );
    }

    #[test]
    fn a_file_touched_after_its_read_is_changed_while_read() {
        let tree = mixed();
        let listing = packed(&tree);
        let compared = compare_changing(&tree, &listing, (Stage::AfterRead, "notes.txt"), |t| {
            filetime::set_file_mtime(
                t.path("proj/notes.txt"),
                FileTime::from_unix_time(1_000_000_009, 0),
            )
            .expect("the time is set");
        })
        .expect("runs");
        only_changed_while_read(compared, "notes.txt");
    }

    #[test]
    fn a_file_replaced_by_a_fifo_between_the_walk_and_its_read_is_changed_while_read() {
        let tree = mixed();
        let listing = packed(&tree);
        let compared = compare_changing(&tree, &listing, (Stage::AfterWalk, "notes.txt"), |t| {
            std::fs::remove_file(t.path("proj/notes.txt")).expect("removed");
            let status = std::process::Command::new("mkfifo")
                .arg(t.path("proj/notes.txt"))
                .status()
                .expect("mkfifo runs");
            assert!(status.success());
        })
        .expect("runs");
        only_changed_while_read(compared, "notes.txt");
    }

    #[test]
    fn a_file_that_vanished_between_the_walk_and_its_read_is_changed_while_read() {
        let tree = mixed();
        let listing = packed(&tree);
        let compared = compare_changing(&tree, &listing, (Stage::AfterWalk, "notes.txt"), |t| {
            std::fs::remove_file(t.path("proj/notes.txt")).expect("removed");
        })
        .expect("runs");
        only_changed_while_read(compared, "notes.txt");
    }

    #[test]
    fn a_directory_replaced_by_a_symlink_before_its_file_is_read_is_changed_while_read() {
        let tree = mixed();
        let listing = packed(&tree);
        let compared = compare_changing(&tree, &listing, (Stage::AfterWalk, "src/main.rs"), |t| {
            std::fs::rename(t.path("proj/src"), t.path("proj/moved")).expect("moved");
            std::os::unix::fs::symlink("moved", t.path("proj/src")).expect("linked");
        })
        .expect("runs");
        only_changed_while_read(compared, "src/main.rs");
    }

    #[test]
    fn a_file_the_owner_cannot_read_fails_the_comparison_naming_it() {
        use std::os::unix::fs::PermissionsExt;
        let tree = mixed();
        let listing = packed(&tree);
        let file = tree.path("proj/notes.txt");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let unreadable = std::fs::File::open(&file).is_err();
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let result = compare_with_listing(&listing, &root, &WalkRules::default());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(unreadable, "the test needs a user that mode 000 locks out");
        let error = result.expect_err("the file cannot be read");
        assert!(
            matches!(&error, PackError::Read { path, .. } if path.to_string() == "notes.txt"),
            "{error:?}"
        );
    }

    // ---- the strict verify ----

    fn verify_with(tree: &Tree, package: &[u8], rules: &WalkRules) -> Result<Verified, PackError> {
        let (_dir, file) = on_disk(package);
        let root = TreeRoot::open(&tree.path("proj")).expect("the folder opens");
        verify_strict(&file, &root, rules, &RealFs)
    }

    fn verify(tree: &Tree, package: &[u8]) -> Result<Verified, PackError> {
        verify_with(tree, package, &WalkRules::default())
    }

    /// The differences a strict verify of the tree against the package
    /// reports.
    fn differences(tree: &Tree, package: &[u8]) -> Vec<Difference> {
        match verify(tree, package).expect_err("the folder differs") {
            PackError::Differs(differences) => differences,
            other => panic!("expected Differs, got {other:?}"),
        }
    }

    #[test]
    fn a_fresh_pack_verifies() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        verify(&tree, &packaged.bytes).expect("the folder equals its package");
    }

    #[test]
    fn an_edited_file_is_reported_with_its_content() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        let file = tree.path("proj/notes.txt");
        std::fs::write(&file, "HELLO").expect("the file is written");
        filetime::set_file_mtime(&file, FileTime::from_unix_time(1_000_000_001, 0))
            .expect("the time is set");
        assert_eq!(
            differences(&tree, &packaged.bytes),
            [changed("notes.txt", &[Field::Content])]
        );
    }

    #[test]
    fn a_deleted_file_is_reported() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        std::fs::remove_file(tree.path("proj/notes.txt")).expect("removed");
        assert_eq!(
            differences(&tree, &packaged.bytes),
            [Difference::Removed(path("notes.txt"))]
        );
    }

    #[test]
    fn an_added_file_is_reported() {
        let mut tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        tree.file("proj/extra", "x");
        assert_eq!(
            differences(&tree, &packaged.bytes),
            [Difference::Added(path("extra"))]
        );
    }

    #[test]
    fn a_changed_mode_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        std::fs::set_permissions(
            tree.path("proj/notes.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("the mode is set");
        assert_eq!(
            differences(&tree, &packaged.bytes),
            [changed("notes.txt", &[Field::Mode])]
        );
    }

    #[test]
    fn a_repointed_symlink_is_reported() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        let link = tree.path("proj/src/link");
        std::fs::remove_file(&link).expect("removed");
        std::os::unix::fs::symlink("other.rs", &link).expect("a new link");
        let found = differences(&tree, &packaged.bytes);
        assert!(
            matches!(&found[..], [Difference::Changed { path, fields }]
                if path.to_string() == "src/link" && fields.contains(&Field::Link)),
            "{found:?}"
        );
    }

    #[test]
    fn a_changed_file_time_is_reported() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        filetime::set_file_mtime(
            tree.path("proj/notes.txt"),
            FileTime::from_unix_time(1_000_000_005, 0),
        )
        .expect("the time is set");
        assert_eq!(
            differences(&tree, &packaged.bytes),
            [changed("notes.txt", &[Field::Mtime])]
        );
    }

    #[test]
    fn a_truncated_stream_fails_the_integrity_check_before_the_folder_is_read() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        let cut = &packaged.bytes[..packaged.bytes.len() - 4];
        let error = verify(&tree, cut).expect_err("the stream is cut");
        assert!(matches!(error, PackError::Damaged(_)), "{error:?}");
    }

    #[test]
    fn a_newer_directory_time_on_a_users_folder_does_not_count() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        for directory in ["proj", "proj/src", "proj/empty"] {
            filetime::set_file_mtime(
                tree.path(directory),
                FileTime::from_unix_time(2_000_000_000, 0),
            )
            .expect("the time is set");
        }
        verify(&tree, &packaged.bytes).expect("directory times do not count");
    }

    #[test]
    fn ignored_entries_on_disk_are_left_out_of_a_strict_verify() {
        use crate::testing::package::pack_piece_with;
        let mut tree = mixed();
        let packaged = pack_piece_with(&tree, "proj", &ds_store_rules().rules);
        tree.file("proj/src/.DS_Store", "x");
        tree.file("proj/.DS_Store/inside", "x");
        verify_with(&tree, &packaged.bytes, &ds_store_rules())
            .expect("ignored entries are left out");
    }

    #[test]
    fn a_staged_tree_is_verified_with_no_rules() {
        let mut tree = mixed();
        tree.file("proj/.DS_Store", "x");
        tree.file("proj/node_modules/dep", "x");
        let packaged = pack_piece(&tree, "proj");
        verify_with(&tree, &packaged.bytes, &staged()).expect("a staged tree has no rules");
    }

    #[test]
    fn a_renamed_folder_verifies() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        std::fs::rename(tree.path("proj"), tree.path("renamed")).expect("renamed");
        let (_dir, file) = on_disk(&packaged.bytes);
        let root = TreeRoot::open(&tree.path("renamed")).expect("opens");
        verify_strict(&file, &root, &WalkRules::default(), &RealFs)
            .expect("the name does not count");
    }

    #[test]
    fn a_hard_link_pair_verifies_and_a_changed_link_structure_does_not() {
        let mut tree = mixed();
        tree.hard_link("proj/notes.txt", "proj/src/copy.txt");
        let linked = pack_piece(&tree, "proj");
        verify(&tree, &linked.bytes).expect("the pair matches");
        assert_eq!(linked.packed.listing.len(), 6);

        // Two files with one content become a pair: the package holds
        // them apart.
        let mut apart = mixed();
        apart.file_with("proj/src/copy.txt", "hello", 0o644, 1_000_000_001);
        let packaged = pack_piece(&apart, "proj");
        std::fs::remove_file(apart.path("proj/src/copy.txt")).expect("removed");
        apart.hard_link("proj/notes.txt", "proj/src/copy.txt");
        assert_eq!(
            differences(&apart, &packaged.bytes),
            [changed("src/copy.txt", &[Field::Type])]
        );
    }

    #[test]
    fn the_verified_listing_is_the_one_in_the_package() {
        let mut tree = mixed();
        tree.hard_link("proj/notes.txt", "proj/src/copy.txt");
        let packaged = pack_piece(&tree, "proj");
        let verified = verify(&tree, &packaged.bytes).expect("verifies");
        assert_eq!(verified.listing(), packaged.packed.listing);
    }

    #[test]
    fn the_record_of_a_verified_folder_holds_every_entry() {
        let mut tree = mixed();
        tree.fifo("proj/queue");
        let packaged = pack_piece(&tree, "proj");
        let verified = verify(&tree, &packaged.bytes).expect("verifies");
        let paths: Vec<String> = verified
            .record()
            .iter()
            .map(|r| r.path.to_string())
            .collect();
        assert_eq!(
            paths,
            [
                "",
                "empty",
                "notes.txt",
                "queue",
                "src",
                "src/link",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn a_folder_past_1024_bytes_of_path_verifies() {
        use crate::testing::deep_path;
        use std::io::Write;
        let tree = Tree::new();
        let collection = TreeRoot::open(tree.root()).expect("opens");
        collection
            .create_dir(&path("proj"), 0o755)
            .expect("the piece is made");
        let piece = collection.sub(&path("proj")).expect("opens");
        let deep = deep_path(6);
        for level in 1..=6 {
            let directory: Vec<&[u8]> = deep.components().take(level).collect();
            piece
                .create_dir(
                    &RelPath::from_bytes(directory.join(&b'/')).expect("valid"),
                    0o755,
                )
                .expect("the directory is made");
        }
        piece
            .create_file(&deep.concat(&path("data.txt")), 0o644)
            .expect("the file is made")
            .write_all(b"deep")
            .expect("written");
        let packaged = pack_piece(&tree, "proj");
        verify(&tree, &packaged.bytes).expect("verifies");
    }

    fn staged() -> WalkRules {
        WalkRules {
            staged: true,
            ..WalkRules::default()
        }
    }

    #[test]
    fn the_top_directory_mode_is_compared_on_a_users_folder() {
        use std::os::unix::fs::PermissionsExt;
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        std::fs::set_permissions(tree.path("proj"), std::fs::Permissions::from_mode(0o700))
            .expect("the mode is set");
        assert_eq!(
            differences(&tree, &packaged.bytes),
            [Difference::Changed {
                path: RelPath::root(),
                fields: vec![Field::Mode]
            }]
        );
    }

    #[test]
    fn a_staged_top_directory_with_another_mode_verifies() {
        use std::os::unix::fs::PermissionsExt;
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        std::fs::set_permissions(tree.path("proj"), std::fs::Permissions::from_mode(0o700))
            .expect("the mode is set");
        verify_with(&tree, &packaged.bytes, &staged()).expect("the mode is the caller's");
    }

    #[test]
    fn a_staged_top_directory_time_is_compared() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        filetime::set_file_mtime(
            tree.path("proj"),
            FileTime::from_unix_time(2_000_000_000, 0),
        )
        .expect("the time is set");
        let error = verify_with(&tree, &packaged.bytes, &staged()).expect_err("differs");
        assert!(
            matches!(&error, PackError::Differs(found) if found
                == &[Difference::Changed { path: RelPath::root(), fields: vec![Field::Mtime] }]),
            "{error:?}"
        );
    }

    #[test]
    fn a_staged_directory_time_is_compared() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        filetime::set_file_mtime(
            tree.path("proj/src"),
            FileTime::from_unix_time(2_000_000_000, 0),
        )
        .expect("the time is set");
        let error = verify_with(&tree, &packaged.bytes, &staged()).expect_err("differs");
        assert!(
            matches!(&error, PackError::Differs(found)
                if found == &[changed("src", &[Field::Mtime])]),
            "{error:?}"
        );
    }

    /// Restores the permissions of directories a test locked, so that the
    /// temp directory can be removed.
    struct Unlock(Vec<std::path::PathBuf>);

    impl Drop for Unlock {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            for path in &self.0 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
            }
        }
    }

    /// A staging tree `proj/a/f` whose directory `a` has `mode`, and the
    /// package that holds the same, so that only the mode makes the walk
    /// unable to read it.
    fn staging_with_locked_directory(mode: u32) -> (Tree, Vec<u8>, Unlock) {
        use std::os::unix::fs::PermissionsExt;
        let mut tree = Tree::new();
        tree.file_with("proj/a/f", "x", 0o644, 1_000_000_000);
        for directory in ["proj/a", "proj"] {
            filetime::set_file_mtime(
                tree.path(directory),
                FileTime::from_unix_time(1_000_000_000, 0),
            )
            .expect("the time is set");
        }
        let locked = tree.path("proj/a");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(mode))
            .expect("the mode is set");
        let unlock = Unlock(vec![locked.clone()]);
        // A mode without owner read cannot list; one without owner search
        // cannot reach the file inside.
        let locked_out =
            std::fs::read_dir(&locked).is_err() || std::fs::metadata(locked.join("f")).is_err();
        assert!(
            locked_out,
            "the test needs a user that mode {mode:o} locks out"
        );
        let mut inner = RawMember::dir(b"proj/a/");
        inner.mode = mode;
        let package = compress(&raw_tar(&[
            RawMember::dir(b"proj/"),
            inner,
            RawMember::file(b"proj/a/f", b"x"),
        ]));
        (tree, package, unlock)
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("the path exists")
            .permissions()
            .mode()
            & 0o7777
    }

    fn staged_directory_with_mode_is_read_and_restored(mode: u32) {
        let (tree, package, _unlock) = staging_with_locked_directory(mode);
        let verified = verify_with(&tree, &package, &staged()).expect("the staging verifies");
        assert_eq!(mode_of(&tree.path("proj/a")), mode, "the mode is back");
        let recorded = verified
            .record()
            .iter()
            .find(|entry| entry.path == path("a"))
            .expect("the directory is recorded");
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let now = root.lstat(&path("a")).expect("the directory exists");
        assert_eq!(
            recorded.ctime_ns, now.ctime_ns,
            "the record is of the final state"
        );
    }

    #[test]
    fn a_staged_directory_without_any_owner_access_is_read_and_restored() {
        staged_directory_with_mode_is_read_and_restored(0o000);
    }

    #[test]
    fn a_staged_directory_without_owner_search_is_read_and_restored() {
        staged_directory_with_mode_is_read_and_restored(0o600);
    }

    #[test]
    fn a_staged_directory_without_owner_read_is_read_and_restored() {
        staged_directory_with_mode_is_read_and_restored(0o300);
    }

    #[test]
    fn a_staged_directory_with_owner_read_only_is_read_and_restored() {
        staged_directory_with_mode_is_read_and_restored(0o400);
    }

    #[test]
    fn a_locked_directory_inside_a_locked_directory_is_read_and_restored() {
        use std::os::unix::fs::PermissionsExt;
        let mut tree = Tree::new();
        tree.file_with("proj/a/b/f", "x", 0o644, 1_000_000_000);
        for directory in ["proj/a/b", "proj/a", "proj"] {
            filetime::set_file_mtime(
                tree.path(directory),
                FileTime::from_unix_time(1_000_000_000, 0),
            )
            .expect("the time is set");
        }
        let (outer, inner) = (tree.path("proj/a"), tree.path("proj/a/b"));
        let _unlock = Unlock(vec![outer.clone(), inner.clone()]);
        for directory in [&inner, &outer] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o000))
                .expect("the mode is set");
        }
        assert!(std::fs::read_dir(&outer).is_err(), "mode 000 locks out");
        let mut a = RawMember::dir(b"proj/a/");
        a.mode = 0;
        let mut b = RawMember::dir(b"proj/a/b/");
        b.mode = 0;
        let package = compress(&raw_tar(&[
            RawMember::dir(b"proj/"),
            a,
            b,
            RawMember::file(b"proj/a/b/f", b"x"),
        ]));
        verify_with(&tree, &package, &staged()).expect("both are read");
        // The inner directory can only be looked at once the outer one lets
        // the test in.
        let outer_mode = mode_of(&outer);
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o700))
            .expect("the mode is set");
        assert_eq!((outer_mode, mode_of(&inner)), (0, 0));
    }

    #[test]
    fn a_staged_verify_that_finds_differences_still_restores_the_modes() {
        let (tree, package, _unlock) = staging_with_locked_directory(0o000);
        std::fs::write(tree.path("proj/extra"), "new").expect("written");
        let error = verify_with(&tree, &package, &staged()).expect_err("differs");
        assert!(matches!(error, PackError::Differs(_)), "{error:?}");
        assert_eq!(mode_of(&tree.path("proj/a")), 0);
    }

    #[test]
    fn a_users_folder_with_a_locked_directory_is_refused_and_left_alone() {
        let (tree, package, _unlock) = staging_with_locked_directory(0o000);
        let error = verify(&tree, &package).expect_err("the walk cannot read it");
        assert!(
            matches!(&error, PackError::TreeHasProblems(Problem::Unreadable { first })
                if first.to_string() == "a"),
            "{error:?}"
        );
        assert_eq!(mode_of(&tree.path("proj/a")), 0);
    }

    /// A staging tree `proj/a/f` whose directory `a` has `mode`, and the
    /// listing of the same tree.
    fn staging_and_listing(mode: u32) -> (Tree, Vec<ListingEntry>, Unlock) {
        let (tree, _, unlock) = staging_with_locked_directory(mode);
        let mut open = Tree::new();
        open.file_with("proj/a/f", "x", 0o644, 1_000_000_000);
        for directory in ["proj/a", "proj"] {
            filetime::set_file_mtime(
                open.path(directory),
                FileTime::from_unix_time(1_000_000_000, 0),
            )
            .expect("the time is set");
        }
        let mut listing = packed(&open);
        for line in &mut listing {
            if line.path == path("a") {
                line.mode = mode;
            }
        }
        (tree, listing, unlock)
    }

    fn compare_staged(tree: &Tree, listing: &[ListingEntry]) -> Compared {
        let root = TreeRoot::open(&tree.path("proj")).expect("the folder opens");
        compare_with_listing(listing, &root, &staged()).expect("the comparison runs")
    }

    fn staged_directory_with_mode_equals_its_listing(mode: u32) {
        let (tree, listing, _unlock) = staging_and_listing(mode);
        let compared = compare_staged(&tree, &listing);
        assert_eq!(compared.differences(), []);
        assert_eq!(mode_of(&tree.path("proj/a")), mode, "the mode is back");
        let recorded = compared
            .record()
            .iter()
            .find(|entry| entry.path == path("a"))
            .expect("the directory is recorded");
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let now = root.lstat(&path("a")).expect("the directory exists");
        assert_eq!(
            recorded.ctime_ns, now.ctime_ns,
            "the record is of the final state"
        );
    }

    #[test]
    fn a_staged_directory_without_any_owner_access_equals_its_listing() {
        staged_directory_with_mode_equals_its_listing(0o000);
    }

    #[test]
    fn a_staged_directory_without_owner_read_equals_its_listing() {
        staged_directory_with_mode_equals_its_listing(0o300);
    }

    #[test]
    fn a_staged_comparison_that_finds_differences_still_restores_the_modes() {
        let (tree, listing, _unlock) = staging_and_listing(0o000);
        std::fs::write(tree.path("proj/extra"), "new").expect("written");
        let compared = compare_staged(&tree, &listing);
        assert_eq!(compared.differences(), [Difference::Added(path("extra"))]);
        assert_eq!(mode_of(&tree.path("proj/a")), 0);
    }

    #[test]
    fn a_staged_directory_time_is_compared_with_the_listing() {
        let tree = mixed();
        let listing = packed(&tree);
        filetime::set_file_mtime(
            tree.path("proj/src"),
            FileTime::from_unix_time(2_000_000_000, 0),
        )
        .expect("the time is set");
        let compared = compare_staged(&tree, &listing);
        assert_eq!(compared.differences(), [changed("src", &[Field::Mtime])]);
    }

    #[test]
    fn a_directory_time_does_not_count_on_a_users_folder() {
        let tree = mixed();
        let listing = packed(&tree);
        filetime::set_file_mtime(
            tree.path("proj/src"),
            FileTime::from_unix_time(2_000_000_000, 0),
        )
        .expect("the time is set");
        assert_eq!(compare(&tree, &listing), []);
    }

    #[test]
    fn a_locked_directory_of_a_users_folder_is_refused_by_the_comparison() {
        let (tree, listing, _unlock) = staging_and_listing(0o000);
        let root = TreeRoot::open(&tree.path("proj")).expect("the folder opens");
        let error = compare_with_listing(&listing, &root, &WalkRules::default())
            .expect_err("the walk cannot read it");
        assert!(
            matches!(
                &error,
                PackError::TreeHasProblems(Problem::Unreadable { .. })
            ),
            "{error:?}"
        );
        assert_eq!(mode_of(&tree.path("proj/a")), 0);
    }

    #[test]
    fn a_difference_prints_as_a_sentence() {
        let lines = [
            Difference::Removed(path("a")).to_string(),
            Difference::Added(path("b")).to_string(),
            Difference::ChangedWhileRead(path("c")).to_string(),
            changed("d", &[Field::Mode, Field::Content]).to_string(),
        ];
        assert_eq!(
            lines,
            [
                "`a` is gone from the folder",
                "`b` is new in the folder",
                "`c` changed while it was read",
                "`d` differs in Mode, Content",
            ]
        );
    }

    #[test]
    fn the_error_for_one_difference_says_difference() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        std::fs::remove_file(tree.path("proj/notes.txt")).expect("removed");
        let error = verify(&tree, &packaged.bytes).expect_err("differs");
        assert_eq!(
            error.to_string(),
            "the folder differs from the package: 1 difference, the first: `notes.txt` is gone from the folder"
        );
    }

    #[test]
    fn a_mode_that_cannot_be_restored_is_an_error_naming_the_directory() {
        use std::os::unix::fs::PermissionsExt;
        let (tree, _package, _unlock) = staging_with_locked_directory(0o000);
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let mut granted = Granted::new(&root);
        granted.grant();
        // The directory is gone by the time its mode is to be put back.
        std::fs::set_permissions(tree.path("proj/a"), std::fs::Permissions::from_mode(0o700))
            .expect("the mode is set");
        std::fs::remove_file(tree.path("proj/a/f")).expect("removed");
        std::fs::remove_dir(tree.path("proj/a")).expect("removed");
        let error = granted.restore().expect_err("the directory is gone");
        assert!(
            matches!(&error, PackError::RestoreMode { path, mode: 0, .. } if path.to_string() == "a"),
            "{error:?}"
        );
        assert!(error.to_string().contains("keeps owner read and search"));
    }

    #[test]
    fn a_staged_verify_that_cannot_read_a_file_still_restores_the_modes() {
        use std::os::unix::fs::PermissionsExt;
        let (tree, package, _unlock) = staging_with_locked_directory(0o000);
        let secret = tree.path("proj/secret");
        std::fs::write(&secret, "x").expect("written");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let unreadable = std::fs::File::open(&secret).is_err();
        let result = verify_with(&tree, &package, &staged());
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(unreadable, "the test needs a user that mode 000 locks out");
        let error = result.expect_err("the file cannot be read");
        assert!(
            matches!(&error, PackError::Read { path, .. } if path.to_string() == "secret"),
            "{error:?}"
        );
        assert_eq!(mode_of(&tree.path("proj/a")), 0);
    }

    #[test]
    fn a_granted_directory_that_changed_during_the_verify_is_changed_while_read() {
        use std::os::unix::fs::PermissionsExt;
        let (tree, package, _unlock) = staging_with_locked_directory(0o000);
        let (_dir, file) = on_disk(&package);
        let root = TreeRoot::open(&tree.path("proj")).expect("opens");
        let mut observe = |stage: Stage, seen: &RelPath| {
            if stage == Stage::AfterRead && seen.to_string() == "a/f" {
                // The directory is granted read and search only; the test
                // lets itself write and adds a file.
                let directory = tree.path("proj/a");
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                    .expect("chmod");
                std::fs::write(directory.join("g"), "new").expect("written");
            }
        };
        let error = verify_strict_observed(&file, &root, &staged(), &RealFs, &mut observe)
            .expect_err("the directory changed");
        assert!(
            matches!(&error, PackError::Differs(found)
                if found == &[Difference::ChangedWhileRead(path("a"))]),
            "{error:?}"
        );
        assert_eq!(mode_of(&tree.path("proj/a")), 0, "the mode is back");
    }

    #[test]
    fn the_error_names_the_count_and_the_first_difference() {
        let tree = mixed();
        let packaged = pack_piece(&tree, "proj");
        std::fs::remove_file(tree.path("proj/notes.txt")).expect("removed");
        std::fs::remove_file(tree.path("proj/src/main.rs")).expect("removed");
        let error = verify(&tree, &packaged.bytes).expect_err("differs");
        assert_eq!(
            error.to_string(),
            "the folder differs from the package: 2 differences, the first: `notes.txt` is gone from the folder"
        );
    }
}
