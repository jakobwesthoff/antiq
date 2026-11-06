// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rsync stand-in of `support::fake_rsync`, seen from a binary test: it
//! answers each transfer by its number and logs every call.

mod support;

use std::process::{Command, Output};
use support::fake_rsync::FakeRsync;

const RSYNC_3_5_1: &str = "rsync  version 3.5.1  protocol version 33\nCopyright (C) 1996-2026\n";

fn run(fake: &FakeRsync, args: &[&str]) -> Output {
    Command::new(fake.path())
        .args(args)
        .output()
        .expect("the fixture runs")
}

#[test]
fn each_transfer_reads_the_answer_of_its_own_number() {
    let fake = FakeRsync::new(RSYNC_3_5_1);
    fake.answer(1, "", "rsync: link_stat failed\n", 23);
    fake.answer(2, "sent it\n", "", 0);
    let first = run(&fake, &["a"]);
    assert_eq!(first.status.code(), Some(23));
    assert_eq!(first.stderr, b"rsync: link_stat failed\n");
    let second = run(&fake, &["b"]);
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(second.stdout, b"sent it\n");
}

#[test]
fn a_call_without_a_numbered_answer_gets_the_plain_one() {
    let fake = FakeRsync::new(RSYNC_3_5_1);
    fake.answer_all("plain\n", "", 7);
    fake.answer(2, "second\n", "", 0);
    assert_eq!(run(&fake, &["a"]).status.code(), Some(7));
    assert_eq!(run(&fake, &["b"]).stdout, b"second\n");
    assert_eq!(run(&fake, &["c"]).stdout, b"plain\n");
}

#[test]
fn a_version_call_is_not_counted_as_a_transfer_and_can_fail() {
    let fake = FakeRsync::new(RSYNC_3_5_1);
    fake.answer(1, "first\n", "", 0);
    assert_eq!(run(&fake, &["--version"]).stdout, RSYNC_3_5_1.as_bytes());
    assert_eq!(run(&fake, &["a"]).stdout, b"first\n");
    fake.version_exit(3);
    assert_eq!(run(&fake, &["--version"]).status.code(), Some(3));
}

#[test]
fn a_numbered_signal_kills_only_that_transfer() {
    let fake = FakeRsync::new(RSYNC_3_5_1);
    fake.kill_on(2);
    assert_eq!(run(&fake, &["a"]).status.code(), Some(0));
    assert_eq!(run(&fake, &["b"]).status.code(), None);
}

#[test]
fn the_log_splits_into_calls_with_empty_arguments_kept() {
    let fake = FakeRsync::new(RSYNC_3_5_1);
    run(&fake, &["--version"]);
    run(&fake, &["-a", "", "dir/"]);
    assert_eq!(
        fake.calls(),
        [
            vec!["version", "--version"],
            vec!["transfer", "-a", "", "dir/"]
        ]
    );
    assert!(fake.dir().join("rsync").exists());
}
