// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` command.
//!
//! The candidates are the pieces `status` marks `idle` that are new or
//! live, so the command and `status` share one definition of what the next
//! `archive` takes.

pub mod new;
pub mod unearthed;

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::fsx::FsxError;
use crate::fsx::device::{self, FsKind};
use crate::fsx::space::available_space;
use crate::fsx::tree::TreeRoot;
use crate::id::{IdSource, ObjectId};
use crate::index;
use crate::machine::MachineError;
use crate::ops::status::{self, Flag, PieceKind, StatusError, StatusRequest};
use crate::pack::pool::Pool;
use crate::pack::{PackError, Packager, TarZstd};
use crate::plan::{Plan, PlanAction};
use crate::relpath::RelPath;
use crate::remote::RemoteState;
use crate::report::{CommandReport, Warning};
use crate::run::{self, RunError, Started};
use crate::select::{Rules, SelectError};
use crate::store::StoreError;
use crate::tracking::TrackingError;
use crate::tree::{EntryKind, PieceTree};
use crate::walk;
use jiff::Timestamp;
use serde::Serialize;
use std::collections::HashSet;
use std::io;
use std::path::Path;

/// Errors of the `archive` command. Only what stops the whole run is an
/// error; whatever goes wrong with one piece is a `skipped` row of the
/// report.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error(transparent)]
    Status(#[from] StatusError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Idle(#[from] crate::idle::IdleError),
    #[error(transparent)]
    Walk(#[from] walk::WalkError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
}

impl HasKind for ArchiveError {
    fn kind(&self) -> ErrorKind {
        match self {
            ArchiveError::Run(error) => error.kind(),
            ArchiveError::Machine(error) => error.kind(),
            ArchiveError::Store(error) => error.kind(),
            ArchiveError::Tracking(error) => error.kind(),
            _ => ErrorKind::Other,
        }
    }
}

// =========================================================
// Seams
// =========================================================

/// The steps of archiving one piece, in the order they run. A test seam is
/// asked before each step, and an error it returns is that step's own
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Walk,
    Pack,
    Verify,
    StorePayload,
    StoreListing,
    WriteMetadata,
    WriteEntry,
    MarkRemoving,
}

/// The calls of a run that a test can answer for. The defaults are the
/// real ones.
pub(crate) trait Seams: Sync {
    /// What the walks see of the filesystem.
    fn walk_hooks(&self) -> &(dyn walk::Hooks + Sync) {
        &walk::RealFs
    }

    /// The bytes free on the volume holding `dir`, for the piece `piece`.
    fn free_space(&self, _piece: &RelPath, dir: &Path) -> Result<u64, FsxError> {
        available_space(dir)
    }

    fn packager(&self) -> &(dyn Packager + Sync) {
        &TarZstd
    }

    /// Called before `step` works on `piece`.
    fn at(&self, _step: Step, _piece: &RelPath) -> io::Result<()> {
        Ok(())
    }
}

struct RealSeams;

impl Seams for RealSeams {}

/// What `status` asks the world while it picks the candidates: the seams'
/// walks, the real filesystem kind, and no remote. A remote that is away
/// changes nothing about which pieces are idle.
struct CandidateProbes<'a>(&'a dyn Seams);

impl status::Probes for CandidateProbes<'_> {
    fn remote_state(&self, _: &Collection) -> RemoteState {
        RemoteState::None
    }

    fn walk_hooks(&self) -> &dyn walk::Hooks {
        self.0.walk_hooks()
    }

    fn fs_kind(&self, root: &Path) -> Result<FsKind, FsxError> {
        device::fs_kind(root)
    }

    fn away_warning(
        &self,
        _: &Collection,
        _: RemoteState,
        _: bool,
        _: Timestamp,
    ) -> Option<Warning> {
        None
    }
}

// =========================================================
// The report
// =========================================================

/// One piece a dry run would archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ArchiveAction {
    Archive {
        path: RelPath,
        /// `null` for a piece that has no ID yet.
        id: Option<ObjectId>,
        /// Apparent size in bytes.
        size: u64,
    },
}

impl PlanAction for ArchiveAction {
    fn describe(&self) -> String {
        match self {
            ArchiveAction::Archive { path, size, .. } => {
                format!("archive {path} ({})", size_text(*size))
            }
        }
    }
}

/// A piece that was archived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Archived {
    pub id: ObjectId,
    pub path: RelPath,
    /// Apparent size of the folder in bytes.
    pub size: u64,
    /// Bytes of the stored payload.
    pub archive_size: u64,
    pub payload_version: u32,
}

