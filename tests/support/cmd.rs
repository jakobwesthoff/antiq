// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Running the antiq binary the way a script would.

use serde_json::Value;
use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use tempfile::TempDir;

/// A command that runs `program` in a session of its own.
///
/// Without a session the child keeps the controlling terminal of whoever
/// runs `cargo test`, and a command that asks a question would wait for a
/// keypress. In a session of its own, opening `/dev/tty` fails, so such a
/// command sees no terminal on a developer machine and in CI alike.
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

/// The antiq binary under test, with a home directory of its own.
pub struct Antiq {
    command: Command,
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
        Antiq {
            command,
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

    pub fn stdout(mut self, stdout: impl Into<Stdio>) -> Antiq {
        self.command.stdout(stdout);
        self
    }

    pub fn run(mut self) -> Run {
        let output = self.command.output().expect("the antiq binary starts");
        Run {
            status: output.status,
            stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
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

    /// The `message` of the error object of a `--json` run.
    pub fn error_message(&self) -> String {
        self.json()["error"]["message"]
            .as_str()
            .expect("the error object has a message")
            .to_string()
    }
}
