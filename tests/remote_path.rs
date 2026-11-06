// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq remote` with a path remote, seen from outside the binary. The
//! binary runs detached from any terminal.

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::tree::{TempCollection, Tree};

const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";
const OTHER_OBJECT: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

/// `antiq -C <collection> remote <args>`.
fn remote_in(collection: &TempCollection, args: &[&str]) -> Run {
    Antiq::new()
        .arg("-C")
        .arg(collection.root())
        .arg("remote")
        .args(args)
        .run()
}

fn config_of(collection: &TempCollection) -> String {
    fs::read_to_string(collection.root().join(".antiq/config.toml")).expect("the config is read")
}

fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .expect("the directory is listed")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
        .collect();
    found.sort();
    found
}

/// A directory that the remotes of a test are made in.
fn remotes() -> Tree {
    Tree::new()
}

/// A remote root with the marker of `REMOTE_ID` and two objects.
fn existing_remote(tree: &mut Tree) -> PathBuf {
    let root = tree.dir("antiq");
    fs::write(
        root.join("antiq-remote.json"),
        format!("{{\"format\":1,\"id\":\"{REMOTE_ID}\",\"layout\":1}}\n"),
    )
    .expect("the marker is written");
    for (object, number, bytes) in [
        (REMOTE_ID, 1, "one"),
        (REMOTE_ID, 2, "two"),
        (OTHER_OBJECT, 1, "other"),
    ] {
        let dir = root.join("objects").join(object);
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(dir.join(format!("meta.{number}")), bytes).expect("write");
    }
    root
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}

// =========================================================
// remote add --init
// =========================================================

#[test]
fn init_creates_the_remote_and_attaches_it() {
    let collection = TempCollection::minimal();
    let tree = remotes();
    let root = tree.path("antiq");
    let run = remote_in(&collection, &["add", "--init", "--json", path_text(&root)]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["command"], "remote add");
    assert_eq!(json["kind"], "path");
    assert_eq!(json["url"], path_text(&root));
    assert_eq!(json["attached"], true);
    assert_eq!(json["created"], true);
    let marker: Value = serde_json::from_slice(
        &fs::read(root.join("antiq-remote.json")).expect("the marker exists"),
    )
    .expect("the marker is JSON");
    assert_eq!(marker["id"], json["id"]);
    assert_eq!(
        (marker["format"].clone(), marker["layout"].clone()),
        (1.into(), 1.into())
    );
    let config = config_of(&collection);
    assert!(config.contains("[[remote]]"), "{config}");
    assert!(
        config.contains(&format!("url = \"{}\"", path_text(&root))),
        "{config}"
    );
    assert!(config.contains(&format!("id = \"{}\"", json["id"].as_str().expect("an id"))));
}

#[test]
fn init_prints_what_it_made_and_warns_about_a_root_that_is_no_mount_point() {
    let collection = TempCollection::minimal();
    let tree = remotes();
    let root = tree.path("antiq");
    let run = remote_in(&collection, &["add", "--init", path_text(&root)]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let id = {
        let marker: Value = serde_json::from_slice(
            &fs::read(root.join("antiq-remote.json")).expect("the marker exists"),
        )
        .expect("JSON");
        marker["id"].as_str().expect("an id").to_string()
    };
    let shown = run
        .stdout
        .replace(path_text(&root), "<remote>")
        .replace(&id, "<id>");
    insta::assert_snapshot!(shown, @r"
    Created the path remote at <remote>
      id: <id>
    ");
    let notes = run.stderr.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(notes, @"note: <remote> is not a mount point
    ");
}

#[test]
fn init_refuses_a_root_that_holds_an_objects_directory() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = tree.dir("antiq");
    fs::create_dir(root.join("objects")).expect("dir");
    let before = config_of(&collection);
    let run = remote_in(&collection, &["add", "--init", "--json", path_text(&root)]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "other");
    assert!(
        run.error_message().contains("objects"),
        "{}",
        run.error_message()
    );
    assert_eq!(config_of(&collection), before);
    assert_eq!(names(&root), ["objects"]);
}

#[test]
fn init_in_a_directory_that_cannot_be_written_refuses_and_writes_nothing() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = tree.dir("antiq");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).expect("chmod");
    let denied = fs::write(root.join("probe"), "").is_err();
    let before = config_of(&collection);
    let run = remote_in(&collection, &["add", "--init", "--json", path_text(&root)]);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("restore");
    assert!(
        denied,
        "the test needs a user for whom mode 555 denies writing; run it as a non-root user"
    );
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert!(
        run.error_message().contains("exclusive creation"),
        "{}",
        run.error_message()
    );
    assert_eq!(config_of(&collection), before);
    assert_eq!(names(&root), Vec::<String>::new());
}

