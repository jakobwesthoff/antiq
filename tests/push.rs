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
use support::scenario::world::run_on_terminal;
use support::ssh_shim::SshShim;

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

/// Like `push`, under the file mode creation mask `mask`.
fn push_with_umask(collection: &Collection, mask: u32, args: &[&str]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .umask(mask)
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
    let message = run.error_text();
    assert!(message.contains("[target]"), "{message}");
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
    assert!(
        run.stderr
            .starts_with("error: no push target is configured, so push has nowhere to mirror the antiq collection to\n  fix: "),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("fix: Add this to .antiq/config.toml:\n")
    );
    assert!(run.stderr.contains("[target]"), "{}", run.stderr);
    assert!(
        run.stderr
            .ends_with("  while attempting to push the antiq collection\n"),
        "{}",
        run.stderr
    );
}

#[test]
fn the_json_message_of_no_target_is_the_library_text() {
    let collection = collection();

    let run = push(&collection, &[]);

    assert_eq!(
        run.error_message(),
        "no push target is configured, so push has nowhere to mirror the antiq collection to"
    );
    assert!(
        run.error_fix()
            .contains("Then run `antiq push --init` once on an empty directory of the target"),
        "{}",
        run.error_fix()
    );
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
        let keys: Vec<&str> = error
            .as_object()
            .expect("the error is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["kind", "code", "message", "fix", "pid", "started"],
            "{args:?}"
        );
        assert_eq!(error["code"], "antiq::lock::held", "{args:?}");
        assert_eq!(
            error["fix"], "Wait until that run ends, then run the command again.",
            "{args:?}"
        );
        assert_eq!(
            error["message"],
            format!(
                "cannot start the run: another antiq run holds the lock (pid {}, started {}), and runs that change an antiq collection take turns",
                std::process::id(),
                error["started"].as_str().expect("a time")
            ),
            "{args:?}"
        );
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

    goes_on(&run);
    assert_eq!(snapshot(&collection.root()), before);
}

#[test]
fn a_real_run_repairs_before_it_goes_on() {
    let collection = stored(&[]);
    let stale = collection.path(".antiq/tmp/stale-entry");
    fs::create_dir_all(stale.parent().expect("has a parent")).expect("tmp exists");
    fs::write(&stale, "left behind").expect("the entry is written");

    let run = push(&collection, &[]);

    goes_on(&run);
    assert!(!Path::new(&stale).exists(), "repair 2 removes the entry");
}

/// A leftover in `tmp/` that repair 2 removes.
fn leave_stale_entry(collection: &Collection) {
    let stale = collection.path(".antiq/tmp/stale-entry");
    fs::create_dir_all(stale.parent().expect("has a parent")).expect("tmp exists");
    fs::write(&stale, "left behind").expect("the entry is written");
}

#[test]
fn a_real_run_lists_its_repairs_in_json_and_on_stderr() {
    let collection = stored(&[]);
    leave_stale_entry(&collection);
    let run = push(&collection, &[]);
    goes_on(&run);
    let repairs = run.json()["repairs"].clone();
    assert_eq!(repairs.as_array().map(Vec::len), Some(1), "{}", run.stdout);
    assert_eq!(repairs[0]["kind"], "repair", "{repairs}");

    leave_stale_entry(&collection);
    let text = push_text(&collection, &[]);
    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert!(text.stderr.contains("note: repaired: "), "{}", text.stderr);
}

