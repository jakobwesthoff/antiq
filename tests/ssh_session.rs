// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ssh session against the loopback shim, `tests/fixtures/ssh-shim.sh`.

mod support;

use antiq::remote::ssh::script::{EXCLUSIVE_CREATE, EXISTS_STATUS, Script};
use antiq::remote::ssh::session::{Mode, Session, SessionError, SessionSettings, Target};
use rustix::fs::FlockOperation;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A directory under /tmp, since a socket path must stay short.
fn short_dir() -> TempDir {
    let dir = tempfile::Builder::new()
        .tempdir_in("/tmp")
        .expect("/tmp is writable");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("the directory is ours");
    dir
}

/// The shim, reached the way production reaches ssh: through a program
/// path. The state directory holds the symlink named `ssh`.
struct Shim {
    state: TempDir,
    control: TempDir,
}

impl Shim {
    fn new() -> Shim {
        let perl = std::process::Command::new("perl").arg("-v").output();
        assert!(
            perl.is_ok_and(|output| output.status.success()),
            "the ssh shim needs perl to run a master process"
        );
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "these tests assume a non-root user"
        );
        let shim = Shim {
            state: short_dir(),
            control: short_dir(),
        };
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-shim.sh");
        std::os::unix::fs::symlink(script, shim.state.path().join("ssh")).expect("symlink");
        shim
    }

    fn settings(&self) -> SessionSettings {
        let mut settings = SessionSettings::new(Target::new("nas"));
        settings.program = self.state.path().join("ssh");
        settings.control_dir = self.control.path().to_path_buf();
        settings
    }

    fn session(&self) -> Session {
        Session::new(self.settings()).expect("session")
    }

    fn fault(&self, name: &str, on: &str) {
        std::fs::write(self.state.path().join("fault"), format!("{name} {on}\n"))
            .expect("write fault");
    }

    fn clear_fault(&self) {
        std::fs::remove_file(self.state.path().join("fault")).expect("remove fault");
    }

    /// One entry per call of the shim, one string per argument.
    fn calls(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(self.state.path().join("calls.log")).unwrap_or_default();
        text.lines()
            .map(|line| {
                line.split('\t')
                    .filter(|arg| !arg.is_empty())
                    .map(String::from)
                    .collect()
            })
            .collect()
    }

    /// What each call was: `start`, `check`, `exit` or `cmd`.
    fn kinds(&self) -> Vec<&'static str> {
        self.calls().iter().map(|call| kind(call)).collect()
    }

    fn master_pid(&self) -> i32 {
        std::fs::read_to_string(self.state.path().join("master.pid"))
            .expect("a master was started")
            .trim()
            .parse()
            .expect("a pid")
    }

    fn master_lock(&self) -> std::fs::File {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state.path().join("master.lock"))
            .expect("open the lock")
    }

    /// Returns once the master process has exited: the kernel releases its
    /// lock when the process ends, whether or not anyone has reaped it.
    fn wait_for_master_exit(&self) {
        let lock = self.master_lock();
        rustix::fs::flock(&lock, FlockOperation::LockExclusive).expect("lock");
    }

    fn master_is_gone(&self) -> bool {
        rustix::fs::flock(self.master_lock(), FlockOperation::NonBlockingLockExclusive).is_ok()
    }

    fn leaked(&self) -> bool {
        self.state.path().join("leaked").exists()
    }
}

fn kind(call: &[String]) -> &'static str {
    if call.iter().any(|arg| arg == "-M") {
        "start"
    } else if let Some(at) = call.iter().position(|arg| arg == "-O") {
        if call[at + 1] == "check" {
            "check"
        } else {
            "exit"
        }
    } else {
        "cmd"
    }
}

fn run(session: &Session, body: &str) -> antiq::remote::ssh::session::Output {
    session.run(&Script::new(body)).expect("the script runs")
}

fn stdout(output: &antiq::remote::ssh::session::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 output")
}

// =========================================================
// Starting the master
// =========================================================

#[test]
fn a_phase_starts_a_master_and_checks_it() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    assert_eq!(session.mode(), Mode::Master);
    assert_eq!(shim.kinds(), ["check", "exit", "start", "check"]);
    assert!(session.control_path().exists());
}

