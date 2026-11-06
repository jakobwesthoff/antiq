// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq import` seen from outside the binary. The binary runs detached
//! from any terminal, so every run here is one without a question.

mod support;

use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use support::cmd::{Antiq, Run};
use support::tree::{TempCollection, Tree};

/// A collection whose root also holds the files to import.
struct World {
    temp: TempCollection,
}

impl World {
    fn new() -> World {
        World {
            temp: TempCollection::minimal(),
        }
    }

    fn root(&self) -> PathBuf {
        fs::canonicalize(self.temp.root()).expect("the root exists")
    }

    /// A zstd tar of the folder `proj` holding `files` (name, content,
    /// modification time), written to `name` in the collection's root.
    fn archive(&mut self, name: &str, files: &[(&str, &str, i64)]) -> PathBuf {
        let mut tree = Tree::new();
        for (file, content, mtime) in files {
            tree.file_with(&format!("proj/{file}"), content, 0o644, *mtime);
        }
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_dir_all("proj", tree.path("proj"))
            .expect("the folder is added");
        let tar = builder.into_inner().expect("the tar ends");
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("an encoder");
        encoder.include_checksum(true).expect("on");
        encoder.write_all(&tar).expect("writes");
        self.temp
            .tree()
            .file(name, encoder.finish().expect("the frame ends"))
    }

    fn import(&self, args: &[&str]) -> Run {
        Antiq::new()
            .arg("-C")
            .arg(self.temp.root())
            .arg("import")
            .args(args)
            .run()
    }

    fn json_import(&self, args: &[&str]) -> Run {
        Antiq::new()
            .arg("--json")
            .arg("-C")
            .arg(self.temp.root())
            .arg("import")
            .args(args)
            .run()
    }

    fn objects(&self) -> usize {
        fs::read_dir(self.temp.root().join(".antiq/objects"))
            .expect("lists")
            .count()
    }
}

/// Text with what differs between runs replaced.
fn normalised(text: &str) -> String {
    let mut shown = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let is_id = rest.len() >= 26
            && rest[..26]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
            && !rest[..26].bytes().all(|byte| byte.is_ascii_digit());
        if is_id {
            shown.push_str("<id>");
            rest = &rest[26..];
        } else {
            let next = rest.chars().next().expect("not empty");
            shown.push(next);
            rest = &rest[next.len_utf8()..];
        }
    }
    shown
}

const FILES: &[(&str, &str, i64)] = &[("a.txt", "hello", 1_000_000_000)];

// =========================================================
// Importing
// =========================================================

#[test]
fn an_archive_is_imported_and_the_console_says_where_it_went() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    let run = world.import(&["proj.tar.zst"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.stderr, "");
    insta::assert_snapshot!(normalised(&run.stdout), @"Imported proj.tar.zst as proj (<id>)
");
    assert_eq!(world.objects(), 1);
}

#[test]
fn json_lists_what_was_imported() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    let run = world.json_import(&["proj.tar.zst"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.stderr, "");
    let json = run.json();
    assert_eq!(json["command"], "import");
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(
        keys,
        [
            "format", "command", "imported", "skipped", "refused", "warnings"
        ]
    );
    let imported = &json["imported"][0];
    assert_eq!(imported["file"], "proj.tar.zst");
    assert_eq!(imported["path"], "proj");
    assert_eq!(imported["name"], "proj");
    assert_eq!(imported["created"], "2001-09-09T01:46:40Z");
    assert_eq!(imported["payload_version"], 1);
    assert_eq!(imported["id"].as_str().expect("an ID").len(), 26);
    assert_eq!(imported["sha256"].as_str().expect("a digest").len(), 64);
    assert_eq!(json["skipped"], json!([]));
    assert_eq!(json["refused"], json!([]));
}

#[test]
fn the_imported_piece_is_archived_in_status() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    world.import(&["proj.tar.zst"]);
    let run = Antiq::new()
        .arg("--json")
        .arg("-C")
        .arg(world.temp.root())
        .arg("status")
        .run();
    let json = run.json();
    let pieces = json["pieces"].as_array().expect("pieces");
    let piece = pieces
        .iter()
        .find(|piece| piece["path"] == "proj")
        .expect("the piece is listed");
    assert_eq!(piece["state"], "archived");
    assert!(
        piece["flags"]
            .as_array()
            .expect("flags")
            .contains(&json!("pending_upload")),
        "{piece}"
    );
}

#[test]
fn find_shows_the_imported_piece_by_its_name_and_by_a_member() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    world.import(&["proj.tar.zst"]);
    for query in ["proj", "a.txt"] {
        let run = Antiq::new()
            .arg("--json")
            .arg("-C")
            .arg(world.temp.root())
            .args(["find", query])
            .run();
        assert_eq!(
            run.code(),
            0,
            "stdout: {}, stderr: {}",
            run.stdout,
            run.stderr
        );
        let json = run.json();
        let matches = json["matches"].as_array().expect("matches");
        assert_eq!(matches.len(), 1, "{query}: {json}");
        assert_eq!(matches[0]["path"], "proj");
        assert_eq!(matches[0]["state"], "archived");
    }
}

