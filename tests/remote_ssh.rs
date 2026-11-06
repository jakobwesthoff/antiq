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
use support::sshd::Sshd;
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

// =========================================================
// remote add and remote show
// =========================================================

use std::path::PathBuf;

/// The first rsync of `PATH` that antiq would accept: 3.1 or later and no
/// openrsync.
fn real_rsync() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("rsync"))
        .find(|candidate| {
            std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.stdout.starts_with(b"rsync  version 3."))
        })
        .expect("these tests need an rsync 3.1 or later")
}

/// A directory with a stand-in rsync that prints `version` and the path of
/// the stand-in.
fn fake_rsync(version: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-rsync.sh");
    let path = dir.path().join("rsync");
    std::os::unix::fs::symlink(fixture, &path).expect("link");
    fs::write(dir.path().join("version"), version).expect("version");
    (dir, path)
}

/// Every path below `root`, relative to it, sorted.
fn listing(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, found: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("the directory is listed") {
            let entry = entry.expect("entry");
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            found.push(name.clone());
            if entry.file_type().expect("type").is_dir() {
                walk(&entry.path(), &format!("{name}/"), found);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, "", &mut found);
    found.sort();
    found
}

fn config_of(collection: &TempCollection) -> String {
    fs::read_to_string(collection.root().join(".antiq/config.toml")).expect("the config is read")
}

impl Nas {
    /// A NAS whose root exists and holds no marker.
    fn markerless() -> Nas {
        let nas = Nas::new();
        fs::remove_file(nas.root.path().join("antiq-remote.json")).expect("the marker goes");
        nas
    }

    /// The URL of a directory below the root, which may not exist.
    fn url_below(&self, name: &str) -> String {
        format!("{}/{name}", self.url())
    }

    fn below(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// Puts a tool called `name` in front of the far side's `PATH`.
    fn wrap(&self, name: &str, body: &str) {
        let dir = self.state.path().join("wrappers");
        fs::create_dir_all(&dir).expect("wrapper directory");
        let file = dir.join(name);
        fs::write(&file, format!("#!/bin/sh\n{body}\n")).expect("wrapper");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("executable");
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").expect("PATH"));
        fs::write(self.state.path().join("remote_path"), path).expect("remote_path");
    }

    /// Makes the far side's `PATH` a directory of `tools` and nothing else.
    fn only(&self, tools: &[&str]) {
        let dir = self.state.path().join("bare");
        fs::create_dir_all(&dir).expect("bare directory");
        for name in tools {
            let found = ["/bin", "/usr/bin", "/sbin"]
                .iter()
                .map(|base| Path::new(base).join(name))
                .find(|candidate| candidate.exists())
                .expect("the tool exists");
            std::os::unix::fs::symlink(found, dir.join(name)).expect("symlink");
        }
        fs::write(
            self.state.path().join("remote_path"),
            dir.display().to_string(),
        )
        .expect("remote_path");
    }

    /// The commands antiq ran against the shim, one per line.
    fn calls(&self) -> String {
        fs::read_to_string(self.state.path().join("calls.log")).unwrap_or_default()
    }

    /// A collection with no remote attached and `machine` as the text of
    /// its `[machine]` table.
    fn unattached(&self, machine: &str) -> TempCollection {
        let collection = TempCollection::minimal();
        fs::write(
            collection.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n\n[machine]\n{machine}"),
        )
        .expect("a config can be written");
        collection
    }

    fn remote_add(&self, collection: &TempCollection, args: &[&str], url: &str) -> Run {
        let mut all = vec!["--json", "remote", "add"];
        all.extend_from_slice(args);
        all.push(url);
        self.antiq(collection, &all)
    }
}

/// The tools the scripts of `remote add` use besides the hash tools.
const BASIC_TOOLS: [&str; 9] = [
    "sh", "cat", "dirname", "mkdir", "ln", "rm", "rmdir", "sync", "stat",
];

fn message(run: &Run) -> String {
    run.error_message()
}

fn assert_refused_and_untouched(
    nas: &Nas,
    collection: &TempCollection,
    run: &Run,
    kind: &str,
    mentions: &str,
) {
    let before_nas = listing(nas.root.path());
    assert_eq!(
        run.code(),
        1,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), kind);
    assert!(message(run).contains(mentions), "{}", message(run));
    assert_eq!(listing(nas.root.path()), before_nas);
    assert!(!config_of(collection).contains("[[remote]]"));
}

// ----- remote add --init -----

#[test]
fn init_makes_the_marker_and_attaches_the_nas() {
    let nas = Nas::markerless();
    let root = nas.root.path().display().to_string();
    let log = nas.state.path().join("sync.log");
    nas.wrap(
        "sync",
        &format!(
            "echo call >> {}; ls -A {root} >> {}",
            log.display(),
            log.display()
        ),
    );
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--init"], &nas.url());
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["command"], "remote add");
    assert_eq!(json["kind"], "ssh");
    assert_eq!(json["created"], true);
    assert_eq!(json["attached"], true);
    assert_eq!(json["pulled"], 0);

    let marker: serde_json::Value = serde_json::from_slice(
        &fs::read(nas.below("antiq-remote.json")).expect("the marker exists"),
    )
    .expect("the marker is JSON");
    assert_eq!(marker["id"], json["id"]);
    assert_eq!(listing(nas.root.path()), ["antiq-remote.json"]);

    let config = config_of(&collection);
    assert!(
        config.contains(&format!("url = \"{}\"", nas.url())),
        "{config}"
    );
    assert!(
        config.contains(&format!("rsync = \"{}\"", real_rsync().display())),
        "{config}"
    );

    // The temp is flushed before the link step and the marker after it.
    let synced = fs::read_to_string(log).expect("sync ran");
    let lines: Vec<&str> = synced.lines().collect();
    assert_eq!(lines.len(), 4, "{synced}");
    assert_eq!(lines[0], "call");
    assert!(
        lines[1].starts_with(".antiq-remote.json.") && lines[1].ends_with(".tmp"),
        "{synced}"
    );
    assert_eq!(lines[2], "call");
    assert_eq!(lines[3], "antiq-remote.json");
}

