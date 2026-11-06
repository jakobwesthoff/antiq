// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq push` seen from outside the binary.

mod support;

use std::fs;
use std::path::Path;
use support::cmd::{Antiq, Run, hold_lock, snapshot};
use support::fake_rsync::FakeRsync;
use support::scenario::collection::{Clock, Collection};
use support::scenario::target::Target;

fn collection() -> Collection {
    Collection::build(&Clock::now())
        .piece("projects/a", |piece| {
            piece.age_days(3).file("a.txt", "a");
        })
        .finish()
}

/// Appends a `[target]` table with the given body lines to the config.
fn add_target_table(collection: &Collection, lines: &str) {
    let path = collection.path(".antiq/config.toml");
    let mut text = fs::read_to_string(&path).expect("the config exists");
    text.push_str(&format!("\n[target]\n{lines}\n"));
    fs::write(&path, text).expect("the config can be written");
}

fn set_url(collection: &Collection, url: &str) {
    let quoted = serde_json::to_string(url).expect("a string serialises");
    add_target_table(collection, &format!("url = {quoted}"));
}

/// Adds a key line to the `[target]` table that `set_url` wrote.
fn add_target_key(collection: &Collection, line: &str) {
    let path = collection.path(".antiq/config.toml");
    let text = fs::read_to_string(&path).expect("the config exists");
    assert!(text.contains("[target]\n"), "the target table exists");
    fs::write(
        &path,
        text.replacen("[target]\n", &format!("[target]\n{line}\n"), 1),
    )
    .expect("the config can be written");
}

/// Replaces the `url` of the `[target]` table.
fn set_target_url(collection: &Collection, url: &Path) {
    let path = collection.path(".antiq/config.toml");
    let text = fs::read_to_string(&path).expect("the config exists");
    let quoted = serde_json::to_string(url.to_str().expect("a UTF-8 path")).expect("quotes");
    let replaced: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with("url = ") {
                format!("url = {quoted}")
            } else {
                line.to_string()
            }
        })
        .collect();
    fs::write(&path, replaced.join("\n") + "\n").expect("the config can be written");
}

/// A stand-in rsync that reports `version`, set as `[target] rsync`.
fn use_fake_rsync(collection: &Collection, version: &str) -> FakeRsync {
    let fake = FakeRsync::new(version);
    let path = serde_json::to_string(fake.path().to_str().expect("a UTF-8 path"))
        .expect("a string serialises");
    add_target_key(collection, &format!("rsync = {path}"));
    fake
}

fn push(collection: &Collection, args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .arg("--json")
        .arg("push")
        .args(args)
        .run()
}

fn push_text(collection: &Collection, args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .arg("push")
        .args(args)
        .run()
}

#[test]
fn a_config_without_a_target_table_is_no_target() {
    let collection = collection();

    let run = push(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "no_target");
    let message = run.error_message();
    assert!(message.contains("[target] url"), "{message}");
    assert!(message.contains("push --init"), "{message}");
}

#[test]
fn a_target_table_without_a_url_is_no_target() {
    let collection = collection();
    add_target_table(&collection, "fsync = false");

    let run = push(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "no_target");
}

#[test]
fn no_target_is_named_on_the_console_too() {
    let collection = collection();

    let run = push_text(&collection, &[]);

    assert_eq!(run.code(), 1);
    assert!(run.stderr.contains("[target] url"), "{}", run.stderr);
    assert!(run.stderr.contains("push --init"), "{}", run.stderr);
}

