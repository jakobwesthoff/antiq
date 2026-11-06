// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The idle verdict of a piece.

use crate::fsx::tree::TreeRoot;
use crate::pin::{self, PinError};
use crate::relpath::{Name, RelPath};
use crate::tree::{EntryKind, PieceTree};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use jiff::Timestamp;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The ignore files of a piece.
const IGNORE_FILE: &[u8] = b".antiqignore";

/// Names whose changes are no activity, at any level of a piece.
const MARKER_NAMES: [&[u8]; 2] = [IGNORE_FILE, pin::PIN_FILE.as_bytes()];

/// A day, in nanoseconds. Idle time is counted in 24-hour days.
const DAY_NS: i128 = 86_400_000_000_000;

/// Errors of the idle check.
#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("the piece has entries the walk could not read or may not enter")]
    Problems,
    #[error(transparent)]
    Pin(#[from] PinError),
    #[error("cannot read the ignore file {path}: {source}")]
    IgnoreFile {
        path: RelPath,
        #[source]
        source: io::Error,
    },
    #[error("the ignore file {path} is not UTF-8 text")]
    NotUtf8 { path: RelPath },
    #[error("{} line {line} is not a valid ignore rule: {message}", rule_source(.file))]
    BadRule {
        /// The ignore file, or `None` for the `collection.ignore` setting.
        file: Option<RelPath>,
        line: usize,
        message: String,
    },
}

impl crate::error::HasKind for IdleError {}

fn rule_source(file: &Option<RelPath>) -> String {
    match file {
        Some(file) => file.to_string(),
        None => "collection.ignore".to_string(),
    }
}

/// What the idle check makes of a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleVerdict {
    /// The piece carries a pin; nothing else is looked at.
    Pinned,
    /// No file counts, after the ignore rules, the marker files and the
    /// junk list.
    Empty,
    /// Files count only by name: all their times lie too far ahead.
    OnlyFuture,
    /// Not old enough yet. `clock` is the later of the newest counted file
    /// time and the unearthing time, `newest_file` the newest file time.
    Active {
        clock: Timestamp,
        newest_file: Timestamp,
    },
    /// The idle clock is older than the configured days.
    Idle {
        clock: Timestamp,
        newest_file: Timestamp,
    },
}

/// The verdict and the files whose times lie too far ahead to count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleCheck {
    pub verdict: IdleVerdict,
    pub future: Vec<RelPath>,
}

/// What the check reads from the config.
#[derive(Debug, Clone)]
pub struct Settings {
    idle_days: u32,
    /// The `collection.ignore` rules, matched against paths relative to the
    /// top of the piece.
    config_rules: Gitignore,
}

impl Settings {
    /// `ignore_lines` are the lines of `collection.ignore`, in gitignore
    /// syntax. A line that is no valid rule is an error.
    pub fn new(idle_days: u32, ignore_lines: &[String]) -> Result<Settings, IdleError> {
        let config_rules = compile(None, ignore_lines.iter().map(String::as_str))?;
        Ok(Settings {
            idle_days,
            config_rules,
        })
    }
}

/// Builds one matcher from rule lines. Paths are matched relative to the
/// directory the rules belong to, so the builder's root is `.`: any other
/// root makes the matcher cut a byte prefix off the path it is given.
fn compile<'a>(
    file: Option<&RelPath>,
    lines: impl Iterator<Item = &'a str>,
) -> Result<Gitignore, IdleError> {
    let bad_rule = |line: usize, error: ignore::Error| IdleError::BadRule {
        file: file.cloned(),
        line,
        message: error.to_string(),
    };
    let mut builder = GitignoreBuilder::new(".");
    for (index, line) in lines.enumerate() {
        builder
            .add_line(None, line)
            .map_err(|error| bad_rule(index + 1, error))?;
    }
    builder.build().map_err(|error| bad_rule(0, error))
}

