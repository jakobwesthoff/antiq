// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The survey that judges every live piece as due, soon, pinned or refused.

use crate::collection::Collection;
use crate::error::HasKind;
use crate::filesystem::tree::TreeRoot;
use crate::idle::{self, IdleCheck, IdleError, IdleVerdict};
use crate::markers::{self, RESTORED_FILE};
use crate::pin::PIN_FILE;
use crate::relpath::{Name, RelPath};
use crate::rules::{PieceRules, RulesError};
use crate::select::{Rules, SelectError, select};
use crate::shown::Shown;
use crate::tree::{PieceTree, Problem};
use crate::walk::{Hooks, RealFs, WalkError, walk_piece_with};
use jiff::Timestamp;
use std::fmt;

/// A day, in nanoseconds. Idle time is counted in 24-hour days.
const DAY_NS: i128 = 86_400_000_000_000;

/// Why a survey of the live pieces failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum SurveyError {
    #[error("cannot open the collection {path}: {source}")]
    Root {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot select the pieces: {0}")]
    Select(
        #[from]
        #[diagnostic_source]
        SelectError,
    ),
    #[error("cannot use the ignore and timeless rules: {0}")]
    Rules(
        #[from]
        #[diagnostic_source]
        RulesError,
    ),
    #[error("cannot walk a piece: {0}")]
    Walk(
        #[from]
        #[diagnostic_source]
        WalkError,
    ),
}

impl HasKind for SurveyError {}

/// What the survey found: every live piece, sorted by path, and the
/// directories selection could not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Survey {
    pub pieces: Vec<SurveyedPiece>,
    pub unreadable: Vec<RelPath>,
}

/// One live piece with its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurveyedPiece {
    pub piece: RelPath,
    pub verdict: Verdict,
    /// The idle clock, when it can be told: not for a piece without a
    /// counted file, not for a tree the walk could not read completely and
    /// not when the marker cannot be read.
    pub clock: Option<Timestamp>,
    /// Files and markers of the piece with a time after the survey's `now`.
    pub future: Vec<FutureTime>,
}

/// A file or marker whose time lies in the future.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FutureTime {
    /// Relative to the collection root.
    pub path: RelPath,
    pub time: Timestamp,
}

/// What the survey makes of a piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Neither due nor soon, or without a file that counts.
    Plain,
    /// Due in at most `soon_days`; `days_left` counts started days, at
    /// least one.
    Soon {
        days_left: u32,
    },
    Due,
    Pinned,
    /// `archive` refuses the piece, for every reason in the list.
    Refused(Vec<Refusal>),
}

/// Why `archive` refuses a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    MountPoint,
    NestedCollection,
    UnreadableEntry,
    ExcludeEntry,
    Locked,
    RuleFile,
    NotAFile,
    UnreadableMarker,
}

impl RefusalKind {
    /// The name of the kind in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            RefusalKind::MountPoint => "mount_point",
            RefusalKind::NestedCollection => "nested_collection",
            RefusalKind::UnreadableEntry => "unreadable_entry",
            RefusalKind::ExcludeEntry => "exclude_entry",
            RefusalKind::Locked => "locked",
            RefusalKind::RuleFile => "rule_file",
            RefusalKind::NotAFile => "not_a_file",
            RefusalKind::UnreadableMarker => "unreadable_marker",
        }
    }
}

/// One reason for a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: RefusalKind,
    /// Relative to the collection root.
    pub path: RelPath,
    /// The line of a rule file at fault.
    pub line: Option<usize>,
}