#[test]
fn init_makes_the_last_component_of_the_root() {
    let nas = Nas::new();
    fs::remove_file(nas.below("antiq-remote.json")).expect("no marker");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--init"], &nas.url_below("fresh"));
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(
        listing(nas.root.path()),
        ["fresh", "fresh/antiq-remote.json"]
    );
}

#[test]
fn init_below_a_missing_parent_refuses() {
    let nas = Nas::new();
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--init"], &nas.url_below("a/b"));
    assert_eq!(
        run.code(),
        1,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(
        message(&run).contains("does not exist"),
        "{}",
        message(&run)
    );
    assert!(!config_of(&collection).contains("[[remote]]"));
}

#[test]
fn init_on_a_root_that_holds_a_marker_refuses() {
    let nas = Nas::new();
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--init"], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "other", "antiq-remote.json");
}

#[test]
fn init_on_a_root_that_holds_objects_refuses() {
    let nas = Nas::markerless();
    fs::create_dir(nas.below("objects")).expect("objects");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--init"], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "other", "objects");
}

// ----- remote add: attaching -----

#[test]
fn attaching_copies_the_metadata_of_another_machine() {
    let nas = Nas::new();
    let object = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    fs::create_dir_all(nas.below(&format!("objects/{object}"))).expect("dirs");
    fs::write(nas.below(&format!("objects/{object}/meta.1")), "other").expect("meta");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["kind"], "ssh");
    assert_eq!(json["id"], REMOTE_ID);
    assert_eq!(json["created"], false);
    assert_eq!(json["pulled"], 1);
    // The writes and the copy run over one master connection.
    assert!(nas.calls().contains("-M\t-N\t-f"), "{}", nas.calls());
    assert_eq!(
        fs::read(
            collection
                .root()
                .join(format!(".antiq/objects/{object}/meta.1"))
        )
        .expect("copied"),
        b"other"
    );
    assert!(config_of(&collection).contains("[[remote]]"));
}