/// Decides whether the piece in `tree` is pinned, empty, active or idle at
/// `now`.
///
/// `unearthed` is the time the piece was last unearthed, if it was. The
/// pin is looked up on disk, not in `tree`, so that the answer is the one
/// `pin::is_pinned` gives to the `pin` commands, whatever the junk list
/// left out of the walk. A piece the walk could not fully read has no
/// verdict.
pub fn check(
    collection: &TreeRoot,
    tree: &PieceTree,
    unearthed: Option<Timestamp>,
    settings: &Settings,
    now: Timestamp,
) -> Result<IdleCheck, IdleError> {
    if pin::is_pinned(collection, tree.top())? {
        return Ok(IdleCheck {
            verdict: IdleVerdict::Pinned,
            future: Vec::new(),
        });
    }
    if !tree.problems().is_empty() {
        return Err(IdleError::Problems);
    }

    // Times are compared as nanoseconds: a time the file system reports can
    // lie outside what a `Timestamp` holds.
    let now_ns = now.as_nanosecond();
    let latest_counted = now_ns + DAY_NS;
    let mut rules = Rules::new(collection, tree, settings);
    let mut candidates = 0usize;
    let mut future = Vec::new();
    let mut newest: Option<i128> = None;
    for entry in tree.entries() {
        match entry.kind {
            EntryKind::Directory => {
                rules.enter_directory(&entry.path)?;
                continue;
            }
            EntryKind::File => {}
            _ => continue,
        }
        let is_marker = entry
            .path
            .components()
            .last()
            .is_some_and(|name| MARKER_NAMES.contains(&name));
        if is_marker || rules.ignores(&entry.path, false) {
            continue;
        }
        candidates += 1;
        if entry.mtime_ns > latest_counted {
            future.push(entry.path.clone());
        } else {
            newest = Some(newest.map_or(entry.mtime_ns, |seen| seen.max(entry.mtime_ns)));
        }
    }

    let verdict = if candidates == 0 {
        IdleVerdict::Empty
    } else if let Some(newest) = newest {
        // The unearthing time is bounded like a file time, so a wrong clock
        // cannot hold a piece back for more than a day.
        let clock = unearthed.map_or(newest, |time| {
            newest.max(time.as_nanosecond().min(latest_counted))
        });
        let times = (timestamp(clock), timestamp(newest));
        if now_ns - clock > i128::from(settings.idle_days) * DAY_NS {
            IdleVerdict::Idle {
                clock: times.0,
                newest_file: times.1,
            }
        } else {
            IdleVerdict::Active {
                clock: times.0,
                newest_file: times.1,
            }
        }
    } else {
        IdleVerdict::OnlyFuture
    };
    Ok(IdleCheck { verdict, future })
}

/// The timestamp for `ns` nanoseconds since the epoch. Only times at or
/// before one day after `now` get here, so one that `Timestamp` cannot
/// hold lies before its range and becomes its start.
fn timestamp(ns: i128) -> Timestamp {
    Timestamp::from_nanosecond(ns).unwrap_or(Timestamp::MIN)
}

// =========================================================
// Ignore rules
// =========================================================

/// The rules that apply while the entries of a piece are walked top down.
///
/// A directory comes before everything below it, so by the time an entry is
/// judged its ancestors are decided: an ignored directory ignores
/// everything below it, whatever the rules inside say, and the
/// `.antiqignore` of a directory is read only once the directory is known
/// not to be ignored.
struct Rules<'a> {
    collection: &'a TreeRoot,
    top: &'a RelPath,
    config: &'a Gitignore,
    /// Directories, as path bytes, that hold a regular `.antiqignore`.
    with_ignore_file: HashSet<&'a [u8]>,
    /// The rules read so far, by the directory they belong to.
    loaded: HashMap<&'a [u8], Gitignore>,
    ignored_directories: HashSet<Vec<u8>>,
}

