// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The idle verdict of a piece.

use crate::fsx::tree::TreeRoot;
use crate::pin::{self, PinError};
use crate::relpath::RelPath;
use crate::tree::{EntryKind, PieceTree};
use jiff::Timestamp;

/// Errors of the idle check.
#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("the piece has entries the walk could not read or may not enter")]
    Problems,
    #[error(transparent)]
    Pin(#[from] PinError),
}

impl crate::error::HasKind for IdleError {}

/// What the idle check makes of a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleVerdict {
    Pinned,
    Empty,
    OnlyFuture,
    Active {
        clock: Timestamp,
        newest_file: Timestamp,
    },
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
}

impl Settings {
    pub fn new(idle_days: u32) -> Settings {
        Settings { idle_days }
    }
}

/// A day, in nanoseconds. Idle time is counted in 24-hour days.
const DAY_NS: i128 = 86_400_000_000_000;

/// Names whose changes are no activity, at any level of a piece.
const MARKER_NAMES: [&[u8]; 2] = [b".antiqignore", b".antiq-pin"];

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
    let mut candidates = 0usize;
    let mut future = Vec::new();
    let mut newest: Option<i128> = None;
    for entry in tree.entries() {
        let is_marker = entry
            .path
            .components()
            .last()
            .is_some_and(|name| MARKER_NAMES.contains(&name));
        if entry.kind != EntryKind::File || is_marker {
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
        check(
            &root,
            &walk(&root),
            unearthed,
            &Settings::new(idle_days),
            at(0),
        )
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
            check(&root, &walked, None, &Settings::new(14), at(0)).expect("the check succeeds");

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

        let refused = check(&root, &walk(&root), None, &Settings::new(14), at(0))
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
            check(&root, &walk(&root), None, &Settings::new(14), now).expect("the check succeeds");

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
        };
        let walked = PieceTree::new(path("piece"), vec![ancient], vec![]);

        let checked =
            check(&root, &walked, None, &Settings::new(14), at(0)).expect("the check succeeds");

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
        };
        let walked = PieceTree::new(path("piece"), vec![far], vec![]);

        let checked =
            check(&root, &walked, None, &Settings::new(14), at(0)).expect("the check succeeds");

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
}
