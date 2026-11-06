// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The idle verdict of a piece.

use crate::filesystem::tree::TreeRoot;
use crate::markers::{self, MarkerError, RESTORED_FILE};
use crate::pin::{self, PinError};
use crate::relpath::RelPath;
use crate::tree::{Class, EntryKind, PieceTree};
use jiff::Timestamp;

/// A day, in nanoseconds. Idle time is counted in 24-hour days.
const DAY_NS: i128 = 86_400_000_000_000;

/// Errors of the idle check.
#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("the piece has entries the walk could not read or may not enter")]
    Problems,
    #[error(transparent)]
    Pin(#[from] PinError),
    #[error(transparent)]
    Marker(#[from] MarkerError),
}

impl crate::error::HasKind for IdleError {}

/// What the idle check makes of a piece. `clock` is the idle clock: the
/// later of the newest counted file time and the time in
/// `.antiq-restored`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleVerdict {
    /// The piece carries a pin; nothing else is looked at.
    Pinned,
    /// No file counts: the ignore and timeless rules and the control files
    /// leave none.
    Empty,
    /// Not due, and not due within `soon_days` either.
    Active { clock: Timestamp },
    /// Not due yet, but due in at most `soon_days`.
    Soon { clock: Timestamp },
    /// The idle clock is older than `idle_days`.
    Idle { clock: Timestamp },
}

/// The verdict and the paths, relative to the piece's top, of the files
/// and markers whose times lie in the future. They count like any other
/// time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleCheck {
    pub verdict: IdleVerdict,
    pub future: Vec<RelPath>,
}

/// What the check reads from the config. The ignore and timeless rules are
/// the walk's: the check counts the entries the walk classified as
/// [`Class::Counted`].
#[derive(Debug, Clone)]
pub struct Settings {
    idle_days: u32,
    soon_days: u32,
}

impl Settings {
    pub fn new(idle_days: u32, soon_days: u32) -> Settings {
        Settings {
            idle_days,
            soon_days,
        }
    }
}

/// Decides whether the piece in `tree` is pinned, empty, active, soon or
/// idle at `now`.
///
/// The pin is looked up on disk, not in `tree`, so that the answer is the
/// one `pin::is_pinned` gives to the `pin` commands, whatever an ignore
/// rule made of it in the walk. A pinned piece is not looked at further.
/// A piece the walk could not fully read has no verdict, and neither has
/// one whose `.antiq-restored` cannot be read or parsed.
pub fn check(
    collection: &TreeRoot,
    tree: &PieceTree,
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
    let Times {
        newest,
        marker_ns,
        future,
    } = times(collection, tree, now.as_nanosecond())?;

    // The marker's time never makes a piece non-empty: a piece without a
    // countable file is empty whatever the marker says.
    let Some(newest) = newest else {
        return Ok(IdleCheck {
            verdict: IdleVerdict::Empty,
            future,
        });
    };
    let now_ns = now.as_nanosecond();
    let clock_ns = marker_ns.map_or(newest, |time| newest.max(time));
    let clock = timestamp(clock_ns);
    let left_ns = i128::from(settings.idle_days) * DAY_NS - (now_ns - clock_ns);
    let verdict = if left_ns < 0 {
        IdleVerdict::Idle { clock }
    } else if left_ns <= i128::from(settings.soon_days) * DAY_NS {
        IdleVerdict::Soon { clock }
    } else {
        IdleVerdict::Active { clock }
    };
    Ok(IdleCheck { verdict, future })
}

/// The idle clock of a piece and the paths of its files and markers whose
/// times lie in the future, whatever the verdict is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleClock {
    /// `None` when no file counts.
    pub clock: Option<Timestamp>,
    pub future: Vec<RelPath>,
}

/// The idle clock of the piece in `tree` at `now`, also for a pinned piece
/// and for one the walk could not read completely, which have no verdict.
/// The clock of an incomplete tree is the newest time among the entries
/// the walk did see.
pub fn clock(
    collection: &TreeRoot,
    tree: &PieceTree,
    now: Timestamp,
) -> Result<IdleClock, MarkerError> {
    let Times {
        newest,
        marker_ns,
        future,
    } = times(collection, tree, now.as_nanosecond())?;
    let clock = newest.map(|newest| timestamp(marker_ns.map_or(newest, |time| newest.max(time))));
    Ok(IdleClock { clock, future })
}

