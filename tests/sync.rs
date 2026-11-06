// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq sync` seen from outside the binary, against a path remote.

mod support;

use antiq::meta::Lifecycle;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::objects::{archive_with_members, hold_lock, id, metadata, put_meta};
use support::tree::{COLLECTION_ID, TempCollection, Tree};

const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";
const OTHER_COLLECTION_ID: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

/// `antiq -C <root> <args>`.
fn antiq_in(temp: &TempCollection, args: &[&str]) -> Run {
    Antiq::new().arg("-C").arg(temp.root()).args(args).run()
}

/// A remote root with the marker of `REMOTE_ID`.
fn remote_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("a temp dir");
    fs::write(
        root.path().join("antiq-remote.json"),
        format!("{{\"format\":1,\"id\":\"{REMOTE_ID}\",\"layout\":1}}\n"),
    )
    .expect("the marker is written");
    root
}

/// A collection of the machine `name` attached to `remote`.
fn attached(remote: &Path, name: &str) -> TempCollection {
    attached_as(remote, name, OTHER_COLLECTION_ID)
}

fn attached_as(remote: &Path, name: &str, collection: &str) -> TempCollection {
    let mut temp = TempCollection::minimal();
    temp.tree().file(
        ".antiq/config.toml",
        format!(
            "[collection]\nid = \"{collection}\"\n\n[machine]\nname = \"{name}\"\n\n\
             [[remote]]\nurl = \"{}\"\nid = \"{REMOTE_ID}\"\n",
            remote.display()
        ),
    );
    temp
}

/// Imports a zstd tar of a folder holding one file into `temp`.
fn import_piece(temp: &mut TempCollection, at: &str) {
    let mut tree = Tree::new();
    tree.file_with("proj/a.txt", "hello", 0o644, 1_000_000_000);
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_dir_all("proj", tree.path("proj"))
        .expect("the folder is added");
    let tar = builder.into_inner().expect("the tar ends");
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("an encoder");
    encoder.include_checksum(true).expect("on");
    encoder.write_all(&tar).expect("writes");
    let package = temp
        .tree()
        .file("input.tar.zst", encoder.finish().expect("the frame ends"));
    let run = Antiq::new()
        .arg("-C")
        .arg(temp.root())
        .args(["import", "--path", at])
        .arg(&package)
        .run();
    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
}

/// Puts an archived object with one member on the remote, as another
/// machine's commit would have left it.
fn publish_archive(remote: &Path, number: u8, at: &str) -> PathBuf {
    let origin = TempCollection::minimal();
    let archive = archive_with_members(origin.root(), number, 1, 10, &["src/main.rs"]);
    put_meta(
        origin.root(),
        &metadata(
            number,
            at,
            Lifecycle::Archived,
            COLLECTION_ID,
            vec![archive],
        ),
    );
    let from = origin
        .root()
        .join(".antiq/objects")
        .join(id(number).to_string());
    let to = remote.join("objects").join(id(number).to_string());
    fs::create_dir_all(&to).expect("the object directory");
    for entry in fs::read_dir(&from).expect("lists") {
        let entry = entry.expect("an entry");
        fs::copy(entry.path(), to.join(entry.file_name())).expect("copied");
    }
    to
}

