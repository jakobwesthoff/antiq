// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ssh session against a real `sshd` on 127.0.0.1 and the unmodified
//! `ssh`. The tests that start a server are opt-in:
//! `cargo test -- --include-ignored`.

mod support;

use antiq::remote::ssh::script::{EXCLUSIVE_CREATE, EXISTS_STATUS, Script};
use antiq::remote::ssh::session::{Mode, Output, Session, SessionError, SessionSettings, Target};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use support::sshd::Sshd;
use tempfile::TempDir;

const CEILING: Duration = Duration::from_secs(10);
const STEP: Duration = Duration::from_millis(50);

/// A directory under /tmp, since a socket path must stay short.
fn short_dir() -> TempDir {
    let dir = tempfile::Builder::new()
        .tempdir_in("/tmp")
        .expect("/tmp is writable");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("the directory is ours");
    dir
}

fn server() -> Sshd {
    assert_ne!(
        rustix::process::geteuid().as_raw(),
        0,
        "these tests assume a non-root user"
    );
    Sshd::start().unwrap_or_else(|error| panic!("the real sshd cannot start: {error}"))
}

fn settings(sshd: &Sshd, host: &str, control: &Path) -> SessionSettings {
    let mut settings = SessionSettings::new(Target::new(host));
    settings.program = sshd.program();
    settings.control_dir = control.to_path_buf();
    settings
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 output")
}

/// Polls until `done` holds. A test that waits for a process to end or a
/// file to appear has no event to wait on, so it polls in short steps up
/// to a ceiling.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < CEILING, "gave up waiting for {what}");
        std::thread::sleep(STEP);
    }
}

fn master_pid(sshd: &Sshd, session: &Session) -> i32 {
    let output = std::process::Command::new(sshd.program())
        .arg("-S")
        .arg(session.control_path())
        .args(["-O", "check", sshd.host()])
        .output()
        .expect("ssh runs");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    text.split("pid=")
        .nth(1)
        .and_then(|rest| rest.trim_end_matches([')', '\r', '\n']).parse().ok())
        .unwrap_or_else(|| panic!("no pid in {text:?}"))
}

fn descendants(pid: u32) -> Vec<i32> {
    let mut found = Vec::new();
    let mut queue = vec![pid.to_string()];
    while let Some(parent) = queue.pop() {
        let output = std::process::Command::new("pgrep")
            .args(["-P", &parent])
            .output()
            .expect("pgrep runs");
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(child) = line.trim().parse::<i32>() {
                found.push(child);
                queue.push(child.to_string());
            }
        }
    }
    found
}

// =========================================================
// The harness
// =========================================================

#[test]
fn a_missing_executable_is_named_with_every_candidate() {
    let error = support::sshd::find_executable(&["/no/such/sshd", "/no/such/other"])
        .expect_err("nothing exists there");
    assert!(error.contains("/no/such/sshd"), "{error}");
    assert!(error.contains("/no/such/other"), "{error}");
}

#[test]
fn an_existing_executable_is_found() {
    let found = support::sshd::find_executable(&["/no/such/sshd", "/bin/sh"]).expect("sh exists");
    assert_eq!(found, PathBuf::from("/bin/sh"));
}

// =========================================================
// Against the server
// =========================================================

#[test]
#[ignore = "needs sshd, ssh-keygen and a loopback port on 127.0.0.1"]
fn a_master_starts_and_runs_a_script_with_arguments() {
    let sshd = server();
    let control = short_dir();
    let session = Session::new(settings(&sshd, sshd.host(), control.path())).expect("session");
    session.begin_phase().expect("phase");
    assert_eq!(session.mode(), Mode::Master);
    let script = Script::new("printf '%s|' \"$@\"")
        .arg("a b")
        .arg("it's")
        .arg("x\ny");
    let output = session.run(&script).expect("runs");
    assert_eq!(stdout(&output), "a b|it's|x\ny|");
}

#[test]
#[ignore = "needs sshd, ssh-keygen and a loopback port on 127.0.0.1"]
fn a_second_exclusive_create_reports_that_the_file_exists() {
    let sshd = server();
    let control = short_dir();
    let session = Session::new(settings(&sshd, sshd.host(), control.path())).expect("session");
    session.begin_phase().expect("phase");
    let target = control.path().join("claim");
    let script = Script::new(EXCLUSIVE_CREATE).arg(target.as_os_str().as_encoded_bytes());
    assert_eq!(session.run(&script).expect("first").status, 0);
    assert_eq!(session.run(&script).expect("second").status, EXISTS_STATUS);
}