#[test]
fn the_master_start_carries_every_option() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let calls = shim.calls();
    let start = calls
        .iter()
        .find(|call| kind(call) == "start")
        .expect("a start");
    let joined = start.join(" ");
    for expected in [
        "-o BatchMode=yes",
        "-o ConnectTimeout=30",
        "-o ServerAliveInterval=15",
        "-o ServerAliveCountMax=4",
        "-o LogLevel=INFO",
        "-o ClearAllForwardings=yes",
        "-o PermitLocalCommand=no",
        "-M -N -f",
        "-o ControlPersist=60",
        "-- nas",
    ] {
        assert!(joined.contains(expected), "{expected} missing in {joined}");
    }
    assert!(joined.contains(&format!("-S {}", session.control_path().display())));
    assert!(joined.contains(&format!("-E {}", session.log_path().display())));
}

#[test]
fn a_command_goes_over_the_master_with_every_option() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    run(&session, "true");
    let calls = shim.calls();
    let command = calls.last().expect("a call");
    let joined = command.join(" ");
    for expected in [
        "-o BatchMode=yes",
        "-o LogLevel=INFO",
        "-T",
        "-o RemoteCommand=none",
        "-o ControlMaster=no",
        "-- nas /bin/sh -s",
    ] {
        assert!(joined.contains(expected), "{expected} missing in {joined}");
    }
    assert!(joined.contains(&format!("-S {}", session.control_path().display())));
}

// =========================================================
// Running scripts
// =========================================================

#[test]
fn a_script_gets_its_values_as_arguments() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let script = Script::new("printf '%s|' \"$@\"")
        .arg("a b")
        .arg("it's")
        .arg("x\ny");
    let output = session.run(&script).expect("runs");
    assert_eq!(output.status, 0);
    assert_eq!(stdout(&output), "a b|it's|x\ny|");
}

#[test]
fn the_exit_status_of_the_script_is_returned_not_an_error() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let output = run(&session, "echo out; echo err >&2; exit 3");
    assert_eq!(output.status, 3);
    assert_eq!(stdout(&output), "out\n");
    assert_eq!(output.stderr, b"err\n");
}

#[test]
fn a_second_exclusive_create_reports_that_the_file_exists() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let target = shim.control.path().join("claim");
    let script = Script::new(EXCLUSIVE_CREATE).arg(target.as_os_str().as_encoded_bytes());
    assert_eq!(session.run(&script).expect("first").status, 0);
    assert_eq!(session.run(&script).expect("second").status, EXISTS_STATUS);
}

#[test]
fn the_clock_comes_first_and_the_rest_follows() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let script = Script::new("echo body").with_clock();
    let output = session.run(&script).expect("runs");
    let (clock, rest) = output.clock_and_rest().expect("a clock line");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs() as i64;
    assert!((now - clock).abs() < 60, "clock {clock} against {now}");
    assert_eq!(rest, b"body\n");
}

#[test]
fn a_large_script_and_a_large_answer_do_not_block_each_other() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let mut body = String::new();
    while body.len() < 1 << 20 {
        body.push_str("# padding that has to go through the pipe before the script ends\n");
    }
    body.push_str("head -c 1048576 /dev/zero | tr '\\0' 'a'\n");
    let output = run(&session, &body);
    assert_eq!(output.stdout.len(), 1 << 20);
}

// =========================================================
// A master that dies, a control path that is taken
// =========================================================

#[test]
fn a_killed_master_is_replaced_in_the_next_phase() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let pid = rustix::process::Pid::from_raw(shim.master_pid()).expect("a pid");
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("kill");
    shim.wait_for_master_exit();
    let before = shim.calls().len();

    session.begin_phase().expect("second phase");

    let kinds: Vec<&str> = shim.kinds().split_off(before);
    assert_eq!(kinds, ["check", "exit", "start", "check"]);
    assert_eq!(session.mode(), Mode::Master);
    assert_ne!(shim.master_pid(), pid.as_raw_nonzero().get());
    run(&session, "true");
    assert!(
        shim.calls()
            .last()
            .expect("a call")
            .contains(&"-S".to_string())
    );
}

#[test]
fn a_healthy_master_is_only_checked_in_the_next_phase() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let before = shim.calls().len();
    session.begin_phase().expect("second phase");
    assert_eq!(shim.kinds().split_off(before), ["check"]);
    assert_eq!(session.mode(), Mode::Master);
}