#[test]
fn attaching_prints_what_it_copied() {
    let nas = Nas::new();
    let object = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    fs::create_dir_all(nas.below(&format!("objects/{object}"))).expect("dirs");
    fs::write(nas.below(&format!("objects/{object}/meta.1")), "other").expect("meta");
    let collection = nas.unattached("");
    let run = nas.antiq(&collection, &["remote", "add", &nas.url()]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let shown = run.stdout.replace(&nas.url(), "<remote>");
    insta::assert_snapshot!(shown, @r"
    Attached the ssh remote at <remote>
      id: 01BX5ZZKBKACTAV9WEVGEMMVRY
      metadata copied for 1 object
    ");
}

#[test]
fn a_dry_run_of_an_attach_plans_the_copy_and_writes_nothing() {
    let nas = Nas::new();
    let object = "01D39ZY06FGSCTVN4T2V9PKHFZ";
    fs::create_dir_all(nas.below(&format!("objects/{object}"))).expect("dirs");
    fs::write(nas.below(&format!("objects/{object}/meta.1")), "other").expect("meta");
    let collection = nas.unattached("");
    let before = (listing(nas.root.path()), config_of(&collection));
    let run = nas.antiq(&collection, &["remote", "add", "--dry-run", &nas.url()]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let shown = run.stdout.replace(&nas.url(), "<remote>");
    insta::assert_snapshot!(shown, @r"
    Would attach the remote at <remote>
    Would copy the metadata of 1 object
    ");
    assert_eq!((listing(nas.root.path()), config_of(&collection)), before);
    assert!(!collection.root().join(".antiq/lock").exists());
}

#[test]
fn a_dry_run_of_init_plans_the_root_and_writes_nothing() {
    let nas = Nas::new();
    let collection = nas.unattached("");
    let before = (listing(nas.root.path()), config_of(&collection));
    let run = nas.antiq(
        &collection,
        &[
            "remote",
            "add",
            "--init",
            "--dry-run",
            &nas.url_below("fresh"),
        ],
    );
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let shown = run.stdout.replace(&nas.url(), "<remote>");
    insta::assert_snapshot!(shown, @"Would create the remote at <remote>/fresh
    ");
    assert_eq!((listing(nas.root.path()), config_of(&collection)), before);
}

#[test]
fn a_dry_run_refuses_what_a_real_run_refuses() {
    let (_dir, fake) = fake_rsync("openrsync: protocol version 29\n");
    let nas = Nas::new();
    let collection = nas.unattached(&format!("remote_rsync = \"{}\"\n", fake.display()));
    let run = nas.antiq(
        &collection,
        &["--json", "remote", "add", "--dry-run", &nas.url()],
    );
    assert_eq!(
        run.code(),
        1,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(message(&run).contains("openrsync"), "{}", message(&run));
}

#[test]
fn a_second_remote_is_refused_before_ssh_is_called() {
    let nas = Nas::new();
    let collection = nas.unattached("");
    let first = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(
        first.code(),
        0,
        "stdout: {} stderr: {}",
        first.stdout,
        first.stderr
    );
    fs::remove_file(nas.state.path().join("calls.log")).expect("the first run called ssh");
    let before = config_of(&collection);
    let second = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(second.code(), 1);
    assert!(message(&second).contains("already"), "{}", message(&second));
    assert_eq!(nas.calls(), "");
    assert_eq!(config_of(&collection), before);
}

#[test]
fn a_host_that_ssh_would_read_as_an_option_is_a_usage_error_before_ssh_is_called() {
    let nas = Nas::new();
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], "ssh://-oProxyCommand=x/srv/antiq");
    assert_eq!(
        run.code(),
        2,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "usage");
    assert_eq!(nas.calls(), "");
}

#[test]
fn an_unresolvable_host_is_away_and_nothing_is_written() {
    let nas = Nas::new();
    nas.fault("resolve");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(
        run.code(),
        4,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "remote_away");
    assert!(!config_of(&collection).contains("[[remote]]"));
}

#[test]
fn a_local_rsync_the_config_names_but_that_is_missing_refuses() {
    let nas = Nas::new();
    let collection = nas.unattached("rsync = \"/nonexistent/rsync\"\n");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "other", "/nonexistent/rsync");
    assert_eq!(nas.calls(), "");
}

// ----- remote add: the state of the root and of the host -----

#[test]
fn a_nas_that_refuses_the_connection_is_away() {
    let nas = Nas::new();
    nas.fault("connect-refused");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(
        run.code(),
        4,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "remote_away");
    assert!(!config_of(&collection).contains("[[remote]]"));
}

#[test]
fn a_nas_that_refuses_the_login_is_broken() {
    let nas = Nas::new();
    nas.fault("permission-denied");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(
        run.code(),
        1,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "remote_broken");
    assert!(!config_of(&collection).contains("[[remote]]"));
}

#[test]
fn an_absent_root_without_init_is_away() {
    let nas = Nas::new();
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url_below("absent"));
    assert_eq!(
        run.code(),
        4,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "remote_away");
    assert_eq!(listing(nas.root.path()), ["antiq-remote.json"]);
}

#[test]
fn a_root_without_a_marker_is_broken_and_points_to_init() {
    let nas = Nas::markerless();
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "--init");
}

#[test]
fn a_root_that_is_a_file_is_broken() {
    let nas = Nas::new();
    fs::write(nas.below("plain"), "x").expect("a file");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url_below("plain"));
    assert_eq!(
        run.code(),
        1,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "remote_broken");
}