#[test]
fn a_url_that_is_no_target_names_the_key() {
    for url in ["", "-host:p", "relative/path", "host:"] {
        let collection = collection();
        set_url(&collection, url);

        let run = push(&collection, &[]);

        assert_eq!(run.code(), 1, "url {url:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "other", "url {url:?}");
        let message = run.error_message();
        assert!(message.contains("[target] url"), "url {url:?}: {message}");
    }
}

#[test]
fn the_target_is_checked_before_the_lock_is_taken() {
    let collection = collection();
    let _held = hold_lock(&collection.root());

    let run = push(&collection, &[]);

    assert_eq!(run.error_kind(), "no_target");
}

#[test]
fn a_held_lock_is_exit_1_with_the_holder_also_under_if_reachable() {
    for args in [&[][..], &["--if-reachable"][..]] {
        let collection = collection();
        set_url(&collection, "/nonexistent/target");
        let _held = hold_lock(&collection.root());

        let run = push(&collection, args);

        assert_eq!(run.code(), 1, "{args:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "lock_held", "{args:?}");
        let error = &run.json()["error"];
        assert_eq!(error["pid"], std::process::id(), "{args:?}");
        assert!(error["started"].is_string(), "{args:?}");
    }
}

#[test]
fn a_dry_run_with_the_lock_held_names_the_holder_and_lists_nothing() {
    let collection = collection();
    set_url(&collection, "/nonexistent/target");
    let _held = hold_lock(&collection.root());

    let run = push(&collection, &["-n"]);

    assert_eq!(run.code(), 0, "{}", run.stdout);
    let json = run.json();
    assert_eq!(json["command"], "push");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["lock_held"]["pid"], std::process::id());
    assert_eq!(json["actions"], serde_json::json!([]));

    let text = push_text(&collection, &["-n"]);
    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert!(
        text.stdout.contains("The lock is held by pid"),
        "{}",
        text.stdout
    );
}

#[test]
fn a_dry_run_changes_nothing_and_takes_no_lock() {
    let collection = stored(&[]);
    let before = snapshot(&collection.root());

    let run = push(&collection, &["-n"]);

    // The steps behind the pre-check are not built yet.
    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
    assert_eq!(snapshot(&collection.root()), before);
}

#[test]
fn a_real_run_repairs_before_it_goes_on() {
    let collection = stored(&[]);
    let stale = collection.path(".antiq/tmp/stale-entry");
    fs::create_dir_all(stale.parent().expect("has a parent")).expect("tmp exists");
    fs::write(&stale, "left behind").expect("the entry is written");

    let run = push(&collection, &[]);

    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
    assert!(!Path::new(&stale).exists(), "repair 2 removes the entry");
}

#[test]
fn init_and_force_are_usage_errors_with_if_reachable() {
    let collection = collection();
    for flag in ["--init", "--force"] {
        let run = push(&collection, &[flag, "--if-reachable"]);

        assert_eq!(run.code(), 2, "{flag}: {}", run.stdout);
        assert_eq!(run.error_kind(), "usage", "{flag}");
    }
}

#[test]
fn every_flag_combination_parses_and_needs_a_collection() {
    for args in [
        &["push"][..],
        &["push", "--init", "--dry-run"],
        &["push", "--force"],
        &["push", "--if-reachable", "-n"],
    ] {
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let run = Antiq::new()
            .args(["--json", "-C"])
            .arg(dir.path())
            .args(args)
            .run();

        assert_eq!(run.code(), 1, "{args:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "not_a_collection", "{args:?}");
        assert_eq!(run.json()["command"], "push", "{args:?}");
    }
}

// =========================================================
// Step 2: the store
// =========================================================

const STAMP: &str = "2024-06-30T081245";
const OTHER_STAMP: &str = "2024-07-01T081245";

/// A collection with `projects/a` and one stored version of it per stamp,
/// pushing to an empty target of its own that carries its marker, with a
/// stand-in rsync of the given version.
struct Stored {
    collection: Collection,
    _target: Target,
    fake: FakeRsync,
}

impl std::ops::Deref for Stored {
    type Target = Collection;

    fn deref(&self) -> &Collection {
        &self.collection
    }
}

fn stored_with(stamps: &[&str], rsync_version: &str) -> Stored {
    let mut builder = Collection::build(&Clock::now());
    builder.piece("projects/a", |piece| {
        piece.age_days(3).file("a.txt", "a");
    });
    for stamp in stamps {
        builder.version("projects/a", stamp, &[("a.txt", "a")]);
    }
    let collection = builder.finish();
    let target = Target::new();
    target.marker(&collection);
    set_url(&collection, target.path().to_str().expect("a UTF-8 path"));
    let fake = use_fake_rsync(&collection, rsync_version);
    Stored {
        collection,
        _target: target,
        fake,
    }
}

fn stored(stamps: &[&str]) -> Stored {
    stored_with(stamps, RSYNC_3_2_4)
}

/// The message of a refused push.
fn refusal(collection: &Collection) -> String {
    let run = push(collection, &[]);
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other", "{}", run.stdout);
    run.error_message()
}

#[test]
fn a_sound_store_goes_on_to_the_next_step() {
    let collection = stored(&[STAMP]);

    let run = push(&collection, &[]);

    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
}

#[test]
fn a_version_without_its_listing_is_mended_by_rebuild() {
    let collection = stored(&[STAMP]);
    collection.damage_remove_listing("projects/a", STAMP);

    let message = refusal(&collection);

    assert!(
        message.contains(&format!("`antiq rebuild projects/a {STAMP}`")),
        "{message}"
    );
    assert!(!message.contains("antiq forget"), "{message}");
}

#[test]
fn an_archive_of_the_wrong_size_is_forgotten_or_copied_back() {
    let collection = stored(&[STAMP]);
    collection.damage_truncate_archive("projects/a", STAMP);

    let message = refusal(&collection);

    assert!(
        message.contains(&format!("`antiq forget projects/a {STAMP}`")),
        "{message}"
    );
    assert!(message.contains("copy the version back"), "{message}");
}

#[test]
fn a_damaged_checksum_file_may_be_deleted_and_rebuilt() {
    let collection = stored(&[STAMP]);
    collection.damage_corrupt_checksum("projects/a", STAMP);

    let message = refusal(&collection);

    assert!(
        message.contains(&format!("`antiq forget projects/a {STAMP}`")),
        "{message}"
    );
    assert!(
        message.contains(&format!("and run `antiq rebuild projects/a {STAMP}`")),
        "{message}"
    );
}

#[test]
fn an_incomplete_version_is_forgotten_or_copied_back() {
    let collection = stored(&[STAMP]);
    collection.damage_remove_archive("projects/a", STAMP);

    let message = refusal(&collection);

    assert!(
        message.contains(&format!("`antiq forget projects/a {STAMP}`")),
        "{message}"
    );
}

#[test]
fn every_damaged_version_is_named() {
    let collection = stored(&[STAMP, OTHER_STAMP]);
    collection.damage_remove_listing("projects/a", STAMP);
    collection.damage_remove_archive("projects/a", OTHER_STAMP);

    let message = refusal(&collection);

    assert!(message.contains(STAMP), "{message}");
    assert!(message.contains(OTHER_STAMP), "{message}");
}

#[test]
fn a_version_of_a_newer_format_does_not_refuse() {
    let collection = stored(&[STAMP]);
    let listing = collection.version_files("projects/a", STAMP).listing;
    let newer = zstd::stream::encode_all(&b"# antiq listing 2\n"[..], 3).expect("encodes");
    fs::write(listing, newer).expect("the listing is written");

    let run = push(&collection, &[]);

    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
}

#[test]
fn a_tree_that_trash_cannot_give_back_refuses() {
    use std::os::unix::fs::PermissionsExt;
    let collection = stored(&[]);
    // `projects` is taken, so the tree cannot go back, and `recovered/`
    // refuses it too.
    let tree = collection.path(&format!(".antiq/trash/{STAMP}/projects/a"));
    fs::create_dir_all(&tree).expect("the tree's folder is made");
    fs::write(tree.join("kept.txt"), "kept").expect("the file is written");
    let recovered = collection.path(".antiq/recovered");
    fs::set_permissions(&recovered, fs::Permissions::from_mode(0o555))
        .expect("recovered/ is locked");
    assert!(
        fs::File::create(recovered.join("probe")).is_err(),
        "the test runs as a user who cannot write into a 0555 directory"
    );

    let message = refusal(&collection);

    fs::set_permissions(&recovered, fs::Permissions::from_mode(0o755))
        .expect("recovered/ is unlocked");
    assert!(message.contains(".antiq/trash/"), "{message}");
    assert!(message.contains("kept.txt"), "{message}");
}

#[test]
fn trash_that_holds_only_empty_folders_does_not_refuse() {
    let collection = stored(&[]);
    fs::create_dir_all(collection.path(".antiq/trash/chain/of/folders")).expect("made");

    let run = push(&collection, &[]);

    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
}

#[test]
fn a_store_folder_that_cannot_be_read_is_an_error_not_a_pass() {
    use std::os::unix::fs::PermissionsExt;
    let collection = stored(&[STAMP]);
    let folder = collection.path(".antiq/pieces/projects/a");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).expect("locked");
    assert!(
        fs::read_dir(&folder).is_err(),
        "the test runs as a user without access"
    );

    let run = push(&collection, &[]);

    fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).expect("unlocked");
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other", "{}", run.stdout);
    assert!(run.error_message().contains("store"), "{}", run.stdout);
}