#[test]
fn an_unreachable_target_under_if_reachable_still_lists_the_repairs() {
    let gone = Target::new();
    let (collection, _fake) = targeted(&gone.path().join("not-there"));
    leave_stale_entry(&collection);
    let run = push(&collection, &["--if-reachable"]);
    assert_eq!(run.code(), 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(run.json()["result"], "unreachable");
    assert_eq!(
        run.json()["repairs"].as_array().map(Vec::len),
        Some(1),
        "{}",
        run.stdout
    );

    leave_stale_entry(&collection);
    let text = push_text(&collection, &["--if-reachable"]);
    assert!(text.stderr.starts_with("note: "), "{}", text.stderr);
    assert!(text.stderr.contains("not-there"), "{}", text.stderr);
    assert!(text.stderr.contains("note: repaired: "), "{}", text.stderr);
}

#[test]
fn an_unknown_target_key_is_a_warning_in_json_and_a_note_on_stderr() {
    let collection = stored(&[]);
    add_target_key(&collection, "rsyc_args = []");
    let note = "unknown setting `rsyc_args` under [target], which antiq ignores";

    let run = push(&collection, &[]);

    goes_on(&run);
    assert_eq!(
        run.json()["warnings"],
        serde_json::json!([{
            "code": "antiq::config::unknown_key",
            "message": note,
            "fix": "Correct the name or remove the line in `.antiq/config.toml`."
        }])
    );
    let text = push_text(&collection, &[]);
    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert!(
        text.stderr.contains(&format!("warning: {note}")),
        "{}",
        text.stderr
    );
}

#[test]
fn the_help_of_init_names_the_empty_directory_and_the_last_missing_level_over_ssh() {
    let run = Antiq::new().arg("push").arg("--help").run();

    assert_eq!(run.code(), 0, "{}", run.stderr);
    let help = run.stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(!help.contains("marker and directory"), "{help}");
    assert!(
        help.contains(
            "Write the target's marker on an empty directory (over ssh, also create its last missing level)"
        ),
        "{help}"
    );
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
    run.error_text()
}

#[test]
fn a_sound_store_goes_on_to_the_next_step() {
    let collection = stored(&[STAMP]);

    let run = push(&collection, &[]);

    goes_on(&run);
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
fn the_commands_of_a_refused_push_carry_the_directory_option_on_a_console_only() {
    let collection = stored(&[STAMP]);
    collection.damage_remove_listing("projects/a", STAMP);
    let root = collection.root();

    let text = push_text(&collection, &[]);
    assert_eq!(text.code(), 1, "{}", text.stderr);
    let pasteable = format!("antiq -C {} rebuild projects/a {STAMP}", root.display());
    assert!(text.stderr.contains(&pasteable), "{}", text.stderr);

    let json = push(&collection, &[]);
    let fix = json.error_fix();
    assert!(
        fix.contains(&format!("`antiq rebuild projects/a {STAMP}`")),
        "{fix}"
    );
    assert!(!fix.contains("-C"), "{fix}");
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

    goes_on(&run);
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

    goes_on(&run);
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

        goes_on(&run);
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
    run.error_text()
}

/// What the program said, as the groups of the error: each normalised line
/// with its example.
fn said_text(run: &Run) -> String {
    let said = &run.json()["error"]["program_said"];
    let groups = said["groups"].as_array().expect("the error holds groups");
    groups
        .iter()
        .map(|group| {
            format!(
                "{} {}",
                group["line"].as_str().expect("a line"),
                group["example"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn passes_the_precheck(run: &Run) {
    goes_on(run);
}

/// Asserts that a push ran through and reports `pushed`.
fn goes_on(run: &Run) {
    assert_eq!(run.code(), 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(run.json()["result"], "pushed", "{}", run.stdout);
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
        assert_eq!(json["dry_run"] == true, args.contains(&"-n"), "{args:?}");
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
fn an_unreachable_dry_run_under_if_reachable_prints_no_nothing_to_do() {
    let collection = collection();
    set_url(&collection, "/nonexistent/not-there");
    let _fake = use_fake_rsync(&collection, RSYNC_3_2_4);

    let text = push_text(&collection, &["-n", "--if-reachable"]);

    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert_eq!(text.stdout, "");
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
            let run = push(&collection, args);
            let message = format!("{}\n{}", refused_message(&run), said_text(&run));
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
        let run = push(&collection, args);
        let message = format!("{}\n{}", refused_message(&run), said_text(&run));
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

// =========================================================
// Step 5: the marker
// =========================================================

/// The first rsync of 3.2.4 or later that a developer machine has in the
/// places antiq searches. The tests that send files need a real one.
fn real_rsync() -> std::path::PathBuf {
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    for dir in dirs {
        let candidate = dir.join("rsync");
        let Ok(output) = std::process::Command::new(&candidate)
            .arg("--version")
            .output()
        else {
            continue;
        };
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let version = text
            .split_whitespace()
            .nth(2)
            .and_then(|v| {
                let mut parts = v.split('.').map(|p| p.parse::<u32>().ok());
                Some((
                    parts.next()??,
                    parts.next()??,
                    parts.next().flatten().unwrap_or(0),
                ))
            })
            .unwrap_or((0, 0, 0));
        if text.starts_with("rsync") && version >= (3, 2, 4) {
            return candidate;
        }
    }
    panic!("these tests need an rsync 3.2.4 or newer in PATH, /opt/homebrew/bin or /usr/local/bin");
}

/// The bytes `push --init` writes for `collection` on this machine.
fn expected_marker(collection: &Collection) -> Vec<u8> {
    antiq::target::marker::Marker::new(
        collection.id().parse().expect("a collection ID"),
        support::cmd::machine_name(),
        collection.root().to_str().expect("UTF-8").to_string(),
    )
    .to_bytes()
}

/// A collection pushing to `target` with the real rsync.
fn with_real_rsync(target: &Path) -> Collection {
    let collection = collection();
    set_url(&collection, target.to_str().expect("a UTF-8 path"));
    let rsync = real_rsync();
    let quoted = serde_json::to_string(rsync.to_str().expect("UTF-8")).expect("quotes");
    add_target_key(&collection, &format!("rsync = {quoted}"));
    collection
}

fn tmp_entries(collection: &Collection) -> Vec<std::ffi::OsString> {
    fs::read_dir(collection.path(".antiq/tmp"))
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default()
}

#[test]
fn init_writes_the_marker_byte_for_byte_with_mode_644() {
    use std::os::unix::fs::MetadataExt;
    let target = Target::new();
    let root = target.path();
    let collection = with_real_rsync(&root);

    let run = push_with_umask(&collection, 0o022, &["--init"]);

    goes_on(&run);
    let marker = root.join(".antiq-target");
    assert_eq!(
        fs::read(&marker).expect("written"),
        expected_marker(&collection)
    );
    assert_eq!(fs::metadata(&marker).expect("exists").mode() & 0o777, 0o644);
    assert_eq!(tmp_entries(&collection), Vec::<std::ffi::OsString>::new());
}

#[test]
fn the_marker_mode_is_masked_by_the_umask_of_the_receiving_side() {
    use std::os::unix::fs::MetadataExt;
    let target = Target::new();
    let collection = with_real_rsync(&target.path());

    let run = push_with_umask(&collection, 0o027, &["--init"]);

    goes_on(&run);
    let meta = fs::metadata(target.path().join(".antiq-target")).expect("written");
    assert_eq!(meta.mode() & 0o777, 0o640);
}

#[test]
fn a_dry_run_with_init_writes_nothing() {
    let target = Target::new();
    let collection = with_real_rsync(&target.path());
    let before = snapshot(&collection.root());

    let run = push(&collection, &["--init", "-n"]);

    goes_on(&run);
    assert_eq!(fs::read_dir(target.path()).expect("exists").count(), 0);
    assert_eq!(snapshot(&collection.root()), before);
}

#[test]
fn force_and_a_matching_marker_send_no_marker() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker_with(&collection.id(), "Some Other Mac", "/elsewhere");
    passes_the_precheck(&push(&collection, &["--force"]));
    target.marker(&collection);
    passes_the_precheck(&push(&collection, &[]));

    let calls = fake.calls();
    let sends = calls
        .iter()
        .filter(|call| call.contains(&"--chmod=F644".to_string()));
    assert_eq!(sends.count(), 0, "no marker was sent: {calls:?}");
    assert_eq!(calls.len(), 4, "a probe and a mirror per push: {calls:?}");
}

#[test]
fn init_over_an_ssh_target_sends_the_marker_from_a_directory_of_its_own() {
    let (collection, fake) = ssh_targeted();
    fake.answer(1, DOT, "", 0);
    fake.answer(2, "", "", 0);

    let run = push(&collection, &["--init"]);

    goes_on(&run);
    let calls = fake.calls();
    assert_eq!(
        calls.len(),
        4,
        "a probe, the listing, the marker and the mirror"
    );
    let send = &calls[2];
    assert!(send.contains(&"-r".to_string()), "{send:?}");
    assert!(send.contains(&"--chmod=F644".to_string()), "{send:?}");
    assert!(send.contains(&"--fsync".to_string()), "{send:?}");
    assert_eq!(send.last().expect("a target"), "backup@nas:/srv/antiq/");
    let from = &send[send.len() - 2];
    assert!(from.contains(".antiq/tmp/push-"), "{from}");
    assert!(!Path::new(from).exists(), "the directory is removed again");
    assert_eq!(tmp_entries(&collection), Vec::<std::ffi::OsString>::new());
}

#[test]
fn init_creates_the_last_level_of_a_missing_ssh_directory() {
    let missing =
        "rsync: [sender] change_dir \"/srv/antiq\" failed: No such file or directory (2)\n";
    let (collection, fake) = ssh_targeted();
    fake.answer(1, "", missing, 23);
    fake.answer(2, "", "", 0);

    let run = push(&collection, &["--init"]);

    goes_on(&run);
    assert_eq!(fake.calls().len(), 4);
}

#[test]
fn a_marker_run_that_fails_is_exit_1_with_rsyncs_text() {
    let (collection, fake) = ssh_targeted();
    fake.answer(1, DOT, "", 0);
    fake.answer(
        2,
        "",
        "rsync: mkdir \"/srv/antiq/a/b\" failed: No such file or directory (2)\n",
        11,
    );

    let run = push(&collection, &["--init"]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "other");
    let message = run.error_message();
    assert!(message.contains("marker"), "{message}");
    assert!(message.contains("exit 11"), "{message}");
    assert!(said_text(&run).contains("mkdir"), "{}", said_text(&run));
    assert_eq!(tmp_entries(&collection), Vec::<std::ffi::OsString>::new());
}

#[test]
fn a_dry_run_with_init_over_ssh_sends_no_marker_and_mirrors_with_n() {
    let (collection, fake) = ssh_targeted();
    fake.answer_all(DOT, "", 0);

    let run = push(&collection, &["--init", "-n"]);

    goes_on(&run);
    let calls = fake.calls();
    assert_eq!(
        calls.len(),
        3,
        "a probe, the listing and the mirror: {calls:?}"
    );
    assert!(calls[2].contains(&"-n".to_string()), "{:?}", calls[2]);
    assert!(
        !calls[2].contains(&"--chmod=F644".to_string()),
        "{:?}",
        calls[2]
    );
    assert_eq!(run.json()["actions"][0]["kind"], "write_marker");
    let text = push_text(&collection, &["--init", "-n"]);
    assert!(
        text.stdout.contains("Would write the marker"),
        "{}",
        text.stdout
    );
}

// =========================================================
// Steps 6 to 9: the mirror and its result
// =========================================================

/// The mirror call of a run: the last call the stand-in saw.
fn mirror_call(fake: &FakeRsync) -> Vec<String> {
    let calls = fake.calls();
    let call = calls.last().expect("rsync was called").clone();
    assert_eq!(call[0], "transfer");
    call
}

/// What the stand-in prints for a mirror run that changed these paths.
const ITEMIZED: &str = ">f+++++++++ projects/a/new.txt\n\
    >f.st...... projects/a/old.txt\n\
    *deleting   projects/a/gone.txt\n\
    .d..t...... projects/a/\n\
    >f+++++++++ loose.txt\n\
    >f+++++++++ .antiq/pieces/projects/a/2024-06-30T081245--a.tar.zst\n";

#[test]
fn the_mirror_runs_with_the_collection_and_the_canonical_target() {
    let target = Target::new();
    let link_dir = Target::new();
    let link = link_dir.path().join("spelling");
    std::os::unix::fs::symlink(target.path(), &link).expect("link");
    let (collection, fake) = targeted(&link);
    target.marker(&collection);

    let run = push(&collection, &[]);

    goes_on(&run);
    let call = mirror_call(&fake);
    assert!(call.contains(&"--delete-delay".to_string()), "{call:?}");
    assert!(!call.contains(&"-n".to_string()), "{call:?}");
    assert_eq!(
        call[call.len() - 2],
        format!("{}/", collection.root().display())
    );
    assert_eq!(
        call[call.len() - 1],
        format!("{}/", target.path().display())
    );
}

#[test]
fn a_push_summarises_the_changes_per_piece_in_json_and_text() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);
    fake.answer(1, ITEMIZED, "", 0);
    fake.answer(2, "", "", 0);
    fake.answer(3, ITEMIZED, "", 0);
    fake.answer(4, "", "", 0);

    let json = push(&collection, &[]).json();

    assert_eq!(json["result"], "pushed");
    assert!(json.get("dry_run").is_none(), "{json}");
    assert_eq!(
        json["pieces"],
        serde_json::json!([{"piece": "projects", "created": 2, "updated": 1, "deleted": 1}])
    );
    assert_eq!(
        json["other"],
        serde_json::json!({"created": 1, "updated": 0, "deleted": 0})
    );
    assert_eq!(json["vanished"], serde_json::json!([]));
    assert_eq!(json["second_pass"], false);

    let text = push_text(&collection, &[]);
    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert!(
        text.stdout
            .contains("projects: 2 created, 1 updated, 1 deleted"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("other files: 1 created"),
        "{}",
        text.stdout
    );
}

#[test]
fn a_dry_run_lists_the_repairs_and_the_mirror_and_sends_no_marker() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);
    fake.answer_all(ITEMIZED, "", 0);
    let stale = collection.path(".antiq/tmp/stale-entry");
    fs::create_dir_all(stale.parent().expect("has a parent")).expect("tmp exists");
    fs::write(&stale, "left behind").expect("written");

    let json = push(&collection, &["-n"]).json();

    assert_eq!(json["dry_run"], true);
    assert_eq!(json["actions"][0]["kind"], "repair");
    assert_eq!(json["actions"][0]["repair"], "delete_temporary");
    let mirror = &json["actions"][1];
    assert_eq!(mirror["kind"], "mirror");
    assert_eq!(mirror["pieces"][0]["piece"], "projects");
    assert_eq!(mirror["other"]["created"], 1);
    assert_eq!(json["actions"].as_array().expect("a list").len(), 2);
    assert!(mirror_call(&fake).contains(&"-n".to_string()));
    assert!(stale.exists(), "a dry run repairs nothing");

    let text = push_text(&collection, &["-n"]);
    assert!(text.stdout.contains("Would delete"), "{}", text.stdout);
    assert!(
        text.stdout.contains("Would mirror the collection"),
        "{}",
        text.stdout
    );
    assert!(text.stdout.contains("Would change on"), "{}", text.stdout);
}

#[test]
fn files_that_vanished_are_a_success_with_a_note() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);
    fake.answer_all(
        "",
        "file has vanished: \"/c/projects/a/tmp\"\nrsync warning: some files vanished (code 24)\n",
        24,
    );

    let json = push(&collection, &[]).json();
    assert_eq!(json["vanished"], serde_json::json!(["/c/projects/a/tmp"]));
    assert_eq!(json["result"], "pushed");

    let text = push_text(&collection, &[]);
    assert_eq!(text.code(), 0, "{}", text.stderr);
    assert!(!text.stdout.contains("vanished"), "{}", text.stdout);
    assert!(
        text.stderr
            .contains("note: 1 files vanished while rsync ran"),
        "{}",
        text.stderr
    );
}

#[test]
fn a_failed_mirror_is_exit_1_with_rsyncs_text_also_under_if_reachable() {
    let cases: [(i32, &str, &str, &str); 5] = [
        (23, "", "rsync: some files could not be read\n", "exit 23"),
        (30, "", "Timeout in data send/receive\n", "exit 30"),
        (
            12,
            "",
            "openrsync: unrecognized option `--fsync'\n",
            "fsync = false",
        ),
        (
            12,
            "",
            "rsync: --fsync: unknown option\n",
            "[target] rsync_path",
        ),
        (
            23,
            "IO error encountered -- skipping file deletion\n",
            "rsync: [sender] opendir \"/c/locked\" failed: Permission denied (13)\n",
            "no deletion reached the target",
        ),
    ];
    for (code, stdout, stderr, expected) in cases {
        for args in [&[][..], &["-n"][..]] {
            let target = Target::new();
            let (collection, fake) = targeted(&target.path());
            target.marker(&collection);
            fake.answer_all(stdout, stderr, code);

            let message = refused_message(&push(&collection, args));

            assert!(message.contains(expected), "{code} {args:?}: {message}");
        }
    }
}

/// What rsync prints when the share refuses `chmod`: a line for each file.
fn refused_modes_stderr(files: usize) -> String {
    let lines: String = (1..=files)
        .map(|number| {
            format!(
                "rsync: [receiver] mkstemp \"/mnt/shadow/beta/.note{number}.md.A3Kcxa\" failed: Operation not permitted (1)\n"
            )
        })
        .collect();
    format!("{lines}rsync error: some files/attrs were not transferred (code 23) at main.c(1338)\n")
}

#[test]
fn a_target_that_refuses_file_modes_gets_the_option_that_fixes_it() {
    for args in [&[][..], &["-n"][..]] {
        let target = Target::new();
        let (collection, fake) = targeted(&target.path());
        target.marker(&collection);
        fake.answer_all("", &refused_modes_stderr(40), 23);

        let run = push(&collection, args);

        assert_eq!(run.code(), 1, "{}", run.stdout);
        assert_eq!(run.error_kind(), "other");
        let json = run.json();
        assert_eq!(
            json["error"]["code"], "antiq::push::target_refuses_modes",
            "{args:?}"
        );
        let fix = run.error_fix();
        assert!(fix.contains("rsync_args = [\"--no-perms\"]"), "{fix}");
        assert!(
            fix.contains("get the share's own modes, while the archived versions keep theirs"),
            "{fix}"
        );
        assert!(!fix.contains("does not let rsync set file modes"), "{fix}");
        assert!(!fix.contains("[receiver]"), "{fix}");
        assert!(json["error"].get("rsync_said").is_none());
        let said = &json["error"]["program_said"];
        assert_eq!(said["program"], "rsync");
        let groups = said["groups"].as_array().expect("the groups are a list");
        assert_eq!(groups.len(), 2, "{said}");
        assert_eq!(groups[0]["count"], 40, "{said}");
        assert_eq!(
            groups[0]["line"],
            "rsync: [receiver] mkstemp \"…\" failed: Operation not permitted (1)"
        );
        assert_eq!(groups[0]["example"], "/mnt/shadow/beta/.note1.md.A3Kcxa");
    }
}

#[test]
fn the_text_of_a_target_that_refuses_file_modes_is_short_in_a_pipe() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);
    fake.answer_all("", &refused_modes_stderr(40), 23);

    let run = push_text(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert!(
        run.stderr
            .contains("the target does not let rsync set file modes"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("--no-perms"), "{}", run.stderr);
    assert_eq!(
        run.stderr.matches("[receiver]").count(),
        1,
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("40× rsync: [receiver] mkstemp"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("e.g. /mnt/shadow/beta/.note1.md.A3Kcxa"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_mirror_ended_by_a_signal_is_exit_1() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);
    fake.kill_on(1);

    let message = refused_message(&push(&collection, &[]));

    assert!(message.contains("signal"), "{message}");
}

#[test]
fn a_forced_push_over_another_marker_says_so() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());
    target.marker_with(&collection.id(), "Some Other Mac", "/elsewhere");

    let json = push(&collection, &["--force"]).json();

    assert_eq!(json["forced"], true);
    let note = json["notes"][0].as_str().expect("a note");
    assert!(
        note.contains("Some Other Mac") && note.contains("/elsewhere"),
        "{note}"
    );
    assert_eq!(json["initialized"], false);
}

#[test]
fn a_dry_run_judges_the_store_by_the_repairs_it_lists() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());
    target.marker(&collection);
    // The tree can go back to its place, which repair 4 would do.
    let tree = collection.path(&format!(".antiq/trash/{STAMP}/restored/dir"));
    fs::create_dir_all(&tree).expect("made");
    fs::write(tree.join("kept.txt"), "kept").expect("written");

    let json = push(&collection, &["-n"]).json();

    assert_eq!(json["result"], "pushed");
    assert_eq!(json["actions"][0]["repair"], "give_back");
    assert!(tree.join("kept.txt").exists(), "a dry run moves nothing");
}

/// A collection with one stored version of `projects/a` that pushes to
/// `target`, whose marker it matches, with a stand-in rsync.
fn versioned(target: &Target) -> (Collection, FakeRsync) {
    let mut builder = Collection::build(&Clock::now());
    builder.piece("projects/a", |piece| {
        piece.age_days(3).file("a.txt", "a");
    });
    builder.version("projects/a", STAMP, &[("a.txt", "a")]);
    let collection = builder.finish();
    set_url(&collection, target.path().to_str().expect("UTF-8"));
    let fake = use_fake_rsync(&collection, RSYNC_3_2_4);
    target.marker(&collection);
    (collection, fake)
}

#[test]
fn a_dry_run_refuses_what_no_listed_repair_mends() {
    let target = Target::new();
    let (collection, _fake) = versioned(&target);
    collection.damage_truncate_archive("projects/a", STAMP);

    let message = refused_message(&push(&collection, &["-n"]));

    assert!(
        message.contains(&format!("antiq forget projects/a {STAMP}")),
        "{message}"
    );
}

#[test]
fn a_dry_run_does_not_refuse_a_version_whose_staging_a_repair_finishes() {
    let target = Target::new();
    let (collection, _fake) = versioned(&target);
    // The archive is staged and not yet moved into the store, as after a
    // run that died; repair 1 deletes the sidecars it left.
    let archive = collection.version_files("projects/a", STAMP).archive;
    let staged = collection.path(&format!(".antiq/tmp/pack/{STAMP}/projects/a"));
    fs::create_dir_all(&staged).expect("made");
    fs::copy(&archive, staged.join(archive.file_name().expect("a name"))).expect("copied");
    fs::remove_file(&archive).expect("removed");

    let json = push(&collection, &["-n"]).json();

    assert_eq!(json["result"], "pushed", "{json}");
    assert_eq!(json["actions"][0]["repair"], "delete_sidecars");
}

#[test]
fn a_planned_repair_with_no_staged_archive_saves_a_dry_run() {
    let target = Target::new();
    let mut builder = Collection::build(&Clock::now());
    builder.patterns(&["projects/*"]);
    builder.piece("projects/a", |piece| {
        piece.age_days(3).file("a.txt", "a");
    });
    builder.version("projects/a", STAMP, &[("a.txt", "a")]);
    let collection = builder.finish();
    set_url(&collection, target.path().to_str().expect("UTF-8"));
    let _fake = use_fake_rsync(&collection, RSYNC_3_2_4);
    target.marker(&collection);
    // An unearth that died left a damaged copy of the version; repair 5
    // deletes it.
    collection.damage_truncate_archive("projects/a", STAMP);
    fs::write(
        collection.path("projects/a/.antiq-restored"),
        format!("restored 2024-07-01T00:00:00Z version {STAMP}\n"),
    )
    .expect("written");

    let json = push(&collection, &["-n"]).json();

    assert_eq!(json["result"], "pushed", "{json}");
    assert_eq!(json["actions"][0]["repair"], "delete_restored_version");
}

// =========================================================
// Step 7: the second pass
// =========================================================

/// The number of transfers the stand-in saw.
fn transfers(fake: &FakeRsync) -> Vec<Vec<String>> {
    fake.calls()
        .into_iter()
        .filter(|call| call[0] == "transfer")
        .collect()
}

fn pushing() -> (Collection, FakeRsync, Target) {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);
    (collection, fake, target)
}

#[test]
fn deletions_are_followed_by_a_dry_run_and_a_quiet_one_ends_the_push() {
    let (collection, fake, _target) = pushing();
    fake.answer(1, "*deleting   projects/gone.txt\n", "", 0);
    fake.answer(2, "", "", 0);

    let json = push(&collection, &[]).json();

    let calls = transfers(&fake);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(!calls[0].contains(&"-n".to_string()));
    assert!(calls[1].contains(&"-n".to_string()));
    assert_eq!(json["second_pass"], false);
    assert_eq!(json["pieces"][0]["deleted"], 1);
}

#[test]
fn a_dry_run_that_finds_more_is_followed_by_the_mirror_and_counts_both() {
    let (collection, fake, _target) = pushing();
    fake.answer(1, "*deleting   projects/Gone.txt\n", "", 0);
    fake.answer(2, ">f+++++++++ projects/gone.txt\n", "", 0);
    fake.answer(3, ">f+++++++++ projects/gone.txt\n", "", 0);

    let json = push(&collection, &[]).json();

    let calls = transfers(&fake);
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(!calls[2].contains(&"-n".to_string()));
    assert_eq!(json["second_pass"], true);
    assert_eq!(json["pieces"][0]["deleted"], 1);
    assert_eq!(json["pieces"][0]["created"], 1);
}

#[test]
fn a_failing_second_mirror_is_exit_1_and_no_deletion_means_one_call() {
    let (collection, fake, _target) = pushing();
    fake.answer(1, "*deleting   projects/gone.txt\n", "", 0);
    fake.answer(2, ">f+++++++++ projects/x\n", "", 0);
    fake.answer(3, "", "rsync: write failed\n", 11);

    let run = push(&collection, &[]);
    refused_message(&run);
    assert!(
        said_text(&run).contains("write failed"),
        "{}",
        said_text(&run)
    );

    let (collection, fake, _target) = pushing();
    fake.answer_all(">f+++++++++ projects/x\n", "", 0);
    goes_on(&push(&collection, &[]));
    assert_eq!(transfers(&fake).len(), 1);
}

#[test]
fn a_dry_run_of_the_push_runs_no_second_pass() {
    let (collection, fake, _target) = pushing();
    fake.answer_all("*deleting   projects/gone.txt\n", "", 0);

    let json = push(&collection, &["-n"]).json();

    assert_eq!(transfers(&fake).len(), 1);
    assert_eq!(json["actions"][0]["pieces"][0]["deleted"], 1);
}

// =========================================================
// The real rsync
// =========================================================

fn pushed(run: &Run) {
    assert_eq!(run.code(), 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(run.json()["result"], "pushed", "{}", run.stdout);
}

#[test]
fn init_then_push_copies_the_collection_and_leaves_antiqs_scratch_behind() {
    let target = Target::new();
    let collection = with_real_rsync(&target.path());
    fs::create_dir_all(collection.path("projects/a/node_modules/dep")).expect("made");
    fs::write(collection.path("projects/a/node_modules/dep/i.js"), "js").expect("written");
    fs::write(collection.path(".antiq/tmp/scratch"), "x").expect("written");
    fs::create_dir_all(collection.path(".antiq/trash")).expect("made");
    fs::create_dir_all(collection.path(".antiq/doomed")).expect("made");

    pushed(&push(&collection, &["--init"]));

    let root = target.path();
    assert_eq!(
        fs::read(root.join("projects/a/a.txt")).expect("copied"),
        b"a"
    );
    assert!(root.join("projects/a/node_modules/dep/i.js").exists());
    assert!(root.join(".antiq/config.toml").exists());
    assert_eq!(
        fs::read(root.join(".antiq-target")).expect("the marker"),
        expected_marker(&collection)
    );
    for absent in [".antiq/tmp", ".antiq/trash", ".antiq/doomed", ".antiq/lock"] {
        assert!(!root.join(absent).exists(), "{absent} is not mirrored");
    }
}

#[test]
fn what_is_deleted_locally_is_deleted_on_the_target_and_the_marker_stays() {
    let target = Target::new();
    let collection = with_real_rsync(&target.path());
    fs::write(collection.path("projects/a/second.txt"), "2").expect("written");
    pushed(&push(&collection, &["--init"]));
    let marker = fs::read(target.path().join(".antiq-target")).expect("the marker");
    fs::remove_file(collection.path("projects/a/second.txt")).expect("removed");

    let json = push(&collection, &[]).json();

    assert_eq!(json["result"], "pushed", "{json}");
    assert_eq!(json["pieces"][0]["deleted"], 1);
    assert!(!target.path().join("projects/a/second.txt").exists());
    assert!(target.path().join("projects/a/a.txt").exists());
    assert_eq!(
        fs::read(target.path().join(".antiq-target")).expect("kept"),
        marker
    );
}

#[test]
fn hard_links_arrive_as_hard_links_and_read_only_folders_arrive() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let target = Target::new();
    let collection = with_real_rsync(&target.path());
    fs::hard_link(
        collection.path("projects/a/a.txt"),
        collection.path("projects/a/linked.txt"),
    )
    .expect("linked");
    fs::create_dir_all(collection.path("projects/a/frozen")).expect("made");
    fs::write(collection.path("projects/a/frozen/f.txt"), "f").expect("written");
    fs::set_permissions(
        collection.path("projects/a/frozen"),
        fs::Permissions::from_mode(0o555),
    )
    .expect("frozen");

    let run = push(&collection, &["--init"]);

    fs::set_permissions(
        collection.path("projects/a/frozen"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("thawed");
    fs::set_permissions(
        target.path().join("projects/a/frozen"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("thawed on the target");
    pushed(&run);
    let a = fs::metadata(target.path().join("projects/a/a.txt")).expect("copied");
    let b = fs::metadata(target.path().join("projects/a/linked.txt")).expect("copied");
    assert_eq!(a.ino(), b.ino());
    assert!(target.path().join("projects/a/frozen/f.txt").exists());
}

/// Whether the volume of `dir` treats `lower` and `LOWER` as one name.
fn folds_case(dir: &Path) -> bool {
    let probe = dir.join("antiq-probe-case");
    fs::write(&probe, "").expect("the probe is written");
    let folds = dir.join("ANTIQ-PROBE-CASE").exists();
    fs::remove_file(&probe).expect("the probe is removed");
    folds
}

/// Pushes `Name.txt`, renames it to `name.txt` and pushes again. Returns the
/// JSON of the second push and the names the target lists afterwards.
fn push_a_case_only_rename(target: &Target) -> (serde_json::Value, Vec<String>) {
    let collection = with_real_rsync(&target.path());
    fs::write(collection.path("projects/a/Name.txt"), "n").expect("written");
    pushed(&push(&collection, &["--init"]));
    fs::rename(
        collection.path("projects/a/Name.txt"),
        collection.path("projects/a/name.txt"),
    )
    .expect("renamed");

    let json = push(&collection, &[]).json();

    let mut names: Vec<String> = fs::read_dir(target.path().join("projects/a"))
        .expect("the piece folder arrived")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    (json, names)
}

#[test]
fn a_name_that_changes_only_in_case_goes_through_the_second_pass_on_a_volume_that_folds() {
    let target = Target::new();
    if !folds_case(&target.path()) {
        eprintln!("skipped: the volume of the target is case-sensitive");
        return;
    }

    let (json, names) = push_a_case_only_rename(&target);

    assert_eq!(json["result"], "pushed", "{json}");
    assert_eq!(json["second_pass"], true, "{json}");
    assert_eq!(names, ["a.txt", "name.txt"]);
    assert_eq!(
        fs::read(target.path().join("projects/a/name.txt")).expect("restored"),
        b"n"
    );
}

#[test]
fn a_name_that_changes_only_in_case_needs_no_second_pass_on_a_volume_that_keeps_case() {
    let target = Target::new();
    if folds_case(&target.path()) {
        eprintln!("skipped: the volume of the target folds case");
        return;
    }

    let (json, names) = push_a_case_only_rename(&target);

    assert_eq!(json["result"], "pushed", "{json}");
    assert_eq!(json["second_pass"], false, "{json}");
    assert_eq!(names, ["a.txt", "name.txt"]);
}

/// A collection that pushes through the ssh stand-in to the directory
/// `target`, with the real rsync on both ends.
fn over_ssh(target: &Path, shim: &SshShim) -> Collection {
    let collection = collection();
    set_url(&collection, &format!("shimhost:{}", target.display()));
    let rsync = real_rsync();
    let rsync = serde_json::to_string(rsync.to_str().expect("UTF-8")).expect("quotes");
    let shell = serde_json::to_string(shim.path().to_str().expect("UTF-8")).expect("quotes");
    add_target_key(&collection, &format!("rsync = {rsync}"));
    add_target_key(&collection, &format!("rsync_path = {rsync}"));
    add_target_key(&collection, &format!("rsync_args = [\"-e\", {shell}]"));
    collection
}

#[test]
fn an_ssh_target_is_initialised_pushed_to_and_checked_against_its_marker() {
    let target = Target::new();
    let shim = SshShim::new();
    let collection = over_ssh(&target.path(), &shim);

    let listed = push(&collection, &[]);
    unreachable_reason(&listed);
    pushed(&push(&collection, &["--init"]));
    assert_eq!(
        fs::read(target.path().join(".antiq-target")).expect("the marker"),
        expected_marker(&collection)
    );
    assert!(target.path().join("projects/a/a.txt").exists());

    pushed(&push(&collection, &[]));
    assert!(!shim.calls().is_empty());

    target.marker_with(&collection.id(), "Some Other Mac", "/elsewhere");
    let message = refused_message(&push(&collection, &[]));
    assert!(message.contains("Some Other Mac"), "{message}");
    pushed(&push(&collection, &["--force"]));
    assert!(tmp_entries(&collection).is_empty());
}

#[test]
fn an_ssh_target_that_refuses_the_connection_is_unreachable() {
    let target = Target::new();
    let shim = SshShim::new();
    shim.fault("connect-refused");
    let collection = over_ssh(&target.path(), &shim);

    let reason = unreachable_reason(&push(&collection, &[]));

    assert!(reason.contains("Connection refused"), "{reason}");
    let again = push(&collection, &["--if-reachable"]);
    assert_eq!(again.code(), 0, "{} {}", again.stdout, again.stderr);
}

#[test]
fn a_claim_that_sends_nothing_has_the_headline_and_the_marker_sentence_but_no_table() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());
    let antiq = Antiq::new()
        .env("TZ", "UTC")
        .env("NO_COLOR", "1")
        .arg("-C")
        .arg(collection.root())
        .args(["push", "--init"]);
    let (run, stdout) = run_on_terminal(antiq);
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    assert!(stdout.starts_with("✔ Claimed the push target "), "{stdout}");
    assert!(!stdout.contains("CREATED"), "{stdout}");
}

// =========================================================
// `--take-over`
// =========================================================

const MOVED_HOST: &str = "Some Other Mac";
const MOVED_PATH: &str = "/Users/jakob/playground-antiq";

/// A collection pushing to `target` with the real rsync, whose marker names
/// the same collection on another machine and at another place.
fn moved_collection(target: &Target) -> Collection {
    let collection = with_real_rsync(&target.path());
    target.marker_with(&collection.id(), MOVED_HOST, MOVED_PATH);
    collection
}

fn marker_bytes(target: &Target) -> Vec<u8> {
    fs::read(target.path().join(".antiq-target")).expect("the marker exists")
}

/// How many marker runs a fake rsync saw.
fn marker_sends(fake: &FakeRsync) -> usize {
    fake.calls()
        .iter()
        .filter(|call| call.contains(&"--chmod=F644".to_string()))
        .count()
}

#[test]
fn take_over_rewrites_the_marker_of_this_collection_and_pushes_on() {
    let target = Target::new();
    let collection = moved_collection(&target);

    let run = push(&collection, &["--take-over"]);

    goes_on(&run);
    assert_eq!(marker_bytes(&target), expected_marker(&collection));
    let json = run.json();
    assert_eq!(json["took_over"], true, "{json}");
    assert_eq!(json["initialized"], false, "{json}");
    assert_eq!(json["forced"], false, "{json}");
    let notes = json["notes"].to_string();
    assert!(
        notes.contains(MOVED_HOST) && notes.contains(MOVED_PATH),
        "{notes}"
    );
    let own_host = support::cmd::machine_name();
    let own_path = collection.root().to_str().expect("UTF-8").to_string();
    assert!(
        notes.contains(&own_host) && notes.contains(&own_path),
        "{notes}"
    );
    assert_eq!(tmp_entries(&collection), Vec::<std::ffi::OsString>::new());
}

#[test]
fn take_over_in_text_prints_the_note_on_stderr_and_no_claim() {
    let target = Target::new();
    let collection = moved_collection(&target);

    let run = push_text(&collection, &["--take-over"]);

    assert_eq!(run.code(), 0, "{} {}", run.stdout, run.stderr);
    assert!(run.stderr.contains("note: "), "{}", run.stderr);
    assert!(run.stderr.contains(MOVED_HOST), "{}", run.stderr);
    assert!(!run.stdout.contains("Claimed"), "{}", run.stdout);
    assert!(!run.stderr.contains("Claimed"), "{}", run.stderr);
}

#[test]
fn take_over_with_only_another_host_names_only_the_host() {
    let target = Target::new();
    let collection = with_real_rsync(&target.path());
    let own_path = collection.root().to_str().expect("UTF-8").to_string();
    target.marker_with(&collection.id(), MOVED_HOST, &own_path);

    let run = push(&collection, &["--take-over"]);

    goes_on(&run);
    assert_eq!(marker_bytes(&target), expected_marker(&collection));
    let notes = run.json()["notes"].to_string();
    assert!(notes.contains(MOVED_HOST), "{notes}");
    assert!(!notes.contains("path"), "{notes}");
}

#[test]
fn take_over_with_only_another_path_names_only_the_path() {
    let target = Target::new();
    let collection = with_real_rsync(&target.path());
    target.marker_with(&collection.id(), &support::cmd::machine_name(), MOVED_PATH);

    let run = push(&collection, &["--take-over"]);

    goes_on(&run);
    assert_eq!(marker_bytes(&target), expected_marker(&collection));
    let notes = run.json()["notes"].to_string();
    assert!(notes.contains(MOVED_PATH), "{notes}");
    assert!(!notes.contains("host"), "{notes}");
}

#[test]
fn take_over_rewrites_a_marker_that_a_size_only_check_would_skip() {
    let target = Target::new();
    let collection = with_real_rsync(&target.path());
    add_target_key(&collection, "rsync_args = [\"--size-only\"]");
    let own_path = collection.root().to_str().expect("UTF-8").to_string();
    // Same size as the new marker: the old host has the length of this
    // machine's name.
    let old_host = "x".repeat(support::cmd::machine_name().len());
    target.marker_with(&collection.id(), &old_host, &own_path);
    assert_eq!(
        marker_bytes(&target).len(),
        expected_marker(&collection).len()
    );

    let run = push(&collection, &["--take-over"]);

    goes_on(&run);
    assert_eq!(marker_bytes(&target), expected_marker(&collection));
}

#[test]
fn take_over_on_a_marker_of_another_collection_is_refused_as_without_it() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker_with(
        "01BX5ZZKBKACTAV9WEVGEMMVRY",
        &support::cmd::machine_name(),
        "/elsewhere",
    );
    let before = marker_bytes(&target);

    let without = refused_message(&push(&collection, &[]));
    let with = refused_message(&push(&collection, &["--take-over"]));

    assert_eq!(without, with);
    assert_eq!(marker_bytes(&target), before);
    assert_eq!(marker_sends(&fake), 0);
}

#[test]
fn take_over_on_a_matching_marker_is_a_plain_push_without_a_note() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);

    let run = push(&collection, &["--take-over"]);

    passes_the_precheck(&run);
    let json = run.json();
    assert_eq!(json["notes"], serde_json::json!([]), "{json}");
    assert_eq!(json["took_over"], false, "{json}");
    assert_eq!(marker_sends(&fake), 0);
}

