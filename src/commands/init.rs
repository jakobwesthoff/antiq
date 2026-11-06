// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `init` command: making a directory a collection.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::commands::survey::{Verdict, survey_with};
use crate::config::{ConfigDoc, ConfigError};
use crate::filesystem::atomic::{flush_dir, write_atomic};
use crate::filesystem::tree::TreeRoot;
use crate::filesystem::{FilesystemError, io_error};
use crate::id::{IdSource, ObjectId};
use crate::relpath::{Name, RelPath};
use crate::report::{CommandReport, Warning};
use crate::select::{Rules, SelectError, select};
use crate::tree::EntryKind;
use crate::walk::{Hooks, RealFs};
use jiff::Timestamp;
use serde::Serialize;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Errors of the `init` command.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("{} is already a collection", root.display())]
    AlreadyACollection { root: PathBuf },
    #[error("{} lies inside the collection at {}", directory.display(), root.display())]
    InsideACollection { directory: PathBuf, root: PathBuf },
    #[error("{} exists and is not a directory", path.display())]
    AntiqIsNotADirectory { path: PathBuf },
    #[error(
        "these folders would be pieces and hold a collection of their own: {}",
        list(roots)
    )]
    NestedCollections { roots: Vec<PathBuf> },
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Filesystem(#[from] FilesystemError),
}

fn list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl crate::error::HasKind for InitError {}

/// What `init` is going to do, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InitAction {
    CreateCollection { path: String },
}

/// Everything `init` decided before it creates anything.
#[derive(Debug, Clone)]
pub struct InitPlan {
    root: PathBuf,
    /// The patterns in effect: the default one when none was given.
    patterns: Vec<String>,
    /// The patterns as given, which are the ones the config states.
    given_patterns: Vec<String>,
    warnings: Vec<Warning>,
}

/// What a run of `init` did or would do.
#[derive(Debug, Serialize)]
pub struct InitReport {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub path: String,
    pub patterns: Vec<String>,
    /// How many folders the next `archive` would archive. Absent in a dry
    /// run, and when the new collection could not be surveyed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<usize>,
    /// The `config.toml` a dry run would write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    /// What a dry run would do; a real run lists nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<InitAction>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for InitReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// What the caller asks `init` to do.
#[derive(Debug)]
pub struct InitRequest<'a> {
    pub directory: &'a Path,
    /// Empty selects with the default pattern.
    pub patterns: &'a [String],
    pub dry_run: bool,
}

/// The directories of the local storage layout that exist from the start.
/// `lock` is made by the first run that needs it.
const LAYOUT: [&str; 5] = ["tmp", "trash", "doomed", "recovered", "pieces"];

/// How many `.antiq-init-<pid>-<n>` names are tried before giving up.
const STAGING_ATTEMPTS: u32 = 100;

fn path(text: &str) -> RelPath {
    RelPath::parse(text).expect("a fixed name is a valid path")
}

/// Works out what `init` would do in `directory`, and refuses where it
/// cannot do it. Nothing is written.
pub fn plan(directory: &Path, patterns: &[String]) -> Result<InitPlan, InitError> {
    let root = fs::canonicalize(directory).map_err(io_error("resolve", directory))?;

    // `find_root` can only fail here with "not a collection": the path was
    // resolved just above.
    if let Ok(found) = Collection::find_root(&root) {
        return Err(if found == root {
            InitError::AlreadyACollection { root: found }
        } else {
            InitError::InsideACollection {
                directory: root,
                root: found,
            }
        });
    }
    let tree = TreeRoot::open(&root).map_err(io_error("open", &root))?;

    // `find_root` ignores a `.antiq` that is not a directory, so this is
    // the only place that refuses it.
    if tree.lstat(&path(".antiq")).is_ok() {
        return Err(InitError::AntiqIsNotADirectory {
            path: root.join(".antiq"),
        });
    }

    let given_patterns = patterns.to_vec();
    let patterns = if patterns.is_empty() {
        vec!["*".to_string()]
    } else {
        patterns.to_vec()
    };
    let rules = Rules::new(&patterns, &[])?.bind(&tree)?;
    let pieces = select(&tree, &rules)?.pieces().to_vec();
    refuse_nested_collections(&tree, &root, &pieces)?;

    let warnings = patterns
        .iter()
        .filter(|pattern| pattern.contains("**"))
        .map(|pattern| {
            Warning::new(format!(
                "pattern {pattern} contains `**`, which matches any number of directories; \
                 where matches nest, the shallowest match is the piece"
            ))
        })
        .collect();
    Ok(InitPlan {
        root,
        patterns,
        given_patterns,
        warnings,
    })
}