#[test]
fn a_pattern_that_cannot_be_compiled_is_an_error_not_a_pass() {
    let collection = Collection::build(&Clock::now())
        .patterns(&["projects/[a"])
        .piece("projects/a", |piece| {
            piece.age_days(3).file("a.txt", "a");
        })
        .finish();
    set_url(&collection, "/nonexistent/target");

    let run = push(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other", "{}", run.stdout);
    assert!(run.error_message().contains("select"), "{}", run.stdout);
}

#[test]
fn a_listing_that_cannot_be_read_is_an_error_not_a_pass() {
    use std::os::unix::fs::PermissionsExt;
    let collection = stored(&[STAMP]);
    let listing = collection.version_files("projects/a", STAMP).listing;
    fs::set_permissions(&listing, fs::Permissions::from_mode(0o000)).expect("locked");
    assert!(
        fs::File::open(&listing).is_err(),
        "the test runs as a user without access"
    );

    let run = push(&collection, &[]);

    fs::set_permissions(&listing, fs::Permissions::from_mode(0o644)).expect("unlocked");
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other", "{}", run.stdout);
    assert!(run.error_message().contains("store"), "{}", run.stdout);
}

// =========================================================
// Step 3: the local rsync
// =========================================================

#[test]
fn an_rsync_that_is_too_old_or_not_rsync_is_refused_with_its_path() {
    let versions = [
        "openrsync: protocol version 29\nrsync version 2.6.9 compatible\n",
        "rsync  version 3.2.3  protocol version 31\n",
        "rsync  version 3.1.0  protocol version 31\n",
    ];
    for version in versions {
        let collection = stored_with(&[], version);
        let fake = &collection.fake;

        let run = push(&collection, &[]);

        assert_eq!(run.code(), 1, "{version:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "other", "{version:?}");
        let message = run.error_message();
        assert!(message.contains("[target] rsync"), "{version:?}: {message}");
        let path = fake.path();
        assert!(
            message.contains(path.to_str().expect("a UTF-8 path")),
            "{version:?}: {message}"
        );
    }
}

#[test]
fn a_dry_run_checks_the_rsync_too() {
    let collection = stored_with(&[], "rsync  version 3.2.3  protocol version 31\n");

    let run = push(&collection, &["-n"]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other");
    assert!(run.error_message().contains("[target] rsync"));
}

#[test]
fn an_rsync_of_3_2_4_goes_on_to_the_next_step() {
    for args in [&[][..], &["-n"][..]] {
        let collection = stored(&[]);

        let run = push(&collection, args);

        assert_eq!(
            run.error_kind(),
            "not_implemented",
            "{args:?}: {}",
            run.stdout
        );
    }
}

// =========================================================
// Step 4: the pre-check of a path target
// =========================================================

const RSYNC_3_2_4: &str = "rsync  version 3.2.4  protocol version 31\n";

/// A collection with a sound rsync and `url` as its target.
fn targeted(url: &Path) -> (Collection, FakeRsync) {
    let collection = collection();
    set_url(&collection, url.to_str().expect("a UTF-8 path"));
    let fake = use_fake_rsync(&collection, RSYNC_3_2_4);
    (collection, fake)
}

/// Asserts that a push stopped as unreachable and returns the reason.
fn unreachable_reason(run: &Run) -> String {
    assert_eq!(run.code(), 4, "{}", run.stdout);
    assert_eq!(run.error_kind(), "target_unreachable", "{}", run.stdout);
    run.error_message()
}

/// Asserts that a push was refused with exit 1 and returns the message.
fn refused_message(run: &Run) -> String {
    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_ne!(run.error_kind(), "not_implemented", "{}", run.stdout);
    run.error_message()
}

fn passes_the_precheck(run: &Run) {
    assert_eq!(run.error_kind(), "not_implemented", "{}", run.stdout);
}

#[test]
fn a_missing_directory_is_unreachable_also_with_init_and_in_a_dry_run() {
    let gone = Target::new();
    let (collection, _fake) = targeted(&gone.path().join("not-there"));

    for args in [&[][..], &["--init"][..], &["-n"][..], &["--force"][..]] {
        let reason = unreachable_reason(&push(&collection, args));
        assert!(reason.contains("not-there"), "{args:?}: {reason}");
    }
}

#[test]
fn if_reachable_turns_an_unreachable_target_into_exit_0_with_the_reason() {
    let gone = Target::new();
    let (collection, _fake) = targeted(&gone.path().join("not-there"));

    for args in [&["--if-reachable"][..], &["--if-reachable", "-n"][..]] {
        let run = push(&collection, args);

        assert_eq!(run.code(), 0, "{args:?}: {}", run.stdout);
        let json = run.json();
        assert_eq!(json["command"], "push", "{args:?}");
        assert_eq!(json["result"], "unreachable", "{args:?}");
        assert_eq!(json["dry_run"], args.contains(&"-n"), "{args:?}");
        assert!(
            json["reason"]
                .as_str()
                .is_some_and(|r| r.contains("not-there")),
            "{args:?}: {json}"
        );
    }

    let text = push_text(&collection, &["--if-reachable"]);
    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert_eq!(text.stdout, "");
    assert!(text.stderr.starts_with("note: "), "{}", text.stderr);
    assert!(text.stderr.contains("not-there"), "{}", text.stderr);
}

#[test]
fn an_empty_directory_without_a_marker_is_unreachable_and_names_init() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());

    for args in [&[][..], &["--force"][..], &["-n"][..]] {
        let reason = unreachable_reason(&push(&collection, args));
        assert!(reason.contains("push --init"), "{args:?}: {reason}");
    }
    let run = push(&collection, &["--if-reachable"]);
    assert_eq!(run.code(), 0, "{}", run.stdout);
    assert_eq!(run.json()["result"], "unreachable");
}

#[test]
fn init_takes_an_empty_directory_through_the_pre_check() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());

    passes_the_precheck(&push(&collection, &["--init"]));
}

