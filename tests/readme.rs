// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `.antiq/README.md` seen from outside the binary: the commands it shows
//! exist, and every run that takes the lock writes it again when it is
//! missing or differs from the text of this antiq.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use antiq::collection::README;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use support::cmd::{Antiq, Run, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection};
use support::scenario::target::Target;
use support::tree::Tree;

/// The commands the README shows are the lines of its code blocks that
/// start with `antiq `, cut where the description of the command begins:
/// at the first run of two spaces.
fn commands_of_the_readme() -> Vec<Vec<&'static str>> {
    README
        .lines()
        .filter(|line| line.starts_with("antiq "))
        .map(|line| {
            let command = line.split("  ").next().expect("a line has a first part");
            command.split_whitespace().skip(1).collect()
        })
        .collect()
}

#[test]
fn every_command_the_readme_shows_exists() {
    let commands = commands_of_the_readme();
    assert!(!commands.is_empty(), "the README shows no antiq command");
    for words in commands {
        let name = words.first().expect("a command line names a command");
        let run = Antiq::new().args([*name, "--help"]).run();
        assert_eq!(run.code(), 0, "`antiq {name} --help`: {}", run.stderr);
    }
}

// =========================================================
// Collections as an older antiq left them, and as init does
// =========================================================

const OLD: &str = "2026-09-03T121500";
const NEW: &str = "2026-11-04T090211";
const WRITE: &str = "write .antiq/README.md";
const REPLACE: &str = "replace .antiq/README.md, which differs from the text of this antiq";

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
        .version("tools/old", NEW, &[("run.sh", "echo\n"), ("notes", "r\n")]);
    builder.finish()
}

fn readme_of(collection: &Collection) -> PathBuf {
    collection.path(".antiq/README.md")
}