#[test]
fn a_file_at_the_control_path_is_left_alone_and_no_master_starts() {
    let shim = Shim::new();
    let session = shim.session();
    std::fs::write(session.control_path(), b"not a socket").expect("write");

    session.begin_phase().expect("phase");

    assert_eq!(shim.kinds(), ["check", "exit"]);
    assert_eq!(session.mode(), Mode::Direct);
    assert_eq!(
        std::fs::read(session.control_path()).expect("still there"),
        b"not a socket"
    );
    let warnings = session.take_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].message.contains("already exists"),
        "{:?}",
        warnings[0]
    );
    assert!(!shim.leaked());
}

#[test]
fn commands_run_directly_after_the_fallback_and_say_so_in_their_arguments() {
    let shim = Shim::new();
    let session = shim.session();
    std::fs::write(session.control_path(), b"x").expect("write");
    session.begin_phase().expect("phase");
    assert_eq!(stdout(&run(&session, "echo direct")), "direct\n");
    let calls = shim.calls();
    let joined = calls.last().expect("a call").join(" ");
    assert!(joined.contains("-o ControlPath=none"), "{joined}");
    assert!(!joined.contains("-S "), "{joined}");
}

#[test]
fn a_master_that_does_not_come_up_falls_back_with_a_warning() {
    let shim = Shim::new();
    shim.fault("no-master", "start");
    let session = shim.session();
    session.begin_phase().expect("phase");
    assert_eq!(shim.kinds(), ["check", "exit", "start", "check"]);
    assert_eq!(session.mode(), Mode::Direct);
    let warnings = session.take_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].message.contains("direct connections"),
        "{:?}",
        warnings[0]
    );
    assert_eq!(stdout(&run(&session, "echo ok")), "ok\n");
}

#[test]
fn a_phase_after_a_fallback_tries_for_a_master_again() {
    let shim = Shim::new();
    shim.fault("no-master", "start");
    let session = shim.session();
    session.begin_phase().expect("phase");
    assert_eq!(session.mode(), Mode::Direct);
    shim.clear_fault();
    session.begin_phase().expect("second phase");
    assert_eq!(session.mode(), Mode::Master);
}

// =========================================================
// Away and broken
// =========================================================

fn start_failure(fault: &str) -> SessionError {
    let shim = Shim::new();
    shim.fault(fault, "start");
    let session = shim.session();
    let error = session.begin_phase().expect_err("the master cannot start");
    assert_eq!(
        shim.kinds().iter().filter(|kind| **kind == "start").count(),
        1,
        "no second attempt"
    );
    error
}

#[test]
fn a_refused_connection_at_start_is_away() {
    assert!(matches!(
        start_failure("connect-refused"),
        SessionError::Away(_)
    ));
}

#[test]
fn an_unresolvable_name_at_start_is_away() {
    assert!(matches!(start_failure("resolve"), SessionError::Away(_)));
}

#[test]
fn a_timeout_at_start_is_away() {
    assert!(matches!(start_failure("timeout"), SessionError::Away(_)));
}

#[test]
fn a_refused_key_at_start_is_broken() {
    assert!(matches!(
        start_failure("permission-denied"),
        SessionError::Broken(_)
    ));
}

fn command_failure(fault: &str) -> SessionError {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    shim.fault(fault, "command");
    session
        .run(&Script::new("true"))
        .expect_err("the command fails")
}

#[test]
fn a_refused_connection_in_a_command_is_away() {
    assert!(matches!(
        command_failure("connect-refused"),
        SessionError::Away(_)
    ));
}

#[test]
fn a_refused_key_in_a_command_is_broken() {
    assert!(matches!(
        command_failure("permission-denied"),
        SessionError::Broken(_)
    ));
}

#[test]
fn a_failure_over_a_direct_connection_is_classified_from_its_stderr() {
    let shim = Shim::new();
    let session = shim.session();
    std::fs::write(session.control_path(), b"x").expect("write");
    session.begin_phase().expect("phase");
    assert_eq!(session.mode(), Mode::Direct);
    shim.fault("connect-refused", "command");
    assert!(matches!(
        session.run(&Script::new("true")),
        Err(SessionError::Away(_))
    ));
}

#[test]
fn ssh_ended_by_a_signal_is_broken() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    shim.fault("die", "command");
    match session.run(&Script::new("true")) {
        Err(SessionError::Broken(message)) => assert!(message.contains("signal"), "{message}"),
        other => panic!("expected Broken, got {other:?}"),
    }
}

