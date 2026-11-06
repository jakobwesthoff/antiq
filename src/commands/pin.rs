// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` and `unpin` operations.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::tree::TreeRoot;
use crate::markers::{RealSite, Site};
use crate::pin::{self, PinError};
use crate::plan::LockHeld;
use crate::reference::{self, CandidateSet, ReferenceError};
use crate::relpath::RelPath;
use crate::repair::{Destination, Repair};
use crate::report::{self, CommandReport, Warning};
use crate::run::{self, RunError, Started};
use crate::select::{Rules, SelectError, select};
use serde::Serialize;
use std::ffi::OsStr;
use std::path::Path;

/// Why pin or unpin failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PinCommandError {
    /// The lock is held, or the machine has no name to write into it.
    #[error("cannot start the run")]
    Run(
        #[from]
        #[diagnostic_source]
        RunError,
    ),
    #[error("cannot find the piece")]
    Reference(
        #[from]
        #[diagnostic_source]
        ReferenceError,
    ),
    #[error("cannot select the pieces")]
    Select(
        #[from]
        #[diagnostic_source]
        SelectError,
    ),
    #[error("cannot change the pin")]
    Pin(
        #[from]
        #[diagnostic_source]
        PinError,
    ),
    #[error("cannot open the collection {path}: {source}")]
    Root {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The piece on the command line is empty.
    #[error("{0}")]
    Usage(&'static str),
}

impl HasKind for PinCommandError {
    fn kind(&self) -> ErrorKind {
        match self {
            PinCommandError::Run(error) => error.kind(),
            PinCommandError::Reference(error) => error.kind(),
            PinCommandError::Usage(_) => ErrorKind::Usage,
            PinCommandError::Select(_) | PinCommandError::Pin(_) | PinCommandError::Root { .. } => {
                ErrorKind::Other
            }
        }
    }
}

/// What the command does to the pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Pin,
    Unpin,
}

#[derive(Debug)]
pub struct PinRequest<'a> {
    pub collection: &'a Collection,
    /// The directory a piece given as a relative path starts at.
    pub cwd: &'a Path,
    /// A path or a search word.
    pub piece: &'a OsStr,
    pub direction: Direction,
}

/// One step of a run, as a plan lists it and as the report names the
/// repairs that were done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Repair(Repair),
    Pin { piece: RelPath },
    Unpin { piece: RelPath },
}

/// What a real run did.
#[derive(Debug, Serialize)]
pub struct PinReport {
    pub piece: RelPath,
    /// Whether the piece is pinned now.
    pub pinned: bool,
    /// Whether the piece was in the other state before the run.
    pub changed: bool,
    /// The repairs at the start of the run that were done.
    pub repairs: Vec<Action>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for PinReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// What a dry run found.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PinPlan {
    /// Another run holds the lock, so nothing else was read.
    Held {
        dry_run: bool,
        lock_held: LockHeld,
        actions: Vec<Action>,
    },
    /// The repairs a real run starts with, then the change to the pin.
    Listed {
        dry_run: bool,
        piece: RelPath,
        pinned: bool,
        changed: bool,
        actions: Vec<Action>,
        warnings: Vec<Warning>,
    },
}

/// Pins or unpins the piece the request names. The run lock is taken and
/// the repairs run first.
pub fn pin(request: &PinRequest<'_>, clock: &dyn Clock) -> Result<PinReport, PinCommandError> {
    pin_with(request, clock, &RealSite)
}