// =========================================================
// remote add
// =========================================================

#[test]
fn attaching_copies_the_metadata_and_status_then_shows_the_remote() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    let run = remote_in(&collection, &["add", "--json", path_text(&root)]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["id"], REMOTE_ID);
    assert_eq!(json["created"], false);
    assert_eq!(json["pulled"], 2);

    let objects = collection.root().join(".antiq/objects");
    assert_eq!(
        fs::read(objects.join(REMOTE_ID).join("meta.2")).expect("copied"),
        b"two"
    );
    assert_eq!(
        fs::read(objects.join(OTHER_OBJECT).join("meta.1")).expect("copied"),
        b"other"
    );

    let status = Antiq::new()
        .arg("-C")
        .arg(collection.root())
        .args(["status", "--json"])
        .run();
    let json = status.json();
    assert_eq!(json["remote"]["state"], "reachable", "{}", status.stdout);
    assert_eq!(json["remote"]["url"], path_text(&root));
}

#[test]
fn attaching_prints_what_it_copied() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    let run = remote_in(&collection, &["add", path_text(&root)]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let shown = run.stdout.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(shown, @r"
    Attached the path remote at <remote>
      id: 01BX5ZZKBKACTAV9WEVGEMMVRY
      metadata copied for 2 objects
    ");
}

#[test]
fn a_directory_without_a_marker_is_away_and_nothing_is_written() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = tree.dir("antiq");
    let before = config_of(&collection);
    let run = remote_in(&collection, &["add", "--json", path_text(&root)]);
    assert_eq!(run.code(), 4);
    assert_eq!(run.error_kind(), "remote_away");
    assert_eq!(config_of(&collection), before);
    assert_eq!(names(&root), Vec::<String>::new());
}

#[test]
fn a_missing_root_is_away() {
    let collection = TempCollection::minimal();
    let tree = remotes();
    let run = remote_in(
        &collection,
        &["add", "--json", path_text(&tree.path("absent"))],
    );
    assert_eq!(run.code(), 4);
    assert_eq!(run.error_kind(), "remote_away");
}

#[test]
fn a_marker_that_cannot_be_read_is_broken() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = tree.dir("antiq");
    fs::write(root.join("antiq-remote.json"), "not json").expect("write");
    let run = remote_in(&collection, &["add", "--json", path_text(&root)]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "remote_broken");
}

#[test]
fn a_second_remote_is_refused_and_the_config_stays_as_it_was() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    let first = remote_in(&collection, &["add", path_text(&root)]);
    assert_eq!(first.code(), 0, "stderr: {}", first.stderr);
    let before = config_of(&collection);
    let second = remote_in(&collection, &["add", "--json", path_text(&root)]);
    assert_eq!(second.code(), 1);
    assert!(
        second.error_message().contains("already"),
        "{}",
        second.error_message()
    );
    assert_eq!(config_of(&collection), before);
}

#[test]
fn a_host_key_alias_for_a_path_remote_is_a_usage_error() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    let run = remote_in(
        &collection,
        &["add", "--json", "--host-key-alias", "nas", path_text(&root)],
    );
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
}