/// What the entries and the marker of a piece say about time, in
/// nanoseconds since the epoch.
struct Times {
    /// The newest time of a counted file.
    newest: Option<i128>,
    /// The time in `.antiq-restored`.
    marker_ns: Option<i128>,
    /// The files and the marker whose times lie after `now`, sorted.
    future: Vec<RelPath>,
}

fn times(collection: &TreeRoot, tree: &PieceTree, now_ns: i128) -> Result<Times, MarkerError> {
    let restored = markers::read_restored(collection, tree.top())?;

    // Times are compared as nanoseconds: a time the file system reports
    // can lie outside what a `Timestamp` holds.
    let mut future = Vec::new();
    let mut newest: Option<i128> = None;
    for entry in tree.entries() {
        if entry.kind != EntryKind::File || entry.class != Class::Counted {
            continue;
        }
        if entry.mtime_ns > now_ns {
            future.push(entry.path.clone());
        }
        newest = Some(newest.map_or(entry.mtime_ns, |seen| seen.max(entry.mtime_ns)));
    }
    let marker_ns = restored.map(|restored| restored.time.as_nanosecond());
    if marker_ns.is_some_and(|time| time > now_ns) {
        future.push(RelPath::parse(RESTORED_FILE).expect("the marker name is a valid path"));
    }
    future.sort();
    Ok(Times {
        newest,
        marker_ns,
        future,
    })
}