#[test]
fn path_names_the_piece_and_files_and_path_start_in_the_directory_c_names() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    fs::write(world.temp.root().join(".antiq/config.toml"), {
        format!(
            "[collection]\nid = \"{}\"\npatterns = [\"*/*\"]\n",
            support::tree::COLLECTION_ID
        )
    })
    .expect("a config");
    let run = world.json_import(&["--path", "clients/acme", "proj.tar.zst"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["imported"][0]["path"], "clients/acme");
    assert_eq!(json["imported"][0]["name"], "acme");
}

// =========================================================
// Skipping
// =========================================================

#[test]
fn the_same_archive_again_is_skipped_with_exit_0() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    world.import(&["proj.tar.zst"]);
    let run = world.import(&["proj.tar.zst"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.stderr, "");
    insta::assert_snapshot!(normalised(&run.stdout), @"Skipped proj.tar.zst: already imported as <id>
");
    assert_eq!(world.objects(), 1);
}

// =========================================================
// Refusing
// =========================================================

#[test]
fn a_refused_file_is_named_with_its_reason_and_the_run_exits_3() {
    let mut world = World::new();
    let good = world.archive("good.tar.zst", FILES);
    let _ = good;
    world.temp.tree().file("bad.tar.gz", [0x1F, 0x8B, 8, 0]);
    let run = world.import(&["bad.tar.gz", "good.tar.zst"]);
    assert_eq!(
        run.code(),
        3,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    insta::assert_snapshot!(normalised(&run.stdout), @"Imported good.tar.zst as proj (<id>)
");
    insta::assert_snapshot!(run.stderr, @"bad.tar.gz was not imported: the file is not zstd but gzip
");
}

#[test]
fn a_refusal_in_json_is_listed_and_the_exit_is_3() {
    let world = World::new();
    world.temp_file("bad.tar.gz", &[0x1F, 0x8B, 8, 0]);
    let run = world.json_import(&["bad.tar.gz"]);
    assert_eq!(run.code(), 3);
    assert_eq!(
        run.json()["refused"],
        json!([{"file": "bad.tar.gz", "reason": "the file is not zstd but gzip"}])
    );
    assert_eq!(world.objects(), 0);
}

impl World {
    fn temp_file(&self, name: &str, bytes: &[u8]) {
        fs::write(self.temp.root().join(name), bytes).expect("written");
    }
}

#[test]
fn path_with_several_files_is_a_usage_error() {
    let mut world = World::new();
    world.archive("one.tar.zst", FILES);
    world.archive("two.tar.zst", &[("b.txt", "other", 1_000_000_001)]);
    let run = world.json_import(&["--path", "x", "one.tar.zst", "two.tar.zst"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert_eq!(world.objects(), 0);
}

#[test]
fn a_path_outside_the_collection_is_an_error_of_the_run() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    let outside = tempfile::tempdir().expect("a temp dir");
    let run = world.json_import(&[
        "--path",
        outside.path().to_str().expect("UTF-8"),
        "proj.tar.zst",
    ]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "outside_collection");
    assert_eq!(world.objects(), 0);
}

// =========================================================
// Dry runs
// =========================================================

#[test]
fn a_dry_run_prints_the_plan_and_writes_nothing() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    let run = world.import(&["--dry-run", "proj.tar.zst"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    insta::assert_snapshot!(run.stdout, @"Would import proj.tar.zst as proj
");
    assert_eq!(world.objects(), 0);
}

#[test]
fn a_dry_run_in_json_has_the_actions() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    let run = world.json_import(&["--dry-run", "proj.tar.zst"]);
    assert_eq!(run.code(), 0);
    let json = run.json();
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(
        keys,
        [
            "format", "command", "dry_run", "actions", "imported", "skipped", "refused", "warnings"
        ]
    );
    assert_eq!(json["dry_run"], true);
    assert_eq!(
        json["actions"],
        json!([{"action": "import", "file": "proj.tar.zst", "path": "proj"}])
    );
    assert_eq!(world.objects(), 0);
}

#[test]
fn a_dry_run_says_what_it_would_skip() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    world.import(&["proj.tar.zst"]);
    let run = world.import(&["--dry-run", "proj.tar.zst"]);
    assert_eq!(run.code(), 0);
    insta::assert_snapshot!(normalised(&run.stdout), @"Would skip proj.tar.zst: already imported as <id>
");
}

#[test]
fn a_dry_run_names_the_run_that_holds_the_lock() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    let _held = antiq::lock::acquire(
        &world.root().join(".antiq/lock"),
        "laptop",
        &antiq::clock::SystemClock,
    )
    .expect("the lock is free");
    let run = world.import(&["--dry-run", "proj.tar.zst"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr
            .starts_with("note: another run holds the lock (pid "),
        "{}",
        run.stderr
    );
    insta::assert_snapshot!(run.stdout, @"Would import proj.tar.zst as proj
");
}

#[test]
fn a_run_while_another_holds_the_lock_is_refused() {
    let mut world = World::new();
    world.archive("proj.tar.zst", FILES);
    let _held = antiq::lock::acquire(
        &world.root().join(".antiq/lock"),
        "laptop",
        &antiq::clock::SystemClock,
    )
    .expect("the lock is free");
    let run = world.json_import(&["proj.tar.zst"]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "lock_held");
    assert_eq!(world.objects(), 0);
}