/// A piece is archived whole, so a piece that is a collection itself
/// would take that collection's `.antiq/` with it.
fn refuse_nested_collections(
    tree: &TreeRoot,
    root: &Path,
    pieces: &[RelPath],
) -> Result<(), InitError> {
    let marker = Name::from_bytes(b".antiq".to_vec()).expect("a fixed name is valid");
    let roots: Vec<PathBuf> = pieces
        .iter()
        .filter(|piece| {
            tree.lstat(&piece.join(&marker))
                .is_ok_and(|entry| entry.kind == EntryKind::Directory)
        })
        .map(|piece| root.join(OsStr::from_bytes(piece.as_bytes())))
        .collect();
    if roots.is_empty() {
        Ok(())
    } else {
        Err(InitError::NestedCollections { roots })
    }
}

/// Mints the collection's ID and renders the `config.toml` that carries it.
fn render_config(
    plan: &InitPlan,
    clock: &dyn Clock,
    ids: &dyn IdSource,
) -> Result<(ObjectId, String), InitError> {
    let id = ids.mint(clock.now());
    let config = ConfigDoc::new(id, &plan.given_patterns).render()?;
    Ok((id, config))
}

/// Creates the collection the plan describes. The `.antiq/` directory is
/// built next to its final place and renamed into it, so it exists
/// complete or not at all.
pub fn apply(
    plan: &InitPlan,
    clock: &dyn Clock,
    ids: &dyn IdSource,
) -> Result<InitReport, InitError> {
    apply_with(plan, clock, ids, &RealFs)
}

fn apply_with(
    plan: &InitPlan,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    hooks: &dyn Hooks,
) -> Result<InitReport, InitError> {
    let (id, config) = render_config(plan, clock, ids)?;

    let staged = create_staging_directory(&plan.root)?;
    let published = fill_and_publish(&plan.root, &staged, &config);
    if published.is_err() {
        // Best effort: the failure being reported matters more than a
        // leftover that cannot be removed either.
        let _ = fs::remove_dir_all(&staged);
    }
    published?;

    let mut report = plan.report(Some(id), None, false);
    match count_due(&plan.root, clock.now(), hooks) {
        Ok(due) => report.due = Some(due),
        Err(reason) => report.warnings.push(Warning::new(format!(
            "cannot tell how many folders the next archive would archive: {reason}"
        ))),
    }
    Ok(report)
}

/// How many folders the next `archive` would archive: the ones `status`
/// shows as due. The error is the text for the warning that replaces the
/// count, since the collection exists by now and stays.
fn count_due(root: &Path, now: Timestamp, hooks: &dyn Hooks) -> Result<usize, String> {
    let collection = Collection::find(root).map_err(|error| error.to_string())?;
    let surveyed = survey_with(&collection, now, hooks).map_err(|error| error.to_string())?;
    Ok(surveyed
        .pieces
        .iter()
        .filter(|piece| piece.verdict == Verdict::Due)
        .count())
}

/// Says what `apply` would write, and writes nothing. The ID in the config
/// is minted for the print; a real run mints its own.
pub fn preview(
    plan: &InitPlan,
    clock: &dyn Clock,
    ids: &dyn IdSource,
) -> Result<InitReport, InitError> {
    let (_, config) = render_config(plan, clock, ids)?;
    Ok(plan.report(None, Some(config), true))
}