/// The collection as an antiq without the README left it.
fn old_collection() -> Collection {
    let collection = collection();
    fs::remove_file(readme_of(&collection)).expect("the fixture has a README");
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

fn text(root: &Path, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(root).args(args).run()
}

fn external_folder() -> Tree {
    let mut tree = Tree::new();
    tree.file("src/main.rs", "fn main() {}\n");
    tree
}

/// The `repair` names of the repairs of a real run, in order.
fn repair_names(json: &Value) -> Vec<String> {
    json["repairs"]
        .as_array()
        .expect("a repairs list")
        .iter()
        .map(|repair| repair["repair"].as_str().expect("a name").to_string())
        .collect()
}

/// A repair as an action of a dry run and as a repair of a real run is
/// written: the same object.
fn the_readme_repair(replaces: bool) -> Value {
    json!({"kind": "repair", "repair": "write_readme", "replaces": replaces})
}

// =========================================================
// Every run that takes the lock writes a missing README
// =========================================================

fn writes_a_missing_readme(args: &[&str]) {
    writes_a_missing_readme_in(old_collection(), args);
}

fn writes_a_missing_readme_in(collection: Collection, args: &[&str]) {
    let run = antiq(collection.root().as_path(), args);
    assert_eq!(
        run.code(),
        0,
        "{args:?}: stdout {} stderr {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(
        fs::read_to_string(readme_of(&collection)).unwrap_or_default(),
        README,
        "{args:?}"
    );
    assert_eq!(
        run.json()["repairs"],
        json!([the_readme_repair(false)]),
        "{args:?}"
    );
}

#[test]
fn pin_writes_a_missing_readme() {
    writes_a_missing_readme(&["pin", "projects/alpha"]);
}

#[test]
fn unpin_writes_a_missing_readme() {
    writes_a_missing_readme(&["unpin", "projects/alpha"]);
}

#[test]
fn archive_writes_a_missing_readme() {
    writes_a_missing_readme(&["archive", "projects/alpha"]);
}

#[test]
fn forget_writes_a_missing_readme() {
    writes_a_missing_readme(&["forget", "tools/old", NEW]);
}

#[test]
fn fork_writes_a_missing_readme() {
    writes_a_missing_readme(&["fork", "tools/old", "--name", "tools/copy"]);
}

#[test]
fn unearth_writes_a_missing_readme() {
    writes_a_missing_readme(&["unearth", "tools/old"]);
}

#[test]
fn import_writes_a_missing_readme() {
    let folder = external_folder();
    let root = folder.root().to_string_lossy().to_string();
    writes_a_missing_readme(&[
        "import",
        &root,
        "--as",
        "projects/imported",
        "--time",
        "2020-01-01 10:00:00",
    ]);
}

#[test]
fn rebuild_writes_a_missing_readme() {
    let collection = old_collection();
    let folder = collection.path(".antiq/pieces/tools/old");
    let listing = fs::read_dir(&folder)
        .expect("the store folder lists")
        .map(|entry| entry.expect("an entry").path())
        .find(|path| {
            path.to_string_lossy()
                .ends_with(&format!("{OLD}.sha256.zst"))
        })
        .expect("the older version has a listing");
    fs::remove_file(listing).expect("the listing is removed");
    writes_a_missing_readme_in(collection, &["rebuild", "tools/old", OLD]);
}

#[test]
fn push_writes_a_missing_readme_before_it_mirrors_the_folder() {
    let target = Target::new();
    let collection = old_collection();
    collection.use_target(&target.path());

    let run = antiq(collection.root().as_path(), &["push", "--init"]);

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert_eq!(
        fs::read_to_string(readme_of(&collection)).unwrap_or_default(),
        README
    );
    assert_eq!(
        fs::read_to_string(target.path().join(".antiq/README.md")).unwrap_or_default(),
        README,
        "the target holds the README the run wrote"
    );
}

#[test]
fn a_run_names_the_repair_on_stderr_in_text_mode() {
    let collection = old_collection();

    let run = text(collection.root().as_path(), &["pin", "projects/alpha"]);

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert!(
        run.stderr.contains(&format!("repaired: {WRITE}")),
        "{}",
        run.stderr
    );
}

#[test]
fn a_hand_edited_readme_is_replaced() {
    let collection = collection();
    fs::write(readme_of(&collection), "my notes\n").expect("the README is edited");

    let run = antiq(collection.root().as_path(), &["pin", "projects/alpha"]);

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert_eq!(
        fs::read_to_string(readme_of(&collection)).unwrap_or_default(),
        README
    );
    assert_eq!(run.json()["repairs"], json!([the_readme_repair(true)]));
}

/// Regression guard: it passes before the repair exists and fails when a
/// run writes the README although it is current.
#[test]
fn a_current_readme_is_not_written_again() {
    let collection = collection();
    let before = fs::metadata(readme_of(&collection)).expect("the README exists");

    let run = antiq(collection.root().as_path(), &["pin", "projects/alpha"]);

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    let after = fs::metadata(readme_of(&collection)).expect("the README exists");
    assert_eq!(after.ino(), before.ino(), "the README was replaced");
    assert_eq!(after.mtime_nsec(), before.mtime_nsec());
    assert_eq!(run.json()["repairs"], json!([]));
}

// =========================================================
// Dry runs list the write and change nothing
// =========================================================

#[test]
fn a_dry_run_lists_a_missing_readme_as_a_json_action_and_changes_nothing() {
    let collection = old_collection();
    let before = snapshot(collection.root().as_path());

    let run = antiq(
        collection.root().as_path(),
        &["pin", "projects/alpha", "-n"],
    );

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    let actions = run.json()["actions"].clone();
    assert_eq!(actions[0], the_readme_repair(false));
    assert_eq!(snapshot(collection.root().as_path()), before);
}

#[test]
fn a_dry_run_lists_a_missing_readme_as_a_line_and_changes_nothing() {
    let collection = old_collection();
    let before = snapshot(collection.root().as_path());

    let run = text(
        collection.root().as_path(),
        &["pin", "projects/alpha", "-n"],
    );

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert!(
        run.stdout
            .lines()
            .any(|line| line == format!("Would {WRITE}")),
        "{}",
        run.stdout
    );
    assert_eq!(snapshot(collection.root().as_path()), before);
}

#[test]
fn a_dry_run_names_a_differing_readme_as_a_replacement() {
    let collection = collection();
    fs::write(readme_of(&collection), "my notes\n").expect("the README is edited");
    let before = snapshot(collection.root().as_path());

    let json = antiq(
        collection.root().as_path(),
        &["pin", "projects/alpha", "-n"],
    );
    let line = text(
        collection.root().as_path(),
        &["pin", "projects/alpha", "-n"],
    );

    assert_eq!(json.json()["actions"][0], the_readme_repair(true));
    assert!(
        line.stdout
            .lines()
            .any(|line| line == format!("Would {REPLACE}")),
        "{}",
        line.stdout
    );
    assert_eq!(snapshot(collection.root().as_path()), before);
}

#[test]
fn a_dry_run_while_another_run_holds_the_lock_lists_no_readme() {
    let collection = old_collection();
    let _lock = hold_lock(collection.root().as_path());

    let run = antiq(
        collection.root().as_path(),
        &["pin", "projects/alpha", "-n"],
    );

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert!(
        run.json()["actions"]
            .as_array()
            .expect("a list")
            .iter()
            .all(|action| action["repair"] != "write_readme"),
        "{}",
        run.stdout
    );
    assert!(!readme_of(&collection).exists());
}

// =========================================================
// A README antiq cannot write is a warning
// =========================================================

#[test]
fn a_readme_that_is_a_symlink_is_a_warning_and_stays() {
    let collection = old_collection();
    std::os::unix::fs::symlink("elsewhere", readme_of(&collection)).expect("links");

    let run = antiq(collection.root().as_path(), &["pin", "projects/alpha"]);

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert_eq!(
        run.json()["warnings"],
        json!([{"message": "cannot write .antiq/README.md: it is not a file, so it stays as it is"}])
    );
    assert_eq!(
        fs::read_link(readme_of(&collection)).expect("still a link"),
        PathBuf::from("elsewhere")
    );
    let dry = antiq(
        collection.root().as_path(),
        &["pin", "projects/alpha", "-n"],
    );
    assert!(
        dry.json()["actions"]
            .as_array()
            .expect("a list")
            .iter()
            .all(|action| action["repair"] != "write_readme"),
        "{}",
        dry.stdout
    );
}

/// Makes `path` writable again when dropped, also after a panic, so the
/// temporary directory can always be removed.
struct Restore(PathBuf);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_readme_that_cannot_be_written_is_a_warning_and_the_run_succeeds() {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*"]);
    builder.piece("projects/alpha", |piece| {
        piece.age_days(1).file("data", "x");
    });
    let collection = builder.finish();
    fs::remove_file(readme_of(&collection)).expect("the fixture has a README");
    let tmp = collection.path(".antiq/tmp");
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).expect("read-only");
    let _restore = Restore(tmp.clone());
    assert!(
        fs::write(tmp.join("probe"), "").is_err(),
        "the test needs a user that a read-only directory stops"
    );

    // Nothing is due, so the run itself writes nothing into `tmp/`.
    let run = antiq(collection.root().as_path(), &["archive"]);

    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    let warnings = run.json()["warnings"].to_string();
    assert!(
        warnings.contains("cannot write the README .antiq/README.md"),
        "{warnings}"
    );
    assert_eq!(repair_names(&run.json()), Vec::<String>::new());
    assert!(!readme_of(&collection).exists());
}

