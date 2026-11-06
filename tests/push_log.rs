// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The log of a push run: what the external programs printed, kept where
//! the platform keeps logs.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::fake_rsync::FakeRsync;
use support::scenario::collection::{Clock, Collection};
use support::scenario::target::Target;

const RSYNC_3_2_4: &str = "rsync  version 3.2.4  protocol version 31\n";

/// A collection pushing to `target` with a stand-in rsync.
fn targeted(target: &Target) -> (Collection, FakeRsync) {
    let collection = Collection::build(&Clock::now())
        .piece("projects/a", |piece| {
            piece.age_days(3).file("a.txt", "a");
        })
        .finish();
    let fake = FakeRsync::new(RSYNC_3_2_4);
    let url = serde_json::to_string(target.path().to_str().expect("a UTF-8 path"))
        .expect("a string serialises");
    let rsync = serde_json::to_string(fake.path().to_str().expect("a UTF-8 path"))
        .expect("a string serialises");
    let path = collection.path(".antiq/config.toml");
    let mut text = fs::read_to_string(&path).expect("the config exists");
    text.push_str(&format!("\n[target]\nurl = {url}\nrsync = {rsync}\n"));
    fs::write(&path, text).expect("the config can be written");
    target.marker(&collection);
    (collection, fake)
}

/// A push whose home directory, and with it the log directory, is `home`.
fn push_at(collection: &Collection, home: &Path) -> Run {
    Antiq::new()
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .arg("-C")
        .arg(collection.root())
        .arg("--json")
        .arg("push")
        .run()
}

/// Every log below `home`, whatever the platform's directory is.
fn logs_below(home: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![home.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.to_string_lossy().ends_with(".log.zst") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn read(log: &Path) -> String {
    let bytes = fs::read(log).expect("the log is readable");
    String::from_utf8(zstd::decode_all(bytes.as_slice()).expect("a complete log"))
        .expect("the log is UTF-8")
}

#[test]
fn a_push_writes_one_log_with_the_probe_and_the_mirror() {
    let target = Target::new();
    let (collection, fake) = targeted(&target);
    fake.answer_all(">f+++++++++ a.txt\n", "", 0);
    let home = tempfile::tempdir().expect("a temp dir");

    let run = push_at(&collection, home.path());

    assert_eq!(run.code(), 0, "{}", run.stdout);
    let logs = logs_below(home.path());
    assert_eq!(logs.len(), 1, "{logs:?}");
    let text = read(&logs[0]);
    assert!(text.contains("--version"), "{text}");
    assert!(text.contains("--delete"), "{text}");
    assert!(text.contains("s out >f+++++++++ a.txt\n"), "{text}");
    assert!(text.contains("# exit 0\n"), "{text}");
}

#[test]
fn the_eleventh_push_leaves_ten_logs() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target);
    let home = tempfile::tempdir().expect("a temp dir");

    for _ in 0..11 {
        let run = push_at(&collection, home.path());
        assert_eq!(run.code(), 0, "{}", run.stdout);
    }

    assert_eq!(logs_below(home.path()).len(), 10);
}

#[test]
fn a_log_that_cannot_be_written_does_not_fail_the_push() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target);
    let home = tempfile::tempdir().expect("a temp dir");
    let blocker = home.path().join("not-a-directory");
    fs::write(&blocker, "a file").expect("writable");

    let run = push_at(&collection, &blocker);

    assert_eq!(run.code(), 0, "{}", run.stdout);
    assert!(logs_below(home.path()).is_empty());
    let warnings = run.json()["warnings"].to_string();
    assert!(warnings.contains("no log was written to"), "{warnings}");
}

/// What a share that refuses modes prints: a line for each of 40 files.
fn refused_modes_stderr() -> String {
    let lines: String = (1..=40)
        .map(|number| {
            format!(
                "rsync: [receiver] mkstemp \"/mnt/shadow/beta/.note{number}.md.A3Kcxa\" failed: Operation not permitted (1)\n"
            )
        })
        .collect();
    format!("{lines}rsync error: some files/attrs were not transferred (code 23) at main.c(1338)\n")
}

/// Like `push_at`, in the text form of a pipe.
fn push_text_at(collection: &Collection, home: &Path) -> Run {
    Antiq::new()
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .arg("-C")
        .arg(collection.root())
        .arg("push")
        .run()
}

#[test]
fn a_failed_push_names_its_log_and_summarises_what_rsync_said() {
    let target = Target::new();
    let (collection, fake) = targeted(&target);
    fake.answer_all("", &refused_modes_stderr(), 23);
    let home = tempfile::tempdir().expect("a temp dir");

    let run = push_text_at(&collection, home.path());

    assert_eq!(run.code(), 1, "{}", run.stderr);
    let logs = logs_below(home.path());
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(
        logs[0].to_string_lossy().ends_with("-failed.log.zst"),
        "{logs:?}"
    );
    let path = logs[0].display().to_string();
    assert!(
        run.stderr.contains(&format!("Full output: {path}")),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(&format!("zstd -dc {path} | less")),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("40× rsync: [receiver] mkstemp"),
        "{}",
        run.stderr
    );
    let text = read(&logs[0]);
    assert_eq!(
        text.matches("s err rsync: [receiver] mkstemp").count(),
        40,
        "{text}"
    );
    assert!(text.contains("# exit 23\n"), "{text}");
}

