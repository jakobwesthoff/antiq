// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A stand-in for ssh that runs the command on this machine, for tests
//! that give rsync the remote shell `-e <shim>`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use tempfile::TempDir;

/// A directory holding an `ssh` that is the fixture script, with its state
/// beside it. The fixture is linked, never copied (see `FakeRsync`).
pub struct SshShim {
    dir: TempDir,
}

impl SshShim {
    pub fn new() -> SshShim {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ssh-shim.sh");
        symlink(fixture, dir.path().join("ssh")).expect("the link is new");
        SshShim { dir }
    }

    /// The program to hand to rsync as `-e`.
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("ssh")
    }

    /// Makes every call print `ssh-texts/<name>.txt` and fail with 255.
    pub fn fault(&self, name: &str) {
        fs::write(self.dir.path().join("fault"), name).expect("the directory is writable");
    }

    /// The calls so far, one line each, with the arguments joined by tabs.
    pub fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("calls.log"))
            .map(|log| log.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }
}