#[test]
fn a_directory_with_files_and_no_marker_is_refused_whatever_the_flags() {
    let mut target = Target::new();
    target.tree_mut().file("stray.txt", "x");
    let (collection, _fake) = targeted(&target.path());

    for args in [&[][..], &["--init"][..], &["--force"][..], &["-n"][..]] {
        let message = refused_message(&push(&collection, args));
        assert!(message.contains("not empty"), "{args:?}: {message}");
    }
}

#[test]
fn a_marker_of_another_collection_is_refused_also_with_force() {
    let target = Target::new();
    target.marker_with(
        "01BX5ZZKBKACTAV9WEVGEMMVRY",
        &target.machine_name(),
        "/elsewhere",
    );
    let (collection, _fake) = targeted(&target.path());

    for args in [&[][..], &["--force"][..], &["--init"][..]] {
        let message = refused_message(&push(&collection, args));
        assert!(
            message.contains("another collection"),
            "{args:?}: {message}"
        );
    }
}

#[test]
fn a_marker_that_cannot_be_read_is_refused() {
    for content in ["not json at all\n", "{\"format\":1}\n", ""] {
        let target = Target::new();
        fs::write(target.path().join(".antiq-target"), content).expect("written");
        let (collection, _fake) = targeted(&target.path());

        let message = refused_message(&push(&collection, &["--force"]));

        assert!(message.contains("not readable"), "{content:?}: {message}");
    }
}

