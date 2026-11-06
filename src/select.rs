// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Selecting pieces.

use crate::filesystem::tree::{DirEntry, TreeRoot};
use crate::relpath::{RelPath, escape};
use crate::tree::EntryKind;
use crate::walk::{Call, Hooks, RealFs, is_out_of_descriptors};
use glob::{MatchOptions, Pattern};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet};
use std::io;

/// Errors of piece selection.
#[derive(Debug, thiserror::Error)]
pub enum SelectError {
    #[error("pattern {pattern} is invalid: {reason}")]
    InvalidPattern { pattern: String, reason: String },
    #[error("the collection root cannot be listed")]
    Unreadable {
        #[source]
        source: io::Error,
    },
    #[error("out of file descriptors while listing {path}")]
    OutOfDescriptors {
        path: RelPath,
        #[source]
        source: io::Error,
    },
}

impl crate::error::HasKind for SelectError {}

/// The patterns and the exclude list that decide what a piece is.
///
/// A pattern is matched component by component, with `glob` deciding each
/// component. The crate cannot decide a whole path: its leading-dot rule
/// does not apply to a literal that follows a `*`, so `*.txt` would select
/// `.txt`, and ADR 0028 allows a dot entry only through a pattern that
/// names the dot itself.
#[derive(Debug, Clone)]
pub struct Rules {
    patterns: Vec<Vec<Part>>,
    exclude: HashSet<RelPath>,
    /// `(device, inode)` of the exclude entries that exist and are
    /// directories; see `bind`.
    identities: HashSet<(u64, u64)>,
}

/// One `/`-separated component of a pattern.
#[derive(Debug, Clone)]
enum Part {
    /// `**`: directories, as many as needed.
    Recursive,
    Glob {
        pattern: Pattern,
        /// The component starts with a literal dot, the only way to name
        /// an entry that starts with one.
        names_the_dot: bool,
    },
}

/// A component is compared as a whole, so a `/` can never match.
const OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