impl<'a> Rules<'a> {
    fn new(collection: &'a TreeRoot, tree: &'a PieceTree, settings: &'a Settings) -> Rules<'a> {
        let with_ignore_file = tree
            .entries()
            .iter()
            .filter(|entry| entry.kind == EntryKind::File)
            .filter_map(|entry| {
                let bytes = entry.path.as_bytes();
                let (parent, name) = split_last(bytes);
                (name == IGNORE_FILE).then_some(parent)
            })
            .collect();
        Rules {
            collection,
            top: tree.top(),
            config: &settings.config_rules,
            with_ignore_file,
            loaded: HashMap::new(),
            ignored_directories: HashSet::new(),
        }
    }

    /// Notes whether the directory at `path` is ignored and, if it is not,
    /// reads its `.antiqignore`.
    fn enter_directory(&mut self, path: &'a RelPath) -> Result<(), IdleError> {
        // The top of the piece is never ignored.
        if !path.is_root() && self.ignores(path, true) {
            self.ignored_directories.insert(path.as_bytes().to_vec());
            return Ok(());
        }
        let bytes = path.as_bytes();
        if let Some(&directory) = self.with_ignore_file.get(bytes) {
            let rules = self.read_ignore_file(path)?;
            self.loaded.insert(directory, rules);
        }
        Ok(())
    }

    /// Whether the entry at `path` is ignored: by an ignored directory above
    /// it, else by the first rule that has a say, looking at the
    /// `.antiqignore` files from the nearest directory outwards and last at
    /// the config.
    fn ignores(&self, path: &RelPath, is_dir: bool) -> bool {
        let bytes = path.as_bytes();
        let (parent, _) = split_last(bytes);
        if self.ignored_directories.contains(parent) {
            return true;
        }
        let mut directory = parent;
        loop {
            if let Some(rules) = self.loaded.get(directory) {
                let below = if directory.is_empty() {
                    bytes
                } else {
                    &bytes[directory.len() + 1..]
                };
                match rules.matched(Path::new(OsStr::from_bytes(below)), is_dir) {
                    ignore::Match::Ignore(_) => return true,
                    ignore::Match::Whitelist(_) => return false,
                    ignore::Match::None => {}
                }
            }
            if directory.is_empty() {
                break;
            }
            directory = split_last(directory).0;
        }
        self.config
            .matched(Path::new(OsStr::from_bytes(bytes)), is_dir)
            .is_ignore()
    }

    /// Reads and compiles the `.antiqignore` in the directory at `directory`.
    fn read_ignore_file(&self, directory: &RelPath) -> Result<Gitignore, IdleError> {
        let name = Name::from_bytes(IGNORE_FILE.to_vec()).expect("the ignore file name is valid");
        let path = directory.join(&name);
        let read = || -> io::Result<Vec<u8>> {
            let mut content = Vec::new();
            self.collection
                .sub(self.top)?
                .open_read(&path)?
                .read_to_end(&mut content)?;
            Ok(content)
        };
        let content = read().map_err(|source| IdleError::IgnoreFile {
            path: path.clone(),
            source,
        })?;
        let text =
            String::from_utf8(content).map_err(|_| IdleError::NotUtf8 { path: path.clone() })?;
        // Editors on some systems put a byte order mark in front of the first
        // rule, which would turn that rule into one that matches nothing.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        compile(Some(&path), text.split('\n'))
    }
}

/// The part of `bytes` before its last `/`, and the part after it. A path
/// without a `/` has the top of the piece, the empty path, as its parent.
fn split_last(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (&bytes[..slash], &bytes[slash + 1..]),
        None => (&bytes[..0], bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relpath::Name;
    use crate::select::Rules;
    use crate::testing::tree::Tree;
    use crate::tree::{EntryKind, TreeEntry};
    use crate::walk::walk_piece;
    use filetime::FileTime;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const DAY: i64 = 86_400;
    const NOW: i64 = 2_000_000_000;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    /// The time `offset` seconds after the test's `now`.
    fn at(offset: i64) -> Timestamp {
        Timestamp::from_second(NOW + offset).expect("in range")
    }

    /// A file in `piece/` modified `offset` seconds after `now`.
    fn file(tree: &mut Tree, relative: &str, offset: i64) -> std::path::PathBuf {
        tree.file_with(&format!("piece/{relative}"), "x", 0o644, NOW + offset)
    }

    fn settings(idle_days: u32) -> Settings {
        Settings::new(idle_days, &[]).expect("no rules to refuse")
    }

    fn walk(root: &TreeRoot) -> PieceTree {
        let rules = Rules::new(&[], &[])
            .expect("no patterns")
            .bind(root)
            .expect("binds");
        let junk = [Name::from_bytes(b".DS_Store".to_vec()).expect("valid name")];
        walk_piece(root, &path("piece"), &junk, &rules).expect("the walk succeeds")
    }

    fn run_with(tree: &Tree, unearthed: Option<Timestamp>, idle_days: u32) -> IdleCheck {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        check(&root, &walk(&root), unearthed, &settings(idle_days), at(0))
            .expect("the check succeeds")
    }

    fn run(tree: &Tree) -> IdleCheck {
        run_with(tree, None, 14)
    }

    fn idle(offset: i64) -> IdleCheck {
        IdleCheck {
            verdict: IdleVerdict::Idle {
                clock: at(offset),
                newest_file: at(offset),
            },
            future: vec![],
        }
    }

    fn active(offset: i64) -> IdleCheck {
        IdleCheck {
            verdict: IdleVerdict::Active {
                clock: at(offset),
                newest_file: at(offset),
            },
            future: vec![],
        }
    }

    fn only(verdict: IdleVerdict) -> IdleCheck {
        IdleCheck {
            verdict,
            future: vec![],
        }
    }

    // =========================================================
    // Pins come first
    // =========================================================

    #[test]
    fn a_pinned_empty_piece_is_pinned_not_empty() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");

        assert_eq!(run(&tree), only(IdleVerdict::Pinned));
    }

    #[test]
    fn a_pinned_piece_with_old_files_is_pinned_not_idle() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        file(&mut tree, "old", -100 * DAY);

        assert_eq!(run(&tree), only(IdleVerdict::Pinned));
    }