#[test]
fn a_marker_that_is_no_regular_file_is_not_readable() {
    let target = Target::new();
    fs::create_dir(target.path().join(".antiq-target")).expect("made");
    let (collection, _fake) = targeted(&target.path());
    let message = refused_message(&push(&collection, &[]));
    assert!(message.contains("not readable"), "{message}");

    let target = Target::new();
    std::os::unix::fs::symlink("elsewhere", target.path().join(".antiq-target")).expect("link");
    let (collection, _fake) = targeted(&target.path());
    let message = refused_message(&push(&collection, &[]));
    assert!(message.contains("not readable"), "{message}");

    // A link to a good marker does not stand in for one.
    let target = Target::new();
    let elsewhere = Target::new();
    let (collection, _fake) = targeted(&target.path());
    elsewhere.marker(&collection);
    std::os::unix::fs::symlink(
        elsewhere.path().join(".antiq-target"),
        target.path().join(".antiq-target"),
    )
    .expect("link");
    let message = refused_message(&push(&collection, &[]));
    assert!(message.contains("not readable"), "{message}");
}

#[test]
fn a_marker_that_cannot_be_opened_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let target = Target::new();
    let marker = target.path().join(".antiq-target");
    fs::write(&marker, "x").expect("written");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).expect("locked");
    assert!(
        fs::File::open(&marker).is_err(),
        "the test runs as a user without access"
    );
    let (collection, _fake) = targeted(&target.path());

    let run = push(&collection, &[]);

    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).expect("unlocked");
    let message = refused_message(&run);
    assert!(message.contains("not readable"), "{message}");
    assert!(message.contains("Permission denied"), "{message}");
}