impl Refusal {
    /// The refusal a walk problem of the piece at `top` stands for. The
    /// walk reports paths relative to the top, except for a locked parent,
    /// which it reports relative to the collection.
    pub fn of(top: &RelPath, problem: &Problem) -> Refusal {
        let (kind, path, line) = match problem {
            Problem::Unreadable { first } => {
                (RefusalKind::UnreadableEntry, top.concat(first), None)
            }
            Problem::MountPoint { other } => (RefusalKind::MountPoint, top.concat(other), None),
            Problem::Excluded { path } => (RefusalKind::ExcludeEntry, top.concat(path), None),
            Problem::NestedCollection { root } => {
                (RefusalKind::NestedCollection, top.concat(root), None)
            }
            Problem::Locked { path } => (RefusalKind::Locked, top.concat(path), None),
            Problem::LockedParent { path } => (RefusalKind::Locked, path.clone(), None),
            Problem::RuleFile { path, line } => (RefusalKind::RuleFile, top.concat(path), *line),
            Problem::MarkerNotAFile { name } => (RefusalKind::NotAFile, top.join(name), None),
        };
        Refusal { kind, path, line }
    }

    pub(crate) fn unreadable_marker(top: &RelPath, file: &str) -> Refusal {
        let name = Name::from_bytes(file.as_bytes().to_vec()).expect("a marker name is a name");
        Refusal {
            kind: RefusalKind::UnreadableMarker,
            path: top.join(&name),
            line: None,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The collection root has no name of its own to show.
        let shown = if self.path.is_root() {
            ".".to_string()
        } else {
            self.path.to_string()
        };
        match self.kind {
            RefusalKind::MountPoint => write!(f, "mount point: {shown}"),
            RefusalKind::NestedCollection => write!(f, "nested collection: {shown}"),
            RefusalKind::UnreadableEntry => write!(f, "unreadable entry: {shown}"),
            RefusalKind::ExcludeEntry => write!(f, "exclude entry: {shown}"),
            RefusalKind::Locked => write!(f, "locked: {shown}"),
            RefusalKind::RuleFile => match self.line {
                Some(line) => write!(f, "rule file: {shown}:{line}"),
                None => write!(f, "rule file: {shown}"),
            },
            RefusalKind::NotAFile => {
                let name = self.path.components().last().unwrap_or_default();
                write!(f, "{} is not a file", crate::relpath::escape(name))
            }
            RefusalKind::UnreadableMarker => write!(f, "unreadable marker: {shown}"),
        }
    }
}

/// Judges every live piece of `collection` at `now`.
pub fn survey(collection: &Collection, now: Timestamp) -> Result<Survey, SurveyError> {
    survey_with(collection, now, &RealFs)
}

pub(crate) fn survey_with(
    collection: &Collection,
    now: Timestamp,
    hooks: &dyn Hooks,
) -> Result<Survey, SurveyError> {
    let config = &collection.config().collection;
    let root = TreeRoot::open(collection.root()).map_err(|source| SurveyError::Root {
        path: Shown(&collection.root()).to_string(),
        source,
    })?;
    let select_rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    let selection = select(&root, &select_rules)?;
    let piece_rules = PieceRules::new(&config.ignore, &config.timeless)?;
    let settings = idle::Settings::new(config.idle_days, config.soon_days);

    let mut pieces = Vec::new();
    for top in selection.pieces() {
        let tree = walk_piece_with(&root, top, &select_rules, &piece_rules, hooks)?;
        pieces.push(judge(&root, &tree, &settings, config.idle_days, now));
    }
    pieces.sort_by(|a, b| a.piece.cmp(&b.piece));
    Ok(Survey {
        pieces,
        unreadable: selection.unreadable().to_vec(),
    })
}

/// The verdict of the piece in `tree`.
///
/// The idle check decides: it looks at the pin first, then refuses a tree
/// with problems, and reads the marker only when the tree is sound. Status
/// shows the idle clock and the future times of every piece, so the ones
/// the check gives none for are asked again through [`idle::clock`].
fn judge(
    root: &TreeRoot,
    tree: &PieceTree,
    settings: &idle::Settings,
    idle_days: u32,
    now: Timestamp,
) -> SurveyedPiece {
    let top = tree.top();
    let unreadable_tree = tree
        .problems()
        .iter()
        .any(|problem| matches!(problem, Problem::Unreadable { .. }));
    // An incomplete tree only shows the times of the entries the walk saw,
    // so its clock would mislead.
    let without_verdict = || {
        let found = idle::clock(root, tree, now).ok();
        let clock = found.as_ref().and_then(|found| found.clock);
        let future = found.map(|found| found.future).unwrap_or_default();
        (if unreadable_tree { None } else { clock }, future)
    };

    let (verdict, clock, future) = match idle::check(root, tree, settings, now) {
        Ok(IdleCheck { verdict, future }) => match verdict {
            IdleVerdict::Pinned => {
                let (clock, future) = without_verdict();
                (Verdict::Pinned, clock, future)
            }
            IdleVerdict::Empty => (Verdict::Plain, None, future),
            IdleVerdict::Active { clock } => (Verdict::Plain, Some(clock), future),
            IdleVerdict::Soon { clock } => {
                let verdict = Verdict::Soon {
                    days_left: days_left(clock, idle_days, now),
                };
                (verdict, Some(clock), future)
            }
            IdleVerdict::Idle { clock } => (Verdict::Due, Some(clock), future),
        },
        Err(IdleError::Problems) => {
            let (clock, future) = without_verdict();
            let reasons = tree
                .problems()
                .iter()
                .map(|problem| Refusal::of(top, problem))
                .collect();
            (Verdict::Refused(reasons), clock, future)
        }
        // The pin file could not be looked at, so the tree's own problems
        // come first and the marker last.
        Err(IdleError::Pin(_)) => {
            let (clock, future) = without_verdict();
            let mut reasons: Vec<Refusal> = tree
                .problems()
                .iter()
                .map(|problem| Refusal::of(top, problem))
                .collect();
            reasons.push(Refusal::unreadable_marker(top, PIN_FILE));
            (Verdict::Refused(reasons), clock, future)
        }
        Err(IdleError::Marker(_)) => {
            let reasons = vec![Refusal::unreadable_marker(top, RESTORED_FILE)];
            (Verdict::Refused(reasons), None, Vec::new())
        }
    };

    SurveyedPiece {
        piece: top.clone(),
        verdict,
        clock,
        future: future_times(root, tree, &future),
    }
}

/// The started days until a piece with `clock` is due.
fn days_left(clock: Timestamp, idle_days: u32, now: Timestamp) -> u32 {
    let left = i128::from(idle_days) * DAY_NS - (now.as_nanosecond() - clock.as_nanosecond());
    let days = (left + DAY_NS - 1).div_euclid(DAY_NS).max(1);
    u32::try_from(days).expect("the days left are at most idle_days")
}

/// The times of the files and markers in `paths`, which are relative to
/// the top of `tree`.
fn future_times(root: &TreeRoot, tree: &PieceTree, paths: &[RelPath]) -> Vec<FutureTime> {
    let top = tree.top();
    paths
        .iter()
        .filter_map(|path| {
            let ns = if path.as_bytes() == RESTORED_FILE.as_bytes() {
                markers::read_restored(root, top)
                    .ok()
                    .flatten()?
                    .time
                    .as_nanosecond()
            } else {
                tree.entries().iter().find(|e| &e.path == path)?.mtime_ns
            };
            let time = Timestamp::from_nanosecond(ns).unwrap_or(Timestamp::MAX);
            Some(FutureTime {
                path: top.concat(path),
                time,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;

    const DAY: i64 = 86_400;
    const NOW: i64 = 2_000_000_000;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn now() -> Timestamp {
        Timestamp::from_second(NOW).expect("in range")
    }

    fn at(offset: i64) -> Timestamp {
        Timestamp::from_second(NOW + offset).expect("in range")
    }

    /// A collection whose config holds `extra` below the ID.
    fn world(extra: &str) -> TempCollection {
        let temp = TempCollection::minimal();
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{extra}\n"),
        )
        .expect("the config is written");
        temp
    }

    /// A file `age` seconds old at the survey's `now`.
    fn old(temp: &mut TempCollection, relative: &str, age: i64) {
        temp.tree().file_with(relative, "x", 0o644, NOW - age);
    }

    fn surveyed(temp: &TempCollection) -> Survey {
        let collection = Collection::find(temp.root()).expect("a collection");
        survey(&collection, now()).expect("the survey succeeds")
    }

    fn piece<'a>(survey: &'a Survey, name: &str) -> &'a SurveyedPiece {
        survey
            .pieces
            .iter()
            .find(|found| found.piece == path(name))
            .unwrap_or_else(|| panic!("the survey has {name}"))
    }

    fn reasons(survey: &Survey, name: &str) -> Vec<String> {
        match &piece(survey, name).verdict {
            Verdict::Refused(list) => list.iter().map(ToString::to_string).collect(),
            other => panic!("{name} is {other:?}, not refused"),
        }
    }

    fn assert_not_root(directory: &std::path::Path) {
        assert!(
            fs::read_dir(directory).is_err(),
            "the test needs a user that mode 000 locks out"
        );
    }

    /// Fakes the roots of mounts the test cannot create.
    struct Mounts(&'static [&'static str]);

    impl Hooks for Mounts {
        fn fault(&self, _path: &RelPath, _call: crate::walk::Call) -> Option<io::Error> {
            None
        }

        fn mount_root(&self, path: &RelPath, reported: bool) -> bool {
            reported || self.0.contains(&path.to_string().as_str())
        }
    }

    // =========================================================
    // Due, soon and plain
    // =========================================================

    #[test]
    fn a_piece_idle_for_longer_than_idle_days_is_due() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);