/// Why a piece was not archived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Unreadable,
    MountPoint,
    Excluded,
    NestedCollection,
    /// The folder differs from what was packed from it.
    Differs,
    NoSpace,
    Error,
}

/// A piece that was left alone, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// Present only for a piece that had an ID before the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub path: RelPath,
    pub reason: SkipReason,
    pub message: String,
}

/// What `archive` did, or with `--dry-run` would do.
#[derive(Debug, Serialize)]
pub struct ArchiveReport {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan<ArchiveAction>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<Vec<Archived>>,
    /// Real runs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<Skipped>>,
    pub warnings: Vec<Warning>,
}

#[cfg(test)]
impl ArchiveReport {
    pub(crate) fn partial_for_test(&self) -> bool {
        self.partial()
    }
}

impl CommandReport for ArchiveReport {
    fn partial(&self) -> bool {
        self.skipped
            .as_ref()
            .is_some_and(|skipped| !skipped.is_empty())
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// A size in decimal units with one decimal below 10: `88 MB`, `1.2 GB`.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && value.round() >= 1000.0 {
        value /= 1000.0;
        unit += 1;
    }
    if unit > 0 && (value * 10.0).round() < 100.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

// =========================================================
// Planning
// =========================================================

/// A piece the next real run archives.
struct Candidate {
    path: RelPath,
    id: Option<ObjectId>,
}

/// The idle, unpinned pieces in state `new` or `live`, in path order, with
/// the warnings `status` has for the collection.
fn candidates(
    collection: &Collection,
    clock: &dyn Clock,
    seams: &dyn Seams,
    own_lock: bool,
) -> Result<(Vec<Candidate>, Vec<Warning>), ArchiveError> {
    let report = status::status_with(&StatusRequest {
        collection,
        clock,
        probes: &CandidateProbes(seams),
        own_lock,
    })?;
    let candidates = report
        .pieces
        .iter()
        .filter(|piece| matches!(piece.state, PieceKind::New | PieceKind::Live))
        .filter(|piece| piece.flags.contains(&Flag::Idle))
        .map(|piece| Candidate {
            path: piece.path.clone(),
            id: piece.id,
        })
        .collect();
    Ok((candidates, report.warnings))
}

/// The sum of the sizes of the regular files, a file with several names
/// counted once.
pub(crate) fn apparent_size(tree: &PieceTree) -> u64 {
    let mut seen = HashSet::new();
    tree.entries()
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .filter(|entry| entry.nlink < 2 || seen.insert((entry.device, entry.inode)))
        .map(|entry| entry.size)
        .sum()
}

/// What a run would archive, as a dry run lists it. Nothing is locked and
/// nothing is written.
pub fn plan(collection: &Collection, clock: &dyn Clock) -> Result<ArchiveReport, ArchiveError> {
    plan_with(collection, clock, &RealSeams)
}

pub(crate) fn plan_with(
    collection: &Collection,
    clock: &dyn Clock,
    seams: &dyn Seams,
) -> Result<ArchiveReport, ArchiveError> {
    let (candidates, mut warnings) = candidates(collection, clock, seams, false)?;
    let config = &collection.config().collection;
    let root = open_root(collection)?;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
    let junk = status::junk_names(&config.junk, &mut warnings);
    let mut plan = Plan::new();
    for candidate in candidates {
        let tree =
            walk::walk_piece_with(&root, &candidate.path, &junk, &rules, seams.walk_hooks())?;
        plan.push(ArchiveAction::Archive {
            size: apparent_size(&tree),
            path: candidate.path,
            id: candidate.id,
        });
    }
    Ok(ArchiveReport {
        dry_run: true,
        plan: Some(plan),
        archived: None,
        skipped: None,
        warnings,
    })
}

// =========================================================
// The run
// =========================================================

/// What a run of `archive` needs.
pub struct ArchiveRequest<'a> {
    pub collection: &'a Collection,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdSource,
    pub pool: &'a Pool,
    pub dry_run: bool,
}

/// Archives every idle piece, or with `dry_run` lists them.
pub fn archive(request: &ArchiveRequest<'_>) -> Result<ArchiveReport, ArchiveError> {
    archive_with(request, &RealSeams)
}

