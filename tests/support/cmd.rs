// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Running the antiq binary the way a script would.

use serde_json::Value;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// =========================================================
// One fork at a time
// =========================================================

// A command with a `pre_exec` hook cannot use `posix_spawn`, so std forks.
// In a test binary with many threads, the fork child can abort in the
// atfork handler of libnotify ("os_once_t is corrupt") when another thread
// is inside the system frameworks at that moment. The first use of those
// frameworks in the process, such as the lookup of the machine name, has to
// happen before any fork, and no thread may be inside them while another
// forks. Both go through this lock.
static FORK: Mutex<()> = Mutex::new(());
static WARMED_UP: Once = Once::new();

fn fork_lock() -> MutexGuard<'static, ()> {
    // A test that panicked while it held the lock left nothing behind.
    let guard = FORK.lock().unwrap_or_else(PoisonError::into_inner);
    WARMED_UP.call_once(|| {
        let _ = antiq::machine::detect();
    });
    guard
}

/// The machine name antiq detects, asked while no test forks.
pub fn machine_name() -> String {
    let _guard = fork_lock();
    antiq::machine::detect().expect("the machine has a name")
}

/// A command that runs `program` in a session of its own.
///
/// Without a session the child keeps the controlling terminal of whoever
/// runs `cargo test`. In a session of its own, opening `/dev/tty` fails, so
/// the command sees no terminal on a developer machine and in CI alike.
pub fn detached(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    // SAFETY: the closure calls only `setsid`, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(io::Error::from)
        });
    }
    command
}

/// `PATH` with a stand-in `ssh` in front, which answers like a host that
/// does not resolve, so that no test reaches the network by accident. A
/// test that wants an ssh sets `PATH` itself.
fn path_without_real_ssh(home: &Path) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("the bin directory is made");
    let ssh = bin.join("ssh");
    std::fs::write(
        &ssh,
        "#!/bin/sh\necho 'ssh: Could not resolve hostname nas: Name or service not known' >&2\nexit 255\n",
    )
    .expect("the stand-in is written");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in is executable");
    let mut dirs = vec![bin];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(dirs).expect("no directory name holds a colon")
}

/// Removes the colour and locale settings of the developer from
/// `command` and sets the ones the tests expect. A test that wants another
/// setting sets it afterwards.
pub fn make_hermetic(command: &mut Command) {
    for name in [
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "CI",
        "LC_ALL",
        "LC_CTYPE",
        "LANG",
        // What the progress cards read to tell a terminal that lays old
        // lines out again from one that does not, so that the terminal of
        // the developer does not decide a test.
        "XTERM_VERSION",
        "STY",
        "INSIDE_EMACS",
        "VIM_TERMINAL",
        "TMUX",
        "VTE_VERSION",
        "KITTY_WINDOW_ID",
        "ALACRITTY_LOG",
        "TERM_PROGRAM",
    ] {
        command.env_remove(name);
    }
    command.env("TERM", "xterm-256color");
    command.env("LC_ALL", "en_US.UTF-8");
}

/// The antiq binary under test, with a home directory of its own.
pub struct Antiq {
    command: Command,
    stdout_set: bool,
    stderr_set: bool,
    _home: TempDir,
}

impl Antiq {
    pub fn new() -> Antiq {
        Antiq::program(env!("CARGO_BIN_EXE_antiq"))
    }