/// Makes `.antiq-init-<pid>-<n>` in `root` with the first `n` that is
/// free, so a directory left by a run that was killed never blocks the
/// next one.
fn create_staging_directory(root: &Path) -> Result<PathBuf, InitError> {
    let pid = std::process::id();
    for attempt in 0..STAGING_ATTEMPTS {
        let staged = root.join(format!(".antiq-init-{pid}-{attempt}"));
        match fs::create_dir(&staged) {
            Ok(()) => return Ok(staged),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error("create", &staged)(error).into()),
        }
    }
    let error = io::Error::from(io::ErrorKind::AlreadyExists);
    Err(io_error("create", &root.join(format!(".antiq-init-{pid}-*")))(error).into())
}

fn fill_and_publish(root: &Path, staged: &Path, config: &str) -> Result<(), InitError> {
    for directory in LAYOUT {
        let made = staged.join(directory);
        fs::create_dir(&made).map_err(io_error("create", &made))?;
    }
    // Written last: the write ends by flushing `staged`, which makes the
    // entries made above durable as well.
    write_atomic(&staged.join("config.toml"), config.as_bytes())?;

    let tree = TreeRoot::open(root).map_err(io_error("open", root))?;
    let name = staged
        .file_name()
        .expect("the staging directory has a name");
    let from = RelPath::from_bytes(name.as_bytes().to_vec()).expect("its name is a valid path");
    tree.rename(&from, &tree, &path(".antiq"))
        .map_err(io_error("create", &root.join(".antiq")))?;
    flush_dir(root)?;
    Ok(())
}

/// Makes the directory a collection, or with a dry run says what it would
/// do. It never asks.
pub fn init(
    request: &InitRequest<'_>,
    clock: &dyn Clock,
    ids: &dyn IdSource,
) -> Result<InitReport, InitError> {
    let plan = plan(request.directory, request.patterns)?;
    if request.dry_run {
        preview(&plan, clock, ids)
    } else {
        apply(&plan, clock, ids)
    }
}