#[test]
fn a_marker_file_is_not_read_beyond_64_kib() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());
    let line = format!(
        "{{\"format\":1,\"id\":\"{}\",\"host\":\"{}\",\"path\":\"{}\",\"pad\":\"{}\"}}\n",
        collection.id(),
        target.machine_name(),
        collection.root().display(),
        "x".repeat(100 * 1024)
    );
    fs::write(target.path().join(".antiq-target"), line).expect("written");

    let message = refused_message(&push(&collection, &[]));

    assert!(message.contains("not readable"), "{message}");
}

#[test]
fn a_marker_of_a_newer_format_has_its_own_error_kind() {
    let target = Target::new();
    fs::write(
        target.path().join(".antiq-target"),
        "{\"format\":2,\"id\":\"x\"}\n",
    )
    .expect("written");
    let (collection, _fake) = targeted(&target.path());

    let run = push(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "format_too_new");
}

#[test]
fn another_host_or_path_in_the_marker_is_refused_naming_both_and_force_pushes_on() {
    let cases = [("Some Other Mac", None), ("", Some("/moved/collection"))];
    for (host, path) in cases {
        let target = Target::new();
        let (collection, _fake) = targeted(&target.path());
        let own_host = target.machine_name();
        let host = if host.is_empty() {
            own_host.as_str()
        } else {
            host
        };
        let own_path = collection.root().to_str().expect("UTF-8").to_string();
        let path = path.unwrap_or(own_path.as_str());
        target.marker_with(&collection.id().to_string(), host, path);
        let marker = target.path().join(".antiq-target");
        let before = fs::read(&marker).expect("the marker exists");

        let message = refused_message(&push(&collection, &[]));
        assert!(message.contains(host), "{message}");
        assert!(message.contains(path), "{message}");
        assert!(message.contains(&own_path), "{message}");
        assert!(message.contains(&own_host), "{message}");

        passes_the_precheck(&push(&collection, &["--force"]));
        assert_eq!(fs::read(&marker).expect("still there"), before);
    }
}

#[test]
fn a_matching_marker_passes_also_with_force_and_is_refused_with_init() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());
    target.marker(&collection);

    passes_the_precheck(&push(&collection, &[]));
    passes_the_precheck(&push(&collection, &["--force"]));
    passes_the_precheck(&push(&collection, &["-n"]));
    let message = refused_message(&push(&collection, &["--init"]));
    assert!(message.contains("already"), "{message}");
}

#[test]
fn a_directory_that_cannot_be_read_is_refused_not_unreachable() {
    use std::os::unix::fs::PermissionsExt;
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());
    fs::set_permissions(target.path(), fs::Permissions::from_mode(0o000)).expect("locked");
    assert!(
        fs::read_dir(target.path()).is_err(),
        "the test runs as a user without access"
    );

    let run = push(&collection, &[]);

    fs::set_permissions(target.path(), fs::Permissions::from_mode(0o755)).expect("unlocked");
    let message = refused_message(&run);
    assert!(message.contains("cannot be read"), "{message}");
}

#[test]
fn a_target_that_is_the_collection_or_below_it_is_refused_with_every_flag() {
    let (collection, _fake) = targeted(Path::new("/nonexistent/x"));
    let inside = collection.root().to_path_buf();
    set_target_url(&collection, &inside);
    let below = collection.path("projects");
    let missing_below = collection.path("projects/a/no-such-dir");
    let link_dir = Target::new();
    let link = link_dir.path().join("spelling");
    std::os::unix::fs::symlink(collection.path("projects"), &link).expect("link");

    for target in [&inside, &below, &missing_below, &link] {
        set_target_url(&collection, target);
        for args in [&[][..], &["--init"][..], &["--force"][..], &["-n"][..]] {
            let message = refused_message(&push(&collection, args));
            assert!(
                message.contains("collection"),
                "{target:?} {args:?}: {message}"
            );
        }
    }
}

#[test]
fn a_target_that_holds_the_collection_is_refused_with_every_flag() {
    let (collection, _fake) = targeted(Path::new("/"));
    let parent = collection
        .root()
        .parent()
        .expect("has a parent")
        .to_path_buf();

    for target in [Path::new("/"), parent.as_path()] {
        set_target_url(&collection, target);
        for args in [&[][..], &["--init"][..], &["--force"][..]] {
            let message = refused_message(&push(&collection, args));
            assert!(
                message.contains("inside the target"),
                "{target:?} {args:?}: {message}"
            );
        }
    }
}