    #[test]
    fn a_pinned_piece_with_only_future_files_is_pinned() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        file(&mut tree, "later", 5 * DAY);

        assert_eq!(run(&tree), only(IdleVerdict::Pinned));
    }

    #[test]
    fn a_pinned_piece_with_an_unreadable_directory_is_pinned() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        let locked = tree.unreadable_dir("piece/locked");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        assert_eq!(run(&tree), only(IdleVerdict::Pinned));
    }

    #[test]
    fn a_junk_list_that_names_the_pin_file_does_not_unpin() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        file(&mut tree, "old", -100 * DAY);
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let rules = Rules::new(&[], &[])
            .expect("no patterns")
            .bind(&root)
            .expect("binds");
        let junk = [Name::from_bytes(b".antiq-pin".to_vec()).expect("valid name")];
        let walked = walk_piece(&root, &path("piece"), &junk, &rules).expect("the walk succeeds");
        assert!(
            walked
                .entries()
                .iter()
                .all(|entry| !entry.path.to_string().ends_with(".antiq-pin")),
            "the walk leaves the pin out"
        );

        let checked =
            check(&root, &walked, None, &settings(14), at(0)).expect("the check succeeds");

        assert_eq!(checked, only(IdleVerdict::Pinned));
    }

    #[test]
    fn a_piece_the_walk_could_not_read_is_refused() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        let locked = tree.unreadable_dir("piece/locked");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let refused = check(&root, &walk(&root), None, &settings(14), at(0))
            .expect_err("a piece with problems has no verdict");

        assert!(matches!(refused, IdleError::Problems), "{refused}");
    }

    // =========================================================
    // Empty pieces
    // =========================================================

    #[test]
    fn a_piece_without_entries_is_empty() {
        let mut tree = Tree::new();
        tree.dir("piece");

        assert_eq!(run(&tree), only(IdleVerdict::Empty));
    }

    #[test]
    fn a_piece_of_directories_only_is_empty() {
        let mut tree = Tree::new();
        tree.dir("piece/a/b");

        assert_eq!(run(&tree), only(IdleVerdict::Empty));
    }

    #[test]
    fn a_piece_of_markers_and_junk_is_empty() {
        let mut tree = Tree::new();
        file(&mut tree, ".antiqignore", -100 * DAY);
        file(&mut tree, "sub/.antiq-pin", -100 * DAY);
        file(&mut tree, "sub/.antiqignore", -100 * DAY);
        file(&mut tree, ".DS_Store", -100 * DAY);
        file(&mut tree, "sub/.DS_Store", -100 * DAY);

        assert_eq!(run(&tree), only(IdleVerdict::Empty));
    }

    #[test]
    fn symlinks_and_fifos_are_no_files() {
        let mut tree = Tree::new();
        tree.symlink("piece/link", "nowhere");
        tree.fifo("piece/queue");

        assert_eq!(run(&tree), only(IdleVerdict::Empty));
    }

    #[test]
    fn a_piece_with_a_file_is_not_empty() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);

        assert_eq!(run(&tree), idle(-100 * DAY));
    }

    // =========================================================
    // What counts as activity
    // =========================================================

    #[test]
    fn the_newest_regular_file_sets_the_time() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "sub/newer", -40 * DAY);
        file(&mut tree, "sub/deep/oldest", -300 * DAY);

        assert_eq!(run(&tree), idle(-40 * DAY));
    }

    #[test]
    fn newer_marker_files_are_no_activity() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, ".antiqignore", -DAY);
        file(&mut tree, "sub/.antiq-pin", -DAY);
        file(&mut tree, "sub/.antiqignore", -DAY);

        assert_eq!(run(&tree), idle(-100 * DAY));
    }

    #[test]
    fn a_newer_directory_time_does_not_count() {
        let mut tree = Tree::new();
        file(&mut tree, "sub/old", -100 * DAY);
        let directory = tree.path("piece/sub");
        filetime::set_file_mtime(&directory, FileTime::from_unix_time(NOW - DAY, 0))
            .expect("the directory time is set");

        assert_eq!(run(&tree), idle(-100 * DAY));
    }

    #[test]
    fn a_newer_symlink_does_not_count() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        let link = tree.symlink("piece/link", "old");
        let time = FileTime::from_unix_time(NOW - DAY, 0);
        filetime::set_symlink_file_times(&link, time, time).expect("the link time is set");

        assert_eq!(run(&tree), idle(-100 * DAY));
    }

    #[test]
    fn a_newer_fifo_does_not_count() {
        // A fifo takes the time of its creation and the fixture has no way
        // to set another, so this test runs on the real clock.
        let mut tree = Tree::new();
        let now = Timestamp::now();
        let old = now.as_second() - 100 * DAY;
        tree.file_with("piece/old", "x", 0o644, old);
        tree.fifo("piece/queue");
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let checked =
            check(&root, &walk(&root), None, &settings(14), now).expect("the check succeeds");

        let old = Timestamp::from_second(old).expect("in range");
        let expected = IdleCheck {
            verdict: IdleVerdict::Idle {
                clock: old,
                newest_file: old,
            },
            future: vec![],
        };
        assert_eq!(checked, expected);
    }

    #[test]
    fn a_lone_junk_file_leaves_the_piece_empty() {
        let mut tree = Tree::new();
        file(&mut tree, ".DS_Store", -DAY);

        assert_eq!(run(&tree), only(IdleVerdict::Empty));
    }

    // =========================================================
    // The idle threshold
    // =========================================================

    #[test]
    fn a_file_exactly_idle_days_old_is_not_yet_old_enough() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -14 * DAY);

        assert_eq!(run(&tree), active(-14 * DAY));
    }

    #[test]
    fn a_file_one_second_older_than_idle_days_is_idle() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -14 * DAY - 1);

        assert_eq!(run(&tree), idle(-14 * DAY - 1));
    }

    #[test]
    fn with_zero_idle_days_a_file_from_this_second_is_active() {
        let mut tree = Tree::new();
        file(&mut tree, "f", 0);

        assert_eq!(run_with(&tree, None, 0), active(0));
    }

    #[test]
    fn with_zero_idle_days_a_file_from_the_second_before_is_idle() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -1);

        assert_eq!(run_with(&tree, None, 0), idle(-1));
    }

    #[test]
    fn a_time_before_1970_is_old() {
        let mut tree = Tree::new();
        tree.file_with("piece/f", "x", 0o644, -DAY);

        let expected = IdleCheck {
            verdict: IdleVerdict::Idle {
                clock: Timestamp::from_second(-DAY).expect("in range"),
                newest_file: Timestamp::from_second(-DAY).expect("in range"),
            },
            future: vec![],
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_time_before_what_a_timestamp_holds_is_old() {
        let mut tree = Tree::new();
        tree.dir("piece");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let ancient = TreeEntry {
            path: path("ancient"),
            kind: EntryKind::File,
            size: 0,
            allocated: 0,
            mtime_ns: i128::from(i64::MIN) * 1_000_000_000,
            ctime_ns: 0,
            inode: 1,
            device: 1,
            mode: 0o100644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
            mount_root: false,
        };
        let walked = PieceTree::new(path("piece"), vec![ancient], vec![]);

        let checked =
            check(&root, &walked, None, &settings(14), at(0)).expect("the check succeeds");

        let expected = IdleCheck {
            verdict: IdleVerdict::Idle {
                clock: Timestamp::MIN,
                newest_file: Timestamp::MIN,
            },
            future: vec![],
        };
        assert_eq!(checked, expected);
    }

    // =========================================================
    // Times in the future
    // =========================================================

    #[test]
    fn a_file_dated_two_days_ahead_does_not_count_and_is_listed() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "sub/later", 2 * DAY);

        let expected = IdleCheck {
            future: vec![path("sub/later")],
            ..idle(-100 * DAY)
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_piece_with_only_future_files_is_neither_empty_nor_idle() {
        let mut tree = Tree::new();
        file(&mut tree, "b", 3 * DAY);
        file(&mut tree, "a", 2 * DAY);

        let expected = IdleCheck {
            verdict: IdleVerdict::OnlyFuture,
            future: vec![path("a"), path("b")],
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_file_exactly_one_day_ahead_still_counts() {
        let mut tree = Tree::new();
        file(&mut tree, "f", DAY);

        assert_eq!(run(&tree), active(DAY));
    }

    #[test]
    fn a_file_one_second_past_one_day_ahead_is_future() {
        let mut tree = Tree::new();
        file(&mut tree, "f", DAY + 1);

        let expected = IdleCheck {
            verdict: IdleVerdict::OnlyFuture,
            future: vec![path("f")],
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_time_beyond_what_a_timestamp_holds_is_future() {
        let mut tree = Tree::new();
        tree.dir("piece");
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let far = TreeEntry {
            path: path("far"),
            kind: EntryKind::File,
            size: 0,
            allocated: 0,
            mtime_ns: i128::from(i64::MAX) * 1_000_000_000,
            ctime_ns: 0,
            inode: 1,
            device: 1,
            mode: 0o100644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
            mount_root: false,
        };
        let walked = PieceTree::new(path("piece"), vec![far], vec![]);

        let checked =
            check(&root, &walked, None, &settings(14), at(0)).expect("the check succeeds");

        let expected = IdleCheck {
            verdict: IdleVerdict::OnlyFuture,
            future: vec![path("far")],
        };
        assert_eq!(checked, expected);
    }

    // =========================================================
    // Unearthed pieces
    // =========================================================

    #[test]
    fn the_unearthing_time_wins_over_older_file_times() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);

        let expected = IdleCheck {
            verdict: IdleVerdict::Active {
                clock: at(-DAY),
                newest_file: at(-100 * DAY),
            },
            future: vec![],
        };
        assert_eq!(run_with(&tree, Some(at(-DAY)), 14), expected);
    }

    #[test]
    fn an_older_unearthing_time_leaves_the_file_times_in_charge() {
        let mut tree = Tree::new();
        file(&mut tree, "recent", -2 * DAY);

        assert_eq!(run_with(&tree, Some(at(-100 * DAY)), 14), active(-2 * DAY));
    }

    #[test]
    fn an_unearthed_piece_becomes_idle_once_the_unearthing_is_old() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -200 * DAY);

        let expected = IdleCheck {
            verdict: IdleVerdict::Idle {
                clock: at(-20 * DAY),
                newest_file: at(-200 * DAY),
            },
            future: vec![],
        };
        assert_eq!(run_with(&tree, Some(at(-20 * DAY)), 14), expected);
    }

    #[test]
    fn an_unearthed_piece_without_files_is_empty() {
        let mut tree = Tree::new();
        file(&mut tree, ".antiqignore", -DAY);

        assert_eq!(
            run_with(&tree, Some(at(-DAY)), 14),
            only(IdleVerdict::Empty)
        );
    }

    #[test]
    fn an_unearthed_piece_with_only_future_files_is_not_idle_either() {
        let mut tree = Tree::new();
        file(&mut tree, "later", 2 * DAY);

        let expected = IdleCheck {
            verdict: IdleVerdict::OnlyFuture,
            future: vec![path("later")],
        };
        assert_eq!(run_with(&tree, Some(at(-100 * DAY)), 14), expected);
    }

    #[test]
    fn an_unearthing_time_far_ahead_counts_as_one_day_ahead() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);

        let expected = IdleCheck {
            verdict: IdleVerdict::Active {
                clock: at(DAY),
                newest_file: at(-100 * DAY),
            },
            future: vec![],
        };
        assert_eq!(run_with(&tree, Some(at(10 * DAY)), 14), expected);
    }

    // =========================================================
    // Ignore rules
    // =========================================================

    fn lines(rules: &[&str]) -> Vec<String> {
        rules.iter().map(|rule| rule.to_string()).collect()
    }

    /// Runs the check with `collection.ignore` set to `rules`.
    fn run_ruled(tree: &Tree, rules: &[&str]) -> Result<IdleCheck, IdleError> {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let settings = Settings::new(14, &lines(rules))?;
        check(&root, &walk(&root), None, &settings, at(0))
    }

    fn ignore_file(tree: &mut Tree, relative: &str, content: impl AsRef<[u8]>) {
        tree.file_with(
            &format!("piece/{relative}"),
            content,
            0o644,
            NOW - 100 * DAY,
        );
    }

    #[test]
    fn a_config_rule_hides_what_is_below_an_ignored_directory() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "target/debug/new", -DAY);

        assert_eq!(
            run_ruled(&tree, &["target/"]).expect("checks"),
            idle(-100 * DAY)
        );
    }

    #[test]
    fn a_piece_whose_only_files_are_ignored_is_empty() {
        let mut tree = Tree::new();
        file(&mut tree, "target/a", -DAY);
        file(&mut tree, "target/b", -100 * DAY);

        assert_eq!(
            run_ruled(&tree, &["target/"]).expect("checks"),
            only(IdleVerdict::Empty)
        );
    }

    #[test]
    fn a_config_rule_hides_matching_files_at_any_depth() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "new.log", -DAY);
        file(&mut tree, "sub/deeper/new.log", -DAY);

        assert_eq!(
            run_ruled(&tree, &["*.log"]).expect("checks"),
            idle(-100 * DAY)
        );
    }

    #[test]
    fn a_config_rule_with_a_leading_slash_means_the_top_of_the_piece() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "build/new", -DAY);

        assert_eq!(
            run_ruled(&tree, &["/build"]).expect("checks"),
            idle(-100 * DAY)
        );
    }

    #[test]
    fn a_config_rule_with_a_leading_slash_leaves_deeper_names_alone() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "build/top", -DAY);
        file(&mut tree, "sub/build/deeper", -2 * DAY);

        assert_eq!(
            run_ruled(&tree, &["/build"]).expect("checks"),
            active(-2 * DAY)
        );
    }

    #[test]
    fn a_directory_rule_does_not_hide_a_file_of_that_name() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "b/data/inner", -DAY);
        file(&mut tree, "a/data", -2 * DAY);

        assert_eq!(
            run_ruled(&tree, &["data/"]).expect("checks"),
            active(-2 * DAY)
        );
    }

    #[test]
    fn a_negated_config_rule_keeps_a_file_visible() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "b.log", -DAY);
        file(&mut tree, "keep.log", -2 * DAY);

        assert_eq!(
            run_ruled(&tree, &["*.log", "!keep.log"]).expect("checks"),
            active(-2 * DAY)
        );
    }

    #[test]
    fn an_ignore_file_hides_with_its_anchored_rules_and_its_negation() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "sub/*.log\n!sub/keep.log\n");
        file(&mut tree, "sub/b.log", -DAY);
        file(&mut tree, "sub/keep.log", -40 * DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), idle(-40 * DAY));
    }

    #[test]
    fn a_nested_ignore_file_hides_with_the_same_rules_relative_to_its_directory() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "sub/.antiqignore", "*.log\n!keep.log\n");
        file(&mut tree, "sub/b.log", -DAY);
        file(&mut tree, "sub/keep.log", -40 * DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), idle(-40 * DAY));
    }

    #[test]
    fn a_nested_ignore_file_does_not_reach_its_siblings() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "sub/.antiqignore", "*.tmp\n");
        file(&mut tree, "sub/x.tmp", -DAY);
        file(&mut tree, "top.tmp", -2 * DAY);
        file(&mut tree, "other/y.tmp", -3 * DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), active(-2 * DAY));
    }

    #[test]
    fn a_nested_negation_overrides_an_outer_rule_for_files() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "*.log\n");
        ignore_file(&mut tree, "sub/.antiqignore", "!keep.log\n");
        file(&mut tree, "sub/keep.log", -2 * DAY);
        file(&mut tree, "sub/other.log", -DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), active(-2 * DAY));
    }

    #[test]
    fn an_ignore_file_ranks_above_the_config_rules() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "sub/.antiqignore", "!keep.log\n");
        file(&mut tree, "sub/keep.log", -2 * DAY);
        file(&mut tree, "sub/other.log", -DAY);

        assert_eq!(
            run_ruled(&tree, &["*.log"]).expect("checks"),
            active(-2 * DAY)
        );
    }

    #[test]
    fn a_directory_that_an_outer_rule_ignores_stays_ignored_whatever_is_inside() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "vendor/\n");
        ignore_file(&mut tree, "vendor/.antiqignore", "!new\n");
        file(&mut tree, "vendor/new", -DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), idle(-100 * DAY));
    }

    #[test]
    fn a_negation_cannot_bring_back_a_file_below_an_ignored_directory() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "vendor/\n!vendor/new\n");
        file(&mut tree, "vendor/new", -DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), idle(-100 * DAY));
    }

    #[test]
    fn an_ignore_file_does_not_reach_a_directory_whose_name_starts_like_its_own() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "sub/.antiqignore", "*.log\n");
        file(&mut tree, "sub/y.log", -DAY);
        file(&mut tree, "subway/x.log", -2 * DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), active(-2 * DAY));
    }

    #[test]
    fn a_percent_in_a_name_is_matched_as_it_is() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "100%.log\n");
        file(&mut tree, "100%.log", -DAY);
        file(&mut tree, "100%25.log", -2 * DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), active(-2 * DAY));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_that_is_not_utf8_is_matched_by_its_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "*.log\n");
        let name = std::ffi::OsStr::from_bytes(b"\xff.log");
        let odd = tree.path("piece").join(name);
        fs::write(&odd, "x").expect("the file is written");
        filetime::set_file_mtime(&odd, FileTime::from_unix_time(NOW - DAY, 0))
            .expect("the time is set");

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), idle(-100 * DAY));
    }

    #[test]
    fn an_ignore_file_with_a_byte_order_mark_applies_its_first_rule() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "\u{feff}*.log\n");
        file(&mut tree, "new.log", -DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), idle(-100 * DAY));
    }

    #[test]
    fn an_ignore_file_with_windows_line_ends_applies_every_rule() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "*.log\r\n*.tmp\r\n");
        file(&mut tree, "new.log", -DAY);
        file(&mut tree, "new.tmp", -DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), idle(-100 * DAY));
    }

    #[test]
    fn a_symlink_named_like_an_ignore_file_is_not_read() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "rules", "*.log\n");
        tree.symlink("piece/.antiqignore", "rules");
        file(&mut tree, "new.log", -DAY);

        assert_eq!(run_ruled(&tree, &[]).expect("checks"), active(-DAY));
    }

    #[test]
    fn a_broken_ignore_file_under_an_ignored_directory_is_never_read() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "vendor/.antiqignore", "broken\\\n");

        assert_eq!(
            run_ruled(&tree, &["vendor/"]).expect("checks"),
            idle(-100 * DAY)
        );
    }

    #[test]
    fn an_unreadable_ignore_file_under_an_ignored_directory_is_never_read() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "vendor/.antiqignore", "*.log\n");
        let locked = tree.path("piece/vendor/.antiqignore");
        fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("the mode is set");
        assert!(
            fs::read(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        assert_eq!(
            run_ruled(&tree, &["vendor/"]).expect("checks"),
            idle(-100 * DAY)
        );
    }

    #[test]
    fn a_bad_rule_in_an_ignore_file_is_reported_with_its_line() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, "sub/.antiqignore", "fine\nbroken\\\n");

        let error = run_ruled(&tree, &[]).expect_err("the rule is refused");

        assert!(
            matches!(&error, IdleError::BadRule { file: Some(file), line: 2, .. } if file.to_string() == "sub/.antiqignore"),
            "{error}"
        );
    }

    #[test]
    fn a_bad_rule_in_the_config_is_reported_with_its_line() {
        let mut tree = Tree::new();
        tree.dir("piece");

        let error = run_ruled(&tree, &["fine", "broken\\"]).expect_err("the rule is refused");

        assert!(
            matches!(
                &error,
                IdleError::BadRule {
                    file: None,
                    line: 2,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn an_ignore_file_that_is_not_text_is_reported() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", b"*.log\n\xff\xfe\n");

        let error = run_ruled(&tree, &[]).expect_err("the file is refused");

        assert!(
            matches!(&error, IdleError::NotUtf8 { path } if path.to_string() == ".antiqignore"),
            "{error}"
        );
    }

    #[test]
    fn an_ignore_file_that_cannot_be_read_is_reported() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        ignore_file(&mut tree, ".antiqignore", "*.log\n");
        let locked = tree.path("piece/.antiqignore");
        fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("the mode is set");
        assert!(
            fs::read(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        let error = run_ruled(&tree, &[]).expect_err("the file is refused");

        assert!(
            matches!(&error, IdleError::IgnoreFile { path, .. } if path.to_string() == ".antiqignore"),
            "{error}"
        );
    }

    #[test]
    fn a_bad_rule_names_where_it_came_from() {
        let from_config = IdleError::BadRule {
            file: None,
            line: 2,
            message: "dangling".to_string(),
        };
        let from_file = IdleError::BadRule {
            file: Some(path("sub/.antiqignore")),
            line: 7,
            message: "dangling".to_string(),
        };

        assert_eq!(
            from_config.to_string(),
            "collection.ignore line 2 is not a valid ignore rule: dangling"
        );
        assert_eq!(
            from_file.to_string(),
            "sub/.antiqignore line 7 is not a valid ignore rule: dangling"
        );
    }
}