#[test]
fn a_marker_that_cannot_be_read_is_broken() {
    let nas = Nas::markerless();
    fs::write(nas.below("antiq-remote.json"), "not json").expect("marker");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(
        &nas,
        &collection,
        &run,
        "remote_broken",
        "antiq-remote.json",
    );
}

#[test]
fn a_marker_that_is_a_directory_is_broken() {
    let nas = Nas::markerless();
    fs::create_dir(nas.below("antiq-remote.json")).expect("a directory");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(
        &nas,
        &collection,
        &run,
        "remote_broken",
        "antiq-remote.json",
    );
}

// ----- remote add: the tools of the NAS -----

#[test]
fn a_nas_rsync_that_is_openrsync_is_refused_and_nothing_is_written() {
    let (_dir, fake) =
        fake_rsync("openrsync: protocol version 29\nrsync version 2.6.9 compatible\n");
    let nas = Nas::new();
    let collection = nas.unattached(&format!("remote_rsync = \"{}\"\n", fake.display()));
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "openrsync");
}

#[test]
fn a_nas_rsync_older_than_3_1_is_refused() {
    let (_dir, fake) = fake_rsync("rsync  version 3.0.9  protocol version 30\n");
    let nas = Nas::new();
    let collection = nas.unattached(&format!("remote_rsync = \"{}\"\n", fake.display()));
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "3.0.9");
}

#[test]
fn a_nas_without_the_rsync_the_config_names_is_refused() {
    let nas = Nas::new();
    let collection = nas.unattached("remote_rsync = \"/nonexistent/rsync\"\n");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "does not run on");
    assert!(
        message(&run).contains("/nonexistent/rsync"),
        "{}",
        message(&run)
    );
}

#[test]
fn a_remote_rsync_of_two_words_is_accepted() {
    let (_dir, fake) = fake_rsync("rsync  version 3.2.7  protocol version 31\n");
    let nas = Nas::new();
    let collection = nas.unattached(&format!(
        "remote_rsync = \"env FAKE=1 {}\"\n",
        fake.display()
    ));
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn a_nas_without_a_sha256_tool_is_refused() {
    let nas = Nas::new();
    nas.only(&BASIC_TOOLS);
    let collection = nas.unattached(&format!("remote_rsync = \"{}\"\n", real_rsync().display()));
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "SHA-256");
}

#[test]
fn a_sha256_tool_that_fails_does_not_count() {
    let nas = Nas::new();
    for tool in ["sha256sum", "shasum", "sha256", "openssl"] {
        nas.wrap(tool, "exit 1");
    }
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "SHA-256");
}

#[test]
fn a_sha256_tool_that_gets_abc_wrong_does_not_count() {
    let nas = Nas::new();
    let wrong = "0".repeat(64);
    for tool in ["sha256sum", "shasum", "sha256", "openssl"] {
        nas.wrap(tool, &format!("echo {wrong}"));
    }
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "SHA-256");
}

#[test]
fn a_nas_without_a_usable_stat_is_refused() {
    let nas = Nas::new();
    nas.wrap("stat", "exit 1");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "remote_broken", "stat");
}

// ----- remote add: what the writes prove -----

#[test]
fn a_failing_ln_refuses_an_attach_and_leaves_no_probe_file() {
    let nas = Nas::new();
    nas.wrap("ln", "exit 1");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "other", "hard link");
}

#[test]
fn a_failing_ln_refuses_init_and_removes_the_root_it_made() {
    let nas = Nas::new();
    nas.wrap("ln", "exit 1");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--init"], &nas.url_below("fresh"));
    assert_refused_and_untouched(&nas, &collection, &run, "other", "hard link");
}

#[test]
fn a_marker_that_cannot_be_linked_leaves_no_temp_and_no_root() {
    let nas = Nas::new();
    nas.wrap(
        "ln",
        "case \"$2\" in *antiq-remote.json) exit 1;; esac\nexec /bin/ln \"$@\"",
    );
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--init"], &nas.url_below("fresh"));
    assert_refused_and_untouched(&nas, &collection, &run, "other", "hard links do not work");
    assert!(
        message(&run).contains("antiq-remote.json"),
        "{}",
        message(&run)
    );
}