#[test]
fn sync_pulls_a_version_and_fetches_its_listing() {
    let remote = remote_root();
    publish_archive(remote.path(), 1, "alpha");
    let ben = attached(remote.path(), "ben");

    let run = antiq_in(&ben, &["sync"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    insta::assert_snapshot!(run.stdout, @r"
    Pulled version 1 of 01EJ3PX0010000000000000001
    Fetched the listing of 01EJ3PX0010000000000000001
    ");
    let found = antiq_in(&ben, &["find", "main.rs"]);
    assert_eq!(found.code(), 0, "{}{}", found.stdout, found.stderr);
    // The query matches a member of the archive, not the piece's name.
    assert!(found.stdout.contains("alpha"), "{}", found.stdout);
}

#[test]
fn a_second_sync_has_nothing_to_do() {
    let remote = remote_root();
    publish_archive(remote.path(), 1, "alpha");
    let ben = attached(remote.path(), "ben");
    assert_eq!(antiq_in(&ben, &["sync"]).code(), 0);

    let run = antiq_in(&ben, &["sync"]);

    assert_eq!(run.code(), 0);
    insta::assert_snapshot!(run.stdout, @"Nothing to sync.");
}

#[test]
fn the_json_of_a_sync_names_what_was_pulled() {
    let remote = remote_root();
    publish_archive(remote.path(), 1, "alpha");
    let ben = attached(remote.path(), "ben");

    let json = antiq_in(&ben, &["--json", "sync"]).json();

    assert_eq!(json["command"], "sync");
    assert_eq!(json["pulled"][0]["version"], 1);
    assert_eq!(json["listings"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["skipped"], serde_json::json!([]));
}

#[test]
fn a_dry_run_lists_the_pull_and_changes_nothing() {
    let remote = remote_root();
    publish_archive(remote.path(), 1, "alpha");
    let ben = attached(remote.path(), "ben");

    let run = antiq_in(&ben, &["sync", "--dry-run"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout.starts_with("Would pull version 1 of "),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("Would fetch "), "{}", run.stdout);
    assert!(!ben.root().join(".antiq/remote-state.json").exists());
    let objects = fs::read_dir(ben.root().join(".antiq/objects")).expect("lists");
    assert_eq!(objects.count(), 0);
}

#[test]
fn a_collection_without_a_remote_cannot_sync() {
    let temp = TempCollection::minimal();

    let run = antiq_in(&temp, &["--json", "sync"]);

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "no_remote");
}

#[test]
fn a_listing_that_does_not_parse_exits_3_and_names_the_piece() {
    let remote = remote_root();
    let object = publish_archive(remote.path(), 1, "alpha");
    for entry in fs::read_dir(&object).expect("lists") {
        let path = entry.expect("an entry").path();
        if path.to_string_lossy().ends_with(".listing.zst") {
            fs::write(path, b"not a listing").expect("overwritten");
        }
    }
    let ben = attached(remote.path(), "ben");

    let run = antiq_in(&ben, &["sync"]);

    assert_eq!(run.code(), 3, "{}{}", run.stdout, run.stderr);
    assert_eq!(
        run.stdout,
        "Pulled version 1 of 01EJ3PX0010000000000000001\n"
    );
    assert!(run.stderr.starts_with("note: alpha: "), "{}", run.stderr);
}

#[test]
fn a_dry_run_in_json_has_the_plan() {
    let remote = remote_root();
    publish_archive(remote.path(), 1, "alpha");
    let ben = attached(remote.path(), "ben");

    let json = antiq_in(&ben, &["--json", "sync", "--dry-run"]).json();

    assert_eq!(json["command"], "sync");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["actions"][0]["action"], "pull_metadata");
    assert_eq!(json["actions"][1]["action"], "fetch_listing");
}

#[test]
fn a_dry_run_says_who_holds_the_lock() {
    let remote = remote_root();
    let ben = attached(remote.path(), "ben");
    let _lock = hold_lock(ben.root());

    let run = antiq_in(&ben, &["sync", "--dry-run"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.stdout, "Nothing to do.\n");
    assert!(run.stderr.starts_with("note: "), "{}", run.stderr);
}

#[test]
fn sync_uploads_commits_and_another_machine_pulls_it() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    import_piece(&mut anna, "notes");

    let run = antiq_in(&anna, &["sync"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", run.stdout);
    assert!(lines[0].starts_with("Uploaded "), "{}", run.stdout);
    assert!(lines[1].starts_with("Uploaded "), "{}", run.stdout);
    assert!(
        lines[2].starts_with("Committed version 1 of "),
        "{}",
        run.stdout
    );

    let ben = attached(remote.path(), "ben");
    let pulled = antiq_in(&ben, &["sync"]);
    assert_eq!(pulled.code(), 0, "{}{}", pulled.stdout, pulled.stderr);
    let found = antiq_in(&ben, &["find", "a.txt"]);
    assert!(found.stdout.contains("notes"), "{}", found.stdout);
}

#[test]
fn the_json_of_a_sync_lists_uploads_and_commits() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    import_piece(&mut anna, "notes");

    let json = antiq_in(&anna, &["--json", "sync"]).json();

    assert_eq!(json["uploaded"].as_array().map(Vec::len), Some(2));
    assert_eq!(json["committed"][0]["version"], 1);
    assert_eq!(json["cleanup"]["removed"], 0);
    assert_eq!(json["skipped"], serde_json::json!([]));
}

// =========================================================
// New pieces
// =========================================================

#[test]
fn a_new_folder_gets_its_id_and_its_metadata_is_committed() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    anna.tree().file("proj/a.txt", "hello");

    let run = antiq_in(&anna, &["sync"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", run.stdout);
    assert!(lines[0].starts_with("Gave proj the ID "), "{}", run.stdout);
    assert!(
        lines[1].starts_with("Uploaded ") && lines[1].contains("-proj.tar.zst of "),
        "{}",
        run.stdout
    );
    assert!(
        lines[2].starts_with("Committed version 1 of "),
        "{}",
        run.stdout
    );
}

#[test]
fn the_json_of_a_sync_names_the_ids_it_minted() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    anna.tree().file("proj/a.txt", "hello");

    let json = antiq_in(&anna, &["--json", "sync"]).json();

    assert_eq!(json["minted"][0]["path"], "proj");
    assert_eq!(json["minted"][0]["id"], json["committed"][0]["id"]);
    assert_eq!(json["unbacked"], serde_json::json!([]));
}

#[test]
fn a_dry_run_says_which_folders_would_get_an_id() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    anna.tree().file("proj/a.txt", "hello");

    let run = antiq_in(&anna, &["sync", "--dry-run"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("Would give proj an ID"),
        "{}",
        run.stdout
    );
    let objects = fs::read_dir(anna.root().join(".antiq/objects")).expect("lists");
    assert_eq!(objects.count(), 0);
}

#[test]
fn a_folder_that_cannot_be_read_gets_no_id_and_the_run_exits_3() {
    use std::os::unix::fs::PermissionsExt;
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    anna.tree().file("proj/a.txt", "hello");
    let locked = anna.tree().dir("proj/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("locked");
    assert!(
        fs::read_dir(&locked).is_err(),
        "the precondition: the directory cannot be read"
    );

    let run = antiq_in(&anna, &["sync"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlocked");

    assert_eq!(run.code(), 3, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stderr.contains("proj: `proj/locked` cannot be read"),
        "{}",
        run.stderr
    );
    let objects = fs::read_dir(anna.root().join(".antiq/objects")).expect("lists");
    assert_eq!(objects.count(), 0);
}

// =========================================================
// Live packages
// =========================================================

fn days_ago(days: i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64;
    now - days * 86_400
}

/// The names in the object directories of the remote, sorted.
fn remote_files(remote: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for object in fs::read_dir(remote.join("objects")).expect("lists") {
        for file in fs::read_dir(object.expect("an object").path()).expect("lists") {
            names.push(
                file.expect("a file")
                    .file_name()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    names.sort();
    names
}

#[test]
fn the_json_of_a_sync_names_the_live_package_it_deletes_after_the_archive_is_committed() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    anna.tree()
        .file_with("proj/a.txt", "hello", 0o644, days_ago(40));
    let first = antiq_in(&anna, &["sync"]);
    assert_eq!(first.code(), 0, "{}{}", first.stdout, first.stderr);
    let before = remote_files(remote.path());
    let package = before
        .iter()
        .find(|name| name.ends_with("-proj.tar.zst"))
        .expect("the live package is on the remote")
        .clone();
    let archived = antiq_in(&anna, &["archive"]);
    assert_eq!(archived.code(), 0, "{}{}", archived.stdout, archived.stderr);

    let json = antiq_in(&anna, &["--json", "sync"]).json();

    assert_eq!(
        json["removed_live"].as_array().map(Vec::len),
        Some(1),
        "{json}"
    );
    assert_eq!(json["removed_live"][0]["file"], package.as_str());
    let after = remote_files(remote.path());
    assert!(!after.contains(&package), "{after:?}");
    assert!(
        after.iter().any(|name| name.ends_with("-proj.tar.zst")),
        "the archive is on the remote: {after:?}"
    );
}

// =========================================================
// An away remote
// =========================================================

/// A collection attached to a remote whose root is gone, with `gap_days`
/// since the remote was last reachable and a change waiting to be
/// uploaded.
fn away_with_pending_work(gap_days: i64) -> TempCollection {
    let missing = tempfile::tempdir().expect("a temp dir");
    let root = missing.path().join("gone");
    let mut collection = attached_as(&root, "anna", COLLECTION_ID);
    import_piece(&mut collection, "notes");
    let last = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * gap_days);
    fs::write(
        collection.root().join(".antiq/remote-state.json"),
        format!("{{\"last_reachable\": \"{last}\"}}"),
    )
    .expect("the state is written");
    collection
}

#[test]
fn an_away_remote_after_a_day_is_not_a_failure_under_if_reachable() {
    let collection = away_with_pending_work(1);

    for args in [
        ["sync", "--if-reachable"].as_slice(),
        ["sync", "--if-reachable", "--dry-run"].as_slice(),
    ] {
        let run = antiq_in(&collection, args);
        assert_eq!(run.code(), 0, "{args:?}: {}{}", run.stdout, run.stderr);
    }
}

#[test]
fn an_away_remote_after_eight_days_with_pending_work_exits_4_even_under_if_reachable() {
    let collection = away_with_pending_work(8);

    for args in [
        ["sync", "--if-reachable"].as_slice(),
        ["sync", "--if-reachable", "--dry-run"].as_slice(),
        ["sync"].as_slice(),
    ] {
        let run = antiq_in(&collection, args);
        assert_eq!(run.code(), 4, "{args:?}: {}{}", run.stdout, run.stderr);
    }
}

#[test]
fn an_away_remote_after_eight_days_without_pending_work_is_not_overdue() {
    let missing = tempfile::tempdir().expect("a temp dir");
    let collection = attached_as(&missing.path().join("gone"), "anna", COLLECTION_ID);
    let last = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * 8);
    fs::write(
        collection.root().join(".antiq/remote-state.json"),
        format!("{{\"last_reachable\": \"{last}\"}}"),
    )
    .expect("the state is written");

    let run = antiq_in(&collection, &["sync", "--if-reachable"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
}

#[test]
fn a_dry_run_leaves_no_probe_file_on_the_remote() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    import_piece(&mut anna, "notes");
    let before: Vec<String> = fs::read_dir(remote.path())
        .expect("lists")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let run = antiq_in(&anna, &["sync", "--dry-run"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    let after: Vec<String> = fs::read_dir(remote.path())
        .expect("lists")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(before, after);
    assert!(run.stdout.contains("Would upload "), "{}", run.stdout);
    assert!(
        run.stdout.contains("Would commit the change to "),
        "{}",
        run.stdout
    );
}

#[test]
fn sync_reports_the_leftovers_it_removes_from_the_remote() {
    let remote = remote_root();
    let mut anna = attached_as(remote.path(), "anna", COLLECTION_ID);
    import_piece(&mut anna, "notes");
    let json = antiq_in(&anna, &["--json", "sync"]).json();
    let id = json["committed"][0]["id"]
        .as_str()
        .expect("an id")
        .to_string();
    // The machine's own temp file in a directory it owns goes at once.
    let leftover = remote
        .path()
        .join("objects")
        .join(&id)
        .join(".0123456789abcdef-x.tar.zst.anna.part");
    fs::write(&leftover, b"partial").expect("planted");

    let run = antiq_in(&anna, &["sync"]);

    assert_eq!(run.code(), 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.stdout, "Removed 1 leftover files from the remote\n");
    assert!(!leftover.exists());
}