impl Rules {
    /// `exclude` lists paths relative to the collection root; they are
    /// compared as they are, not as patterns.
    pub fn new(patterns: &[String], exclude: &[RelPath]) -> Result<Rules, SelectError> {
        let patterns = patterns
            .iter()
            .map(|pattern| {
                compile(pattern).map_err(|reason| SelectError::InvalidPattern {
                    pattern: pattern.clone(),
                    reason,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Rules {
            patterns,
            exclude: exclude.iter().cloned().collect(),
            identities: HashSet::new(),
        })
    }

    /// Looks up the exclude entries that exist in `collection`, so that an
    /// excluded directory is recognised by the directory it is and not
    /// only by its spelling.
    pub fn bind(self, collection: &TreeRoot) -> Result<Rules, SelectError> {
        self.bind_with(collection, &RealFs)
    }

    pub(crate) fn bind_with(
        mut self,
        collection: &TreeRoot,
        hooks: &dyn Hooks,
    ) -> Result<Rules, SelectError> {
        for path in &self.exclude {
            let result = match hooks.fault(path, Call::Stat) {
                Some(error) => Err(error),
                None => collection.lstat(path),
            };
            match result {
                // A file is excluded by its path only: another link to it
                // is a file of its own.
                Ok(entry) => {
                    if entry.kind == EntryKind::Directory {
                        self.identities.insert((entry.device, entry.inode));
                    }
                }
                Err(error) if is_out_of_descriptors(&error) => {
                    return Err(SelectError::OutOfDescriptors {
                        path: path.clone(),
                        source: error,
                    });
                }
                // An exclude entry that does not exist matches by its
                // spelling only, and nothing can be at a path that is not
                // there.
                Err(_) => {}
            }
        }
        Ok(self)
    }

    /// Whether the entry at `path`, relative to the collection root, with
    /// this `device` and `inode`, is on the exclude list. The identity
    /// matches an excluded directory; a file is on the list by its path.
    pub(crate) fn excludes(&self, path: &RelPath, device: u64, inode: u64) -> bool {
        self.exclude.contains(path) || self.identities.contains(&(device, inode))
    }

    /// Whether the exclude list has any entry.
    pub(crate) fn has_exclusions(&self) -> bool {
        !self.exclude.is_empty()
    }

    /// Whether a pattern selects `path`.
    pub fn matches(&self, path: &RelPath) -> bool {
        self.accepts(path, true)
    }

    /// Whether a pattern might select something below `dir`. A directory
    /// that fails this is not listed at all. The answer errs towards yes:
    /// it never says no for a directory above a match.
    pub fn can_descend(&self, dir: &RelPath) -> bool {
        self.pending(dir, true)
    }

    /// `matches` with the dot rule switched off, to tell the entries that
    /// only the dot rule keeps out.
    fn matches_without_dot_rule(&self, path: &RelPath) -> bool {
        self.accepts(path, false)
    }

    fn can_descend_without_dot_rule(&self, dir: &RelPath) -> bool {
        self.pending(dir, false)
    }

    fn accepts(&self, path: &RelPath, dot_rule: bool) -> bool {
        if path.is_root() {
            return false;
        }
        let text = match_text(path);
        let components: Vec<&str> = text.split('/').collect();
        self.patterns
            .iter()
            .any(|parts| run(parts, &components, dot_rule).contains(&parts.len()))
    }

    fn pending(&self, dir: &RelPath, dot_rule: bool) -> bool {
        if dir.is_root() {
            return !self.patterns.is_empty();
        }
        // Once a descendant has a byte that is not UTF-8, its whole path
        // is matched in the escaped form, in which a `%` of this directory
        // reads `%25`. Both spellings are tried, since the descendant is
        // not known yet.
        let raw = match_text(dir);
        let escaped = escape(dir.as_bytes());
        let spellings = if raw == escaped {
            vec![raw]
        } else {
            vec![raw, Cow::Owned(escaped)]
        };
        spellings.iter().any(|text| {
            let components: Vec<&str> = text.split('/').collect();
            self.patterns.iter().any(|parts| {
                run(parts, &components, dot_rule)
                    .iter()
                    .any(|&at| at < parts.len())
            })
        })
    }
}

/// The text a path is matched as: the path itself when it is valid UTF-8,
/// else its escaped form.
fn match_text(path: &RelPath) -> Cow<'_, str> {
    match std::str::from_utf8(path.as_bytes()) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(escape(path.as_bytes())),
    }
}

/// Splits a pattern into its components, or says what is wrong with it.
fn compile(pattern: &str) -> Result<Vec<Part>, String> {
    let mut parts: Vec<Part> = Vec::new();
    for component in pattern.split('/') {
        match component {
            "" => return Err("it has an empty component".to_string()),
            "." | ".." => return Err(format!("it has a `{component}` component")),
            "**" => {
                // `**/**` and `**` select the same.
                if !matches!(parts.last(), Some(Part::Recursive)) {
                    parts.push(Part::Recursive);
                }
            }
            _ => {
                let glob = Pattern::new(component).map_err(|error| error.to_string())?;
                parts.push(Part::Glob {
                    pattern: glob,
                    names_the_dot: component.starts_with('.'),
                });
            }
        }
    }
    Ok(parts)
}

/// Runs the parts over the components of a path and returns how many parts
/// each remaining possibility has consumed. A possibility that has
/// consumed all parts accepts exactly this path.
///
/// `**` takes zero or more directories in front of another part, and one
/// or more at the end of the pattern, as `glob` reads it. It never takes
/// a component that starts with a dot while the dot rule is on.
fn run(parts: &[Part], components: &[&str], dot_rule: bool) -> BTreeSet<usize> {
    let last = parts.len() - 1;
    let skip_zero_directories = |states: &mut BTreeSet<usize>| {
        for (at, part) in parts.iter().enumerate().take(last) {
            if states.contains(&at) && matches!(part, Part::Recursive) {
                states.insert(at + 1);
            }
        }
    };

    let mut states = BTreeSet::from([0]);
    skip_zero_directories(&mut states);
    for component in components {
        let hidden = dot_rule && component.starts_with('.');
        let mut next = BTreeSet::new();
        for &at in states.iter().filter(|&&at| at < parts.len()) {
            match &parts[at] {
                Part::Recursive if !hidden => {
                    next.insert(at);
                    if at == last {
                        next.insert(at + 1);
                    }
                }
                Part::Recursive => {}
                Part::Glob {
                    pattern,
                    names_the_dot,
                } => {
                    if (!hidden || *names_the_dot) && pattern.matches_with(component, OPTIONS) {
                        next.insert(at + 1);
                    }
                }
            }
        }
        skip_zero_directories(&mut next);
        states = next;
    }
    states
}

/// What selecting the pieces of a collection found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pieces: Vec<RelPath>,
    skipped_symlinks: usize,
    skipped_dot_entries: usize,
    unreadable: Vec<RelPath>,
}

impl Selection {
    /// The pieces, sorted by path bytes.
    pub fn pieces(&self) -> &[RelPath] {
        &self.pieces
    }

    /// Symlinks a pattern selects and that are skipped because a piece is
    /// never a symlink.
    pub fn skipped_symlinks(&self) -> usize {
        self.skipped_symlinks
    }