#[test]
fn a_link_that_is_no_regular_file_is_refused() {
    let nas = Nas::new();
    nas.wrap("ln", "exec /bin/ln -s \"$@\"");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_refused_and_untouched(&nas, &collection, &run, "other", "hard links do not work");
    assert!(message(&run).contains("regular file"), "{}", message(&run));
}

#[test]
fn a_dry_run_reports_the_session_warnings_too() {
    let nas = Nas::new();
    nas.fault("session-refused command");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--dry-run"], &nas.url());
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let warnings = run.json()["warnings"].to_string();
    assert!(warnings.contains("refused another session"), "{warnings}");
}

#[test]
fn a_host_key_alias_reaches_ssh_and_the_config() {
    let nas = Nas::new();
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--host-key-alias", "nas-alias"], &nas.url());
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(
        nas.calls().contains("HostKeyAlias=nas-alias"),
        "{}",
        nas.calls()
    );
    assert!(config_of(&collection).contains("host_key_alias = \"nas-alias\""));
}

#[test]
fn session_warnings_reach_the_report() {
    let nas = Nas::new();
    nas.fault("session-refused command");
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &[], &nas.url());
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let warnings = run.json()["warnings"].to_string();
    assert!(warnings.contains("refused another session"), "{warnings}");
}

// ----- remote show -----

fn show_config(nas: &Nas) -> TempCollection {
    let collection = TempCollection::minimal();
    fs::write(
        collection.root().join(".antiq/config.toml"),
        format!(
            "[collection]\nid = \"{COLLECTION_ID}\"\n\n[machine]\nrsync = \"{}\"\n\n[[remote]]\nurl = \"{}\"\nid = \"{REMOTE_ID}\"\n",
            real_rsync().display(),
            nas.url()
        ),
    )
    .expect("a config can be written");
    collection
}

#[test]
fn show_lists_what_it_probed() {
    let nas = Nas::new();
    let collection = show_config(&nas);
    let run = nas.antiq(&collection, &["--json", "remote", "show"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["kind"], "ssh");
    assert_eq!(json["state"], "reachable");
    let probed = json["probed"].as_object().expect("an object");
    let mut keys: Vec<&str> = probed.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["remote_rsync", "rsync", "sha256", "stat"]);
    assert!(
        probed["rsync"]
            .as_str()
            .is_some_and(|v| v.starts_with("3."))
    );
    assert!(
        probed["remote_rsync"]
            .as_str()
            .is_some_and(|v| v.starts_with("3."))
    );
    assert!(
        ["sha256sum", "shasum", "sha256", "openssl"]
            .contains(&probed["sha256"].as_str().expect("a tool")),
        "{probed:?}"
    );
    assert!(["gnu", "bsd"].contains(&probed["stat"].as_str().expect("a flavour")));
}

