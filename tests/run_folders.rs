// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A collection copied back from the target lacks `.antiq/tmp/`,
//! `.antiq/trash/` and `.antiq/doomed/`, which `push` leaves out. Every
//! command that writes creates the folders it finds missing, and a dry run
//! lists their creation.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use support::cmd::{Antiq, Run, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection};
use support::scenario::target::Target;
use support::tree::Tree;

const LEFT_OUT_BY_PUSH: [&str; 3] = ["tmp", "trash", "doomed"];
const LAYOUT: [&str; 5] = ["tmp", "trash", "doomed", "recovered", "pieces"];
const OLD: &str = "2026-09-03T121500";
const NEW: &str = "2026-11-04T090211";

/// `projects/alpha` is live and due. `tools/old` exists as two versions.
fn collection() -> Collection {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*", "tools/*"]);
    builder.piece("projects/alpha", |piece| {
        piece.age_days(40).file("data", "x");
    });
    builder
        .version("tools/old", OLD, &[("run.sh", "echo\n")])
        .version("tools/old", NEW, &[("run.sh", "echo\n"), ("README", "r\n")]);
    builder.finish()
}

/// Removes the listing of the older version, so that `rebuild` has work.
fn damage_the_older_version(collection: &Collection) {
    let folder = collection.path(".antiq/pieces/tools/old");
    let listing = fs::read_dir(&folder)
        .expect("the store folder lists")
        .map(|entry| entry.expect("an entry").path())
        .find(|path| {
            path.to_string_lossy()
                .ends_with(&format!("{}.sha256.zst", OLD))
        })
        .expect("the older version has a listing");
    fs::remove_file(listing).expect("the listing is removed");
}

fn antiq_dir(collection: &Collection, folder: &str) -> std::path::PathBuf {
    collection.path(&format!(".antiq/{folder}"))
}

/// The collection as a copy from the target leaves it.
fn without_run_folders() -> Collection {
    strip(collection())
}

fn strip(collection: Collection) -> Collection {
    for folder in LEFT_OUT_BY_PUSH {
        fs::remove_dir(antiq_dir(&collection, folder)).expect("the folder is empty");
    }
    collection
}

fn antiq(root: &Path, args: &[&str]) -> Run {
    Antiq::new()
        .arg("--json")
        .arg("-C")
        .arg(root)
        .args(args)
        .run()
}

fn assert_layout_complete(collection: &Collection) {
    for folder in LAYOUT {
        assert!(
            antiq_dir(collection, folder).is_dir(),
            ".antiq/{folder} is missing"
        );
    }
}

/// The `repair` names of a real run, in order.
fn repair_names(json: &Value) -> Vec<String> {
    json["repairs"]
        .as_array()
        .expect("a repairs list")
        .iter()
        .map(|repair| repair["repair"].as_str().expect("a name").to_string())
        .collect()
}

/// The created folders of a list of repairs or dry-run actions.
fn created(items: &Value) -> Vec<String> {
    items
        .as_array()
        .expect("a list")
        .iter()
        .filter(|item| item["repair"] == "create_folder")
        .map(|item| item["folder"].as_str().expect("a folder").to_string())
        .collect()
}

fn expected(folders: &[&str]) -> Vec<String> {
    folders.iter().map(ToString::to_string).collect()
}

fn external_folder() -> Tree {
    let mut tree = Tree::new();
    tree.file("src/main.rs", "fn main() {}\n");
    tree
}

// =========================================================
// Every writing command in a collection without the folders
// =========================================================

/// Runs `args` in a collection without the three folders and returns the
/// run, with the folders checked afterwards.
fn writes(args: &[&str]) -> (Collection, Run) {
    writes_in(without_run_folders(), args)
}

fn writes_in(collection: Collection, args: &[&str]) -> (Collection, Run) {
    let run = antiq(collection.root().as_path(), args);
    assert_eq!(
        run.code(),
        0,
        "{args:?}: stdout {} stderr {}",
        run.stdout,
        run.stderr
    );
    assert_layout_complete(&collection);
    (collection, run)
}

fn reports_creation(args: &[&str]) {
    reports_creation_in(without_run_folders(), args);
}

fn reports_creation_in(collection: Collection, args: &[&str]) {
    let (_, run) = writes_in(collection, args);
    assert_eq!(
        created(&run.json()["repairs"]),
        expected(&LEFT_OUT_BY_PUSH),
        "{args:?}"
    );
    assert_eq!(repair_names(&run.json())[..3], ["create_folder"; 3]);
}

#[test]
fn archive_of_a_piece_creates_the_folders() {
    reports_creation(&["archive", "projects/alpha"]);
}

#[test]
fn unearth_creates_the_folders() {
    reports_creation(&["unearth", "tools/old"]);
}

#[test]
fn fork_creates_the_folders() {
    reports_creation(&["fork", "tools/old", "--name", "tools/copy"]);
}

#[test]
fn import_creates_the_folders() {
    let folder = external_folder();
    let root = folder.root().to_string_lossy().to_string();
    reports_creation(&[
        "import",
        &root,
        "--as",
        "projects/imported",
        "--time",
        "2020-01-01 10:00:00",
    ]);
}

#[test]
fn pin_creates_the_folders() {
    reports_creation(&["pin", "projects/alpha"]);
}

#[test]
fn unpin_creates_the_folders() {
    reports_creation(&["unpin", "projects/alpha"]);
}

#[test]
fn rebuild_creates_the_folders() {
    let collection = collection();
    damage_the_older_version(&collection);
    reports_creation_in(strip(collection), &["rebuild", "tools/old", OLD]);
}

#[test]
fn forget_creates_the_folders() {
    reports_creation(&["forget", "tools/old", NEW]);
}

#[test]
fn push_creates_the_folders_and_reports_no_repairs() {
    let target = Target::new();
    let collection = without_run_folders();
    collection.use_target(&target.path());
    let run = antiq(collection.root().as_path(), &["push", "--init"]);
    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert_layout_complete(&collection);
}

#[test]
fn a_collection_missing_the_whole_layout_gets_all_five_folders() {
    let collection = collection();
    for folder in LAYOUT {
        fs::remove_dir_all(antiq_dir(&collection, folder)).expect("the folder is removed");
    }
    let run = antiq(collection.root().as_path(), &["pin", "projects/alpha"]);
    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert_layout_complete(&collection);
    assert_eq!(created(&run.json()["repairs"]), expected(&LAYOUT));
}

#[test]
fn a_second_run_creates_nothing() {
    let (collection, _) = writes(&["pin", "projects/alpha"]);
    let run = antiq(collection.root().as_path(), &["unpin", "projects/alpha"]);
    assert_eq!(run.code(), 0);
    assert_eq!(run.json()["repairs"], Value::Array(Vec::new()));
}

// =========================================================
// The copy back from the target
// =========================================================

#[test]
fn a_copy_back_from_the_target_unearths() {
    let target = Target::new();
    let collection = collection();
    collection.use_target(&target.path());
    let pushed = antiq(collection.root().as_path(), &["push", "--init"]);
    assert_eq!(pushed.code(), 0, "{} {}", pushed.stdout, pushed.stderr);

    let copy = Tree::new();
    let status = Command::new("rsync")
        .arg("-aH")
        .arg(format!("{}/", target.path().display()))
        .arg(format!("{}/", copy.root().display()))
        .status()
        .expect("rsync runs");
    assert!(status.success(), "rsync copies the target back");
    for folder in LEFT_OUT_BY_PUSH {
        assert!(
            !copy.root().join(".antiq").join(folder).exists(),
            "the copy lacks .antiq/{folder}, as the mirror leaves it out"
        );
    }

    let run = antiq(copy.root(), &["unearth", "tools/old"]);
    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert!(copy.root().join("tools/old/run.sh").is_file());
}

// =========================================================
// Dry runs plan the creation and write nothing
// =========================================================

fn plans(args: &[&str]) {
    plans_in(without_run_folders(), args);
}

fn plans_in(collection: Collection, args: &[&str]) {
    let before = snapshot(collection.root().as_path());
    let run = antiq(collection.root().as_path(), args);
    assert_eq!(
        run.code(),
        0,
        "{args:?}: stdout {} stderr {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(
        created(&run.json()["actions"]),
        expected(&LEFT_OUT_BY_PUSH),
        "{args:?}"
    );
    assert_eq!(
        run.json()["actions"][0]["repair"],
        "create_folder",
        "{args:?}: repairs come first"
    );
    assert_eq!(
        snapshot(collection.root().as_path()),
        before,
        "{args:?}: a dry run wrote"
    );
}

#[test]
fn an_archive_dry_run_plans_the_creation() {
    plans(&["archive", "projects/alpha", "-n"]);
}

#[test]
fn an_unearth_dry_run_plans_the_creation() {
    plans(&["unearth", "tools/old", "-n"]);
}

#[test]
fn a_fork_dry_run_plans_the_creation() {
    plans(&["fork", "tools/old", "--name", "tools/copy", "-n"]);
}

#[test]
fn an_import_dry_run_plans_the_creation() {
    let folder = external_folder();
    let root = folder.root().to_string_lossy().to_string();
    plans(&[
        "import",
        &root,
        "--as",
        "projects/imported",
        "--time",
        "2020-01-01 10:00:00",
        "-n",
    ]);
}

#[test]
fn a_pin_dry_run_plans_the_creation() {
    plans(&["pin", "projects/alpha", "-n"]);
}

#[test]
fn an_unpin_dry_run_plans_the_creation() {
    plans(&["unpin", "projects/alpha", "-n"]);
}

#[test]
fn a_forget_dry_run_plans_the_creation() {
    plans(&["forget", "tools/old", NEW, "-n"]);
}

#[test]
fn a_rebuild_dry_run_plans_the_creation() {
    let collection = collection();
    damage_the_older_version(&collection);
    plans_in(strip(collection), &["rebuild", "tools/old", OLD, "-n"]);
}

#[test]
fn a_push_dry_run_plans_the_creation() {
    let target = Target::new();
    let collection = without_run_folders();
    collection.use_target(&target.path());
    let before = snapshot(collection.root().as_path());
    let run = antiq(collection.root().as_path(), &["push", "--init", "-n"]);
    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert_eq!(created(&run.json()["actions"]), expected(&LEFT_OUT_BY_PUSH));
    assert_eq!(snapshot(collection.root().as_path()), before);
}

#[test]
fn a_dry_run_while_another_run_holds_the_lock_lists_no_creation() {
    let collection = without_run_folders();
    let _lock = hold_lock(collection.root().as_path());
    let before = snapshot(collection.root().as_path());
    let run = antiq(
        collection.root().as_path(),
        &["pin", "projects/alpha", "-n"],
    );
    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert!(created(&run.json()["actions"]).is_empty());
    assert_eq!(snapshot(collection.root().as_path()), before);
}

// =========================================================
// A folder that cannot be created is a warning
// =========================================================

/// Makes `path` read-only, and writable again when dropped, also after a
/// panic, so the temporary directory can always be removed.
struct ReadOnly(std::path::PathBuf);

impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_folder_that_cannot_be_created_is_a_warning_and_not_a_repair() {
    let collection = without_run_folders();
    // The run creates the lock file before it repairs, so the lock exists
    // and `.antiq/` can be locked against new entries.
    fs::write(collection.path(".antiq/lock"), "").expect("the lock file is written");
    let antiq_path = collection.path(".antiq");
    fs::set_permissions(&antiq_path, fs::Permissions::from_mode(0o555)).expect("read-only");
    let _restore = ReadOnly(antiq_path.clone());
    assert!(
        fs::write(antiq_path.join("probe"), "").is_err(),
        "the test needs a user that a read-only directory stops"
    );

    let run = antiq(collection.root().as_path(), &["forget", "tools/old", NEW]);

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert!(created(&run.json()["repairs"]).is_empty());
    let warnings = run.json()["warnings"].to_string();
    assert!(
        warnings.contains("cannot create the run folder .antiq/tmp"),
        "{warnings}"
    );
}