// =========================================================
// What does not take the lock never touches the README
// =========================================================

/// Regression guard: it passes before the repair exists. `status`, `find`
/// and `grep` take no lock, so they leave a missing README missing and a
/// differing one as it is, and no output mentions it as damage.
#[test]
fn status_find_and_grep_neither_write_nor_mention_it() {
    for edited in [false, true] {
        let collection = old_collection();
        if edited {
            fs::write(readme_of(&collection), "my notes\n").expect("the README is edited");
        }
        let before = snapshot(collection.root().as_path());

        for args in [&["status"][..], &["find"], &["grep", "run"]] {
            let run = antiq(collection.root().as_path(), args);
            assert_eq!(run.code(), 0, "{args:?}: {} {}", run.stdout, run.stderr);
            assert!(
                !run.stdout.contains("README.md") && !run.stderr.contains("README.md"),
                "{args:?}: {} {}",
                run.stdout,
                run.stderr
            );
        }

        assert_eq!(snapshot(collection.root().as_path()), before);
    }
}

// =========================================================
// push mirrors it like config.toml
// =========================================================

#[test]
fn a_copy_back_from_the_target_holds_the_readme_and_needs_no_write() {
    let target = Target::new();
    let collection = old_collection();
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
    assert_eq!(
        fs::read_to_string(copy.root().join(".antiq/README.md")).unwrap_or_default(),
        README
    );

    let run = antiq(copy.root(), &["unearth", "tools/old"]);
    assert_eq!(run.code(), 0, "stdout {} stderr {}", run.stdout, run.stderr);
    assert!(
        !repair_names(&run.json()).contains(&"write_readme".to_string()),
        "{}",
        run.stdout
    );
}
