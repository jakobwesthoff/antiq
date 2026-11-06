// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A stand-in for rsync for the binary tests: the same fixture script and
//! the same answers as the crate's own `target::fake::Fake`, which a test
//! outside the crate cannot reach.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use tempfile::TempDir;

/// A directory holding a `rsync` that is the fixture script, with the
/// answers the script reads beside it. The fixture is linked, never
/// copied: a test process that writes an executable can see it fail to
/// start with "text file busy" while another test forks.
pub struct FakeRsync {
    dir: TempDir,
}

impl FakeRsync {
    pub fn new(version: &str) -> FakeRsync {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-rsync.sh");
        symlink(fixture, dir.path().join("rsync")).expect("the link is new");
        fs::write(dir.path().join("version"), version).expect("the directory is writable");
        FakeRsync { dir }
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("rsync")
    }

    pub fn version_exit(&self, code: i32) {
        fs::write(self.dir.path().join("version_exit"), code.to_string())
            .expect("the directory is writable");
    }

    pub fn read(&self, name: &str) -> Vec<u8> {
        fs::read(self.dir.path().join(name)).expect("the fixture logged the call")
    }

    /// Sets what every transfer prints and returns, unless a numbered
    /// answer exists for it.
    pub fn answer_all(&self, stdout: &str, stderr: &str, exit: i32) {
        self.write_answer("", stdout, stderr, exit);
    }

    /// Sets what the `call`th transfer (from 1) prints and returns.
    pub fn answer(&self, call: usize, stdout: &str, stderr: &str, exit: i32) {
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
    pub fn kill_on(&self, call: usize) {
        fs::write(self.dir.path().join(format!("signal.{call}")), "")
            .expect("the directory is writable");
    }

    /// The calls made so far in order, each as its mode (`version` or
    /// `transfer`) followed by its arguments.
    pub fn calls(&self) -> Vec<Vec<String>> {
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