pub(crate) fn pin_with(
    request: &PinRequest<'_>,
    clock: &dyn Clock,
    site: &dyn Site,
) -> Result<PinReport, PinCommandError> {
    check_arguments(request)?;
    let Started::Guarded(guard) = run::start(request.collection, clock, false)? else {
        unreachable!("a real run starts a guarded run");
    };

    // The repairs have run, so what they gave back is live and found by
    // the selection like any other folder.
    let located = locate(request, &[])?;
    let was_pinned = pin::current(&located.root, &located.piece)?;
    match request.direction {
        // `create` also covers a piece that is pinned already: it flushes
        // the directory, so the pin is durable even when an earlier run
        // died between the rename and the flush.
        Direction::Pin => pin::create_with(&located.root, &located.piece, site)?,
        Direction::Unpin if was_pinned => pin::remove_with(&located.root, &located.piece, site)?,
        Direction::Unpin => {}
    }

    let wanted = request.direction == Direction::Pin;
    let mut warnings = report::setting_notes(request.collection);
    warnings.extend(guard.warnings().iter().cloned());
    Ok(PinReport {
        piece: located.piece,
        pinned: wanted,
        changed: was_pinned != wanted,
        repairs: guard
            .repaired()
            .iter()
            .cloned()
            .map(Action::Repair)
            .collect(),
        warnings,
    })
}

/// Lists what [`pin`] would do, without taking the lock or changing
/// anything. The piece is looked for as if the repairs had been done.
pub fn plan(request: &PinRequest<'_>, clock: &dyn Clock) -> Result<PinPlan, PinCommandError> {
    plan_with(request, clock, &RealSite)
}

pub(crate) fn plan_with(
    request: &PinRequest<'_>,
    clock: &dyn Clock,
    site: &dyn Site,
) -> Result<PinPlan, PinCommandError> {
    check_arguments(request)?;
    let Started::DryRun {
        holder,
        repairs,
        warnings: unreadable,
    } = run::start(request.collection, clock, true)?
    else {
        unreachable!("a dry run starts no guarded run");
    };
    if let Some(holder) = holder {
        return Ok(PinPlan::Held {
            dry_run: true,
            lock_held: LockHeld::of(&holder),
            actions: Vec::new(),
        });
    }

    // A folder that repair 4 gives back to its live path is a piece once
    // the repairs are done, which a real run does first.
    let given_back: Vec<RelPath> = repairs
        .iter()
        .filter_map(|repair| match repair {
            Repair::GiveBack {
                tree,
                destination: Destination::Live,
                ..
            } => Some(tree.clone()),
            _ => None,
        })
        .collect();
    let located = locate(request, &given_back)?;

    // A folder that is not there yet has no pin and no top to write
    // through, so the checks of the real run cannot be made on it.
    let was_pinned = if located.live {
        pin::current(&located.root, &located.piece)?
    } else {
        false
    };
    if located.live && request.direction == Direction::Pin {
        pin::check_create(&located.root, &located.piece, site)?;
    }

    let wanted = request.direction == Direction::Pin;
    let changed = was_pinned != wanted;
    let mut actions: Vec<Action> = repairs.into_iter().map(Action::Repair).collect();
    if changed {
        let piece = located.piece.clone();
        actions.push(match request.direction {
            Direction::Pin => Action::Pin { piece },
            Direction::Unpin => Action::Unpin { piece },
        });
    }
    let mut warnings = report::setting_notes(request.collection);
    warnings.extend(unreadable);
    Ok(PinPlan::Listed {
        dry_run: true,
        piece: located.piece,
        pinned: wanted,
        changed,
        actions,
        warnings,
    })
}

/// The empty piece is a flaw of the command line: the shell passed a
/// variable that was not set.
fn check_arguments(request: &PinRequest<'_>) -> Result<(), PinCommandError> {
    if request.piece.is_empty() {
        return Err(PinCommandError::Usage("the piece is empty"));
    }
    Ok(())
}

/// The piece the request means, with the root it lies in.
struct Located {
    root: TreeRoot,
    piece: RelPath,
    /// Whether the piece is a folder now, and not one that a repair is
    /// still to give back.
    live: bool,
}