    /// Directories that only the dot rule kept out: a pattern would have
    /// selected them, or looked below them, if they did not start with a
    /// dot.
    pub fn skipped_dot_entries(&self) -> usize {
        self.skipped_dot_entries
    }

    /// Directories selection had to look into and could not read. A path
    /// at or below one of them is neither new nor gone in this run.
    pub fn unreadable(&self) -> &[RelPath] {
        &self.unreadable
    }
}

/// Finds the pieces of the collection that `collection` is the root of.
pub fn select(collection: &TreeRoot, rules: &Rules) -> Result<Selection, SelectError> {
    select_with(collection, rules, &RealFs)
}

pub(crate) fn select_with(
    collection: &TreeRoot,
    rules: &Rules,
    hooks: &dyn Hooks,
) -> Result<Selection, SelectError> {
    let mut selector = Selector {
        rules,
        hooks,
        selection: Selection {
            pieces: Vec::new(),
            skipped_symlinks: 0,
            skipped_dot_entries: 0,
            unreadable: Vec::new(),
        },
    };

    // One directory is open per level, so the levels are kept in a list
    // of their own and not on the call stack: a deep tree costs no deep
    // recursion, and the cap on the depth keeps the open directories few.
    let mut levels = Vec::new();
    levels.extend(selector.open_level(Directory::Borrowed(collection), RelPath::root())?);
    while !levels.is_empty() {
        selector.step(&mut levels)?;
    }
    selector.selection.pieces.sort();
    selector.selection.unreadable.sort();
    Ok(selector.selection)
}

/// The collection root is borrowed, the directories below it are opened
/// and owned.
enum Directory<'a> {
    Borrowed(&'a TreeRoot),
    Owned(TreeRoot),
}

impl Directory<'_> {
    fn root(&self) -> &TreeRoot {
        match self {
            Directory::Borrowed(root) => root,
            Directory::Owned(root) => root,
        }
    }
}

/// A directory being looked through: what is left of its listing.
struct Level<'a> {
    directory: Directory<'a>,
    at: RelPath,
    children: std::vec::IntoIter<DirEntry>,
}

struct Selector<'a> {
    rules: &'a Rules,
    hooks: &'a dyn Hooks,
    selection: Selection,
}

impl<'a> Selector<'a> {
    /// Makes one filesystem call on behalf of `path`, unless the hook
    /// makes it fail. A descriptor shortage ends the run; any other
    /// failure is handed back for the caller to judge.
    fn call<T>(
        &self,
        path: &RelPath,
        kind: Call,
        call: impl FnOnce() -> io::Result<T>,
    ) -> Result<io::Result<T>, SelectError> {
        let result = match self.hooks.fault(path, kind) {
            Some(error) => Err(error),
            None => call(),
        };
        match result {
            Err(error) if is_out_of_descriptors(&error) => Err(SelectError::OutOfDescriptors {
                path: path.clone(),
                source: error,
            }),
            other => Ok(other),
        }
    }

    fn mark_unreadable(&mut self, directory: &RelPath) {
        if !self.selection.unreadable.contains(directory) {
            self.selection.unreadable.push(directory.clone());
        }
    }

