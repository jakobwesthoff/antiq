// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ssh stand-in of `support::ssh_shim`: it skips the options rsync
//! passes, runs the command locally and can be made to fail like ssh.

mod support;

use std::process::{Command, Output};
use support::ssh_shim::SshShim;

fn run(shim: &SshShim, args: &[&str]) -> Output {
    Command::new(shim.path())
        .args(args)
        .output()
        .expect("the shim runs")
}

#[test]
fn the_command_after_the_host_runs_here_and_options_are_skipped() {
    let shim = SshShim::new();

    let output = run(
        &shim,
        &[
            "-o",
            "BatchMode=yes",
            "-l",
            "me",
            "-p",
            "22",
            "somehost",
            "echo",
            "hi there",
        ],
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"hi there\n");
    assert_eq!(
        shim.calls(),
        ["-o\tBatchMode=yes\t-l\tme\t-p\t22\tsomehost\techo\thi there\t"]
    );
}

#[test]
fn a_fault_prints_its_text_and_runs_nothing() {
    let shim = SshShim::new();
    shim.fault("connect-refused");

    let output = run(&shim, &["somehost", "echo", "ran"]);

    assert_eq!(output.status.code(), Some(255));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("ssh: connect to host"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_option_it_does_not_know_fails_the_call() {
    let shim = SshShim::new();

    let output = run(&shim, &["-Z", "somehost", "true"]);

    assert_eq!(output.status.code(), Some(255));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option -Z"));
}