#[test]
fn take_over_does_not_claim_a_missing_path_target() {
    let gone = Target::new();
    let (collection, fake) = targeted(&gone.path().join("not-there"));

    let reason = unreachable_reason(&push(&collection, &["--take-over"]));

    assert!(reason.contains("not-there"), "{reason}");
    assert_eq!(marker_sends(&fake), 0);
}

#[test]
fn take_over_does_not_claim_an_empty_directory() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());

    let reason = unreachable_reason(&push(&collection, &["--take-over"]));

    assert!(reason.contains("push --init"), "{reason}");
    assert_eq!(marker_sends(&fake), 0);
}

#[test]
fn take_over_does_not_claim_a_directory_with_files_and_no_marker() {
    let target = Target::new();
    fs::write(target.path().join("file"), "x").expect("written");
    let (collection, fake) = targeted(&target.path());

    let message = refused_message(&push(&collection, &["--take-over"]));

    assert!(message.contains("not empty"), "{message}");
    assert_eq!(marker_sends(&fake), 0);
}

#[test]
fn take_over_does_not_replace_a_marker_that_cannot_be_read() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    fs::write(target.path().join(".antiq-target"), "not json").expect("written");

    let message = refused_message(&push(&collection, &["--take-over"]));

    assert!(message.contains("not readable"), "{message}");
    assert_eq!(marker_bytes(&target), b"not json");
    assert_eq!(marker_sends(&fake), 0);
}