    /// Any program, started with the same environment as the binary: the
    /// parent's, so that coverage instrumentation finds its profile file,
    /// with a home directory of its own.
    pub fn program(program: impl AsRef<OsStr>) -> Antiq {
        let home = tempfile::tempdir().expect("the temp dir is writable");
        let mut command = detached(program);
        command.env("HOME", home.path());
        command.env("PATH", path_without_real_ssh(home.path()));
        make_hermetic(&mut command);
        Antiq {
            command,
            stdout_set: false,
            stderr_set: false,
            _home: home,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Antiq {
        self.command.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Antiq
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Antiq {
        self.command.current_dir(dir);
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Antiq {
        self.command.env(key, value);
        self
    }

    /// Removes `key` from the environment the command inherits.
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Antiq {
        self.command.env_remove(key);
        self
    }

    /// Runs the command with the file mode creation mask `mask`, which
    /// every program it starts inherits.
    pub fn umask(mut self, mask: u32) -> Antiq {
        // SAFETY: the closure calls only `umask`, which is async-signal-safe.
        unsafe {
            self.command.pre_exec(move || {
                rustix::process::umask(rustix::fs::Mode::from_bits_truncate(
                    mask as rustix::fs::RawMode,
                ));
                Ok(())
            });
        }
        self
    }

    pub fn stdout(mut self, stdout: impl Into<Stdio>) -> Antiq {
        self.command.stdout(stdout);
        self.stdout_set = true;
        self
    }

    pub fn stderr(mut self, stderr: impl Into<Stdio>) -> Antiq {
        self.command.stderr(stderr);
        self.stderr_set = true;
        self
    }

    pub fn run(mut self) -> Run {
        if !self.stdout_set {
            self.command.stdout(Stdio::piped());
        }
        if !self.stderr_set {
            self.command.stderr(Stdio::piped());
        }
        // The lock is held for the fork only, the child runs on without it.
        let child = {
            let _guard = fork_lock();
            self.command.spawn().expect("the antiq binary starts")
        };
        let output = child.wait_with_output().expect("the antiq binary ends");
        Run {
            status: output.status,
            stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
    }
}

impl Antiq {
    /// Like `run`, but kills the command when it has not finished after
    /// `limit` and returns `None`, so that a command that blocks fails a
    /// test instead of hanging the suite. Output that `stdout` sent
    /// elsewhere is not captured.
    pub fn run_within(mut self, limit: Duration) -> Option<Run> {
        if !self.stdout_set {
            self.command.stdout(Stdio::piped());
        }
        if !self.stderr_set {
            self.command.stderr(Stdio::piped());
        }
        let mut child = {
            let _guard = fork_lock();
            self.command.spawn().expect("the antiq binary starts")
        };
        let drain = |stream: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut stream) = stream {
                    stream.read_to_end(&mut bytes).expect("the pipe reads");
                }
                String::from_utf8(bytes).expect("the output is UTF-8")
            })
        };
        let stdout = drain(child.stdout.take().map(|s| Box::new(s) as _));
        let stderr = drain(child.stderr.take().map(|s| Box::new(s) as _));
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("the child can be polled") {
                break Some(status);
            }
            if started.elapsed() > limit {
                child.kill().expect("the child can be killed");
                child.wait().expect("the killed child is reaped");
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = stdout.join().expect("the reader does not panic");
        let stderr = stderr.join().expect("the reader does not panic");
        status.map(|status| Run {
            status,
            stdout,
            stderr,
        })
    }
}

/// What a finished run left behind.
pub struct Run {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn code(&self) -> i32 {
        self.status
            .code()
            .expect("the binary exits instead of dying on a signal")
    }

    /// Stdout as the one JSON object a `--json` run prints.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {:?}", self.stdout))
    }

    /// The `kind` of the error object of a `--json` run.
    pub fn error_kind(&self) -> String {
        self.json()["error"]["kind"]
            .as_str()
            .expect("the error object has a kind")
            .to_string()
    }

    /// The `fix` of the JSON error object: the help of the explanation.
    pub fn error_fix(&self) -> String {
        self.json()["error"]["fix"]
            .as_str()
            .expect("the error object has a fix")
            .to_string()
    }

    /// The message of the JSON error object and, when it has one, its fix
    /// below it.
    pub fn error_text(&self) -> String {
        match self.json()["error"]["fix"].as_str() {
            Some(fix) => format!("{}\n{fix}", self.error_message()),
            None => self.error_message(),
        }
    }

    /// The `message` of the error object of a `--json` run.
    pub fn error_message(&self) -> String {
        self.json()["error"]["message"]
            .as_str()
            .expect("the error object has a message")
            .to_string()
    }
}

/// Holds the run lock of the collection at `root` in this process.
pub fn hold_lock(root: &Path) -> antiq::lock::RunLock {
    antiq::lock::acquire(
        &root.join(".antiq/lock"),
        "imperius",
        &antiq::clock::SystemClock,
    )
    .expect("the lock is free")
}

// Not every test crate that includes this file compares snapshots.
#[allow(unused_imports)]
pub use super::tree::snapshot;