/// The timestamp for `ns` nanoseconds since the epoch; a time that
/// `Timestamp` cannot hold becomes its start or its end.
fn timestamp(ns: i128) -> Timestamp {
    Timestamp::from_nanosecond(ns).unwrap_or(if ns < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::PieceRules;
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
        Settings::new(idle_days, 3)
    }

    fn walk_ruled(root: &TreeRoot, ignore: &[&str]) -> PieceTree {
        let ignore: Vec<String> = ignore.iter().map(|rule| rule.to_string()).collect();
        let piece_rules = PieceRules::new(&ignore, &[]).expect("valid rules");
        let select = Rules::new(&[], &[])
            .expect("no patterns")
            .bind(root)
            .expect("binds");
        walk_piece(root, &path("piece"), &select, &piece_rules).expect("the walk succeeds")
    }

    fn walk(root: &TreeRoot) -> PieceTree {
        walk_ruled(root, &[".DS_Store"])
    }

    /// Runs the check with `.antiq-restored` holding `restored`, if given.
    fn run_with(tree: &Tree, restored: Option<Timestamp>, idle_days: u32) -> IdleCheck {
        if let Some(time) = restored {
            let line = format!("restored {}\n", time.strftime("%Y-%m-%dT%H:%M:%SZ"));
            fs::write(tree.path("piece/.antiq-restored"), line).expect("the marker is written");
        }
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        check(&root, &walk(&root), &settings(idle_days), at(0)).expect("the check succeeds")
    }

    fn run(tree: &Tree) -> IdleCheck {
        run_with(tree, None, 14)
    }

    fn idle(offset: i64) -> IdleCheck {
        only(IdleVerdict::Idle { clock: at(offset) })
    }

    fn soon(offset: i64) -> IdleCheck {
        only(IdleVerdict::Soon { clock: at(offset) })
    }

    fn active(offset: i64) -> IdleCheck {
        only(IdleVerdict::Active { clock: at(offset) })
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
    fn an_ignore_rule_that_names_the_pin_file_does_not_unpin() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        file(&mut tree, "old", -100 * DAY);
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let walked = walk_ruled(&root, &[".antiq-pin"]);
        let pin = walked
            .entries()
            .iter()
            .find(|entry| entry.path.to_string() == ".antiq-pin")
            .expect("the walk records the pin");
        assert_eq!(pin.class, Class::Control);

        let checked = check(&root, &walked, &settings(14), at(0)).expect("the check succeeds");

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

        let refused = check(&root, &walk(&root), &settings(14), at(0))
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
    fn a_piece_of_control_files_and_junk_is_empty() {
        let mut tree = Tree::new();
        file(&mut tree, ".antiqignore", -100 * DAY);
        tree.file("piece/.antiq-restored", "restored 2000-01-01T00:00:00Z\n");
        file(&mut tree, "sub/.antiq-timeless", -100 * DAY);
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
    fn newer_control_files_are_no_activity() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, ".antiqignore", -DAY);
        tree.file("piece/.antiq-restored", "restored 2000-01-01T00:00:00Z\n");
        file(&mut tree, "sub/.antiq-timeless", -DAY);
        file(&mut tree, "sub/.antiqignore", -DAY);

        assert_eq!(run(&tree), idle(-100 * DAY));
    }

    #[test]
    fn a_pin_file_below_the_top_is_an_ordinary_file() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "sub/.antiq-pin", -DAY);

        assert_eq!(run(&tree), active(-DAY));
    }

    #[test]
    fn timeless_files_are_no_activity() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, ".git/HEAD", -DAY);
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        let timeless = vec![".git/".to_string()];
        let piece_rules = PieceRules::new(&[], &timeless).expect("valid rules");
        let select = Rules::new(&[], &[])
            .expect("no patterns")
            .bind(&root)
            .expect("binds");
        let walked = walk_piece(&root, &path("piece"), &select, &piece_rules).expect("walks");

        let checked = check(&root, &walked, &settings(14), at(0)).expect("checks");

        assert_eq!(checked, idle(-100 * DAY));
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

        let checked = check(&root, &walk(&root), &settings(14), now).expect("the check succeeds");

        let old = Timestamp::from_second(old).expect("in range");
        assert_eq!(checked, only(IdleVerdict::Idle { clock: old }));
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

        assert_eq!(run(&tree), soon(-14 * DAY));
    }

    #[test]
    fn a_file_one_second_older_than_idle_days_is_idle() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -14 * DAY - 1);

        assert_eq!(run(&tree), idle(-14 * DAY - 1));
    }

    #[test]
    fn with_zero_idle_days_a_file_from_this_second_is_not_due() {
        let mut tree = Tree::new();
        file(&mut tree, "f", 0);

        assert_eq!(run_with(&tree, None, 0), soon(0));
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

        let expected = only(IdleVerdict::Idle {
            clock: Timestamp::from_second(-DAY).expect("in range"),
        });
        assert_eq!(run(&tree), expected);
    }

    /// A walked piece that holds the one file `entry`.
    fn walked_with(mtime_ns: i128) -> (Tree, PieceTree) {
        let mut tree = Tree::new();
        tree.dir("piece");
        let entry = TreeEntry {
            path: path("only"),
            kind: EntryKind::File,
            size: 0,
            allocated: 0,
            mtime_ns,
            ctime_ns: 0,
            inode: 1,
            device: 1,
            mode: 0o100644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
            mount_root: false,
            uid: 0,
            flag_locked: false,
            class: crate::tree::Class::Counted,
        };
        let walked = PieceTree::new(path("piece"), vec![entry], vec![]);
        (tree, walked)
    }

    #[test]
    fn a_time_before_what_a_timestamp_holds_is_old() {
        let (tree, walked) = walked_with(i128::from(i64::MIN) * 1_000_000_000);
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let checked = check(&root, &walked, &settings(14), at(0)).expect("the check succeeds");

        assert_eq!(
            checked,
            only(IdleVerdict::Idle {
                clock: Timestamp::MIN
            })
        );
    }

    #[test]
    fn a_time_beyond_what_a_timestamp_holds_is_in_the_future_and_clamps_the_clock() {
        let (tree, walked) = walked_with(i128::from(i64::MAX) * 1_000_000_000);
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let checked = check(&root, &walked, &settings(14), at(0)).expect("the check succeeds");

        let expected = IdleCheck {
            verdict: IdleVerdict::Active {
                clock: Timestamp::MAX,
            },
            future: vec![path("only")],
        };
        assert_eq!(checked, expected);
    }

    // =========================================================
    // Soon
    // =========================================================

    #[test]
    fn a_piece_due_in_exactly_soon_days_is_soon() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -11 * DAY);

        assert_eq!(run(&tree), soon(-11 * DAY));
    }

    #[test]
    fn a_piece_due_in_one_second_more_than_soon_days_is_active() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -11 * DAY + 1);

        assert_eq!(run(&tree), active(-11 * DAY + 1));
    }

    #[test]
    fn with_zero_soon_days_no_piece_is_soon_before_the_last_second() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -14 * DAY + 1);
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let checked = check(&root, &walk(&root), &Settings::new(14, 0), at(0)).expect("checks");

        assert_eq!(checked, active(-14 * DAY + 1));
    }

    #[test]
    fn when_soon_days_reach_idle_days_every_piece_that_is_not_due_is_soon() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -DAY);
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let checked = check(&root, &walk(&root), &Settings::new(14, 20), at(0)).expect("checks");

        assert_eq!(checked, soon(-DAY));
    }

    #[test]
    fn a_clock_in_the_future_is_active_with_the_default_soon_days() {
        let mut tree = Tree::new();
        file(&mut tree, "f", 20 * DAY);

        let expected = IdleCheck {
            future: vec![path("f")],
            ..active(20 * DAY)
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_due_piece_is_idle_whatever_soon_days_say() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -15 * DAY);
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let checked = check(&root, &walk(&root), &Settings::new(14, 99), at(0)).expect("checks");

        assert_eq!(checked, idle(-15 * DAY));
    }

    #[test]
    fn day_counts_at_the_top_of_their_range_do_not_overflow() {
        let mut tree = Tree::new();
        file(&mut tree, "f", -DAY);
        let root = TreeRoot::open(tree.root()).expect("the root opens");

        let checked = check(
            &root,
            &walk(&root),
            &Settings::new(u32::MAX, u32::MAX),
            at(0),
        )
        .expect("checks");

        assert_eq!(checked, soon(-DAY));
    }

    // =========================================================
    // Times in the future
    // =========================================================

    #[test]
    fn a_file_one_hour_ahead_counts_and_is_listed() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "sub/later", 3600);

        let expected = IdleCheck {
            future: vec![path("sub/later")],
            ..active(3600)
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_file_two_days_ahead_holds_the_piece_back_and_is_listed() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        file(&mut tree, "sub/later", 2 * DAY);

        let expected = IdleCheck {
            future: vec![path("sub/later")],
            ..active(2 * DAY)
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_piece_with_only_future_files_lists_them_sorted_and_is_not_due() {
        let mut tree = Tree::new();
        file(&mut tree, "b", 3 * DAY);
        file(&mut tree, "a", 2 * DAY);

        let expected = IdleCheck {
            future: vec![path("a"), path("b")],
            ..active(3 * DAY)
        };
        assert_eq!(run(&tree), expected);
    }

    #[test]
    fn a_file_from_this_very_second_is_not_future() {
        let mut tree = Tree::new();
        file(&mut tree, "f", 0);

        assert_eq!(run(&tree), active(0));
    }

    #[test]
    fn a_file_one_second_ahead_is_future() {
        let mut tree = Tree::new();
        file(&mut tree, "f", 1);

        let expected = IdleCheck {
            future: vec![path("f")],
            ..active(1)
        };
        assert_eq!(run(&tree), expected);
    }

    // =========================================================
    // The marker .antiq-restored
    // =========================================================

    #[test]
    fn the_marker_time_wins_over_older_file_times() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);

        assert_eq!(run_with(&tree, Some(at(-DAY)), 14), active(-DAY));
    }

    #[test]
    fn an_older_marker_time_leaves_the_file_times_in_charge() {
        let mut tree = Tree::new();
        file(&mut tree, "recent", -2 * DAY);

        assert_eq!(run_with(&tree, Some(at(-100 * DAY)), 14), active(-2 * DAY));
    }

    #[test]
    fn a_piece_becomes_idle_once_the_marker_is_old() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -200 * DAY);

        assert_eq!(run_with(&tree, Some(at(-20 * DAY)), 14), idle(-20 * DAY));
    }

    #[test]
    fn a_piece_with_a_marker_and_no_files_is_empty() {
        let mut tree = Tree::new();
        file(&mut tree, ".antiqignore", -DAY);

        assert_eq!(
            run_with(&tree, Some(at(-DAY)), 14),
            only(IdleVerdict::Empty)
        );
    }

    #[test]
    fn a_piece_of_directories_and_a_recent_marker_is_empty() {
        let mut tree = Tree::new();
        tree.dir("piece/a/b");

        assert_eq!(run_with(&tree, Some(at(-1)), 14), only(IdleVerdict::Empty));
    }

    #[test]
    fn a_marker_time_far_ahead_counts_in_full_and_is_listed() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);

        let expected = IdleCheck {
            future: vec![path(".antiq-restored")],
            ..active(10 * DAY)
        };
        assert_eq!(run_with(&tree, Some(at(10 * DAY)), 14), expected);
    }

    #[test]
    fn a_future_marker_of_an_empty_piece_is_listed() {
        let mut tree = Tree::new();
        tree.dir("piece");

        let expected = IdleCheck {
            verdict: IdleVerdict::Empty,
            future: vec![path(".antiq-restored")],
        };
        assert_eq!(run_with(&tree, Some(at(DAY)), 14), expected);
    }

    #[test]
    fn a_future_file_and_a_future_marker_are_listed_in_path_order() {
        let mut tree = Tree::new();
        file(&mut tree, "z", DAY);
        file(&mut tree, "a", DAY);

        let expected = IdleCheck {
            future: vec![path(".antiq-restored"), path("a"), path("z")],
            ..active(2 * DAY)
        };
        assert_eq!(run_with(&tree, Some(at(2 * DAY)), 14), expected);
    }

    fn marker_failure(tree: &Tree) -> IdleError {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        check(&root, &walk(&root), &settings(14), at(0)).expect_err("the marker is refused")
    }

    #[test]
    fn an_unparsable_marker_refuses_the_piece_naming_the_file() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        tree.file("piece/.antiq-restored", "yesterday\n");

        let refused = marker_failure(&tree);

        assert!(matches!(refused, IdleError::Marker(_)), "{refused}");
        assert!(refused.to_string().contains("piece/.antiq-restored"));
    }

    #[test]
    fn an_empty_piece_with_an_unparsable_marker_is_refused_not_empty() {
        let mut tree = Tree::new();
        tree.dir("piece");
        tree.file("piece/.antiq-restored", "yesterday\n");

        assert!(matches!(marker_failure(&tree), IdleError::Marker(_)));
    }

    #[test]
    fn a_pinned_piece_with_an_unparsable_marker_is_pinned() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        tree.file("piece/.antiq-restored", "yesterday\n");

        assert_eq!(run(&tree), only(IdleVerdict::Pinned));
    }

    #[test]
    fn a_piece_with_problems_and_an_unparsable_marker_has_the_problems() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-restored", "yesterday\n");
        let locked = tree.unreadable_dir("piece/locked");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        assert!(matches!(marker_failure(&tree), IdleError::Problems));
    }

    #[test]
    fn a_fifo_named_like_the_pin_refuses_the_piece() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        tree.fifo("piece/.antiq-pin");

        assert!(matches!(marker_failure(&tree), IdleError::Problems));
    }

    // =========================================================
    // Ignore rules
    // =========================================================

    /// Runs the check with `collection.ignore` set to `rules`.
    fn run_ruled(tree: &Tree, rules: &[&str]) -> Result<IdleCheck, IdleError> {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        check(&root, &walk_ruled(&root, rules), &settings(14), at(0))
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

    // =========================================================
    // The clock without a verdict
    // =========================================================

    fn clock_of(tree: &Tree) -> Result<IdleClock, MarkerError> {
        let root = TreeRoot::open(tree.root()).expect("the root opens");
        clock(&root, &walk(&root), at(0))
    }

    #[test]
    fn a_pinned_piece_has_its_clock() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        file(&mut tree, "old", -100 * DAY);

        let found = clock_of(&tree).expect("the clock is read");

        assert_eq!(found.clock, Some(at(-100 * DAY)));
    }

    #[test]
    fn the_clock_takes_a_later_marker_time() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        tree.file("piece/.antiq-restored", "restored 2033-05-18T03:33:20Z\n");

        let found = clock_of(&tree).expect("the clock is read");

        // NOW is 2033-05-18T03:33:20Z.
        assert_eq!(found.clock, Some(at(0)));
    }

    #[test]
    fn a_piece_without_a_counted_file_has_no_clock() {
        let mut tree = Tree::new();
        tree.dir("piece/a");
        tree.file("piece/.antiq-restored", "restored 2033-05-18T03:33:20Z\n");

        let found = clock_of(&tree).expect("the clock is read");

        assert_eq!(found.clock, None);
    }

    #[test]
    fn a_piece_the_walk_could_not_read_has_the_clock_of_what_was_seen() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        let locked = tree.unreadable_dir("piece/locked");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        let found = clock_of(&tree).expect("the clock is read");

        assert_eq!(found.clock, Some(at(-100 * DAY)));
    }

    #[test]
    fn a_pinned_piece_lists_its_future_files_and_marker() {
        let mut tree = Tree::new();
        tree.file("piece/.antiq-pin", "");
        file(&mut tree, "later", 5 * DAY);
        tree.file("piece/.antiq-restored", "restored 2033-05-19T03:33:20Z\n");

        let found = clock_of(&tree).expect("the clock is read");

        assert_eq!(found.future, [path(".antiq-restored"), path("later")]);
    }

    #[test]
    fn an_unparsable_marker_fails_the_clock() {
        let mut tree = Tree::new();
        file(&mut tree, "old", -100 * DAY);
        tree.file("piece/.antiq-restored", "garbage\n");

        let error = clock_of(&tree).expect_err("the marker does not parse");

        assert!(matches!(error, MarkerError::Invalid { .. }), "{error}");
    }
}