/// Resolves the request among the live pieces, and among `coming`, the
/// folders that are to become live and that the patterns select.
fn locate(request: &PinRequest<'_>, coming: &[RelPath]) -> Result<Located, PinCommandError> {
    let collection = request.collection;
    let config = &collection.config().collection;
    let root = TreeRoot::open(collection.root()).map_err(|source| PinCommandError::Root {
        path: collection.root().display().to_string(),
        source,
    })?;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    let selection = select(&root, &rules)?;
    let selected: Vec<RelPath> = coming
        .iter()
        .filter(|path| rules.matches(path))
        .cloned()
        .collect();
    let candidates = CandidateSet::live_with(collection, &selection, &selected);
    let piece = reference::resolve_piece(collection, request.cwd, request.piece, &candidates)?;
    let live = selection.pieces().contains(&piece);
    Ok(Located { root, piece, live })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::filesystem::atomic::recording::{Flush, Recording};
    use crate::filesystem::tree::TreeRoot;
    use crate::lock;
    use crate::report::WarningKind;
    use crate::testing::hold_lock;
    use crate::testing::tree::{COLLECTION_ID, TempCollection, snapshot};
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;

    fn clock() -> FixedClock {
        FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"))
    }

    /// A collection whose config holds `extra` below the ID and whose
    /// folders `alpha`, `scanner` and `scan-old` are pieces.
    fn world(extra: &str) -> TempCollection {
        let mut temp = TempCollection::minimal();
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{extra}\n"),
        )
        .expect("the config is written");
        for piece in ["alpha", "scanner", "scan-old"] {
            temp.tree().file(&format!("{piece}/data"), "x");
        }
        temp
    }

    fn found(temp: &TempCollection) -> Collection {
        Collection::find(temp.root()).expect("a collection")
    }

    /// Runs the command once.
    fn run_once(
        temp: &TempCollection,
        piece: &str,
        direction: Direction,
    ) -> Result<PinReport, PinCommandError> {
        let collection = found(temp);
        let request = PinRequest {
            collection: &collection,
            cwd: temp.root(),
            piece: OsStr::new(piece),
            direction,
        };
        pin(&request, &clock())
    }

    /// Runs the command again while the lock a run before released still
    /// reads as held.
    fn run(
        temp: &TempCollection,
        piece: &str,
        direction: Direction,
    ) -> Result<PinReport, PinCommandError> {
        lock::retry_while_held(is_held, || run_once(temp, piece, direction))
    }

    fn run_on(
        temp: &TempCollection,
        piece: &str,
        direction: Direction,
        site: &dyn Site,
    ) -> Result<PinReport, PinCommandError> {
        lock::retry_while_held(is_held, || {
            let collection = found(temp);
            let request = PinRequest {
                collection: &collection,
                cwd: temp.root(),
                piece: OsStr::new(piece),
                direction,
            };
            pin_with(&request, &clock(), site)
        })
    }

    fn is_held(result: &Result<PinReport, PinCommandError>) -> bool {
        matches!(result, Err(PinCommandError::Run(RunError::Lock(_))))
    }

    fn plan_for(
        temp: &TempCollection,
        piece: &str,
        direction: Direction,
    ) -> Result<PinPlan, PinCommandError> {
        plan_on(temp, piece, direction, &RealSite)
    }

    fn plan_on(
        temp: &TempCollection,
        piece: &str,
        direction: Direction,
        site: &dyn Site,
    ) -> Result<PinPlan, PinCommandError> {
        let collection = found(temp);
        let request = PinRequest {
            collection: &collection,
            cwd: temp.root(),
            piece: OsStr::new(piece),
            direction,
        };
        plan_with(&request, &clock(), site)
    }

    fn listed(plan: PinPlan) -> (bool, bool, Vec<Action>, Vec<Warning>) {
        match plan {
            PinPlan::Listed {
                pinned,
                changed,
                actions,
                warnings,
                ..
            } => (pinned, changed, actions, warnings),
            PinPlan::Held { .. } => panic!("the lock is not held"),
        }
    }

    fn pin_path(temp: &TempCollection, piece: &str) -> std::path::PathBuf {
        temp.root().join(piece).join(".antiq-pin")
    }

    fn piece(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    /// A site that puts the folder `alpha` on another device than
    /// `.antiq/tmp/`, and records what is flushed.
    struct OtherDevice(Recording);

    impl Site for OtherDevice {
        fn durable(&self) -> &dyn crate::filesystem::atomic::Durable {
            &self.0
        }
        fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
            let device = root.lstat(path)?.device;
            Ok(if path.to_string() == "alpha" {
                device + 1
            } else {
                device
            })
        }
    }

    struct Recorded(Recording);

    impl Site for Recorded {
        fn durable(&self) -> &dyn crate::filesystem::atomic::Durable {
            &self.0
        }
    }

    // =========================================================
    // Pin and unpin
    // =========================================================

    #[test]
    fn pinning_by_path_writes_an_empty_pin_file() {
        let temp = world("");

        let report = run(&temp, "alpha", Direction::Pin).expect("pinned");

        assert_eq!(report.piece, piece("alpha"));
        assert!(report.pinned && report.changed);
        assert_eq!(fs::read(pin_path(&temp, "alpha")).expect("the pin"), b"");
    }

    #[test]
    fn pinning_by_search_word_finds_the_one_piece_it_matches() {
        let temp = world("");

        let report = run(&temp, "alph", Direction::Pin).expect("pinned");

        assert_eq!(report.piece, piece("alpha"));
        assert!(pin_path(&temp, "alpha").exists());
    }

    #[test]
    fn pinning_twice_changes_nothing_and_flushes_the_directory_again() {
        let temp = world("");
        run(&temp, "alpha", Direction::Pin).expect("pinned");
        let before = snapshot(temp.root());
        let site = Recorded(Recording::new());

        let report = run_on(&temp, "alpha", Direction::Pin, &site).expect("still pinned");

        assert!(report.pinned && !report.changed);
        assert_eq!(snapshot(temp.root()).keys().count(), before.keys().count());
        assert_eq!(site.0.flushes(), [Flush::Dir("alpha".into())]);
    }

    #[test]
    fn unpinning_removes_the_pin_file() {
        let temp = world("");
        run(&temp, "alpha", Direction::Pin).expect("pinned");

        let report = run(&temp, "alpha", Direction::Unpin).expect("unpinned");

        assert!(!report.pinned && report.changed);
        assert!(!pin_path(&temp, "alpha").exists());
    }

    #[test]
    fn unpinning_a_piece_without_a_pin_changes_nothing() {
        let temp = world("");
        let site = Recorded(Recording::new());

        let report = run_on(&temp, "alpha", Direction::Unpin, &site).expect("not pinned");

        assert!(!report.pinned && !report.changed);
        assert!(site.0.flushes().is_empty());
    }

    #[test]
    fn a_symlink_pin_is_removed_and_its_target_stays() {
        let mut temp = world("");
        temp.tree().file("outside", "keep");
        temp.tree().symlink("alpha/.antiq-pin", "../outside");

        let report = run(&temp, "alpha", Direction::Unpin).expect("unpinned");

        assert!(report.changed);
        assert!(fs::symlink_metadata(pin_path(&temp, "alpha")).is_err());
        assert_eq!(
            fs::read(temp.root().join("outside")).expect("target"),
            b"keep"
        );
    }

    #[test]
    fn a_directory_with_the_pin_name_is_refused_by_both_directions_and_kept() {
        let mut temp = world("");
        temp.tree().file("alpha/.antiq-pin/keep", "x");
        let before = snapshot(&temp.root().join("alpha"));

        for direction in [Direction::Pin, Direction::Unpin] {
            let error = run(&temp, "alpha", direction).expect_err("refused");
            assert!(
                matches!(&error, PinCommandError::Pin(PinError::NotAFile { path })
                    if path.to_string() == "alpha/.antiq-pin"),
                "{error:?}"
            );
        }
        assert_eq!(snapshot(&temp.root().join("alpha")), before);
    }

    #[test]
    fn a_top_without_owner_write_is_pinned_and_unpinned_and_keeps_its_mode() {
        let temp = world("");
        let top = temp.root().join("alpha");
        let _restore = crate::filesystem::RestoreModes(vec![top.clone()]);
        fs::set_permissions(&top, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::write(top.join("probe"), "").is_err(),
            "the test needs a user that a read-only directory stops"
        );

        run(&temp, "alpha", Direction::Pin).expect("pinned");
        assert!(pin_path(&temp, "alpha").exists());
        run(&temp, "alpha", Direction::Unpin).expect("unpinned");

        assert!(!pin_path(&temp, "alpha").exists());
        let mode = fs::metadata(&top).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o7777, 0o555);
    }

    #[test]
    fn a_top_on_another_device_is_refused_as_a_mount_point_before_any_write() {
        let temp = world("");
        let site = OtherDevice(Recording::new());

        let error = run_on(&temp, "alpha", Direction::Pin, &site).expect_err("refused");

        assert!(matches!(&error, PinCommandError::Pin(_)), "{error:?}");
        let chain = format!("{error:?}");
        assert!(chain.contains("MountPoint"), "{chain}");
        assert!(!pin_path(&temp, "alpha").exists());
        assert_eq!(
            fs::read_dir(temp.root().join(".antiq/tmp"))
                .expect("tmp")
                .count(),
            0
        );
    }

    #[test]
    fn a_pinned_top_on_another_device_stays_pinned_and_unpins() {
        let mut temp = world("");
        temp.tree().file("alpha/.antiq-pin", "");
        let site = OtherDevice(Recording::new());

        let again = run_on(&temp, "alpha", Direction::Pin, &site).expect("only a flush");
        assert!(!again.changed);
        let gone = run_on(&temp, "alpha", Direction::Unpin, &site).expect("unlink needs no rename");
        assert!(gone.changed);
        assert!(!pin_path(&temp, "alpha").exists());
    }

    // =========================================================
    // Which piece is meant
    // =========================================================

    #[test]
    fn a_folder_that_is_not_a_live_piece_is_an_unknown_reference() {
        let mut temp = world("patterns = [\"a*\", \"s*\"]");
        temp.tree().dir("zeta");

        for given in ["gone", "zeta"] {
            let error = run(&temp, given, Direction::Pin).expect_err("no such piece");
            assert_eq!(error.kind(), ErrorKind::UnknownReference, "{given}");
            let error = run(&temp, given, Direction::Unpin).expect_err("no such piece");
            assert_eq!(error.kind(), ErrorKind::UnknownReference, "{given}");
        }
        assert!(!temp.root().join("zeta/.antiq-pin").exists());
    }

    #[test]
    fn an_ambiguous_word_names_its_candidates_in_the_error() {
        let temp = world("");

        let error = run(&temp, "scan", Direction::Pin).expect_err("two pieces match");

        assert_eq!(error.kind(), ErrorKind::AmbiguousReference);
        let details = crate::Error::from(error).details();
        assert_eq!(
            details["candidates"],
            serde_json::json!([{"path": "scan-old"}, {"path": "scanner"}])
        );
        assert!(!pin_path(&temp, "scanner").exists());
    }

    #[test]
    fn a_path_outside_the_collection_is_refused_as_such() {
        let temp = world("");

        let error = run(&temp, "/", Direction::Pin).expect_err("outside");

        assert_eq!(error.kind(), ErrorKind::OutsideCollection);
    }

    #[test]
    fn a_collection_root_that_cannot_be_opened_is_reported_with_its_path() {
        let temp = world("");
        let collection = found(&temp);
        let request = PinRequest {
            collection: &collection,
            cwd: temp.root(),
            piece: OsStr::new("alpha"),
            direction: Direction::Pin,
        };
        fs::remove_dir_all(temp.root()).expect("the collection is gone");

        let error = locate(&request, &[]).err().expect("nothing to open");

        assert!(
            matches!(&error, PinCommandError::Root { path, .. }
                if *path == collection.root().display().to_string()),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn an_empty_piece_is_a_usage_error_and_nothing_is_locked() {
        let temp = world("");

        let error = run(&temp, "", Direction::Pin).expect_err("empty");
        assert_eq!(error.kind(), ErrorKind::Usage);
        let error = plan_for(&temp, "", Direction::Unpin).expect_err("empty");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(!temp.root().join(".antiq/lock").exists());
    }

    // =========================================================
    // The run
    // =========================================================

    #[test]
    fn a_held_lock_fails_the_run_with_the_holder_in_the_details() {
        let temp = world("");
        let _lock = hold_lock(temp.root());

        let error = run_once(&temp, "alpha", Direction::Pin).expect_err("the lock is held");

        assert_eq!(error.kind(), ErrorKind::LockHeld);
        let details = crate::Error::from(error).details();
        assert_eq!(details["pid"], std::process::id());
        assert!(details["started"].is_string());
        assert!(!pin_path(&temp, "alpha").exists());
    }

    #[test]
    fn the_repairs_run_first_and_are_in_the_report() {
        let mut temp = world("");
        temp.tree().file(".antiq/tmp/leftover", "x");

        let report = run(&temp, "alpha", Direction::Pin).expect("pinned");

        assert!(!temp.root().join(".antiq/tmp/leftover").exists());
        assert_eq!(report.repairs.len(), 1);
        assert!(matches!(
            &report.repairs[0],
            Action::Repair(Repair::DeleteTemporary { .. })
        ));
    }

    #[test]
    fn a_folder_that_a_repair_gives_back_can_be_pinned_in_the_same_run() {
        let mut temp = world("");
        temp.tree()
            .file(".antiq/trash/2026-11-04T090211/gamma/data", "x");

        let report = run(&temp, "gamma", Direction::Pin).expect("pinned after the repair");

        assert_eq!(report.piece, piece("gamma"));
        assert!(pin_path(&temp, "gamma").exists());
    }

    #[test]
    fn the_notes_on_the_settings_come_with_the_report() {
        let temp = world("colour = \"red\"");

        let report = run(&temp, "alpha", Direction::Pin).expect("pinned");

        assert_eq!(
            report.warnings,
            [Warning::of(
                WarningKind::UnknownKey {
                    table: "collection",
                    key: "colour".into()
                },
                "the key `colour` under [collection] is unknown and ignored"
            )]
        );
        assert!(!report.partial());
        assert_eq!(report.warnings(), report.warnings.as_slice());
    }

    // =========================================================
    // The dry run
    // =========================================================

    #[test]
    fn a_dry_run_lists_the_change_and_writes_nothing() {
        let mut temp = world("");
        temp.tree().file("scanner/.antiq-pin", "");
        let before = snapshot(temp.root());

        let (pinned, changed, actions, _) =
            listed(plan_for(&temp, "alpha", Direction::Pin).expect("planned"));
        assert!(pinned && changed);
        assert_eq!(
            actions,
            [Action::Pin {
                piece: piece("alpha")
            }]
        );
        let (pinned, changed, actions, _) =
            listed(plan_for(&temp, "scanner", Direction::Unpin).expect("planned"));
        assert!(!pinned && changed);
        assert_eq!(
            actions,
            [Action::Unpin {
                piece: piece("scanner")
            }]
        );
        assert_eq!(snapshot(temp.root()), before);
    }

    #[test]
    fn a_dry_run_of_the_state_that_holds_lists_no_pin_action() {
        let mut temp = world("");
        temp.tree().file("alpha/.antiq-pin", "");

        let (pinned, changed, actions, _) =
            listed(plan_for(&temp, "alpha", Direction::Pin).expect("planned"));
        assert!(pinned && !changed && actions.is_empty());
        let (pinned, changed, actions, _) =
            listed(plan_for(&temp, "scanner", Direction::Unpin).expect("planned"));
        assert!(!pinned && !changed && actions.is_empty());
    }

    #[test]
    fn a_dry_run_lists_the_repairs_first_and_does_not_do_them() {
        let mut temp = world("");
        temp.tree().file(".antiq/tmp/leftover", "x");

        let (_, _, actions, _) = listed(plan_for(&temp, "alpha", Direction::Pin).expect("planned"));

        assert!(matches!(
            &actions[..],
            [
                Action::Repair(Repair::DeleteTemporary { .. }),
                Action::Pin { .. }
            ]
        ));
        assert!(temp.root().join(".antiq/tmp/leftover").exists());
    }

    #[test]
    fn a_dry_run_finds_a_folder_that_a_repair_would_give_back() {
        let mut temp = world("");
        temp.tree()
            .file(".antiq/trash/2026-11-04T090211/gamma/data", "x");
        let before = snapshot(temp.root());

        let (pinned, changed, actions, _) =
            listed(plan_for(&temp, "gamma", Direction::Pin).expect("planned"));

        assert!(pinned && changed);
        assert!(matches!(
            &actions[..],
            [
                Action::Repair(Repair::GiveBack { .. }),
                Action::Repair(Repair::RemoveEmptyChain { .. }),
                Action::Pin { piece },
            ] if piece.to_string() == "gamma"
        ));
        assert_eq!(snapshot(temp.root()), before);
    }

    #[test]
    fn a_dry_run_ignores_a_give_back_that_no_pattern_selects() {
        let mut temp = world("patterns = [\"a*\"]");
        temp.tree()
            .file(".antiq/trash/2026-11-04T090211/gamma/data", "x");

        let error = plan_for(&temp, "gamma", Direction::Pin).expect_err("not a piece");

        assert_eq!(error.kind(), ErrorKind::UnknownReference);
    }

    #[test]
    fn a_dry_run_names_the_holder_and_lists_no_repairs() {
        let mut temp = world("");
        temp.tree().file(".antiq/tmp/leftover", "x");
        let _lock = hold_lock(temp.root());

        let plan = plan_for(&temp, "alpha", Direction::Pin).expect("planned");

        let PinPlan::Held {
            lock_held, actions, ..
        } = plan
        else {
            panic!("the lock is held");
        };
        assert_eq!(lock_held.pid, std::process::id());
        assert!(actions.is_empty());
    }

    #[test]
    fn a_dry_run_refuses_what_a_real_run_refuses() {
        let mut temp = world("");
        temp.tree().dir("scanner/.antiq-pin");

        for direction in [Direction::Pin, Direction::Unpin] {
            let error = plan_for(&temp, "scanner", direction).expect_err("a directory");
            assert!(
                matches!(&error, PinCommandError::Pin(PinError::NotAFile { .. })),
                "{error:?}"
            );
            let error = plan_for(&temp, "gone", direction).expect_err("unknown");
            assert_eq!(error.kind(), ErrorKind::UnknownReference);
        }
        let site = OtherDevice(Recording::new());
        let error = plan_on(&temp, "alpha", Direction::Pin, &site).expect_err("a mount point");
        assert!(format!("{error:?}").contains("MountPoint"), "{error:?}");
    }

    #[test]
    fn a_dry_run_allows_what_a_real_run_allows_on_another_device() {
        let mut temp = world("");
        temp.tree().file("alpha/.antiq-pin", "");
        let site = OtherDevice(Recording::new());

        let (_, changed, _, _) =
            listed(plan_on(&temp, "alpha", Direction::Pin, &site).expect("only a flush"));
        assert!(!changed);
        let (_, changed, _, _) =
            listed(plan_on(&temp, "alpha", Direction::Unpin, &site).expect("an unlink"));
        assert!(changed);
    }

    #[test]
    fn the_notes_on_the_settings_come_with_the_plan() {
        let temp = world("colour = \"red\"");

        let (_, _, _, warnings) =
            listed(plan_for(&temp, "alpha", Direction::Pin).expect("planned"));

        assert_eq!(
            warnings,
            [Warning::of(
                WarningKind::UnknownKey {
                    table: "collection",
                    key: "colour".into()
                },
                "the key `colour` under [collection] is unknown and ignored"
            )]
        );
    }
}
