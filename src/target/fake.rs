// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A stand-in for rsync, for tests of code that runs it.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use tempfile::TempDir;

/// A directory holding a `rsync` that is the fixture script, with the
/// answers the script reads beside it. The fixture is linked, never
/// copied: a test process that writes an executable can see it fail to
/// start with "text file busy" while another test forks.
pub(crate) struct Fake {
    dir: TempDir,
}

impl Fake {
    pub(crate) fn new(version: &str) -> Fake {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-rsync.sh");
        symlink(fixture, dir.path().join("rsync")).expect("the link is new");
        fs::write(dir.path().join("version"), version).expect("the directory is writable");
        Fake { dir }
    }

    pub(crate) fn dir(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.dir.path().join("rsync")
    }

    pub(crate) fn version_exit(&self, code: i32) {
        fs::write(self.dir.path().join("version_exit"), code.to_string())
            .expect("the directory is writable");
    }

    pub(crate) fn read(&self, name: &str) -> Vec<u8> {
        fs::read(self.dir.path().join(name)).expect("the fixture logged the call")
    }

    /// Sets what every transfer prints and returns, unless a numbered
    /// answer exists for it.
    pub(crate) fn answer_all(&self, stdout: &str, stderr: &str, exit: i32) {
        self.write_answer("", stdout, stderr, exit);
    }

    /// Sets what the `call`th transfer (from 1) prints and returns.
    pub(crate) fn answer(&self, call: usize, stdout: &str, stderr: &str, exit: i32) {
        self.write_answer(&format!(".{call}"), stdout, stderr, exit);
    }

    fn write_answer(&self, suffix: &str, stdout: &str, stderr: &str, exit: i32) {
        for (name, content) in [
            ("stdout", stdout.to_string()),
            ("stderr", stderr.to_string()),
            ("exit", exit.to_string()),
        ] {
            fs::write(self.dir.path().join(format!("{name}{suffix}")), content)
                .expect("the directory is writable");
        }
    }

    /// Makes the `call`th transfer kill itself with SIGKILL.
    pub(crate) fn kill_on(&self, call: usize) {
        fs::write(self.dir.path().join(format!("signal.{call}")), "")
            .expect("the directory is writable");
    }

    /// The calls made so far in order, each as its mode (`version` or
    /// `transfer`) followed by its arguments.
    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        let log = self.read("calls.log");
        log.split(|byte| *byte == 0x1e)
            .filter(|record| !record.is_empty())
            .map(|record| {
                let mut fields: Vec<String> = record
                    .split(|byte| *byte == 0)
                    .map(|field| String::from_utf8_lossy(field).into_owned())
                    .collect();
                // Every field ends with a NUL, so the split leaves an empty
                // last element.
                fields.pop();
                fields
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const RSYNC_3_5_1: &str =
        "rsync  version 3.5.1  protocol version 33\nCopyright (C) 1996-2026\n";

    fn run(fake: &Fake, args: &[&str]) -> std::process::Output {
        Command::new(fake.path())
            .args(args)
            .output()
            .expect("the fixture runs")
    }

    #[test]
    fn each_transfer_reads_the_answer_of_its_own_number() {
        let fake = Fake::new(RSYNC_3_5_1);
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
        let fake = Fake::new(RSYNC_3_5_1);
        fake.answer_all("plain\n", "", 7);
        fake.answer(2, "second\n", "", 0);
        assert_eq!(run(&fake, &["a"]).status.code(), Some(7));
        assert_eq!(run(&fake, &["b"]).stdout, b"second\n");
        assert_eq!(run(&fake, &["c"]).stdout, b"plain\n");
    }

    #[test]
    fn a_version_call_is_not_counted_as_a_transfer() {
        let fake = Fake::new(RSYNC_3_5_1);
        fake.answer(1, "first\n", "", 0);
        run(&fake, &["--version"]);
        assert_eq!(run(&fake, &["a"]).stdout, b"first\n");
    }

    #[test]
    fn a_numbered_signal_kills_only_that_transfer() {
        let fake = Fake::new(RSYNC_3_5_1);
        fake.kill_on(2);
        assert_eq!(run(&fake, &["a"]).status.code(), Some(0));
        assert_eq!(run(&fake, &["b"]).status.code(), None);
    }

    #[test]
    fn the_log_splits_into_calls_with_empty_arguments_kept() {
        let fake = Fake::new(RSYNC_3_5_1);
        run(&fake, &["--version"]);
        run(&fake, &["-a", "", "dir/"]);
        assert_eq!(
            fake.calls(),
            [
                vec!["version", "--version"],
                vec!["transfer", "-a", "", "dir/"]
            ]
        );
    }
}