impl InitPlan {
    fn report(&self, id: Option<ObjectId>, config: Option<String>, dry_run: bool) -> InitReport {
        let path = self.root.display().to_string();
        InitReport {
            dry_run,
            created: id.is_some(),
            id,
            patterns: self.patterns.clone(),
            due: None,
            config,
            actions: if dry_run {
                vec![InitAction::CreateCollection { path: path.clone() }]
            } else {
                Vec::new()
            },
            path,
            warnings: self.warnings.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::config::Config;
    use crate::id::SeqIds;
    use crate::testing::tree::{TempCollection, Tree};
    use std::fs;

    fn clock() -> FixedClock {
        FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a valid time"))
    }

    fn canonical(path: &Path) -> PathBuf {
        fs::canonicalize(path).expect("the path exists")
    }

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn plan_of(tree: &Tree, list: &[&str]) -> InitPlan {
        plan(tree.root(), &patterns(list)).expect("the directory can be planned")
    }

    fn apply_plan(plan: &InitPlan) -> InitReport {
        apply(plan, &clock(), &SeqIds::new(1)).expect("the collection can be created")
    }

    // =========================================================
    // What the patterns select
    // =========================================================

    #[test]
    fn given_patterns_replace_the_default() {
        let mut tree = Tree::new();
        tree.dir("a1");
        tree.dir("b1");
        let plan = plan_of(&tree, &["a*"]);
        assert_eq!(plan.patterns, ["a*"]);
    }

    #[test]
    fn without_patterns_the_default_is_the_plan() {
        let tree = Tree::new();
        assert_eq!(plan_of(&tree, &[]).patterns, ["*"]);
    }

    #[test]
    fn an_invalid_pattern_is_refused() {
        let tree = Tree::new();
        let error =
            plan(tree.root(), &patterns(&["a//b"])).expect_err("an empty component is invalid");
        assert!(matches!(
            error,
            InitError::Select(SelectError::InvalidPattern { .. })
        ));
    }

    #[test]
    fn a_pattern_with_double_star_gets_a_note() {
        let mut tree = Tree::new();
        tree.dir("a/b");
        let plan = plan_of(&tree, &["a/**"]);
        assert_eq!(plan.warnings.len(), 1);
        assert!(
            plan.warnings[0].message.contains("a/**"),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn a_pattern_without_double_star_gets_no_note() {
        let mut tree = Tree::new();
        tree.dir("a");
        let plan = plan_of(&tree, &["a*"]);
        assert!(plan.warnings.is_empty());
    }

    // =========================================================
    // Where a collection cannot be made
    // =========================================================

    #[test]
    fn a_directory_with_antiq_is_refused_naming_itself() {
        let collection = TempCollection::minimal();
        let error = plan(collection.root(), &[]).expect_err("the directory is a collection");
        assert_eq!(
            error.to_string(),
            format!(
                "{} is already a collection",
                canonical(collection.root()).display()
            )
        );
    }

    #[test]
    fn a_directory_below_a_collection_is_refused_naming_the_root() {
        let mut collection = TempCollection::minimal();
        let below = collection.tree().dir("projects/web");
        let error = plan(&below, &[]).expect_err("inside a collection");
        assert_eq!(
            error.to_string(),
            format!(
                "{} lies inside the collection at {}",
                canonical(&below).display(),
                canonical(collection.root()).display()
            )
        );
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let tree = Tree::new();
        let error = plan(&tree.path("missing"), &[]).expect_err("the directory does not exist");
        assert!(matches!(error, InitError::Filesystem(_)), "{error}");
    }

    #[test]
    fn a_file_outside_a_collection_is_an_open_error() {
        let mut tree = Tree::new();
        let file = tree.file("f", "x");
        let error = plan(&file, &[]).expect_err("a file is no directory");
        assert!(matches!(error, InitError::Filesystem(_)), "{error}");
    }

    #[test]
    fn a_file_inside_a_collection_is_refused_naming_the_root() {
        let mut collection = TempCollection::minimal();
        let file = collection.tree().file("f", "x");
        let error = plan(&file, &[]).expect_err("inside a collection");
        assert!(
            matches!(error, InitError::InsideACollection { .. }),
            "{error}"
        );
    }

    #[test]
    fn plan_refuses_a_file_named_antiq() {
        let mut tree = Tree::new();
        tree.file(".antiq", "x");
        let error = plan(tree.root(), &[]).expect_err("the name is taken");
        assert!(
            matches!(error, InitError::AntiqIsNotADirectory { .. }),
            "{error}"
        );
    }

    #[test]
    fn plan_refuses_a_symlink_named_antiq() {
        let mut tree = Tree::new();
        tree.dir("elsewhere");
        tree.symlink(".antiq", "elsewhere");
        let error = plan(tree.root(), &[]).expect_err("the name is taken");
        assert!(
            matches!(error, InitError::AntiqIsNotADirectory { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_piece_holding_a_collection_is_refused_naming_each_root() {
        let mut tree = Tree::new();
        tree.dir("one/.antiq");
        tree.dir("two/.antiq");
        tree.dir("plain");
        let error = plan(tree.root(), &[]).expect_err("nested collections");
        let root = canonical(tree.root());
        assert_eq!(
            error.to_string(),
            format!(
                "these folders would be pieces and hold a collection of their own: {}, {}",
                root.join("one").display(),
                root.join("two").display()
            )
        );
    }

    #[test]
    fn a_nested_collection_outside_the_pieces_is_no_problem() {
        let mut tree = Tree::new();
        tree.dir("one/.antiq");
        tree.dir("plain");
        plan_of(&tree, &["plain"]);
    }

    // =========================================================
    // Creating the collection
    // =========================================================

    #[test]
    fn apply_creates_the_layout_of_the_local_storage() {
        let tree = Tree::new();
        apply_plan(&plan_of(&tree, &[]));
        let mut names: Vec<String> = fs::read_dir(tree.path(".antiq"))
            .expect("the marker can be listed")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "config.toml",
                "doomed",
                "pieces",
                "recovered",
                "tmp",
                "trash"
            ]
        );
        for directory in ["tmp", "trash", "doomed", "recovered", "pieces"] {
            assert!(
                tree.path(".antiq").join(directory).is_dir(),
                "{directory} is missing"
            );
        }
        assert!(
            !tree.path(".antiq/lock").exists(),
            "the first run that needs the lock makes it"
        );
    }

    #[test]
    fn apply_writes_the_settings_into_the_config() {
        let mut tree = Tree::new();
        tree.dir("p");
        let report = apply_plan(&plan_of(&tree, &["p*", "archive"]));
        let config = Config::read(&tree.path(".antiq/config.toml")).expect("a readable config");
        assert_eq!(Some(config.collection.id), report.id);
        assert_eq!(config.collection.patterns, ["p*", "archive"]);
        assert!(config.collection.exclude.is_empty());
    }

    #[test]
    fn the_collection_id_carries_the_time_of_creation() {
        let tree = Tree::new();
        let report = apply_plan(&plan_of(&tree, &[]));
        let id = report.id.expect("a created collection has an id");
        let now = clock().now();
        assert_eq!(
            id.timestamp_ms(),
            u64::try_from(now.as_millisecond()).expect("after 1970")
        );
    }

    #[test]
    fn apply_leaves_no_temporary_directory_behind() {
        let mut tree = Tree::new();
        tree.dir("a");
        apply_plan(&plan_of(&tree, &[]));
        let mut entries: Vec<String> = fs::read_dir(tree.root())
            .expect("the root can be listed")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect();
        entries.sort();
        assert_eq!(entries, [".antiq", "a"]);
    }

    #[test]
    fn a_created_collection_is_found_and_read() {
        let tree = Tree::new();
        let report = apply_plan(&plan_of(&tree, &[]));
        let collection = crate::collection::Collection::find(tree.root()).expect("a collection");
        assert_eq!(Some(collection.id()), report.id);
        assert_eq!(collection.root(), canonical(tree.root()));
    }

    #[test]
    fn a_file_named_antiq_blocks_creation_and_stays() {
        let mut tree = Tree::new();
        let plan = plan_of(&tree, &[]);
        // The plan found no marker; the file appears afterwards, as it
        // would in a race.
        let file = tree.file(".antiq", "mine");
        let error = apply(&plan, &clock(), &SeqIds::new(1)).expect_err("the name is taken");
        assert!(matches!(error, InitError::Filesystem(_)), "{error}");
        assert_eq!(fs::read_to_string(file).expect("still there"), "mine");
        let leftovers: Vec<_> = fs::read_dir(tree.root())
            .expect("the root can be listed")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(leftovers, [std::ffi::OsString::from(".antiq")]);
    }

    #[test]
    fn a_directory_that_cannot_be_written_is_an_error() {
        let mut tree = Tree::new();
        let directory = tree.dir("readonly");
        let _guard = crate::filesystem::RestoreModes(vec![directory.clone()]);
        fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o500),
        )
        .expect("the mode can be set");
        assert!(
            fs::create_dir(directory.join("probe")).is_err(),
            "the test needs a user that mode 500 locks out"
        );
        let plan = plan(&directory, &[]).expect("planning only reads");
        let error = apply(&plan, &clock(), &SeqIds::new(1)).expect_err("nothing can be created");
        assert!(matches!(error, InitError::Filesystem(_)), "{error}");
        assert_eq!(
            fs::read_dir(&directory).expect("listable").count(),
            0,
            "nothing is left behind"
        );
    }

    #[test]
    fn the_default_pattern_is_the_one_of_the_config() {
        let tree = Tree::new();
        let defaults = Config::new(SeqIds::new(1).mint(clock().now()));
        assert_eq!(plan_of(&tree, &[]).patterns, defaults.collection.patterns);
    }

    #[test]
    fn a_stale_temporary_directory_does_not_block_init() {
        let mut tree = Tree::new();
        let stale = tree.dir(&format!(".antiq-init-{}-0", std::process::id()));
        apply_plan(&plan_of(&tree, &[]));
        assert!(stale.is_dir(), "an entry that is not ours stays");
        assert!(tree.path(".antiq/config.toml").is_file());
    }

    #[test]
    fn too_many_stale_temporary_directories_fail_without_a_trace() {
        let mut tree = Tree::new();
        for number in 0..100 {
            tree.dir(&format!(".antiq-init-{}-{number}", std::process::id()));
        }
        let error = apply(&plan_of(&tree, &[]), &clock(), &SeqIds::new(1))
            .expect_err("no free temporary name");
        assert!(matches!(error, InitError::Filesystem(_)), "{error}");
        assert!(!tree.path(".antiq").exists());
    }

    #[test]
    fn init_and_the_minimal_fixture_make_the_same_collection() {
        let tree = Tree::new();
        apply_plan(&plan_of(&tree, &[]));
        let minimal = TempCollection::minimal();
        let entries = |root: &Path| {
            let mut names: Vec<String> = fs::read_dir(root.join(".antiq"))
                .expect("the marker can be listed")
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into()
                })
                .collect();
            names.sort();
            names
        };
        assert_eq!(entries(tree.root()), entries(minimal.root()));
        let mut made = Config::read(&tree.path(".antiq/config.toml")).expect("readable");
        let fixture = Config::read(&minimal.root().join(".antiq/config.toml")).expect("readable");
        made.collection.id = fixture.collection.id;
        assert_eq!(made, fixture);
    }

    // =========================================================
    // Deciding
    // =========================================================

    fn request<'a>(tree: &'a Tree, patterns: &'a [String], dry_run: bool) -> InitRequest<'a> {
        InitRequest {
            directory: tree.root(),
            patterns,
            dry_run,
        }
    }

    fn run(request: &InitRequest<'_>) -> InitReport {
        init(request, &clock(), &SeqIds::new(1)).expect("init succeeds")
    }

    #[test]
    fn init_creates_the_collection() {
        let tree = Tree::new();
        let report = run(&request(&tree, &[], false));
        assert!(report.created);
        assert!(report.config.is_none());
        let config = Config::read(&tree.path(".antiq/config.toml")).expect("readable");
        assert_eq!(Some(config.collection.id), report.id);
    }

    #[test]
    fn a_dry_run_creates_nothing_and_reports_the_config() {
        let tree = Tree::new();
        let patterns = patterns(&["a*"]);
        let report = run(&request(&tree, &patterns, true));
        assert!(!report.created);
        assert!(report.dry_run);
        assert!(report.id.is_none());
        assert!(!tree.path(".antiq").exists());
        let text = report.config.expect("a dry run carries the config");
        let config = Config::parse(&text, None).expect("the text is a config");
        assert_eq!(config.collection.patterns, ["a*"]);
    }

    #[test]
    fn a_dry_run_in_a_collection_is_refused() {
        let collection = TempCollection::minimal();
        let error = init(
            &InitRequest {
                directory: collection.root(),
                patterns: &[],
                dry_run: true,
            },
            &clock(),
            &SeqIds::new(1),
        )
        .expect_err("the directory is a collection");
        assert!(matches!(error, InitError::AlreadyACollection { .. }));
    }

    #[test]
    fn no_run_is_partial() {
        let tree = Tree::new();
        assert!(!run(&request(&tree, &[], true)).partial());
        assert!(!run(&request(&tree, &[], false)).partial());
    }

    // =========================================================
    // The number of folders the next archive would archive
    // =========================================================

    const DAY: i64 = 86_400;

    /// A file in `relative` that was last changed `days` before the clock.
    fn aged(tree: &mut Tree, relative: &str, days: i64) {
        let seconds = clock().now().as_second() - days * DAY;
        tree.file_with(relative, "x", 0o644, seconds);
    }

    #[test]
    fn the_report_counts_the_folders_the_next_archive_would_archive() {
        let mut tree = Tree::new();
        aged(&mut tree, "old/file", 40);
        aged(&mut tree, "older/file", 400);
        aged(&mut tree, "fresh/file", 1);
        let report = run(&request(&tree, &[], false));
        assert_eq!(report.due, Some(2));
    }

    #[test]
    fn a_folder_exactly_idle_days_old_is_not_counted() {
        let mut tree = Tree::new();
        aged(&mut tree, "edge/file", 30);
        assert_eq!(run(&request(&tree, &[], false)).due, Some(0));
    }

    #[test]
    fn a_pinned_folder_and_an_empty_one_are_not_counted() {
        let mut tree = Tree::new();
        aged(&mut tree, "pinned/file", 400);
        tree.file("pinned/.antiq-pin", "");
        tree.dir("empty");
        assert_eq!(run(&request(&tree, &[], false)).due, Some(0));
    }

    #[test]
    fn the_count_follows_the_given_patterns() {
        let mut tree = Tree::new();
        aged(&mut tree, "a1/file", 400);
        aged(&mut tree, "b1/file", 400);
        let patterns = patterns(&["a*"]);
        assert_eq!(run(&request(&tree, &patterns, false)).due, Some(1));
    }

    /// Every call fails as if the process had run out of descriptors.
    struct OutOfDescriptors;

    impl Hooks for OutOfDescriptors {
        fn fault(&self, _path: &RelPath, _call: crate::walk::Call) -> Option<io::Error> {
            Some(io::Error::from_raw_os_error(libc::EMFILE))
        }
    }

    #[test]
    fn a_survey_that_fails_leaves_the_collection_and_a_warning() {
        let mut tree = Tree::new();
        aged(&mut tree, "old/file", 400);
        let report = apply_with(
            &plan_of(&tree, &[]),
            &clock(),
            &SeqIds::new(1),
            &OutOfDescriptors,
        )
        .expect("the collection is created");
        assert!(report.created);
        assert_eq!(report.due, None);
        let [warning] = report.warnings.as_slice() else {
            panic!("one warning expected: {:?}", report.warnings);
        };
        assert!(
            warning
                .message
                .starts_with("cannot tell how many folders the next archive would archive: "),
            "{}",
            warning.message
        );
        assert!(tree.path(".antiq/config.toml").is_file());
    }

    #[test]
    fn a_directory_that_is_no_collection_cannot_be_counted() {
        let tree = Tree::new();
        let reason = count_due(tree.root(), clock().now(), &RealFs).expect_err("no collection");
        assert!(reason.contains("collection"), "{reason}");
    }

    // =========================================================
    // The report
    // =========================================================

    #[test]
    fn a_created_report_serialises_in_the_documented_order() {
        let tree = Tree::new();
        let report = apply_plan(&plan_of(&tree, &["a*"]));
        let root = canonical(tree.root()).display().to_string();
        let id = SeqIds::new(1).mint(clock().now());
        assert_eq!(
            serde_json::to_string(&report).expect("serialises"),
            format!(
                "{{\"created\":true,\"id\":\"{id}\",\"path\":\"{root}\",\
                 \"patterns\":[\"a*\"],\"due\":0,\"warnings\":[]}}"
            )
        );
    }

    #[test]
    fn a_dry_run_report_says_so_and_has_no_id() {
        let tree = Tree::new();
        let report = run(&request(&tree, &[], true));
        let json = serde_json::to_value(&report).expect("serialises");
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["created"], false);
        assert!(json.get("id").is_none());
        assert!(json["config"].is_string());
    }

    #[test]
    fn a_report_without_the_flag_has_no_dry_run_key() {
        let tree = Tree::new();
        let report = run(&request(&tree, &[], false));
        let json = serde_json::to_value(&report).expect("serialises");
        assert!(json.get("dry_run").is_none());
        assert!(json.get("config").is_none());
    }

    #[test]
    fn the_report_keeps_the_notes() {
        let tree = Tree::new();
        let report = run(&request(&tree, &patterns(&["**"]), false));
        assert_eq!(CommandReport::warnings(&report).len(), 1);
    }
}