pub(crate) fn archive_with(
    request: &ArchiveRequest<'_>,
    seams: &dyn Seams,
) -> Result<ArchiveReport, ArchiveError> {
    let collection = request.collection;
    let guard = match run::start(collection, request.clock, request.dry_run)? {
        Started::DryRun { .. } => return plan_with(collection, request.clock, seams),
        Started::Guarded(guard) => guard,
    };

    let (candidates, found) = candidates(collection, request.clock, seams, true)?;
    let mut warnings: Vec<Warning> = guard.warnings().to_vec();
    warnings.extend(found);
    let mut archived = Vec::new();
    let mut skipped = Vec::new();
    if !candidates.is_empty() {
        let config = &collection.config().collection;
        let root = open_root(collection)?;
        let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
        // `status` has warned about the junk list already.
        let junk = status::junk_names(&config.junk, &mut Vec::new());
        let cx = new::Context {
            collection,
            lock: guard.lock(),
            clock: request.clock,
            ids: request.ids,
            pool: request.pool,
            seams,
            junk,
            rules,
            idle: crate::idle::Settings::new(config.idle_days, &config.ignore)?,
            root,
            machine: collection.machine_name()?,
            orphans: if candidates.iter().any(|candidate| candidate.id.is_none()) {
                new::find_orphans(collection)?
            } else {
                Vec::new()
            },
        };
        let mut listing_stored = false;
        for candidate in &candidates {
            let result = new::archive_piece(&cx, candidate)?;
            warnings.extend(result.warnings);
            listing_stored |= result.listing_stored;
            match result.outcome {
                new::PieceOutcome::Dropped => {}
                new::PieceOutcome::Skipped(row) => skipped.push(row),
                new::PieceOutcome::Archived(row) => archived.push(row),
            }
        }
        if listing_stored && let Err(error) = index::update(collection, guard.lock()) {
            warnings.push(Warning::new(format!("cannot update the index: {error}")));
        }
    }
    Ok(ArchiveReport {
        dry_run: false,
        plan: None,
        archived: Some(archived),
        skipped: Some(skipped),
        warnings,
    })
}