#[test]
#[ignore = "needs sshd, ssh-keygen and a loopback port on 127.0.0.1"]
fn a_killed_master_is_replaced_in_the_next_phase() {
    let sshd = server();
    let control = short_dir();
    let session = Session::new(settings(&sshd, sshd.host(), control.path())).expect("session");
    session.begin_phase().expect("phase");
    let pid = master_pid(&sshd, &session);
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).expect("a pid"),
        rustix::process::Signal::KILL,
    )
    .expect("kill");
    // The kernel closes the listening socket when the process ends, so a
    // refused connection says the master is gone.
    wait_until("the killed master to stop answering", || {
        UnixStream::connect(session.control_path()).is_err()
    });
    session.begin_phase().expect("second phase");
    assert_eq!(session.mode(), Mode::Master);
    assert_ne!(master_pid(&sshd, &session), pid);
    let output = session.run(&Script::new("echo again")).expect("runs");
    assert_eq!(stdout(&output), "again\n");
}

#[test]
#[ignore = "needs sshd, ssh-keygen and a loopback port on 127.0.0.1"]
fn a_control_path_of_86_bytes_works_with_the_real_ssh() {
    let sshd = server();
    // "<pid>-<n>.sock" grows by a digit when the session counter does, so
    // try the lengths it can have.
    let name_len = format!("{}-{}.sock", std::process::id(), 0).len();
    for name_len in [name_len, name_len + 1, name_len + 2] {
        let dir = PathBuf::from(format!("/tmp/{}", "z".repeat(86 - 1 - name_len - 5)));
        let Ok(session) = Session::new(settings(&sshd, sshd.host(), &dir)) else {
            let _ = std::fs::remove_dir(&dir);
            continue;
        };
        let fits = session.control_path().as_os_str().len() == 86;
        if fits {
            session.begin_phase().expect("phase");
            assert_eq!(session.mode(), Mode::Master, "86 bytes bind");
        }
        drop(session);
        std::fs::remove_dir_all(&dir).expect("cleanup");
        if fits {
            return;
        }
    }
    panic!("no directory name gave a control path of 86 bytes");
}

#[test]
#[ignore = "needs sshd, ssh-keygen and a loopback port on 127.0.0.1"]
fn a_closed_port_is_away_through_the_master_log() {
    let sshd = server();
    let control = short_dir();
    let mut settings = settings(&sshd, sshd.host(), control.path());
    settings.target = Target::new(sshd.host()).port(support::sshd::free_port().expect("a port"));
    let session = Session::new(settings).expect("session");
    match session.begin_phase() {
        Err(SessionError::Away(message)) => {
            assert!(message.contains("Connection refused"), "{message}")
        }
        other => panic!("expected Away, got {other:?}"),
    }
}

#[test]
#[ignore = "needs sshd, ssh-keygen and a loopback port on 127.0.0.1"]
fn a_refused_key_is_broken() {
    let sshd = server();
    let control = short_dir();
    let session =
        Session::new(settings(&sshd, sshd.host_with_wrong_key(), control.path())).expect("session");
    match session.begin_phase() {
        Err(SessionError::Broken(message)) => {
            assert!(message.contains("Permission denied"), "{message}")
        }
        other => panic!("expected Broken, got {other:?}"),
    }
}

#[test]
#[ignore = "needs sshd, ssh-keygen, pgrep and a loopback port on 127.0.0.1"]
fn a_server_that_stops_answering_is_away_through_the_master_log() {
    let sshd = server();
    let control = short_dir();
    let mut settings = settings(&sshd, sshd.host(), control.path());
    settings.server_alive_interval = 1;
    settings.server_alive_count = 2;
    let session = Session::new(settings).expect("session");
    session.begin_phase().expect("phase");
    let marker = control.path().join("started");
    let script =
        Script::new("touch \"$1\"; exec sleep 15").arg(marker.as_os_str().as_encoded_bytes());

    let result = std::thread::scope(|scope| {
        let running = scope.spawn(|| session.run(&script));
        // The script says it has started; only then is the server frozen.
        wait_until("the script to start", || marker.exists());
        let frozen = descendants(sshd.pid());
        assert!(!frozen.is_empty(), "the server has no session process");
        for pid in &frozen {
            let pid = rustix::process::Pid::from_raw(*pid).expect("a pid");
            rustix::process::kill_process(pid, rustix::process::Signal::STOP).expect("stop");
        }
        let result = running.join().expect("the run ends");
        for pid in &frozen {
            let pid = rustix::process::Pid::from_raw(*pid).expect("a pid");
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        result
    });
    match result {
        Err(SessionError::Away(message)) => {
            assert!(message.contains("not responding"), "{message}")
        }
        other => panic!("expected Away, got {other:?}"),
    }
}