#[test]
fn take_over_does_not_replace_a_marker_of_a_newer_format() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    let text = format!(
        "{{\"format\":2,\"id\":\"{}\",\"host\":\"h\",\"path\":\"/p\"}}\n",
        collection.id()
    );
    fs::write(target.path().join(".antiq-target"), &text).expect("written");

    let run = push(&collection, &["--take-over"]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert_eq!(run.error_kind(), "format_too_new");
    assert_eq!(marker_bytes(&target), text.as_bytes());
    assert_eq!(marker_sends(&fake), 0);
}

#[test]
fn take_over_is_a_usage_error_with_init() {
    let run = push_text(&collection(), &["--take-over", "--init"]);
    assert_eq!(run.code(), 2, "{}", run.stderr);
    assert!(run.stderr.contains("cannot be used with"), "{}", run.stderr);
    assert!(run.stderr.contains("--take-over"), "{}", run.stderr);
}

#[test]
fn take_over_is_a_usage_error_with_force() {
    let run = push_text(&collection(), &["--take-over", "--force"]);
    assert_eq!(run.code(), 2, "{}", run.stderr);
    assert!(run.stderr.contains("cannot be used with"), "{}", run.stderr);
    assert!(run.stderr.contains("--take-over"), "{}", run.stderr);
}