#[test]
fn the_root_of_a_mounted_volume_is_refused_with_every_flag() {
    use std::os::unix::fs::MetadataExt;
    let dev = Path::new("/dev");
    assert_ne!(
        fs::metadata(dev).expect("/dev exists").dev(),
        fs::metadata("/").expect("/ exists").dev(),
        "the test machine mounts /dev on a device of its own"
    );
    let (collection, _fake) = targeted(dev);

    for args in [&[][..], &["--init"][..], &["--force"][..]] {
        let message = refused_message(&push(&collection, args));
        assert!(message.contains("mount"), "{args:?}: {message}");
    }
}

// =========================================================
// Step 4: the pre-check of an ssh target
// =========================================================

const SSH_URL: &str = "backup@nas:/srv/antiq";
const DOT: &str = "drwxr-xr-x          4,096 2026/11/04 20:07:08 .\n";

/// A collection that pushes to an ssh target whose rsync is a stand-in.
fn ssh_targeted() -> (Collection, FakeRsync) {
    let collection = collection();
    set_url(&collection, SSH_URL);
    let fake = use_fake_rsync(&collection, RSYNC_3_2_4);
    (collection, fake)
}

fn ssh_text(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/ssh-texts/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the fixture text exists")
}

#[test]
fn ssh_texts_of_a_host_that_cannot_be_reached_are_unreachable() {
    for name in ["connect-refused", "resolve", "timeout"] {
        let (collection, fake) = ssh_targeted();
        fake.answer_all("", &ssh_text(name), 255);

        let reason = unreachable_reason(&push(&collection, &[]));

        assert!(reason.contains(SSH_URL), "{name}: {reason}");
        assert!(reason.contains("ssh:"), "{name}: {reason}");
        let run = push(&collection, &["--if-reachable"]);
        assert_eq!(run.code(), 0, "{name}: {}", run.stdout);
    }
}

#[test]
fn other_ssh_failures_are_refused_with_the_line() {
    for (name, line) in [
        ("host-key-changed", "Host key verification failed."),
        ("host-key-unknown", "Host key verification failed."),
        ("permission-denied", "Permission denied"),
    ] {
        let (collection, fake) = ssh_targeted();
        fake.answer_all("", &ssh_text(name), 255);

        for args in [&[][..], &["--if-reachable"][..], &["--init"][..]] {
            let message = refused_message(&push(&collection, args));
            assert!(message.contains(line), "{name} {args:?}: {message}");
        }
    }
}

#[test]
fn a_missing_ssh_directory_is_refused_unless_init_creates_it() {
    let missing =
        "rsync: [sender] change_dir \"/srv/antiq\" failed: No such file or directory (2)\n";
    let (collection, fake) = ssh_targeted();
    fake.answer_all("", missing, 23);

    let message = refused_message(&push(&collection, &[]));
    assert!(message.contains("does not exist"), "{message}");
    let message = refused_message(&push(&collection, &["--force"]));
    assert!(message.contains("does not exist"), "{message}");
    passes_the_precheck(&push(&collection, &["--init"]));
}

#[test]
fn another_exit_23_of_the_listing_is_refused_even_with_init() {
    let (collection, fake) = ssh_targeted();
    fake.answer_all(
        "",
        "rsync: [sender] change_dir \"/srv\" failed: Permission denied (13)\n",
        23,
    );

    for args in [&[][..], &["--init"][..]] {
        let message = refused_message(&push(&collection, args));
        assert!(message.contains("Permission denied"), "{args:?}: {message}");
    }
}

#[test]
fn an_empty_ssh_directory_is_unreachable_with_a_hint_and_init_takes_it() {
    let (collection, fake) = ssh_targeted();
    fake.answer_all(DOT, "", 0);

    let reason = unreachable_reason(&push(&collection, &[]));
    assert!(reason.contains("push --init"), "{reason}");
    passes_the_precheck(&push(&collection, &["--init"]));
}

#[test]
fn a_silent_listing_is_no_empty_directory_and_init_marks_nothing() {
    let (collection, fake) = ssh_targeted();
    fake.answer_all("", "", 0);

    for args in [&[][..], &["--init"][..]] {
        let message = refused_message(&push(&collection, args));
        assert!(
            message.contains("not list the directory"),
            "{args:?}: {message}"
        );
    }
    assert!(
        fake.calls()
            .iter()
            .all(|call| !call.contains(&"--chmod=F644".to_string())),
        "no marker run: {:?}",
        fake.calls()
    );
}

