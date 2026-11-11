// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq import` seen from outside the binary: a folder outside the
//! collection becomes a version, and the folder stays.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection};
use support::tree::Tree;

/// 2023-11-14T22:13:20Z.
const OLD: i64 = 1_700_000_000;
const OLD_STAMP: &str = "2023-11-14T221320";

fn run_in(
    collection: &Collection,
    zone: &str,
    global: &[&str],
    args: &[&dyn AsRef<std::ffi::OsStr>],
) -> Run {
    let mut command = Antiq::new()
        .env("TZ", zone)
        .arg("-C")
        .arg(collection.root())
        .args(global)
        .arg("import");
    for arg in args {
        command = command.arg(arg.as_ref());
    }
    command.run()
}

fn import(collection: &Collection, global: &[&str], args: &[&dyn AsRef<std::ffi::OsStr>]) -> Run {
    run_in(collection, "UTC", global, args)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(read) => read
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

fn world() -> Collection {
    Collection::build(&Clock::now())
        .patterns(&["*"])
        .idle_days(30)
        .ignore(&["node_modules/"])
        .piece("existing", |piece| {
            piece.age_days(40).file("e.txt", "old");
        })
        .finish()
}

/// A folder `data` with two files, outside any collection.
fn source() -> Tree {
    let mut tree = Tree::new();
    tree.file_with("data/a.txt", "alpha", 0o644, OLD - 100);
    tree.file_with("data/sub/b.txt", "beta", 0o644, OLD);
    tree
}

fn listing_text(collection: &Collection, piece: &str) -> String {
    let dir = collection.path(&format!(".antiq/pieces/{piece}"));
    let listing = names(&dir)
        .into_iter()
        .find(|name| name.ends_with(".sha256.zst"))
        .expect("a listing file");
    let file = fs::File::open(dir.join(listing)).expect("the listing opens");
    let text = antiq::listing::decode_all(file).expect("the listing decodes");
    String::from_utf8_lossy(&text).into_owned()
}

fn assert_refused(run: &Run, needle: &str) {
    assert_eq!(
        run.code(),
        1,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "other", "{}", run.stdout);
    let message = run.error_message();
    assert!(message.contains(needle), "{needle:?} not in {message:?}");
}

fn assert_nothing_stored(collection: &Collection, piece: &str) {
    assert!(
        names(&collection.path(&format!(".antiq/pieces/{piece}"))).is_empty(),
        "{piece} has files in the store"
    );
}

#[test]
fn a_folder_outside_becomes_a_version_of_its_name_and_the_source_stays() {
    let collection = world();
    let tree = source();
    let before = snapshot(tree.root());

    let run = import(&collection, &["--json"], &[&tree.path("data")]);

    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["command"], "import");
    assert_eq!(json["imported"]["piece"], "data");
    assert_eq!(json["imported"]["stamp"], OLD_STAMP);
    assert_eq!(json["imported"]["files"], 2);
    assert_eq!(json["skipped"], serde_json::Value::Null);
    assert_eq!(names(&collection.path(".antiq/pieces/data")).len(), 3);
    assert_eq!(snapshot(tree.root()), before);
}