#[test]
fn a_url_that_does_not_fit_is_a_usage_error() {
    let collection = TempCollection::minimal();
    let run = remote_in(&collection, &["add", "--json", "relative/path"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
}

// =========================================================
// --dry-run
// =========================================================

#[test]
fn a_dry_run_of_init_prints_the_plan_and_makes_nothing() {
    let collection = TempCollection::minimal();
    let tree = remotes();
    let root = tree.path("antiq");
    let before = config_of(&collection);
    let run = remote_in(
        &collection,
        &["add", "--init", "--dry-run", path_text(&root)],
    );
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let shown = run.stdout.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(shown, @"Would create the remote at <remote>
    ");
    assert!(!root.exists());
    assert_eq!(config_of(&collection), before);
    assert!(!collection.root().join(".antiq/lock").exists());
}

#[test]
fn a_dry_run_of_an_attach_plans_the_copy_and_takes_no_lock() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    let run = remote_in(&collection, &["add", "--dry-run", path_text(&root)]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let shown = run.stdout.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(shown, @r"
    Would attach the remote at <remote>
    Would copy the metadata of 2 objects
    ");
    assert!(!collection.root().join(".antiq/lock").exists());
    assert!(!config_of(&collection).contains("[[remote]]"));
}

#[test]
fn a_dry_run_says_when_another_run_holds_the_lock() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    // This test process is a live holder.
    fs::write(
        collection.root().join(".antiq/lock"),
        format!(
            "{{\"pid\":{},\"machine\":\"desk\",\"started\":\"2026-11-04T10:00:00Z\"}}",
            std::process::id()
        ),
    )
    .expect("the lock file is written");
    let run = remote_in(&collection, &["add", "--dry-run", path_text(&root)]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let expected = format!(
        "note: another run holds the lock (pid {}, machine desk, started 2026-11-04T10:00:00Z)\n",
        std::process::id()
    );
    assert!(run.stderr.contains(&expected), "{}", run.stderr);
}

#[test]
fn a_dry_run_in_json_says_so_before_the_actions() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    let run = remote_in(
        &collection,
        &["add", "--dry-run", "--json", path_text(&root)],
    );
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let dry = run
        .stdout
        .find("\"dry_run\":true")
        .expect("the key is there");
    let actions = run.stdout.find("\"actions\"").expect("the key is there");
    assert!(dry < actions, "{}", run.stdout);
    let json = run.json();
    assert_eq!(json["attached"], false);
    assert_eq!(json["actions"][0]["action"], "attach_remote");
    assert_eq!(json["actions"][1]["objects"], 2);
    assert!(json.get("pulled").is_none());
}

// =========================================================
// remote show
// =========================================================

/// Attaches the remote at `root` to `collection`.
fn attach(collection: &TempCollection, root: &Path) {
    let run = remote_in(collection, &["add", path_text(root)]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn without_a_remote_show_says_so_and_exits_0() {
    let collection = TempCollection::minimal();
    let run = remote_in(&collection, &["show"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "No remote is attached.\n");
    let json = remote_in(&collection, &["show", "--json"]).json();
    assert_eq!(json["command"], "remote show");
    assert_eq!(json["attached"], false);
    assert!(json.get("state").is_none());
}

#[test]
fn a_reachable_remote_is_shown_with_its_kind_url_id_and_state() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    attach(&collection, &root);
    let run = remote_in(&collection, &["show"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let shown = run.stdout.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(shown, @r"
    remote: path <remote>
      id: 01BX5ZZKBKACTAV9WEVGEMMVRY
      state: reachable
    ");
    assert!(
        run.stderr.contains("is not a mount point"),
        "{}",
        run.stderr
    );

    let json = remote_in(&collection, &["show", "--json"]).json();
    assert_eq!(json["attached"], true);
    assert_eq!(json["kind"], "path");
    assert_eq!(json["url"], path_text(&root));
    assert_eq!(json["id"], REMOTE_ID);
    assert_eq!(json["state"], "reachable");
    assert_eq!(json["probed"], serde_json::json!({}));
    assert!(json.get("reason").is_none());
}

#[test]
fn a_remote_whose_root_is_gone_is_shown_as_away() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    attach(&collection, &root);
    fs::remove_dir_all(&root).expect("the share goes");
    let run = remote_in(&collection, &["show", "--json"]);
    assert_eq!(run.code(), 0);
    let json = run.json();
    assert_eq!(json["state"], "away");
    assert!(
        json["reason"]
            .as_str()
            .expect("a reason")
            .contains("does not exist")
    );
}

#[test]
fn a_remote_with_another_marker_is_shown_as_broken() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    attach(&collection, &root);
    fs::write(
        root.join("antiq-remote.json"),
        "{\"format\":1,\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"layout\":1}\n",
    )
    .expect("write");
    let run = remote_in(&collection, &["show"]);
    assert_eq!(run.code(), 0);
    assert!(run.stdout.contains("state: broken ("), "{}", run.stdout);
}

#[test]
fn showing_takes_no_lock_and_writes_nothing() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    attach(&collection, &root);
    let before = (names(&root), config_of(&collection));
    let lock = collection.root().join(".antiq/lock");
    fs::remove_file(&lock).expect("the lock file `add` made is removed");
    remote_in(&collection, &["show"]);
    assert_eq!((names(&root), config_of(&collection)), before);
    assert!(!lock.exists());
}

// =========================================================
// remote remove
// =========================================================

/// Valid metadata for an archive that only the remote holds.
fn remote_only_metadata(at: &str) -> Vec<u8> {
    use antiq::meta::{
        Checksum, CreatedSource, FileName, Lifecycle, Metadata, Payload, PayloadKind,
    };
    use antiq::relpath::{Name, RelPath};
    let path = RelPath::parse(at).expect("a valid path");
    let name =
        Name::from_bytes(path.components().last().expect("a name").to_vec()).expect("a name");
    let known = Metadata {
        format: 1,
        id: OTHER_OBJECT.parse().expect("an ID"),
        collection: support::tree::COLLECTION_ID.parse().expect("an ID"),
        path,
        name,
        created: "2026-01-01T00:00:00Z".parse().expect("a time"),
        created_source: CreatedSource::Birth,
        lifecycle: Lifecycle::Archived,
        payloads: vec![Payload {
            kind: PayloadKind::Archive,
            version: 1,
            file: FileName::new("0123456789abcdef-old.tar.zst").expect("a name"),
            package_format: "tar+zstd/1".into(),
            size: 1,
            checksum: Checksum::sha256("00"),
            listing: None,
            machine: "laptop".into(),
            created: "2026-01-01T00:00:00Z".parse().expect("a time"),
            verified: None,
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        }],
        history: Vec::new(),
        extra: serde_json::Map::new(),
    };
    antiq::meta::to_bytes(&known).expect("metadata serialises")
}

#[test]
fn without_a_remote_removing_fails_with_no_remote() {
    let collection = TempCollection::minimal();
    let run = remote_in(&collection, &["remove", "--json"]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "no_remote");
}

#[test]
fn removing_detaches_the_remote_and_keeps_the_local_metadata() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    attach(&collection, &root);
    let run = remote_in(&collection, &["remove"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let shown = run.stdout.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(shown, @"Detached the remote at <remote>
    ");
    assert!(!config_of(&collection).contains("[[remote]]"));
    let kept = collection
        .root()
        .join(".antiq/objects")
        .join(REMOTE_ID)
        .join("meta.2");
    assert_eq!(fs::read(kept).expect("kept"), b"two");
    assert!(
        root.join("antiq-remote.json").is_file(),
        "the remote is left alone"
    );
    let show = remote_in(&collection, &["show"]);
    assert_eq!(show.stdout, "No remote is attached.\n");
}

#[test]
fn removing_lists_the_pieces_that_only_the_remote_holds() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    fs::write(
        root.join("objects").join(OTHER_OBJECT).join("meta.1"),
        remote_only_metadata("projects/old"),
    )
    .expect("write");
    attach(&collection, &root);
    let run = remote_in(&collection, &["remove", "--force"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let shown = run.stdout.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(shown, @r"
    Detached the remote at <remote>
    These remote-only pieces can no longer be reached:
      projects/old (01D39ZY06FGSCTVN4T2V9PKHFZ)
    ");
}

#[test]
fn a_dry_run_of_removing_prints_the_plan_and_changes_nothing() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    fs::write(
        root.join("objects").join(OTHER_OBJECT).join("meta.1"),
        remote_only_metadata("projects/old"),
    )
    .expect("write");
    attach(&collection, &root);
    let before = config_of(&collection);
    let run = remote_in(&collection, &["remove", "--dry-run"]);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let shown = run.stdout.replace(path_text(&root), "<remote>");
    insta::assert_snapshot!(shown, @r"
    Would detach the remote at <remote>
    The brake would hold at 1 of 2:
      projects/old (01D39ZY06FGSCTVN4T2V9PKHFZ)
    These remote-only pieces would no longer be reachable:
      projects/old (01D39ZY06FGSCTVN4T2V9PKHFZ)
    ");
    assert_eq!(config_of(&collection), before);

    let json = remote_in(&collection, &["remove", "--dry-run", "--json"]);
    let dry = json
        .stdout
        .find("\"dry_run\":true")
        .expect("the key is there");
    let actions = json.stdout.find("\"actions\"").expect("the key is there");
    assert!(dry < actions, "{}", json.stdout);
    assert_eq!(json.json()["remote_only"][0]["path"], "projects/old");
    assert_eq!(json.json()["detached"], false);
    assert_eq!(json.json()["brake"]["planned"], 1);
    assert_eq!(json.json()["brake"]["set_size"], 2);
}

#[test]
fn a_remote_that_is_gone_can_still_be_removed() {
    let collection = TempCollection::minimal();
    let mut tree = remotes();
    let root = existing_remote(&mut tree);
    attach(&collection, &root);
    fs::remove_dir_all(&root).expect("the share goes");
    let run = remote_in(&collection, &["remove", "--json"]);
    assert_eq!(run.code(), 0, "stdout: {}", run.stdout);
    assert_eq!(run.json()["detached"], true);
    assert!(!config_of(&collection).contains("[[remote]]"));
}