#[test]
fn show_prints_the_probed_tools() {
    let nas = Nas::new();
    let collection = show_config(&nas);
    let run = nas.antiq(&collection, &["remote", "show"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(lines[0].starts_with("remote: ssh ssh://nas/"), "{lines:?}");
    assert_eq!(lines[1], format!("  id: {REMOTE_ID}"));
    assert_eq!(lines[2], "  state: reachable");
    for (line, prefix) in
        lines[3..]
            .iter()
            .zip(["  rsync: ", "  remote rsync: ", "  sha256: ", "  stat: "])
    {
        assert!(line.starts_with(prefix), "{lines:?}");
    }
    assert_eq!(lines.len(), 7, "{lines:?}");
}

#[test]
fn show_reports_a_nas_that_is_away_with_nothing_probed_and_exit_0() {
    let nas = Nas::new();
    nas.fault("connect-refused");
    let collection = show_config(&nas);
    let run = nas.antiq(&collection, &["--json", "remote", "show"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["state"], "away");
    assert_eq!(json["probed"], serde_json::json!({}));
}

#[test]
fn show_reports_a_root_without_a_marker_as_broken_with_nothing_probed() {
    let nas = Nas::markerless();
    let collection = show_config(&nas);
    let run = nas.antiq(&collection, &["--json", "remote", "show"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["state"], "broken");
    assert_eq!(json["probed"], serde_json::json!({}));
}

#[test]
fn show_reports_an_absent_root_as_away() {
    let nas = Nas::new();
    let collection = show_config(&nas);
    fs::remove_file(nas.below("antiq-remote.json")).expect("marker");
    fs::remove_dir(nas.root.path()).ok();
    let run = nas.antiq(&collection, &["--json", "remote", "show"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.json()["state"], "away");
}

#[test]
fn show_names_a_marker_of_another_remote_as_broken() {
    let nas = Nas::new();
    let collection = show_config(&nas);
    fs::write(
        nas.below("antiq-remote.json"),
        "{\"format\":1,\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"layout\":1}\n",
    )
    .expect("marker");
    let run = nas.antiq(&collection, &["--json", "remote", "show"]);
    assert_eq!(run.json()["state"], "broken");
}

#[test]
fn show_reads_only_and_writes_nothing_on_the_nas() {
    let nas = Nas::new();
    let collection = show_config(&nas);
    let before = (listing(nas.root.path()), config_of(&collection));
    let run = nas.antiq(&collection, &["remote", "show"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!((listing(nas.root.path()), config_of(&collection)), before);
    assert!(!collection.root().join(".antiq/lock").exists());
    // One script over a connection of its own: no master is started.
    assert!(!nas.calls().contains("-M\t-N\t-f"), "{}", nas.calls());
}

#[test]
fn show_reports_a_nas_that_refuses_the_login_as_broken() {
    let nas = Nas::new();
    nas.fault("permission-denied");
    let collection = show_config(&nas);
    let run = nas.antiq(&collection, &["--json", "remote", "show"]);
    assert_eq!(
        run.code(),
        0,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    let json = run.json();
    assert_eq!(json["state"], "broken");
    assert_eq!(json["probed"], serde_json::json!({}));
}

#[test]
fn a_dry_run_whose_listing_fails_is_refused_and_writes_nothing() {
    let nas = Nas::new();
    // The probe reads the marker with the first `cat`; the listing that
    // follows checks the marker again and finds it unreadable.
    let once = nas.state.path().join("cat-called");
    nas.wrap(
        "cat",
        &format!(
            "if [ -e {0} ]; then exit 1; fi\n: > {0}\nexec /bin/cat \"$@\"",
            once.display()
        ),
    );
    let collection = nas.unattached("");
    let run = nas.remote_add(&collection, &["--dry-run"], &nas.url());
    assert_eq!(
        run.code(),
        1,
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "remote_broken");
}

// ----- against a real sshd -----

#[test]
#[ignore = "needs sshd, ssh-keygen and a loopback port on 127.0.0.1"]
fn a_real_sshd_takes_init_and_shows_the_remote() {
    assert_ne!(
        rustix::process::geteuid().as_raw(),
        0,
        "these tests assume a non-root user"
    );
    let sshd = Sshd::start().unwrap_or_else(|error| panic!("the real sshd cannot start: {error}"));
    let ssh_dir = sshd.program().parent().expect("a directory").to_path_buf();
    let path = format!(
        "{}:{}",
        ssh_dir.display(),
        std::env::var("PATH").expect("PATH")
    );
    let root = tempfile::tempdir().expect("temp dir");
    let url = format!("ssh://{}{}/antiq", sshd.host(), root.path().display());
    // The login shell of the server may find the macOS openrsync first.
    let collection = TempCollection::minimal();
    fs::write(
        collection.root().join(".antiq/config.toml"),
        format!(
            "[collection]\nid = \"{COLLECTION_ID}\"\n\n[machine]\nremote_rsync = \"{}\"\n",
            real_rsync().display()
        ),
    )
    .expect("a config can be written");
    let run_antiq = |args: &[&str]| {
        Antiq::new()
            .env("PATH", &path)
            .arg("-C")
            .arg(collection.root())
            .args(args)
            .run()
    };
    let added = run_antiq(&["--json", "remote", "add", "--init", &url]);
    assert_eq!(
        added.code(),
        0,
        "stdout: {} stderr: {}",
        added.stdout,
        added.stderr
    );
    assert!(root.path().join("antiq/antiq-remote.json").is_file());
    let shown = run_antiq(&["--json", "remote", "show"]);
    assert_eq!(
        shown.code(),
        0,
        "stdout: {} stderr: {}",
        shown.stdout,
        shown.stderr
    );
    assert_eq!(shown.json()["state"], "reachable");
}