#[test]
fn a_folder_name_with_a_newline_cannot_forge_a_line_of_the_result_of_an_import() {
    let collection = world();
    let mut tree = Tree::new();
    tree.file_with("data\nimported fake/a.txt", "alpha", 0o644, OLD);

    let run = import(&collection, &[], &[&tree.path("data\nimported fake")]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    assert_eq!(
        run.stdout.lines().collect::<Vec<_>>(),
        [format!(
            "imported data%0Aimported fake: version {OLD_STAMP}"
        )]
    );
}

#[test]
fn the_text_output_is_one_line() {
    let collection = world();
    let tree = source();

    let run = import(&collection, &[], &[&tree.path("data")]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    assert_eq!(
        run.stdout.lines().collect::<Vec<_>>(),
        [format!("imported data: version {OLD_STAMP}")]
    );
}

#[test]
fn the_version_time_is_the_newest_counted_file_and_the_listing_says_imported() {
    let collection = world();
    let tree = source();

    let run = import(&collection, &[], &[&tree.path("data")]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    let text = listing_text(&collection, "data");
    assert!(
        text.contains("# archived      2023-11-14T22:13:20Z"),
        "{text}"
    );
    let imported = text
        .lines()
        .find(|line| line.starts_with("# imported"))
        .expect("an imported line");
    assert!(
        !imported.contains("2023"),
        "the real time of the import: {imported}"
    );
}

#[test]
fn as_stores_the_version_under_the_given_path() {
    let collection = world();
    let tree = source();

    let run = import(
        &collection,
        &["--json"],
        &[&tree.path("data"), &"--as", &"renamed"],
    );

    assert_eq!(run.code(), 0, "{}", run.stderr);
    assert_eq!(run.json()["imported"]["piece"], "renamed");
    assert_eq!(names(&collection.path(".antiq/pieces/renamed")).len(), 3);
}

#[test]
fn a_path_the_patterns_do_not_select_is_refused() {
    let collection = Collection::build(&Clock::now())
        .patterns(&["projects/*"])
        .finish();
    let tree = source();

    let named = import(
        &collection,
        &["--json"],
        &[&tree.path("data"), &"--as", &"other"],
    );
    let default = import(&collection, &["--json"], &[&tree.path("data")]);

    assert_refused(&named, "do not select");
    assert_refused(&default, "do not select");
    assert_nothing_stored(&collection, "other");
}

#[test]
fn paths_that_cannot_be_pieces_are_refused() {
    let collection = Collection::build(&Clock::now())
        .patterns(&["*", "*/*"])
        .exclude(&["skipped"])
        .piece("outer", |piece| {
            piece.age_days(1).file("f", "x");
        })
        .finish();
    let tree = source();
    let folder = tree.path("data");

    let inside_antiq = import(&collection, &["--json"], &[&folder, &"--as", &".antiq/x"]);
    let excluded = import(&collection, &["--json"], &[&folder, &"--as", &"skipped"]);
    let in_piece = import(&collection, &["--json"], &[&folder, &"--as", &"outer/sub"]);

    assert_refused(&inside_antiq, ".antiq");
    assert_refused(&excluded, "excluded");
    assert_refused(&in_piece, "inside the piece");
}

#[test]
fn an_as_path_outside_the_collection_has_its_own_kind() {
    let collection = world();
    let tree = source();

    let run = import(
        &collection,
        &["--json"],
        &[&tree.path("data"), &"--as", &"../elsewhere"],
    );

    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "outside_collection");
}

#[test]
fn a_source_inside_the_collection_is_refused() {
    let collection = world();

    let piece = import(&collection, &["--json"], &[&collection.path("existing")]);
    let antiq = import(&collection, &["--json"], &[&collection.path(".antiq/tmp")]);
    let dots = import(
        &collection,
        &["--json"],
        &[&collection.path("existing/../existing")],
    );

    for run in [&piece, &antiq, &dots] {
        assert_refused(run, "inside the collection");
    }
    assert!(collection.path("existing/e.txt").exists());
}

#[test]
fn a_pin_at_the_top_refuses_and_a_restored_marker_is_left_out() {
    let collection = world();
    let mut tree = source();
    tree.file("pinned/.antiq-pin", "");
    tree.file_with("pinned/f", "x", 0o644, OLD);
    tree.file_with("restored/f", "x", 0o644, OLD);
    tree.file(
        "restored/.antiq-restored",
        "restored 2099-01-01T00:00:00Z\n",
    );

    let pinned = import(&collection, &["--json"], &[&tree.path("pinned")]);
    let restored = import(&collection, &["--json"], &[&tree.path("restored")]);

    assert_refused(&pinned, "pinned");
    assert_eq!(restored.code(), 0, "{}", restored.stdout);
    assert_eq!(restored.json()["imported"]["stamp"], OLD_STAMP);
    assert!(!listing_text(&collection, "restored").contains("antiq-restored"));
}

#[test]
fn a_pin_that_a_rule_ignores_and_a_pin_that_is_a_directory_still_refuse() {
    let collection = world();
    let mut tree = Tree::new();
    tree.file_with("ignored/f", "x", 0o644, OLD);
    tree.file("ignored/.antiq-pin", "");
    tree.file("ignored/.antiqignore", ".antiq-pin\n");
    tree.file_with("dir/f", "x", 0o644, OLD);
    tree.dir("dir/.antiq-pin");

    let ignored = import(&collection, &["--json"], &[&tree.path("ignored")]);
    let directory = import(&collection, &["--json"], &[&tree.path("dir")]);

    assert_refused(&ignored, "pinned");
    assert_refused(&directory, "not a file");
}

#[test]
fn a_source_without_a_counted_file_is_refused_also_with_a_time() {
    let collection = world();
    let mut tree = Tree::new();
    tree.dir("dirs/inner");
    tree.symlink("links/l", "nowhere");
    tree.file("ignored/node_modules/x.js", "x");
    tree.file("marker/.antiq-restored", "restored 2020-01-01T00:00:00Z\n");
    tree.file_with("future/node_modules/x.js", "x", 0o644, OLD);

    for name in ["dirs", "links", "ignored", "marker"] {
        let run = import(&collection, &["--json"], &[&tree.path(name)]);
        assert_refused(&run, "empty");
        let timed = import(
            &collection,
            &["--json"],
            &[&tree.path(name), &"--time", &"2020-01-01T000000"],
        );
        assert_refused(&timed, "empty");
    }
    let future = import(
        &collection,
        &["--json"],
        &[&tree.path("future"), &"--time", &"2999-01-01T000000"],
    );
    assert_refused(&future, "empty");
}

#[test]
fn a_file_dated_in_the_future_refuses_naming_it() {
    let collection = world();
    let mut tree = Tree::new();
    let next_year = jiff::Timestamp::now().as_second() + 365 * 86_400;
    tree.file_with("data/late.txt", "x", 0o644, next_year);
    tree.file_with("data/early.txt", "x", 0o644, OLD);

    let run = import(&collection, &["--json"], &[&tree.path("data")]);
    let past = import(
        &collection,
        &["--json"],
        &[&tree.path("data"), &"--time", &"2020-01-01T000000"],
    );

    assert_refused(&run, "late.txt");
    assert_eq!(past.code(), 0, "{}", past.stdout);
    assert_eq!(past.json()["imported"]["stamp"], "2020-01-01T000000");
}

#[test]
fn a_time_in_the_future_refuses() {
    let collection = world();
    let tree = source();

    let run = import(
        &collection,
        &["--json"],
        &[&tree.path("data"), &"--time", &"2999-01-01T000000"],
    );

    assert_refused(&run, "2999-01-01");
}

#[test]
fn time_sets_the_stamp_in_utc_or_in_the_local_zone() {
    let collection = world();
    let tree = source();

    let utc = import(
        &collection,
        &["--json"],
        &[
            &tree.path("data"),
            &"--as",
            &"u",
            &"--time",
            &"2020-05-06T070809",
        ],
    );
    let local = run_in(
        &collection,
        "Europe/Berlin",
        &["--json"],
        &[
            &tree.path("data"),
            &"--as",
            &"l",
            &"--time",
            &"2020-05-06 07:08:09",
        ],
    );

    assert_eq!(utc.json()["imported"]["stamp"], "2020-05-06T070809");
    assert_eq!(local.json()["imported"]["stamp"], "2020-05-06T050809");
}

#[test]
fn a_time_that_is_no_exact_time_is_a_usage_error() {
    let collection = world();
    let tree = source();
    let bad = [
        "2020-05-06",
        "2020-05-06T07",
        "2020-05-06T070809-2",
        "garbage",
        "",
    ];

    for time in bad {
        let run = import(
            &collection,
            &["--json"],
            &[&tree.path("data"), &"--time", &time],
        );
        assert_eq!(run.code(), 2, "{time:?}: {}", run.stdout);
        assert_eq!(run.error_kind(), "usage", "{time:?}");
    }
    let repeated_hour = run_in(
        &collection,
        "Europe/Berlin",
        &["--json"],
        &[&tree.path("data"), &"--time", &"2020-10-25 02:30:00"],
    );
    let skipped_hour = run_in(
        &collection,
        "Europe/Berlin",
        &["--json"],
        &[&tree.path("data"), &"--time", &"2020-03-29 02:30:00"],
    );
    assert_eq!(repeated_hour.code(), 2);
    assert_eq!(skipped_hour.code(), 2);
    assert_nothing_stored(&collection, "data");
}

#[test]
fn the_rules_of_the_collection_and_of_the_folder_apply() {
    let collection = world();
    let mut tree = source();
    tree.file_with("data/node_modules/dep.js", "x", 0o644, OLD);
    tree.file_with("data/skip/c.txt", "x", 0o644, OLD);
    tree.file("data/.antiqignore", "skip/\n");

    let run = import(&collection, &["--json"], &[&tree.path("data")]);

    assert_eq!(run.code(), 0, "{}", run.stdout);
    let text = listing_text(&collection, "data");
    assert!(text.contains("a.txt"), "{text}");
    assert!(!text.contains("node_modules"), "{text}");
    assert!(!text.contains("skip"), "{text}");
    assert!(tree.path("data/node_modules/dep.js").exists());
}

#[test]
fn a_nested_collection_in_the_source_refuses() {
    let collection = world();
    let mut tree = source();
    tree.dir("data/sub/.antiq");

    let run = import(&collection, &["--json"], &[&tree.path("data")]);

    assert_refused(&run, "nested collection");
    assert_nothing_stored(&collection, "data");
}

#[test]
fn a_pinned_live_folder_at_the_path_does_not_block_the_import() {
    let collection = Collection::build(&Clock::now())
        .patterns(&["*"])
        .piece("data", |piece| {
            piece.age_days(1).file("live.txt", "x").pinned();
        })
        .finish();
    let tree = source();

    let run = import(&collection, &["--json"], &[&tree.path("data")]);

    assert_eq!(run.code(), 0, "{}", run.stdout);
    assert!(collection.path("data/live.txt").exists());
    assert_eq!(names(&collection.path(".antiq/pieces/data")).len(), 3);
}

#[test]
fn a_second_import_in_the_same_second_gets_a_counter() {
    let collection = world();
    let tree = source();

    let first = import(&collection, &["--json"], &[&tree.path("data")]);
    let second = import(&collection, &["--json"], &[&tree.path("data")]);

    assert_eq!(first.json()["imported"]["stamp"], OLD_STAMP);
    assert_eq!(second.json()["imported"]["stamp"], format!("{OLD_STAMP}-2"));
}

#[test]
fn a_symlink_as_the_source_is_refused() {
    let collection = world();
    let mut tree = source();
    let directory = tree.path("data");
    let file = tree.path("data/a.txt");
    tree.symlink("to-dir", directory);
    tree.symlink("dangling", "nowhere");
    tree.symlink("to-file", file);

    for name in ["to-dir", "dangling", "to-file"] {
        let run = import(&collection, &["--json"], &[&tree.path(name)]);
        assert_refused(&run, "not a folder");
    }
}

#[test]
fn a_relative_folder_and_a_relative_as_start_at_the_directory_of_dash_c() {
    let tree = source();
    let collection = world();
    let relative: PathBuf = {
        let mut up = PathBuf::new();
        for _ in collection.root().components().skip(1) {
            up.push("..");
        }
        up.join(tree.path("data").strip_prefix("/").expect("absolute"))
    };

    let run = import(
        &collection,
        &["--json"],
        &[&relative, &"--as", &"./nested-name"],
    );

    assert_eq!(run.code(), 0, "{}", run.stdout);
    assert_eq!(run.json()["imported"]["piece"], "nested-name");
}

#[test]
fn a_file_name_with_a_newline_cannot_forge_a_line_of_an_import_dry_run() {
    let collection = Collection::build(&Clock::now())
        .patterns(&["*"])
        .idle_days(30)
        .ignore(&["evil*"])
        .finish();
    let mut tree = source();
    tree.file_with("data/evil\nWould import fake", "x", 0o644, OLD);

    let run = import(&collection, &[], &[&tree.path("data"), &"-n"]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1], "Would leave out evil%0AWould import fake");
}

#[test]
fn a_folder_name_with_an_escape_character_is_shown_escaped_in_an_import_dry_run() {
    let collection = world();
    let mut tree = Tree::new();
    tree.file_with("da\u{1b}[31mta/a.txt", "alpha", 0o644, OLD);

    let run = import(&collection, &[], &[&tree.path("da\u{1b}[31mta"), &"-n"]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    assert!(!run.stdout.contains('\u{1b}'), "{:?}", run.stdout);
    assert!(run.stdout.contains("da%1B[31mta"), "{:?}", run.stdout);
}

#[test]
fn a_dry_run_names_the_version_and_what_it_leaves_out_and_changes_nothing() {
    let collection = world();
    let mut tree = source();
    tree.file_with("data/node_modules/dep.js", "x", 0o644, OLD);
    tree.file("data/.antiq-restored", "restored 2020-01-01T00:00:00Z\n");
    fs::create_dir_all(collection.path(".antiq/doomed/2026-11-03T000000/old")).expect("leftover");
    fs::write(
        collection.path(".antiq/doomed/2026-11-03T000000/old/f"),
        "x",
    )
    .expect("a file");
    let before = (snapshot(&collection.root()), snapshot(tree.root()));

    let text = import(&collection, &[], &[&tree.path("data"), &"-n"]);
    let json = import(
        &collection,
        &["--json"],
        &[&tree.path("data"), &"--dry-run"],
    );

    assert_eq!(text.code(), 0, "{}", text.stderr);
    let lines: Vec<&str> = text.stdout.lines().collect();
    assert!(
        lines[0].starts_with("Would delete .antiq/doomed/"),
        "{lines:?}"
    );
    assert_eq!(
        lines[lines.len() - 3..],
        [
            format!(
                "Would import {} as data, version {OLD_STAMP}",
                fs::canonicalize(tree.path("data"))
                    .expect("the folder exists")
                    .display()
            ),
            "Would leave out .antiq-restored".to_string(),
            "Would leave out node_modules/".to_string(),
        ]
    );
    let value = json.json();
    assert_eq!(value["dry_run"], true);
    let last = value["actions"]
        .as_array()
        .expect("actions")
        .last()
        .expect("an action")
        .clone();
    assert_eq!(last["kind"], "import");
    assert_eq!(last["piece"], "data");
    assert_eq!(last["stamp"], OLD_STAMP);
    assert_eq!(
        last["left_out"],
        serde_json::json!([".antiq-restored", "node_modules/"])
    );
    assert_eq!(
        (snapshot(&collection.root()), snapshot(tree.root())),
        before
    );
}

#[test]
fn a_dry_run_predicts_the_exit_code_of_a_refusal() {
    let collection = world();
    let mut tree = Tree::new();
    tree.dir("empty");

    let run = import(&collection, &["--json"], &[&tree.path("empty"), &"-n"]);

    assert_refused(&run, "empty");
}

#[test]
fn a_held_lock_fails_a_real_run_with_the_holder() {
    let collection = world();
    let tree = source();
    let _held = hold_lock(&collection.root());

    let json = import(&collection, &["--json"], &[&tree.path("data")]);
    let text = import(&collection, &[], &[&tree.path("data")]);

    assert_eq!(json.code(), 1, "{}", json.stdout);
    assert_eq!(json.error_kind(), "lock_held");
    let error = &json.json()["error"];
    assert_eq!(error["pid"], std::process::id());
    assert!(error["started"].is_string(), "{error}");
    assert_eq!(text.code(), 1);
    assert!(
        text.stderr.contains(&std::process::id().to_string()),
        "{}",
        text.stderr
    );
    assert_nothing_stored(&collection, "data");
}

#[test]
fn a_dry_run_with_a_held_lock_names_the_holder_and_lists_nothing() {
    let collection = world();
    let tree = source();
    fs::create_dir_all(collection.path(".antiq/doomed/2026-11-03T000000/old")).expect("leftover");
    fs::write(
        collection.path(".antiq/doomed/2026-11-03T000000/old/f"),
        "x",
    )
    .expect("a file");
    let _held = hold_lock(&collection.root());

    let json = import(&collection, &["--json"], &[&tree.path("data"), &"-n"]);
    let text = import(&collection, &[], &[&tree.path("data"), &"-n"]);

    assert_eq!(json.code(), 0, "{}", json.stderr);
    let value = json.json();
    assert_eq!(value["dry_run"], true);
    assert_eq!(value["actions"], serde_json::json!([]));
    assert_eq!(value["lock_held"]["pid"], std::process::id());
    assert_eq!(text.code(), 0);
    assert!(
        text.stdout
            .starts_with(&format!("The lock is held by pid {}", std::process::id())),
        "{}",
        text.stdout
    );
}

#[test]
fn after_an_import_tmp_is_empty_and_a_dry_run_plans_no_repair() {
    let collection = world();
    let tree = source();

    let run = import(&collection, &[], &[&tree.path("data")]);
    let dry = import(&collection, &["--json"], &[&tree.path("data"), &"-n"]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    assert_eq!(names(&collection.path(".antiq/tmp")), Vec::<String>::new());
    let actions = dry.json()["actions"].clone();
    let repairs = actions
        .as_array()
        .expect("actions")
        .iter()
        .filter(|action| action["kind"] == "repair")
        .count();
    assert_eq!(repairs, 0, "{actions}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_locked_file_in_the_source_does_not_refuse_it() {
    let collection = world();
    let mut tree = source();
    tree.immutable("data/a.txt");

    let run = import(&collection, &["--json"], &[&tree.path("data")]);

    assert_eq!(run.code(), 0, "{}", run.stdout);
}

#[test]
fn a_real_run_names_its_repairs_and_the_notes_on_the_settings_on_stderr() {
    let collection = world();
    let tree = source();
    let config = collection.path(".antiq/config.toml");
    let mut text = fs::read_to_string(&config).expect("the config exists");
    text.push_str("\nmystery = 1\n");
    fs::write(&config, text).expect("the config is written");
    fs::create_dir_all(collection.path(".antiq/doomed/2026-11-03T000000/old")).expect("leftover");
    fs::write(
        collection.path(".antiq/doomed/2026-11-03T000000/old/f"),
        "x",
    )
    .expect("a file");

    let run = import(&collection, &[], &[&tree.path("data")]);

    assert_eq!(run.code(), 0, "{}", run.stderr);
    let notes: Vec<&str> = run.stderr.lines().collect();
    assert!(notes[0].starts_with("note: repaired: "), "{notes:?}");
    assert_eq!(
        notes[notes.len() - 1],
        "warning: unknown setting `mystery` under [collection], which antiq ignores"
    );
}
