// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq` with an ssh remote, seen from outside the binary. ssh is the
//! loopback shim `tests/fixtures/ssh-shim.sh`, which runs the commands on
//! this machine, so the "NAS" is a directory.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use support::cmd::{Antiq, Run};
use support::tree::{COLLECTION_ID, TempCollection};
use tempfile::TempDir;

const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

/// The shim in a state directory of its own, and the directory it serves.
struct Nas {
    state: TempDir,
    root: TempDir,
}

impl Nas {
    /// A NAS whose root holds the marker of `REMOTE_ID`.
    fn new() -> Nas {
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "these tests assume a non-root user"
        );
        let perl = std::process::Command::new("perl").arg("-v").output();
        assert!(
            perl.is_ok_and(|output| output.status.success()),
            "the ssh shim needs perl to run a master process"
        );
        let state = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("/tmp is writable");
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700))
            .expect("the directory is ours");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-shim.sh");
        std::os::unix::fs::symlink(script, state.path().join("ssh")).expect("symlink");
        let nas = Nas {
            state,
            root: tempfile::tempdir().expect("temp dir"),
        };
        fs::write(
            nas.root.path().join("antiq-remote.json"),
            format!("{{\"format\":1,\"id\":\"{REMOTE_ID}\",\"layout\":1}}\n"),
        )
        .expect("the marker is written");
        nas
    }

    fn url(&self) -> String {
        format!("ssh://nas{}", self.root.path().display())
    }

    fn fault(&self, name: &str) {
        fs::write(self.state.path().join("fault"), format!("{name}\n")).expect("fault");
    }

    /// A collection that has this NAS attached.
    fn attached(&self) -> TempCollection {
        let collection = TempCollection::minimal();
        fs::write(
            collection.root().join(".antiq/config.toml"),
            format!(
                "[collection]\nid = \"{COLLECTION_ID}\"\n\n[[remote]]\nurl = \"{}\"\nid = \"{REMOTE_ID}\"\n",
                self.url()
            ),
        )
        .expect("a config can be written");
        collection
    }

    /// `antiq -C <collection> <args>` with the shim as `ssh`.
    fn antiq(&self, collection: &TempCollection, args: &[&str]) -> Run {
        let path = format!(
            "{}:{}",
            self.state.path().display(),
            std::env::var("PATH").expect("PATH")
        );
        Antiq::new()
            .env("PATH", path)
            .arg("-C")
            .arg(collection.root())
            .args(args)
            .run()
    }
}

// =========================================================
// status
// =========================================================

fn remote_state(nas: &Nas) -> String {
    let collection = nas.attached();
    let run = nas.antiq(&collection, &["--json", "status"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    run.json()["remote"]["state"]
        .as_str()
        .expect("a state")
        .to_string()
}

#[test]
fn status_finds_a_nas_with_its_marker_reachable() {
    assert_eq!(remote_state(&Nas::new()), "reachable");
}

#[test]
fn status_calls_a_nas_that_refuses_the_connection_away() {
    let nas = Nas::new();
    nas.fault("connect-refused");
    assert_eq!(remote_state(&nas), "away");
}

#[test]
fn status_calls_a_nas_that_refuses_the_login_broken() {
    let nas = Nas::new();
    nas.fault("permission-denied");
    assert_eq!(remote_state(&nas), "broken");
}

#[test]
fn status_calls_a_nas_broken_when_ssh_cannot_be_started() {
    let nas = Nas::new();
    let collection = nas.attached();
    let no_programs = tempfile::tempdir().expect("temp dir");
    let run = Antiq::new()
        .env("PATH", no_programs.path())
        .arg("-C")
        .arg(collection.root())
        .args(["--json", "status"])
        .run();
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.json()["remote"]["state"], "broken");
}