fn open_root(collection: &Collection) -> Result<TreeRoot, ArchiveError> {
    TreeRoot::open(collection.root()).map_err(|source| ArchiveError::Io {
        action: "open",
        path: collection.root().display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::meta::{CreatedSource, Lifecycle, Metadata};
    use crate::relpath::Name;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use crate::tracking::{EntryState, TrackingEntry, VersionTakenOut};
    use std::fs;

    pub(super) const NOW: &str = "2026-11-01T12:00:00Z";

    pub(super) fn clock() -> FixedClock {
        FixedClock::new(NOW.parse().expect("a time"))
    }

    pub(super) fn days_ago(days: i64) -> i64 {
        NOW.parse::<Timestamp>().expect("a time").as_second() - days * 86_400
    }

    /// A collection whose folders a test lays out.
    pub(super) struct World {
        pub(super) temp: TempCollection,
        pub(super) collection: Collection,
    }

    pub(super) fn world() -> World {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        World { temp, collection }
    }

    impl World {
        pub(super) fn file(&mut self, relative: &str, age_days: i64) {
            self.temp
                .tree()
                .file_with(relative, "x", 0o644, days_ago(age_days));
        }

        pub(super) fn configure(&mut self, lines: &str) {
            fs::write(
                self.temp.root().join(".antiq/config.toml"),
                format!("[collection]\nid = \"{COLLECTION_ID}\"\n{lines}\n"),
            )
            .expect("a config can be written");
            self.collection = Collection::find(self.temp.root()).expect("a collection");
        }

        fn plan(&self) -> ArchiveReport {
            plan_with(&self.collection, &clock(), &RealSeams).expect("the plan is made")
        }

        fn planned(&self) -> Vec<String> {
            self.plan()
                .plan
                .expect("a dry run has a plan")
                .actions
                .iter()
                .map(|action| match action {
                    ArchiveAction::Archive { path, .. } => path.to_string(),
                })
                .collect()
        }
    }

    /// Live metadata of `owner` and an entry for the piece at `at`; the
    /// entry is changed by `change` before it is written.
    pub(super) fn live_piece(
        world: &World,
        number: u8,
        at: &str,
        owner: &str,
        change: impl FnOnce(&mut TrackingEntry),
    ) {
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(
            1_600_000_000_000 + u64::from(number),
            u128::from(number),
        ));
        let path = RelPath::parse(at).expect("a valid path");
        let name = Name::from_bytes(at.as_bytes().to_vec()).expect("a valid name");
        let when: Timestamp = "2026-10-01T10:00:00Z".parse().expect("a time");
        let metadata = Metadata {
            format: 1,
            id,
            collection: owner.parse().expect("an ID"),
            path: path.clone(),
            name,
            created: when,
            created_source: CreatedSource::Birth,
            lifecycle: Lifecycle::Live,
            payloads: Vec::new(),
            history: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let directory = world.collection.object_dir(id);
        fs::create_dir_all(&directory).expect("the object directory can be made");
        fs::write(
            directory.join("meta.1"),
            crate::meta::to_bytes(&metadata).expect("metadata serialises"),
        )
        .expect("metadata can be written");
        let mut entry = TrackingEntry::new(
            id,
            path,
            EntryState::Live,
            VersionTakenOut::Committed(1),
            when,
        );
        change(&mut entry);
        fs::write(
            world.collection.tracking_path(id),
            serde_json::to_vec(&entry).expect("an entry serialises"),
        )
        .expect("an entry can be written");
    }

    /// A second device below the piece at `path`, relative to its top.
    struct OtherDevice(&'static str);

    impl walk::Hooks for OtherDevice {
        fn device(&self, path: &RelPath, reported: u64) -> u64 {
            if path.to_string() == self.0 {
                reported + 1
            } else {
                reported
            }
        }

        fn fault(&self, _: &RelPath, _: walk::Call) -> Option<io::Error> {
            None
        }
    }

    struct Hooked(OtherDevice);

    impl Seams for Hooked {
        fn walk_hooks(&self) -> &(dyn walk::Hooks + Sync) {
            &self.0
        }
    }

    // =========================================================
    // Which pieces
    // =========================================================

    #[test]
    fn dry_run_lists_idle_new_and_live_pieces_in_path_order() {
        let mut world = world();
        world.file("c/note.txt", 30);
        world.file("a/note.txt", 30);
        world.file("busy/note.txt", 1);
        world.file("b/note.txt", 30);
        live_piece(&world, 1, "b", COLLECTION_ID, |_| {});
        let plan = world.plan();
        assert!(plan.dry_run);
        let actions = plan.plan.expect("a dry run has a plan").actions;
        let listed: Vec<(String, Option<ObjectId>)> = actions
            .iter()
            .map(|action| match action {
                ArchiveAction::Archive { path, id, .. } => (path.to_string(), *id),
            })
            .collect();
        let live_id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1));
        assert_eq!(
            listed,
            [
                ("a".to_string(), None),
                ("b".to_string(), Some(live_id)),
                ("c".to_string(), None)
            ]
        );
    }

    /// `world` holds an idle piece `sibling` next to whatever the test
    /// made; only the sibling is planned.
    fn only_the_sibling(mut world: World) {
        world.file("sibling/note.txt", 30);
        assert_eq!(world.planned(), ["sibling"]);
    }

    #[test]
    fn a_pinned_piece_is_not_planned() {
        let mut world = world();
        world.file("p/.antiq-pin", 90);
        world.file("p/note.txt", 90);
        only_the_sibling(world);
    }

    #[test]
    fn an_empty_piece_is_not_planned() {
        let mut world = world();
        world.temp.tree().dir("e/sub");
        only_the_sibling(world);
    }

    #[test]
    fn an_unreadable_piece_is_not_planned() {
        let mut world = world();
        world.file("u/note.txt", 30);
        world.temp.tree().unreadable_dir("u/sub");
        assert!(
            fs::read_dir(world.temp.root().join("u/sub")).is_err(),
            "the test needs a directory that cannot be read; it must not run as root"
        );
        only_the_sibling(world);
    }

    #[test]
    fn a_piece_with_a_second_device_is_not_planned() {
        let mut world = world();
        world.file("m/note.txt", 30);
        world.temp.tree().dir("m/mnt");
        world.file("sibling/note.txt", 30);
        let report = plan_with(&world.collection, &clock(), &Hooked(OtherDevice("mnt")))
            .expect("the plan is made");
        let planned: Vec<String> = report
            .plan
            .expect("a plan")
            .actions
            .iter()
            .map(|action| match action {
                ArchiveAction::Archive { path, .. } => path.to_string(),
            })
            .collect();
        assert_eq!(planned, ["sibling"]);
    }

    #[test]
    fn a_piece_holding_a_collection_is_not_planned() {
        let mut world = world();
        world.file("n/note.txt", 30);
        world.temp.tree().dir("n/inner/.antiq");
        only_the_sibling(world);
    }

    #[test]
    fn a_piece_holding_an_exclude_entry_is_not_planned() {
        let mut world = world();
        world.configure("exclude = [\"x/skip\"]");
        world.file("x/note.txt", 30);
        world.temp.tree().dir("x/skip");
        only_the_sibling(world);
    }

    #[test]
    fn a_young_piece_is_not_planned() {
        let mut world = world();
        world.file("busy/note.txt", 3);
        only_the_sibling(world);
    }

    #[test]
    fn an_unearthed_piece_is_not_planned() {
        let mut world = world();
        world.file("work/note.txt", 30);
        live_piece(&world, 1, "work", COLLECTION_ID, |entry| {
            entry.unearthed = Some("2026-10-30T10:00:00Z".parse().expect("a time"));
        });
        only_the_sibling(world);
    }

    #[test]
    fn a_diverged_live_piece_is_not_planned() {
        let mut world = world();
        world.file("work/note.txt", 30);
        live_piece(&world, 1, "work", COLLECTION_ID, |entry| {
            entry.diverged = true;
        });
        only_the_sibling(world);
    }

    #[test]
    fn a_live_piece_of_another_collection_is_not_planned() {
        let world = world();
        live_piece(&world, 1, "theirs", "01D39ZY06FGSCTVN4T2V9PKHFZ", |_| {});
        // Another collection's piece has no entry of ours.
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_001, 1));
        fs::remove_file(world.collection.tracking_path(id)).expect("the entry exists");
        only_the_sibling(world);
    }

    // =========================================================
    // A dry run changes nothing
    // =========================================================

    /// Every path below `root` with its size and modification time.
    fn snapshot(root: &Path) -> Vec<(String, u64, i64)> {
        use std::os::unix::fs::MetadataExt;
        let mut found = Vec::new();
        let mut open = vec![root.to_path_buf()];
        while let Some(directory) = open.pop() {
            for entry in fs::read_dir(&directory).expect("a directory") {
                let path = entry.expect("an entry").path();
                let metadata = fs::symlink_metadata(&path).expect("metadata");
                found.push((path.display().to_string(), metadata.len(), metadata.mtime()));
                if metadata.is_dir() {
                    open.push(path);
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn a_dry_run_writes_nothing_below_antiq() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let before = snapshot(world.temp.root());
        let _ = world.plan();
        assert_eq!(snapshot(world.temp.root()), before);
    }

    #[test]
    fn a_dry_run_names_the_lock_holder() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let _lock = crate::lock::acquire(&world.collection.lock_path(), "laptop", &clock())
            .expect("the lock is free");
        let warnings = world.plan().warnings;
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("another run holds the lock")),
            "{warnings:?}"
        );
    }

    // =========================================================
    // Sizes
    // =========================================================

    #[test]
    fn the_action_carries_the_apparent_size_in_bytes() {
        let mut world = world();
        let tree = world.temp.tree();
        tree.file_with("a/one.txt", "12345", 0o644, days_ago(30));
        tree.file_with("a/two.txt", "123", 0o644, days_ago(30));
        let actions = world.plan().plan.expect("a plan").actions;
        assert_eq!(
            actions,
            [ArchiveAction::Archive {
                path: RelPath::parse("a").expect("valid"),
                id: None,
                size: 8
            }]
        );
        assert_eq!(actions[0].describe(), "archive a (8 B)");
    }

    #[test]
    fn apparent_size_counts_a_hard_linked_file_once() {
        let mut world = world();
        let tree = world.temp.tree();
        tree.file_with("a/one.txt", "12345", 0o644, days_ago(30));
        tree.hard_link("a/one.txt", "a/two.txt");
        let actions = world.plan().plan.expect("a plan").actions;
        assert!(
            matches!(&actions[0], ArchiveAction::Archive { size: 5, .. }),
            "{actions:?}"
        );
    }

    #[test]
    fn sizes_are_written_in_decimal_units() {
        let cases = [
            (0, "0 B"),
            (999, "999 B"),
            (1_000, "1.0 kB"),
            (1_234, "1.2 kB"),
            (9_960_000, "10 MB"),
            (88_000_000, "88 MB"),
            (999_600, "1.0 MB"),
            (412_000_000, "412 MB"),
            (1_600_000_000, "1.6 GB"),
        ];
        for (bytes, text) in cases {
            assert_eq!(size_text(bytes), text, "{bytes} bytes");
        }
    }

    #[test]
    fn a_dry_run_serialises_dry_run_actions_and_warnings_in_that_order() {
        let mut world = world();
        world.file("a/note.txt", 30);
        let text = serde_json::to_string(&world.plan()).expect("the report serialises");
        assert_eq!(
            text,
            r#"{"dry_run":true,"actions":[{"action":"archive","path":"a","id":null,"size":1}],"warnings":[]}"#
        );
    }
}
