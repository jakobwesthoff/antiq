// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Worlds: a collection, optionally a target, and a way to run commands
//! against them in text or JSON mode.

use super::collection::{Clock, Collection};
use super::envelope;
use super::state::State;
use super::target::Target;
use crate::support::cmd::Antiq;
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread::JoinHandle;
use std::time::Duration;

/// The time zone every command runs in, so that output which depends on
/// the local time reads the same on every machine.
/// How long a command on a terminal may run.
const TTY_DEADLINE: Duration = Duration::from_secs(20);

const TIME_ZONE: &str = "Pacific/Auckland";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Text,
    Json,
}

/// What a scenario starts with: a collection, optionally a target, and
/// optionally another program in place of the antiq binary.
pub struct Setup {
    collection: Collection,
    target: Option<Target>,
    program: Option<PathBuf>,
}

impl Setup {
    pub fn new(collection: Collection) -> Setup {
        Setup {
            collection,
            target: None,
            program: None,
        }
    }

    /// Adds the target and writes its path as the collection's
    /// `[target] url`. A scenario that wants a target without a configured
    /// URL keeps it out of the setup and builds it itself.
    pub fn with_target(mut self, target: Target) -> Setup {
        self.collection.use_target(&target.path());
        self.target = Some(target);
        self
    }

    /// Runs `program` instead of the antiq binary, started the same way.
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Setup {
        self.program = Some(program.into());
        self
    }
}

pub struct World {
    collection: Collection,
    target: Option<Target>,
    program: Option<PathBuf>,
    mode: Mode,
}

/// What one run printed and how it ended.
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub mode: Mode,
    code: i32,
    value: Option<Value>,
}