        let found = surveyed(&temp);

        let judged = piece(&found, "piece");
        assert_eq!(judged.verdict, Verdict::Due);
        assert_eq!(judged.clock, Some(at(-40 * DAY)));
    }

    #[test]
    fn a_piece_due_within_soon_days_is_soon_with_the_days_left() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 28 * DAY);

        let found = surveyed(&temp);

        assert_eq!(
            piece(&found, "piece").verdict,
            Verdict::Soon { days_left: 2 }
        );
    }

    #[test]
    fn a_started_day_counts_as_a_day_left() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 28 * DAY + DAY / 2);

        let found = surveyed(&temp);

        // 1.5 days remain.
        assert_eq!(
            piece(&found, "piece").verdict,
            Verdict::Soon { days_left: 2 }
        );
    }

    #[test]
    fn a_piece_exactly_idle_days_old_is_soon_with_one_day_left() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 30 * DAY);

        let found = surveyed(&temp);

        assert_eq!(
            piece(&found, "piece").verdict,
            Verdict::Soon { days_left: 1 }
        );
    }

    #[test]
    fn a_recently_touched_piece_and_an_empty_piece_are_plain() {
        let mut temp = world("");
        old(&mut temp, "fresh/file", 5 * DAY);
        old(&mut temp, "empty/.DS_Store", 400 * DAY);

        let found = surveyed(&temp);

        assert_eq!(piece(&found, "fresh").verdict, Verdict::Plain);
        assert_eq!(piece(&found, "fresh").clock, Some(at(-5 * DAY)));
        assert_eq!(piece(&found, "empty").verdict, Verdict::Plain);
        assert_eq!(piece(&found, "empty").clock, None);
    }

    #[test]
    fn the_pieces_come_sorted_by_path() {
        let mut temp = world("patterns = [\"*/*\", \"b\"]");
        old(&mut temp, "b/file", DAY);
        old(&mut temp, "a/z/file", DAY);
        old(&mut temp, "a/y/file", DAY);

        let found = surveyed(&temp);

        let names: Vec<String> = found.pieces.iter().map(|p| p.piece.to_string()).collect();
        assert_eq!(names, ["a/y", "a/z", "b"]);
    }

    // =========================================================
    // Pinned
    // =========================================================

    #[test]
    fn a_pinned_piece_is_pinned_and_shows_its_idle_clock() {
        let mut temp = world("");
        temp.tree().file("piece/.antiq-pin", "");
        old(&mut temp, "piece/file", 120 * DAY);

        let found = surveyed(&temp);

        let judged = piece(&found, "piece");
        assert_eq!(judged.verdict, Verdict::Pinned);
        assert_eq!(judged.clock, Some(at(-120 * DAY)));
    }

    #[test]
    fn a_pin_wins_over_a_walk_problem() {
        let mut temp = world("");
        temp.tree().file("piece/.antiq-pin", "");
        old(&mut temp, "piece/card/file", 120 * DAY);
        let collection = Collection::find(temp.root()).expect("a collection");

        let found = survey_with(&collection, now(), &Mounts(&["card"])).expect("surveys");

        assert_eq!(piece(&found, "piece").verdict, Verdict::Pinned);
    }

    // =========================================================
    // Refused
    // =========================================================

    #[test]
    fn a_mount_point_below_the_top_refuses_with_its_collection_path() {
        let mut temp = world("");
        old(&mut temp, "media/camera-import/card/file", 52 * DAY);
        let collection = Collection::find(temp.root()).expect("a collection");

        let found =
            survey_with(&collection, now(), &Mounts(&["camera-import/card"])).expect("surveys");

        assert_eq!(
            reasons(&found, "media"),
            ["mount point: media/camera-import/card"]
        );
    }

    #[test]
    fn a_top_that_is_a_mount_root_is_a_mount_point_at_its_own_path() {
        let mut temp = world("patterns = [\"*/*\"]");
        old(&mut temp, "a/b/file", 52 * DAY);
        let collection = Collection::find(temp.root()).expect("a collection");

        let found = survey_with(&collection, now(), &Mounts(&[""])).expect("surveys");

        assert_eq!(reasons(&found, "a/b"), ["mount point: a/b"]);
    }

    #[test]
    fn an_unreadable_directory_refuses_with_the_first_failed_entry() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);
        assert_not_root(&temp.tree().unreadable_dir("piece/locked"));

        let found = surveyed(&temp);

        assert_eq!(reasons(&found, "piece"), ["unreadable entry: piece/locked"]);
        assert_eq!(piece(&found, "piece").clock, None);
    }

    #[test]
    fn an_exclude_entry_below_the_top_refuses() {
        let mut temp = world("exclude = [\"piece/inner\"]");
        old(&mut temp, "piece/inner/file", 40 * DAY);

        let found = surveyed(&temp);

        assert_eq!(reasons(&found, "piece"), ["exclude entry: piece/inner"]);
    }

    #[test]
    fn a_nested_collection_refuses_with_the_directory_that_holds_it() {
        let mut temp = world("");
        old(&mut temp, "piece/sub/file", 40 * DAY);
        temp.tree().dir("piece/sub/.antiq");

        let found = surveyed(&temp);

        assert_eq!(reasons(&found, "piece"), ["nested collection: piece/sub"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_immutable_file_refuses_as_locked() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);
        old(&mut temp, "piece/kept", 40 * DAY);
        temp.tree().immutable("piece/kept");

        let found = surveyed(&temp);

        assert_eq!(reasons(&found, "piece"), ["locked: piece/kept"]);
    }

    #[test]
    fn a_parent_the_user_cannot_write_refuses_as_locked() {
        let mut temp = world("patterns = [\"*/*\"]");
        old(&mut temp, "area/piece/file", 40 * DAY);
        let parent = temp.tree().path("area");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).expect("the mode is set");
        let attempt = fs::File::create(parent.join("probe"));
        let cannot_write = attempt.is_err();
        let found = surveyed(&temp);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("restores");
        assert!(
            cannot_write,
            "the test needs a user that mode 500 keeps from writing"
        );

        assert_eq!(reasons(&found, "area/piece"), ["locked: area"]);
    }

    #[test]
    fn a_broken_rule_file_refuses_with_its_line() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);
        temp.tree().file("piece/.antiqignore", "*.log\nbroken\\\n");

        let found = surveyed(&temp);

        assert_eq!(
            reasons(&found, "piece"),
            ["rule file: piece/.antiqignore:2"]
        );
    }

    #[test]
    fn a_marker_that_is_a_directory_refuses_by_its_name() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);
        temp.tree().dir("piece/.antiq-restored");

        let found = surveyed(&temp);

        assert_eq!(reasons(&found, "piece"), [".antiq-restored is not a file"]);
    }

    #[test]
    fn a_pin_that_is_a_directory_refuses_by_its_name() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);
        temp.tree().dir("piece/.antiq-pin");

        let found = surveyed(&temp);

        assert_eq!(reasons(&found, "piece"), [".antiq-pin is not a file"]);
    }

    #[test]
    fn a_marker_with_garbage_refuses_as_unreadable() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);
        temp.tree().file("piece/.antiq-restored", "garbage\n");

        let found = surveyed(&temp);

        assert_eq!(
            reasons(&found, "piece"),
            ["unreadable marker: piece/.antiq-restored"]
        );
        assert_eq!(piece(&found, "piece").clock, None);
    }

    #[test]
    fn a_top_the_user_cannot_search_names_the_pin_that_cannot_be_looked_at() {
        let mut temp = world("");
        assert_not_root(&temp.tree().unreadable_dir("piece"));

        let found = surveyed(&temp);

        assert_eq!(
            reasons(&found, "piece"),
            [
                "unreadable entry: piece",
                "unreadable marker: piece/.antiq-pin"
            ]
        );
    }

    #[test]
    fn every_problem_of_a_piece_is_a_reason() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 40 * DAY);
        temp.tree().dir("piece/sub/.antiq");
        assert_not_root(&temp.tree().unreadable_dir("piece/locked"));

        let found = surveyed(&temp);

        assert_eq!(
            reasons(&found, "piece"),
            [
                "unreadable entry: piece/locked",
                "nested collection: piece/sub"
            ]
        );
    }

    #[test]
    fn a_refused_piece_with_a_readable_tree_has_its_clock() {
        let mut temp = world("");
        old(&mut temp, "piece/file", 52 * DAY);
        temp.tree().file("piece/.antiqignore", "broken\\\n");

        let found = surveyed(&temp);

        assert_eq!(piece(&found, "piece").clock, Some(at(-52 * DAY)));
    }

    /// The text and the kind of a refusal for every kind of walk problem,
    /// with paths made relative to the collection.
    #[test]
    fn every_walk_problem_has_its_refusal_text() {
        let top = path("area/piece");
        let name = |text: &str| Name::from_bytes(text.as_bytes().to_vec()).expect("a name");
        let cases = [
            (
                Problem::Unreadable { first: path("a/b") },
                RefusalKind::UnreadableEntry,
                "unreadable entry: area/piece/a/b",
            ),
            (
                Problem::MountPoint { other: path("mnt") },
                RefusalKind::MountPoint,
                "mount point: area/piece/mnt",
            ),
            (
                Problem::MountPoint {
                    other: RelPath::root(),
                },
                RefusalKind::MountPoint,
                "mount point: area/piece",
            ),
            (
                Problem::Excluded { path: path("skip") },
                RefusalKind::ExcludeEntry,
                "exclude entry: area/piece/skip",
            ),
            (
                Problem::NestedCollection {
                    root: path("inner"),
                },
                RefusalKind::NestedCollection,
                "nested collection: area/piece/inner",
            ),
            (
                Problem::Locked { path: path("held") },
                RefusalKind::Locked,
                "locked: area/piece/held",
            ),
            (
                Problem::LockedParent { path: path("area") },
                RefusalKind::Locked,
                "locked: area",
            ),
            (
                Problem::LockedParent {
                    path: RelPath::root(),
                },
                RefusalKind::Locked,
                "locked: .",
            ),
            (
                Problem::RuleFile {
                    path: path(".antiqignore"),
                    line: Some(3),
                },
                RefusalKind::RuleFile,
                "rule file: area/piece/.antiqignore:3",
            ),
            (
                Problem::RuleFile {
                    path: path("x/.antiq-timeless"),
                    line: None,
                },
                RefusalKind::RuleFile,
                "rule file: area/piece/x/.antiq-timeless",
            ),
            (
                Problem::MarkerNotAFile {
                    name: name(".antiq-pin"),
                },
                RefusalKind::NotAFile,
                ".antiq-pin is not a file",
            ),
        ];
        for (problem, kind, text) in cases {
            let refusal = Refusal::of(&top, &problem);
            assert_eq!(refusal.kind, kind, "{problem:?}");
            assert_eq!(refusal.to_string(), text, "{problem:?}");
        }
    }

    #[test]
    fn every_kind_has_its_json_name() {
        let names = [
            (RefusalKind::MountPoint, "mount_point"),
            (RefusalKind::NestedCollection, "nested_collection"),
            (RefusalKind::UnreadableEntry, "unreadable_entry"),
            (RefusalKind::ExcludeEntry, "exclude_entry"),
            (RefusalKind::Locked, "locked"),
            (RefusalKind::RuleFile, "rule_file"),
            (RefusalKind::NotAFile, "not_a_file"),
            (RefusalKind::UnreadableMarker, "unreadable_marker"),
        ];
        for (kind, name) in names {
            assert_eq!(kind.as_str(), name);
        }
    }

    // =========================================================
    // Times in the future
    // =========================================================

    #[test]
    fn a_file_and_a_marker_in_the_future_are_listed_with_their_times() {
        let mut temp = world("");
        temp.tree()
            .file_with("piece/later", "x", 0o644, NOW + 3 * DAY);
        temp.tree()
            .file("piece/.antiq-restored", "restored 2033-05-19T03:33:20Z\n");

        let found = surveyed(&temp);

        let judged = piece(&found, "piece");
        assert_eq!(judged.verdict, Verdict::Plain);
        assert_eq!(
            judged.future,
            [
                FutureTime {
                    path: path("piece/.antiq-restored"),
                    time: at(DAY),
                },
                FutureTime {
                    path: path("piece/later"),
                    time: at(3 * DAY),
                },
            ]
        );
    }

    #[test]
    fn a_pinned_and_a_refused_piece_list_their_future_files() {
        let mut temp = world("");
        temp.tree().file("pinned/.antiq-pin", "");
        temp.tree().file_with("pinned/later", "x", 0o644, NOW + DAY);
        temp.tree().file("refused/.antiqignore", "broken\\\n");
        temp.tree()
            .file_with("refused/later", "x", 0o644, NOW + 2 * DAY);

        let found = surveyed(&temp);

        assert_eq!(
            piece(&found, "pinned").future,
            [FutureTime {
                path: path("pinned/later"),
                time: at(DAY),
            }]
        );
        assert_eq!(
            piece(&found, "refused").future,
            [FutureTime {
                path: path("refused/later"),
                time: at(2 * DAY),
            }]
        );
    }

    // =========================================================
    // What selection and the config can fail
    // =========================================================

    #[test]
    fn a_directory_selection_cannot_read_is_reported() {
        let mut temp = world("patterns = [\"*/*\"]");
        old(&mut temp, "ok/piece/file", DAY);
        assert_not_root(&temp.tree().unreadable_dir("area"));

        let found = surveyed(&temp);

        assert_eq!(found.unreadable, [path("area")]);
        assert_eq!(found.pieces.len(), 1);
    }

    #[test]
    fn a_collection_root_that_cannot_be_opened_fails_the_survey() {
        let id = COLLECTION_ID.parse().expect("an ID");
        let collection = Collection::new(
            std::path::PathBuf::from("/nonexistent/collection"),
            crate::config::Config::new(id),
        );

        let error = survey(&collection, now()).expect_err("the root is missing");

        assert!(matches!(error, SurveyError::Root { .. }), "{error}");
    }

    #[test]
    fn an_invalid_ignore_line_in_the_config_fails_the_survey() {
        let mut temp = world("ignore = [\"broken\\\\\"]");
        old(&mut temp, "piece/file", DAY);
        let collection = Collection::find(temp.root()).expect("a collection");

        let error = survey(&collection, now()).expect_err("the rule is invalid");

        assert!(matches!(error, SurveyError::Rules(_)), "{error}");
    }
}
