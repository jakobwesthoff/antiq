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
}