#[test]
fn take_over_is_a_usage_error_with_if_reachable() {
    let run = push_text(&collection(), &["--take-over", "--if-reachable"]);
    assert_eq!(run.code(), 2, "{}", run.stderr);
    assert!(run.stderr.contains("cannot be used with"), "{}", run.stderr);
    assert!(run.stderr.contains("--take-over"), "{}", run.stderr);
}

#[test]
fn a_dry_run_of_a_take_over_lists_it_and_writes_nothing() {
    let target = Target::new();
    let collection = moved_collection(&target);
    let before = marker_bytes(&target);

    let text = push_text(&collection, &["--take-over", "-n"]);

    assert_eq!(text.code(), 0, "{} {}", text.stdout, text.stderr);
    let own_host = support::cmd::machine_name();
    let own_path = collection.root().to_str().expect("UTF-8").to_string();
    let expected = format!(
        "Would take over the target: host {MOVED_HOST} -> {own_host}, path {MOVED_PATH} -> {own_path}"
    );
    assert!(text.stdout.contains(&expected), "{}", text.stdout);
    assert!(!text.stderr.contains("note:"), "{}", text.stderr);
    assert_eq!(marker_bytes(&target), before);

    let json = push(&collection, &["--take-over", "-n"]).json();
    let kinds: Vec<&str> = json["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|action| action["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(kinds, ["take_over", "mirror"], "{json}");
    assert_eq!(marker_bytes(&target), before);
}

#[test]
fn the_refusal_of_a_moved_collection_names_take_over_and_force() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target.path());
    target.marker_with(&collection.id(), MOVED_HOST, MOVED_PATH);

    let run = push(&collection, &[]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    let fix = run.json()["error"]["fix"].to_string();
    assert!(fix.contains("--take-over"), "{fix}");
    assert!(fix.contains("--force"), "{fix}");
}

#[test]
fn a_push_that_fails_after_a_take_over_has_sent_the_marker_first() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker_with(&collection.id(), MOVED_HOST, MOVED_PATH);
    fake.answer(
        2,
        "",
        "rsync error: some files could not be transferred\n",
        23,
    );

    let run = push(&collection, &["--take-over"]);

    assert_eq!(run.code(), 1, "{}", run.stdout);
    let calls = fake.calls();
    let send = calls
        .iter()
        .position(|call| call.contains(&"--chmod=F644".to_string()))
        .expect("the marker was sent");
    let mirror = calls
        .iter()
        .position(|call| call.contains(&"--delete".to_string()))
        .expect("the mirror ran");
    assert!(send < mirror, "{calls:?}");
}

#[test]
fn every_marker_run_compares_checksums() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());

    goes_on(&push(&collection, &["--init"]));

    let calls = fake.calls();
    let send = calls
        .iter()
        .find(|call| call.contains(&"--chmod=F644".to_string()))
        .expect("the marker was sent");
    assert!(send.contains(&"--checksum".to_string()), "{send:?}");
}