#[test]
fn the_json_error_carries_the_groups_and_the_path_of_the_log() {
    let target = Target::new();
    let (collection, fake) = targeted(&target);
    fake.answer_all(
        "",
        "rsync: write failed on \"/t/a\"\nrsync: write failed on \"/t/b\"\n",
        11,
    );
    let home = tempfile::tempdir().expect("a temp dir");

    let run = push_at(&collection, home.path());

    assert_eq!(run.code(), 1, "{}", run.stdout);
    let error = &run.json()["error"];
    assert_eq!(
        error["message"],
        "the mirror failed: rsync ended with exit 11 (error in file I/O)"
    );
    let groups = error["program_said"]["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["count"], 2);
    assert_eq!(groups[0]["line"], "rsync: write failed on \"…\"");
    assert_eq!(groups[0]["example"], "/t/a");
    let logs = logs_below(home.path());
    assert_eq!(error["log"], logs[0].display().to_string());
}

#[test]
fn an_error_without_words_of_rsync_still_names_the_log() {
    let target = Target::new();
    let (collection, fake) = targeted(&target);
    fake.answer_all("", "", 1);
    let home = tempfile::tempdir().expect("a temp dir");

    let run = push_at(&collection, home.path());

    assert_eq!(run.code(), 1, "{}", run.stdout);
    let error = &run.json()["error"];
    assert_eq!(
        error["message"],
        "the mirror failed: rsync ended with exit 1 (syntax or usage error)"
    );
    assert_eq!(error["program_said"]["groups"], serde_json::json!([]));
    assert!(error["log"].is_string(), "{error}");
}

#[test]
fn a_probe_that_refuses_the_rsync_names_the_log_with_its_output() {
    let target = Target::new();
    let (collection, _fake) = targeted(&target);
    let old = FakeRsync::new("rsync  version 3.1.0  protocol version 31\n");
    let path = collection.path(".antiq/config.toml");
    let text = fs::read_to_string(&path).expect("the config exists");
    let rsync = serde_json::to_string(old.path().to_str().expect("a UTF-8 path")).expect("quotes");
    let replaced: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with("rsync = ") {
                format!("rsync = {rsync}")
            } else {
                line.to_string()
            }
        })
        .collect();
    fs::write(&path, replaced.join("\n") + "\n").expect("writable");
    let home = tempfile::tempdir().expect("a temp dir");

    let run = push_at(&collection, home.path());

    assert_eq!(run.code(), 1, "{}", run.stdout);
    let logs = logs_below(home.path());
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert_eq!(run.json()["error"]["log"], logs[0].display().to_string());
    assert!(
        read(&logs[0]).contains("s out rsync  version 3.1.0"),
        "{}",
        read(&logs[0])
    );
}

#[test]
fn a_receiver_refusal_of_another_operation_gets_the_generic_title_and_no_fix() {
    let target = Target::new();
    let (collection, fake) = targeted(&target);
    fake.answer_all(
        "",
        "rsync: [receiver] rename \"/t/.a.XXXX\" -> \"a\": Operation not permitted (1)\n",
        23,
    );
    let home = tempfile::tempdir().expect("a temp dir");

    let run = push_at(&collection, home.path());

    assert_eq!(run.code(), 1, "{}", run.stdout);
    let error = &run.json()["error"];
    assert_eq!(
        error["message"],
        "the mirror failed: rsync ended with exit 23 (some files or attributes were not transferred)"
    );
    assert!(error["code"].is_null(), "{error}");
    assert!(error["fix"].is_null(), "{error}");
}

#[test]
fn the_log_of_the_last_failed_push_stays_beyond_ten_later_pushes() {
    let target = Target::new();
    let (collection, fake) = targeted(&target);
    let home = tempfile::tempdir().expect("a temp dir");
    fake.answer(1, "", "rsync: oops\n", 1);

    let failed = push_at(&collection, home.path());
    assert_eq!(failed.code(), 1, "{}", failed.stdout);
    for _ in 0..11 {
        let run = push_at(&collection, home.path());
        assert_eq!(run.code(), 0, "{}", run.stdout);
    }

    let logs = logs_below(home.path());
    assert_eq!(logs.len(), 11, "{logs:?}");
    assert_eq!(
        logs.iter()
            .filter(|log| log.to_string_lossy().ends_with("-failed.log.zst"))
            .count(),
        1
    );
}

#[test]
fn a_push_that_fails_before_any_program_starts_has_no_log_to_name() {
    let collection = Collection::build(&Clock::now())
        .piece("projects/a", |piece| {
            piece.age_days(3).file("a.txt", "a");
        })
        .finish();
    let home = tempfile::tempdir().expect("a temp dir");

    let run = push_at(&collection, home.path());

    assert_eq!(run.code(), 1, "{}", run.stdout);
    assert!(run.json()["error"].get("log").is_none());
    assert!(logs_below(home.path()).is_empty());
}