    /// Lists `directory`, which lies at `at`. A directory that cannot be
    /// listed is reported and skipped, except the collection root, which
    /// fails the run.
    fn open_level(
        &mut self,
        directory: Directory<'a>,
        at: RelPath,
    ) -> Result<Option<Level<'a>>, SelectError> {
        let listing = self.call(&at, Call::List, || {
            directory.root().read_dir(&RelPath::root())
        })?;
        let mut listing = match listing {
            Ok(listing) => listing,
            Err(source) if at.is_root() => return Err(SelectError::Unreadable { source }),
            Err(_) => {
                self.mark_unreadable(&at);
                return Ok(None);
            }
        };
        listing.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(Level {
            directory,
            at,
            children: listing.into_iter(),
        }))
    }

    /// Looks at the next entry of the innermost directory, for pieces and
    /// for directories worth looking through. A match is a piece and is
    /// not looked into: the shallowest match wins.
    fn step(&mut self, levels: &mut Vec<Level<'a>>) -> Result<(), SelectError> {
        let depth = levels.len();
        let level = levels
            .last_mut()
            .expect("selection steps only while a level is open");
        let Some(listed) = level.children.next() else {
            levels.pop();
            return Ok(());
        };
        let directory = level.directory.root();
        let at = &level.at;
        let child = at.join(&listed.name);
        if (at.is_root() && listed.name.as_bytes() == b".antiq")
            || self.rules.exclude.contains(&child)
        {
            return Ok(());
        }
        let single = RelPath::from_bytes(listed.name.as_bytes().to_vec())
            .expect("a listed name is a valid path of one component");

        // The kind a listing reports can be missing, and the identity of
        // a directory needs a stat. Only directories have an identity on
        // the exclude list.
        let mut kind = self.hooks.listed_kind(&child, listed.kind);
        let must_stat = kind == EntryKind::Unknown
            || (kind == EntryKind::Directory && !self.rules.identities.is_empty());
        if must_stat {
            match self.call(&child, Call::Stat, || directory.lstat(&single))? {
                Ok(entry) => {
                    if self.rules.identities.contains(&(entry.device, entry.inode)) {
                        return Ok(());
                    }
                    kind = entry.kind;
                }
                // Deleted since it was listed: nothing left to select.
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(_) => {
                    let parent = at.clone();
                    self.mark_unreadable(&parent);
                    return Ok(());
                }
            }
        }

        match kind {
            EntryKind::Symlink => {
                if self.rules.matches(&child) {
                    self.selection.skipped_symlinks += 1;
                }
            }
            EntryKind::Directory => {
                if self.rules.matches(&child) {
                    self.selection.pieces.push(child);
                } else if self.rules.can_descend(&child) {
                    if depth > self.hooks.max_depth() {
                        self.mark_unreadable(&child);
                        return Ok(());
                    }
                    match self.call(&child, Call::Open, || directory.sub(&single))? {
                        Ok(inner) => {
                            let opened = self.open_level(Directory::Owned(inner), child)?;
                            levels.extend(opened);
                        }
                        // Deleted since it was listed: nothing left to select.
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => self.mark_unreadable(&child),
                    }
                } else if self.rules.matches_without_dot_rule(&child)
                    || self.rules.can_descend_without_dot_rule(&child)
                {
                    self.selection.skipped_dot_entries += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::Tree;
    use rustix::io::Errno;
    use std::fs;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn bytes(raw: &[u8]) -> RelPath {
        RelPath::from_bytes(raw.to_vec()).expect("valid path")
    }

    fn rules(patterns: &[&str], exclude: &[&str]) -> Rules {
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        let exclude: Vec<RelPath> = exclude.iter().map(|p| path(p)).collect();
        Rules::new(&patterns, &exclude).expect("the patterns are valid")
    }

    fn invalid(pattern: &str) -> String {
        let error = Rules::new(&[pattern.to_string()], &[]).expect_err("the pattern is invalid");
        error.to_string()
    }

    fn select_in(tree: &Tree, patterns: &[&str], exclude: &[&str]) -> Selection {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let rules = rules(patterns, exclude)
            .bind(&root)
            .expect("the exclude list binds");
        select(&root, &rules).expect("the selection succeeds")
    }

    fn pieces(selection: &Selection) -> Vec<String> {
        selection.pieces().iter().map(|p| p.to_string()).collect()
    }

    fn assert_cannot_read(directory: &std::path::Path) {
        assert!(
            fs::read_dir(directory).is_err(),
            "the test needs a user that mode 000 locks out"
        );
    }

    /// Fails one kind of call at one path.
    struct Failing {
        at: RelPath,
        call: Call,
        code: Errno,
    }

    impl Hooks for Failing {
        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            (path == &self.at && call == self.call)
                .then(|| io::Error::from_raw_os_error(self.code.raw_os_error()))
        }
    }

    #[test]
    fn the_default_pattern_selects_directories_and_counts_the_dot_entries_it_skips() {
        let mut tree = Tree::new();
        tree.dir("a");
        tree.dir("b");
        tree.dir(".git");
        tree.file("loose.txt", "x");
        let selection = select_in(&tree, &["*"], &[]);
        assert_eq!(pieces(&selection), ["a", "b"]);
        assert_eq!(selection.skipped_dot_entries(), 1);
    }

    #[test]
    fn a_pattern_with_a_leading_dot_selects_dot_entries_but_never_the_collection_marker() {
        let mut tree = Tree::new();
        tree.dir(".git");
        tree.dir(".cache");
        tree.dir(".antiq");
        tree.dir("a");
        let selection = select_in(&tree, &[".*"], &[]);
        assert_eq!(pieces(&selection), [".cache", ".git"]);
        assert_eq!(selection.skipped_dot_entries(), 0);
    }

    #[test]
    fn a_dot_component_below_the_top_is_selected_by_a_pattern_naming_it() {
        let mut tree = Tree::new();
        tree.dir("projects/.x");
        tree.dir("projects/y");
        let selection = select_in(&tree, &["projects/.*"], &[]);
        assert_eq!(pieces(&selection), ["projects/.x"]);
    }

    #[test]
    fn a_literal_after_a_star_does_not_select_a_dot_entry() {
        let mut tree = Tree::new();
        tree.dir(".txt");
        tree.dir("a.txt");
        let selection = select_in(&tree, &["*.txt"], &[]);
        assert_eq!(pieces(&selection), ["a.txt"]);
        assert_eq!(selection.skipped_dot_entries(), 1);
    }

    #[test]
    fn the_recursive_pattern_selects_what_the_default_selects() {
        let mut tree = Tree::new();
        tree.dir("a/inner/deep");
        tree.dir("b");
        tree.dir(".git/objects");
        let selection = select_in(&tree, &["**/*"], &[]);
        assert_eq!(pieces(&selection), ["a", "b"]);
    }

    #[test]
    fn the_shallowest_match_is_the_piece() {
        let mut tree = Tree::new();
        tree.dir("a/b");
        let selection = select_in(&tree, &["*/*", "*"], &[]);
        assert_eq!(pieces(&selection), ["a"]);
    }

    #[test]
    fn a_symlink_is_never_a_piece_nor_entered_and_is_counted() {
        let mut tree = Tree::new();
        tree.file("real/x/f", "x");
        tree.dir("p");
        tree.symlink("p/link", tree.path("real"));
        let selection = select_in(&tree, &["*/*"], &["real"]);
        assert_eq!(pieces(&selection), Vec::<String>::new());
        assert_eq!(selection.skipped_symlinks(), 1);
    }

    #[test]
    fn a_symlink_that_no_pattern_matches_is_not_counted() {
        let mut tree = Tree::new();
        tree.dir("p");
        tree.symlink("p/link", "nowhere");
        let selection = select_in(&tree, &["*"], &[]);
        assert_eq!(pieces(&selection), ["p"]);
        assert_eq!(selection.skipped_symlinks(), 0);
    }

    #[test]
    fn an_excluded_path_and_everything_below_it_is_no_piece() {
        let mut tree = Tree::new();
        tree.dir("archive/x");
        tree.dir("keep/x");
        tree.dir("projects/old/x");
        tree.dir("projects/new");
        let selection = select_in(&tree, &["*/*"], &["archive"]);
        assert_eq!(
            pieces(&selection),
            ["keep/x", "projects/new", "projects/old"]
        );
        let selection = select_in(&tree, &["projects/*"], &["projects/old"]);
        assert_eq!(pieces(&selection), ["projects/new"]);
    }

    #[test]
    fn an_exclude_entry_also_matches_the_directory_it_names_under_another_spelling() {
        let mut tree = Tree::new();
        tree.dir("archive/x");
        tree.dir("keep");
        // Only a volume that ignores case has a second spelling to catch.
        let insensitive = fs::metadata(tree.path("ARCHIVE")).is_ok();
        let both = ["archive", "keep"];
        let expected = &both[usize::from(insensitive)..];
        let selection = select_in(&tree, &["*"], &["Archive"]);
        assert_eq!(pieces(&selection), expected);
    }

    #[test]
    fn only_an_exclude_entry_that_is_a_directory_is_recognised_by_its_identity() {
        let mut tree = Tree::new();
        tree.dir("archive");
        tree.file("notes.txt", "x");
        tree.symlink("link", "archive");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let exclude: Vec<RelPath> = ["archive", "notes.txt", "link"]
            .iter()
            .map(|name| RelPath::parse(name).expect("valid path"))
            .collect();

        let rules = Rules::new(&[], &exclude)
            .expect("no patterns are valid")
            .bind(&root)
            .expect("binds");

        let archive = root
            .lstat(&RelPath::parse("archive").expect("valid path"))
            .expect("the directory exists");
        assert_eq!(
            rules.identities,
            HashSet::from([(archive.device, archive.inode)])
        );
    }

    #[test]
    fn pieces_come_out_in_path_byte_order() {
        let mut tree = Tree::new();
        tree.dir("a0");
        tree.dir("a-b");
        tree.dir("a");
        let selection = select_in(&tree, &["*"], &[]);
        assert_eq!(pieces(&selection), ["a", "a-b", "a0"]);
    }

    #[test]
    fn a_directory_no_pattern_can_reach_into_is_never_listed() {
        let mut tree = Tree::new();
        tree.file("p/x/f", "x");
        let locked = tree.unreadable_dir("q");
        assert_cannot_read(&locked);
        let selection = select_in(&tree, &["p/*"], &[]);
        assert_eq!(pieces(&selection), ["p/x"]);
        assert!(selection.unreadable().is_empty());
    }

    #[test]
    fn a_directory_that_must_be_listed_and_cannot_is_reported_and_skipped() {
        let mut tree = Tree::new();
        tree.dir("b/c");
        let locked = tree.unreadable_dir("a");
        assert_cannot_read(&locked);
        let selection = select_in(&tree, &["*/*"], &[]);
        assert_eq!(pieces(&selection), ["b/c"]);
        assert_eq!(selection.unreadable(), [path("a")]);
    }

    #[test]
    fn a_collection_root_that_cannot_be_listed_fails_the_run() {
        let tree = Tree::new();
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let hooks = Failing {
            at: RelPath::root(),
            call: Call::List,
            code: Errno::ACCESS,
        };
        let error =
            select_with(&root, &rules(&["*"], &[]), &hooks).expect_err("the root cannot be listed");
        assert!(matches!(error, SelectError::Unreadable { .. }), "{error:?}");
    }

    #[test]
    fn a_pattern_naming_a_dot_directory_still_reaches_below_it() {
        let mut tree = Tree::new();
        tree.dir("a");
        tree.dir(".x/y");
        let selection = select_in(&tree, &["*", ".x/*"], &[]);
        assert_eq!(pieces(&selection), [".x/y", "a"]);
        assert_eq!(selection.skipped_dot_entries(), 0);
    }

    #[test]
    fn a_dot_directory_that_only_hides_pieces_below_it_is_counted() {
        let mut tree = Tree::new();
        tree.dir(".config/x");
        tree.dir("a/b");
        let selection = select_in(&tree, &["*/*"], &[]);
        assert_eq!(pieces(&selection), ["a/b"]);
        assert_eq!(selection.skipped_dot_entries(), 1);
    }

    #[test]
    fn a_failing_listing_is_reported_or_ends_the_run_by_its_error() {
        let mut tree = Tree::new();
        tree.dir("sub/dir");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let all = rules(&["*/*/*"], &[]);

        let shortage = Failing {
            at: path("sub/dir"),
            call: Call::List,
            code: Errno::MFILE,
        };
        let error = select_with(&root, &all, &shortage).expect_err("a shortage ends the run");
        assert!(
            matches!(&error, SelectError::OutOfDescriptors { path: p, .. } if p == &path("sub/dir")),
            "{error:?}"
        );

        let denied = Failing {
            code: Errno::ACCESS,
            ..shortage
        };
        let selection = select_with(&root, &all, &denied).expect("a refusal is reported");
        assert_eq!(selection.unreadable(), [path("sub/dir")]);
    }

    #[test]
    fn a_failing_stat_or_open_skips_or_reports_without_ending_the_run() {
        let mut tree = Tree::new();
        tree.dir("sub/dir/deep");
        tree.dir("other");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        // An exclude entry that exists makes every directory get stat'd.
        let all = rules(&["*/*/*"], &["other"])
            .bind(&root)
            .expect("the exclude list binds");
        for (call, code, unreadable) in [
            (Call::Stat, Errno::NOENT, None),
            (Call::Stat, Errno::ACCESS, Some("sub")),
            (Call::Open, Errno::NOENT, None),
            (Call::Open, Errno::ACCESS, Some("sub/dir")),
        ] {
            let hooks = Failing {
                at: path("sub/dir"),
                call,
                code,
            };
            let selection = select_with(&root, &all, &hooks).expect("the selection succeeds");
            assert_eq!(selection.pieces(), [], "{call:?} {code:?}");
            let expected: Vec<RelPath> = unreadable.map(path).into_iter().collect();
            assert_eq!(
                selection.unreadable(),
                expected.as_slice(),
                "{call:?} {code:?}"
            );
        }
    }

    #[test]
    fn running_out_of_descriptors_while_opening_ends_the_run() {
        let mut tree = Tree::new();
        tree.dir("sub/dir/deep");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let hooks = Failing {
            at: path("sub/dir"),
            call: Call::Open,
            code: Errno::NFILE,
        };
        let error = select_with(&root, &rules(&["*/*/*"], &[]), &hooks)
            .expect_err("a shortage ends the run");
        assert!(
            matches!(error, SelectError::OutOfDescriptors { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn bad_patterns_are_refused_with_the_pattern_named() {
        for pattern in [
            "[", "*/", "./a", "a//b", "../x", "**/", "x/**/", "[a/]*", "", "a**",
        ] {
            let message = invalid(pattern);
            assert!(
                message.starts_with(&format!("pattern {pattern} is invalid: ")),
                "{message}"
            );
        }
    }

    #[test]
    fn dot_rule_cases_match_the_way_the_selection_needs() {
        let case =
            |pattern: &str, candidate: &str| rules(&[pattern], &[]).matches(&path(candidate));
        assert!(!case("*.txt", ".txt"));
        assert!(case("*.txt", "a.txt"));
        assert!(case(".*", ".txt"));
        assert!(!case("*", ".txt"));
        assert!(!case("a/*.x", "a/.x"));
        assert!(case("a/.*", "a/.x"));
        assert!(!case("?x", ".x"));
        assert!(!case("[.]x", ".x"));
    }

    #[test]
    fn glob_alone_would_select_a_dot_entry_through_a_literal_after_a_star() {
        // The reason the engine enforces the dot rule itself: this is what
        // the crate does with its own leading-dot rule switched on.
        let options = MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: true,
        };
        let glob = Pattern::new("*.txt").expect("a valid glob");
        assert!(glob.matches_with(".txt", options));
    }

    #[test]
    fn recursive_parts_count_the_directories_glob_counts() {
        let case =
            |pattern: &str, candidate: &str| rules(&[pattern], &[]).matches(&path(candidate));
        assert!(!case("a/**", "a"));
        assert!(case("a/**", "a/b"));
        assert!(case("a/**", "a/b/c"));
        assert!(!case("a/**", "a/.x"));
        assert!(case("**/b", "b"));
        assert!(case("a/**/b", "a/b"));
        assert!(case("a/**/b", "a/x/y/b"));
        assert!(!case("a/**/b", "a/.x/b"));
        assert!(!rules(&["**"], &[]).matches(&RelPath::root()));
    }

    const PATTERNS: [&str; 12] = [
        "*",
        "*/*",
        "**/*",
        "a/**",
        "a/**/b",
        "**/b",
        "**",
        "a*",
        "?x",
        "[ab]*/c",
        "a/*/c",
        "**/a/**/c",
    ];
    const CANDIDATES: [&str; 14] = [
        "a", "b", "ab", "a/b", "a/b/c", "a/c", "x/b", "a/x/b", "a/b/b", "bx", "ax/c", "ax",
        "a/x/y/b", "q/a/r/c",
    ];

    #[test]
    fn the_engine_agrees_with_glob_where_no_dot_is_involved() {
        let options = MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: true,
        };
        for pattern in PATTERNS {
            let engine = rules(&[pattern], &[]);
            let glob = Pattern::new(pattern).expect("a valid glob");
            for candidate in CANDIDATES {
                assert_eq!(
                    engine.matches(&path(candidate)),
                    glob.matches_with(candidate, options),
                    "{pattern} against {candidate}"
                );
            }
        }
    }

    #[test]
    fn every_directory_above_a_match_can_be_descended_into() {
        for pattern in PATTERNS {
            let engine = rules(&[pattern], &[]);
            for candidate in CANDIDATES {
                if !engine.matches(&path(candidate)) {
                    continue;
                }
                let mut prefix = Vec::new();
                let parts: Vec<&str> = candidate.split('/').collect();
                for part in &parts[..parts.len() - 1] {
                    prefix.push(*part);
                    let dir = prefix.join("/");
                    assert!(
                        engine.can_descend(&path(&dir)),
                        "{pattern} matches {candidate} but not into {dir}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_percent_in_a_directory_name_does_not_hide_a_non_utf8_match_below_it() {
        let engine = rules(&["100%25/*"], &[]);
        assert!(engine.matches(&bytes(b"100%/\xff")));
        assert!(engine.can_descend(&bytes(b"100%")));
    }

    #[test]
    fn descending_follows_what_a_pattern_can_still_reach() {
        let star = rules(&["*"], &[]);
        assert!(!star.can_descend(&path("a")));
        assert!(star.can_descend(&RelPath::root()));
        assert!(!rules(&[], &[]).can_descend(&RelPath::root()));
        let all = rules(&["**/*"], &[]);
        assert!(all.can_descend(&path("a")));
        assert!(!all.can_descend(&path(".git")));
        let below = rules(&["a/**"], &[]);
        assert!(below.can_descend(&path("a")));
        assert!(!below.can_descend(&path("b")));
        let dotted = rules(&["*", ".x/*"], &[]);
        assert!(dotted.can_descend(&path(".x")));
        assert!(!dotted.can_descend(&path(".y")));
    }

    #[test]
    fn names_that_are_not_utf8_match_through_their_escaped_form() {
        let case =
            |pattern: &str, candidate: &[u8]| rules(&[pattern], &[]).matches(&bytes(candidate));
        assert!(case("a%FF", b"a\xff"));
        assert!(case("*", b"\xffx"));
        assert!(!case("*", b".\xff"));
        assert!(case(".*", b".\xff"));
        assert!(
            case("a%25b/%FF", b"a%b/\xff"),
            "a % of the name reads %25 once the path has a byte that is not UTF-8"
        );
    }

    #[test]
    fn a_name_that_is_valid_utf8_matches_as_it_is() {
        let case =
            |pattern: &str, candidate: &[u8]| rules(&[pattern], &[]).matches(&bytes(candidate));
        assert!(case("a%FF", b"a%FF"));
        assert!(!case("a%25FF", b"a%FF"));
    }

    #[test]
    fn composed_and_decomposed_names_are_different_names() {
        let composed = "\u{e9}";
        let decomposed = "e\u{301}";
        let engine = rules(&[composed], &[]);
        assert!(engine.matches(&path(composed)));
        assert!(!engine.matches(&path(decomposed)));
    }

    /// Several faults at once, a listing that reports no kinds, and a depth
    /// cap.
    #[derive(Default)]
    struct Scripted {
        faults: Vec<(RelPath, Call, Errno)>,
        unknown_kinds: bool,
        depth: Option<usize>,
    }

    impl Hooks for Scripted {
        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            self.faults
                .iter()
                .find(|(at, kind, _)| at == path && *kind == call)
                .map(|(_, _, code)| io::Error::from_raw_os_error(code.raw_os_error()))
        }

        fn listed_kind(&self, _path: &RelPath, reported: EntryKind) -> EntryKind {
            [reported, EntryKind::Unknown][usize::from(self.unknown_kinds)]
        }

        fn max_depth(&self) -> usize {
            self.depth.unwrap_or(crate::walk::MAX_DEPTH)
        }
    }

    #[test]
    fn a_listing_that_reports_no_kinds_gives_the_same_selection() {
        let mut tree = Tree::new();
        tree.dir("a/inner");
        tree.file("f", "x");
        tree.symlink("l", "a");
        tree.dir(".d");
        tree.dir("other");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        for exclude in [&[][..], &["other"][..]] {
            let all = rules(&["*"], exclude).bind(&root).expect("binds");
            let known = select(&root, &all).expect("the selection succeeds");
            let unknown = Scripted {
                unknown_kinds: true,
                ..Scripted::default()
            };
            let guessed = select_with(&root, &all, &unknown).expect("the selection succeeds");
            assert_eq!(guessed, known, "{exclude:?}");
            assert_eq!(known.skipped_symlinks(), 1);
            assert_eq!(known.skipped_dot_entries(), 1);
        }
    }

    #[test]
    fn several_unreadable_children_list_their_directory_once() {
        let mut tree = Tree::new();
        tree.dir("sub/a");
        tree.dir("sub/b");
        tree.dir("other");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let all = rules(&["*/*/*"], &["other"]).bind(&root).expect("binds");
        let hooks = Scripted {
            faults: vec![
                (path("sub/a"), Call::Stat, Errno::ACCESS),
                (path("sub/b"), Call::Stat, Errno::ACCESS),
            ],
            ..Scripted::default()
        };
        let selection = select_with(&root, &all, &hooks).expect("the selection succeeds");
        assert_eq!(selection.unreadable(), [path("sub")]);
    }

    #[test]
    fn directories_below_the_depth_cap_are_reported_not_entered() {
        let mut tree = Tree::new();
        tree.dir("a/b/c");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let all = rules(&["*/*/*"], &[]);
        let capped = Scripted {
            depth: Some(1),
            ..Scripted::default()
        };
        let selection = select_with(&root, &all, &capped).expect("the selection succeeds");
        assert_eq!(selection.pieces(), []);
        assert_eq!(selection.unreadable(), [path("a/b")]);

        let roomy = Scripted {
            depth: Some(2),
            ..Scripted::default()
        };
        let selection = select_with(&root, &all, &roomy).expect("the selection succeeds");
        assert_eq!(pieces(&selection), ["a/b/c"]);
        assert!(selection.unreadable().is_empty());
    }

    #[test]
    fn running_out_of_descriptors_while_binding_the_exclude_list_ends_the_run() {
        let mut tree = Tree::new();
        tree.dir("other");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let hooks = Failing {
            at: path("other"),
            call: Call::Stat,
            code: Errno::MFILE,
        };
        let error = rules(&["*"], &["other"])
            .bind_with(&root, &hooks)
            .expect_err("a shortage ends the run");
        assert!(
            matches!(&error, SelectError::OutOfDescriptors { path: p, .. } if p == &path("other")),
            "{error:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_with_a_non_utf8_name_is_selected_through_its_escaped_form() {
        use std::os::unix::ffi::OsStrExt;
        let tree = Tree::new();
        fs::create_dir(tree.root().join(std::ffi::OsStr::from_bytes(b"a\xff")))
            .expect("Linux accepts any byte in a name");
        let selection = select_in(&tree, &["a%FF"], &[]);
        assert_eq!(pieces(&selection), ["a%FF"]);
    }
}