#[test]
fn a_quiet_option_in_rsync_args_is_refused_before_any_rsync_runs() {
    for option in ["-q", "--quiet", "-aq"] {
        let (collection, fake) = ssh_targeted();
        add_target_key(&collection, &format!("rsync_args = [\"{option}\"]"));
        fake.answer_all(DOT, "", 0);

        for args in [
            &[][..],
            &["--init"][..],
            &["-n"][..],
            &["--if-reachable"][..],
        ] {
            let message = refused_message(&push(&collection, args));
            assert!(
                message.contains("rsync_args") && message.contains(option),
                "{option} {args:?}: {message}"
            );
        }
        assert!(
            !fake.dir().join("calls.log").exists(),
            "{option}: rsync was called"
        );
    }
}

#[test]
fn an_ssh_directory_with_files_and_no_marker_is_refused_also_with_init() {
    let (collection, fake) = ssh_targeted();
    fake.answer_all(
        &format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 stray file.txt\n"),
        "",
        0,
    );

    for args in [&[][..], &["--init"][..]] {
        let message = refused_message(&push(&collection, args));
        assert!(message.contains("not empty"), "{args:?}: {message}");
    }
}

#[test]
fn a_marker_that_is_a_directory_on_the_ssh_target_is_not_readable() {
    let (collection, fake) = ssh_targeted();
    fake.answer_all(
        &format!("{DOT}drwxr-xr-x  64 2026/11/04 20:07:08 .antiq-target\n"),
        "",
        0,
    );

    let message = refused_message(&push(&collection, &[]));

    assert!(message.contains("not readable"), "{message}");
    assert_eq!(
        fake.calls().len(),
        2,
        "the version probe and the listing only"
    );
}

#[test]
fn a_marker_that_cannot_be_fetched_is_not_readable_and_leaves_no_directory() {
    let marker = format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 .antiq-target\n");
    let (collection, fake) = ssh_targeted();
    fake.answer(1, &marker, "", 0);
    fake.answer(2, "", "rsync: link_stat failed\n", 23);

    let message = refused_message(&push(&collection, &["--force"]));

    assert!(message.contains("not readable"), "{message}");
    let leftovers: Vec<_> = fs::read_dir(collection.path(".antiq/tmp"))
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_dry_run_fetches_the_marker_without_touching_the_collection() {
    let marker = format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 .antiq-target\n");
    let (collection, fake) = ssh_targeted();
    fake.answer(1, &marker, "", 0);
    fake.answer(2, "", "", 0);
    let before = snapshot(&collection.root());

    let message = refused_message(&push(&collection, &["-n"]));

    assert!(message.contains("not readable"), "{message}");
    assert_eq!(snapshot(&collection.root()), before);
}

#[test]
fn the_listing_and_the_fetch_are_run_in_the_c_locale_with_the_options_of_the_mirror() {
    let marker = format!("{DOT}-rw-r--r--  12 2026/11/04 20:07:08 .antiq-target\n");
    let (collection, fake) = ssh_targeted();
    add_target_key(&collection, "rsync_path = \"/opt/bin/rsync\"");
    add_target_key(&collection, "rsync_args = [\"--bwlimit=100\"]");
    fake.answer(1, &marker, "", 0);
    fake.answer(2, "", "", 0);

    refused_message(&push(&collection, &[]));

    let calls = fake.calls();
    let list = &calls[1];
    assert_eq!(list[0], "transfer");
    assert!(list.contains(&"--list-only".to_string()), "{list:?}");
    assert!(list.contains(&"--timeout=300".to_string()), "{list:?}");
    assert!(
        list.contains(&"--rsync-path=/opt/bin/rsync".to_string()),
        "{list:?}"
    );
    assert!(list.contains(&"--bwlimit=100".to_string()), "{list:?}");
    assert!(!list.contains(&"--fsync".to_string()), "{list:?}");
    assert_eq!(list.last().expect("a target"), "backup@nas:/srv/antiq/");
    let fetch = &calls[2];
    assert!(fetch.contains(&"--fsync".to_string()), "{fetch:?}");
    assert!(fetch.contains(&"--max-size=64K".to_string()), "{fetch:?}");
    assert_eq!(
        fetch[fetch.len() - 2],
        "backup@nas:/srv/antiq/.antiq-target"
    );
    let env = String::from_utf8(fake.read("env.log")).expect("UTF-8");
    assert!(
        env.lines()
            .filter(|l| l.starts_with("LC_ALL="))
            .all(|l| l == "LC_ALL=C")
    );
}