#[test]
fn a_program_that_does_not_exist_is_an_io_error() {
    let shim = Shim::new();
    let mut settings = shim.settings();
    settings.program = shim.state.path().join("no-such-ssh");
    let session = Session::new(settings).expect("session");
    assert!(matches!(
        session.begin_phase(),
        Err(SessionError::Io { .. })
    ));
    assert!(matches!(
        session.run(&Script::new("true")),
        Err(SessionError::Io { .. })
    ));
}

#[test]
fn a_master_that_fails_without_saying_why_falls_back() {
    let shim = Shim::new();
    let mut settings = shim.settings();
    settings.program = PathBuf::from("/usr/bin/false");
    let session = Session::new(settings).expect("session");
    session.begin_phase().expect("phase");
    assert_eq!(session.mode(), Mode::Direct);
    let warnings = session.take_warnings();
    assert!(
        warnings[0].message.contains("did not come up"),
        "{:?}",
        warnings[0]
    );
}

#[test]
fn a_script_that_cannot_be_quoted_is_refused_before_ssh_runs() {
    let shim = Shim::new();
    let session = shim.session();
    let script = Script::new("true").arg(b"a\0b");
    assert!(matches!(session.run(&script), Err(SessionError::Script(_))));
    assert!(shim.calls().is_empty());
}

#[test]
fn a_script_that_exits_255_is_broken_without_a_message() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    match session.run(&Script::new("exit 255")) {
        Err(SessionError::Broken(message)) => assert!(message.contains("no message"), "{message}"),
        other => panic!("expected Broken, got {other:?}"),
    }
}

#[test]
fn a_master_that_loses_its_server_reports_away_from_its_log() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    shim.fault("master-drop", "command");
    match session.run(&Script::new("true")) {
        Err(SessionError::Away(message)) => {
            assert!(message.contains("not responding"), "{message}")
        }
        other => panic!("expected Away, got {other:?}"),
    }
}

#[test]
fn only_the_log_lines_written_during_a_run_count() {
    // An old keepalive timeout in the log must not turn a later, unrelated
    // 255 into "away".
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    let mut log = std::fs::read(session.log_path()).unwrap_or_default();
    log.extend_from_slice(b"Timeout, server 127.0.0.1 not responding.\n");
    std::fs::write(session.log_path(), log).expect("write log");
    match session.run(&Script::new("exit 255")) {
        Err(SessionError::Broken(_)) => {}
        other => panic!("expected Broken, got {other:?}"),
    }
}

#[test]
fn a_refused_session_gives_one_warning_per_run() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    shim.fault("session-refused", "command");
    assert_eq!(run(&session, "echo one").status, 0);
    assert_eq!(run(&session, "echo two").status, 0);
    let warnings = session.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].message.contains("refused another session"),
        "{:?}",
        warnings[0]
    );
}

#[test]
fn a_session_warning_is_given_once_even_after_the_warnings_were_taken() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    shim.fault("session-refused", "command");
    run(&session, "true");
    assert_eq!(session.take_warnings().len(), 1);
    run(&session, "true");
    assert!(session.take_warnings().is_empty());
}

// =========================================================
// The end of the run
// =========================================================

#[test]
fn finishing_stops_the_master_and_removes_the_socket() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    session.finish();
    assert!(shim.master_is_gone());
    assert!(!session.control_path().exists());
}

#[test]
fn finishing_twice_asks_the_master_once() {
    let shim = Shim::new();
    let session = shim.session();
    session.begin_phase().expect("phase");
    session.finish();
    let calls = shim.calls().len();
    session.finish();
    assert_eq!(shim.calls().len(), calls);
}

#[test]
fn finishing_without_a_master_asks_for_nothing() {
    let shim = Shim::new();
    let session = shim.session();
    session.finish();
    assert!(shim.calls().is_empty());
}

#[test]
fn dropping_a_session_stops_its_master() {
    let shim = Shim::new();
    {
        let session = shim.session();
        session.begin_phase().expect("phase");
    }
    assert!(shim.master_is_gone());
}

#[test]
fn a_path_that_is_too_long_never_reaches_ssh() {
    let shim = Shim::new();
    let mut settings = shim.settings();
    settings.control_dir = PathBuf::from(format!("/tmp/{}", "d".repeat(90)));
    assert!(matches!(
        Session::new(settings),
        Err(SessionError::ControlPathTooLong { .. })
    ));
    assert!(shim.calls().is_empty());
}