impl World {
    pub fn new(setup: Setup, mode: Mode) -> World {
        World {
            collection: setup.collection,
            target: setup.target,
            program: setup.program,
            mode,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn collection(&self) -> &Collection {
        &self.collection
    }

    pub fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

    /// Runs the command against the world's collection (`-C <root>`).
    pub fn run(&self, args: &[&str]) -> Outcome {
        self.execute(Some(&self.collection), None, args, None)
    }

    /// Runs the command with `dir` as the current directory and no `-C`:
    /// a piece given as a path, or the collection found from where the
    /// command runs.
    pub fn run_in(&self, dir: &Path, args: &[&str]) -> Outcome {
        self.execute(None, Some(dir), args, None)
    }

    /// Runs the command against another collection, such as a copy back
    /// from the target.
    pub fn run_as(&self, other: &Collection, args: &[&str]) -> Outcome {
        self.execute(Some(other), None, args, None)
    }

    /// Runs the command with stdout on a terminal, for output that differs
    /// between a terminal and a pipe, such as local times. The run is in
    /// text mode whatever the world's mode is, and nothing checks it as an
    /// envelope. The terminal's `\r\n` line ends come back as `\n`.
    pub fn run_tty(&self, args: &[&str]) -> Outcome {
        self.run_tty_within(args, TTY_DEADLINE)
    }

    /// `run_tty` with a deadline of its own. A command that has not
    /// finished by then is killed and the run fails.
    pub fn run_tty_within(&self, args: &[&str], limit: Duration) -> Outcome {
        let mut terminal = Terminal::open();
        let slave = terminal.slave();
        let mut outcome = self.execute(Some(&self.collection), None, args, Some((slave, limit)));
        outcome.stdout = terminal.finish();
        outcome
    }

    /// Runs `script` and fails when it changed the collection or the
    /// target in any way: for a dry run.
    pub fn assert_unchanged(&self, script: impl FnOnce()) {
        let before = self.raw_states();
        script();
        let after = self.raw_states();
        for (label, before, after) in [
            ("collection", &before.0, &after.0),
            ("target", &before.1, &after.1),
        ] {
            if let Some(diff) = before.diff(after) {
                panic!("the commands changed the {label}:\n{diff}");
            }
        }
    }

    fn raw_states(&self) -> (State, State) {
        let target = match &self.target {
            Some(target) if target.path().exists() => State::raw(&target.path()),
            _ => State::empty(),
        };
        (State::raw(&self.collection.root()), target)
    }

    /// The collection and the target as they are, with what differs
    /// between two worlds replaced, for the comparison of the modes.
    fn end_states(&self) -> (State, State) {
        let collection_root = self.collection.root();
        let mut roots = vec![(collection_root.as_path(), "<root>")];
        let target_root = self.target.as_ref().map(Target::path);
        if let Some(root) = &target_root {
            roots.push((root.as_path(), "<target>"));
        }
        let target = match &target_root {
            Some(root) if root.exists() => State::normalised(root, &roots),
            _ => State::empty(),
        };
        (State::normalised(&collection_root, &roots), target)
    }

    fn execute(
        &self,
        collection: Option<&Collection>,
        current_dir: Option<&Path>,
        args: &[&str],
        terminal: Option<(Stdio, Duration)>,
    ) -> Outcome {
        let mode = if terminal.is_some() {
            Mode::Text
        } else {
            self.mode
        };
        let mut full: Vec<String> = Vec::new();
        if let Some(collection) = collection {
            full.push("-C".to_string());
            full.push(
                collection
                    .root()
                    .to_str()
                    .expect("the path is UTF-8")
                    .to_string(),
            );
        }
        if mode == Mode::Json {
            full.push("--json".to_string());
        }
        full.extend(args.iter().map(|arg| arg.to_string()));

        let antiq = match &self.program {
            Some(program) => Antiq::program(program),
            None => Antiq::new(),
        };
        let mut antiq = antiq.args(&full).env("TZ", TIME_ZONE);
        if let Some(dir) = current_dir {
            antiq = antiq.current_dir(dir);
        }
        let run = match terminal {
            Some((slave, limit)) => antiq.stdout(slave).run_within(limit).unwrap_or_else(|| {
                panic!("{full:?} did not finish within {limit:?} on a terminal and was killed")
            }),
            None => antiq.run(),
        };

        // Directory times change with every file a command creates, and
        // archives and listings record them.
        self.collection.settle();
        if let Some(collection) = collection {
            collection.settle();
        }

        let value = match mode {
            Mode::Text => None,
            Mode::Json => {
                if let Err(reason) = envelope::check(&full, &run.stdout, run.code()) {
                    panic!(
                        "the JSON output of {full:?} is wrong: {reason}\nstdout: {:?}\nstderr: {:?}",
                        run.stdout, run.stderr
                    );
                }
                serde_json::from_str(&run.stdout).ok()
            }
        };
        Outcome {
            code: run.code(),
            stdout: run.stdout,
            stderr: run.stderr,
            mode,
            value,
        }
    }
}

impl Outcome {
    pub fn code(&self) -> i32 {
        self.code
    }

    pub fn expect_code(&self, code: i32) -> &Outcome {
        assert_eq!(
            self.code, code,
            "expected exit {code}, got {}\nstdout: {:?}\nstderr: {:?}",
            self.code, self.stdout, self.stderr
        );
        self
    }

    /// Runs `check` on the parsed output in JSON mode and does nothing in
    /// text mode, so one scenario states the expectations of both.
    pub fn json_only(&self, check: impl FnOnce(&Value)) -> &Outcome {
        if self.mode == Mode::Json {
            check(self.value.as_ref().expect("JSON mode prints a JSON object"));
        }
        self
    }

    /// Runs `check` on stdout in text mode and does nothing in JSON mode.
    pub fn text_only(&self, check: impl FnOnce(&str)) -> &Outcome {
        if self.mode == Mode::Text {
            check(&self.stdout);
        }
        self
    }

    /// In JSON mode, asserts the keys of the object in their order. The
    /// key order is part of the output.
    pub fn keys(&self, keys: &[&str]) -> &Outcome {
        self.json_only(|value| {
            let actual: Vec<&str> = value
                .as_object()
                .expect("the output is an object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                actual, keys,
                "the keys of the JSON object differ from the expected ones"
            );
        })
    }

    /// Stdout without colour sequences, for terminal output asserted by
    /// its text.
    pub fn plain(&self) -> String {
        strip_colour(&self.stdout)
    }

    /// The `kind` of the error object; `None` in text mode and without an
    /// error.
    pub fn error_kind(&self) -> Option<String> {
        let value = self.value.as_ref()?;
        value["error"]["kind"].as_str().map(str::to_string)
    }
}

/// `text` without the `ESC [ ... m` sequences that colour it.
pub fn strip_colour(text: &str) -> String {
    let mut plain = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("\x1b[") {
        plain.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        rest = match after.find('m') {
            Some(end) => &after[end + 1..],
            None => "",
        };
    }
    plain.push_str(rest);
    plain
}

/// Builds the scenario's world once per mode from one clock, runs `script`
/// in each and fails when the end states differ between the modes: the
/// collection and the target, with what is allowed to differ replaced.
pub fn both_modes(build: impl Fn(&Clock) -> Setup, script: impl Fn(&World)) {
    let clock = Clock::now();
    let mut states = Vec::new();
    for mode in [Mode::Text, Mode::Json] {
        let world = World::new(build(&clock), mode);
        script(&world);
        states.push(world.end_states());
    }
    let (json, text) = (states.remove(1), states.remove(0));
    let mut report = String::new();
    for (label, text, json) in [
        ("collection", &text.0, &json.0),
        ("target", &text.1, &json.1),
    ] {
        if let Some(diff) = text.diff(json) {
            report.push_str(&format!("in the {label}:\n{diff}\n"));
        }
    }
    assert!(
        report.is_empty(),
        "the end states of the text run (first) and the JSON run (second) differ\n{report}"
    );
}

/// A pseudo-terminal pair. A reader thread drains the master end while the
/// command runs, as a command that prints more than the terminal buffer
/// holds would otherwise block.
struct Terminal {
    slave: Option<OwnedFd>,
    reader: JoinHandle<Vec<u8>>,
}

impl Terminal {
    fn open() -> Terminal {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: both out-pointers refer to live integers, and the three
        // optional arguments (name, termios, window size) are null.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "openpty failed");
        // SAFETY: `openpty` returned two new descriptors that nothing else owns.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let mut master = File::from(master);
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            // The read ends with end of file or, once every slave
            // descriptor is closed, with an I/O error on some systems.
            while let Ok(count) = master.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            bytes
        });
        Terminal {
            slave: Some(slave),
            reader,
        }
    }

    /// The slave end, to become the command's stdout. The command holds the
    /// only copy, so the reader sees the end when the command is gone.
    fn slave(&mut self) -> Stdio {
        Stdio::from(self.slave.take().expect("the slave is taken once"))
    }

    /// What the command printed, with the terminal's `\r\n` as `\n`.
    fn finish(self) -> String {
        let bytes = self
            .reader
            .join()
            .expect("the reader thread does not panic");
        String::from_utf8(bytes)
            .expect("the output is UTF-8")
            .replace("\r\n", "\n")
    }
}
