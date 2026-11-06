// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq sync` seen from outside the binary, against a path remote.

mod support;

use antiq::meta::Lifecycle;
use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::objects::{archive_with_members, hold_lock, id, metadata, put_meta};
use support::tree::{COLLECTION_ID, TempCollection};

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
    let mut temp = TempCollection::minimal();
    temp.tree().file(
        ".antiq/config.toml",
        format!(
            "[collection]\nid = \"{OTHER_COLLECTION_ID}\"\n\n[machine]\nname = \"{name}\"\n\n\
             [[remote]]\nurl = \"{}\"\nid = \"{REMOTE_ID}\"\n",
            remote.display()
        ),
    );
    temp
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
