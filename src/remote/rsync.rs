// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rsync executable.

use crate::config::MachineConfig;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Errors of finding and running rsync.
#[derive(Debug, thiserror::Error)]
pub enum RsyncError {
    #[error("no rsync is stored in machine.rsync")]
    NotResolved,
    #[error("machine.rsync must be an absolute path, not {}", .0.display())]
    RelativePath(PathBuf),
    #[error("cannot run {}: {source}", path.display())]
    Spawn { path: PathBuf, source: io::Error },
    #[error("{} is refused: {reason}", path.display())]
    Refused { path: PathBuf, reason: Refusal },
    #[error("no rsync 3.1 or later found{}", refusals_text(.refusals))]
    NoneFound { refusals: Vec<(PathBuf, String)> },
    /// rsync ran and ended with a non-zero status, or was killed by a
    /// signal (`code` is `None`). `stderr` is what it printed, so that the
    /// caller can tell an ssh failure (status 255) from a transfer problem.
    #[error("{}", failure_text(*code, stderr))]
    Failed { code: Option<i32>, stderr: String },
}

impl crate::error::HasKind for RsyncError {}

fn failure_text(code: Option<i32>, stderr: &str) -> String {
    let how = match code {
        Some(code) => format!("rsync failed with exit code {code}"),
        None => "rsync was killed by a signal".to_string(),
    };
    if stderr.is_empty() {
        how
    } else {
        format!("{how}: {stderr}")
    }
}

fn refusals_text(refusals: &[(PathBuf, String)]) -> String {
    refusals
        .iter()
        .map(|(path, reason)| format!("; {}: {reason}", path.display()))
        .collect()
}

/// Why a probed executable is not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    OpenRsync,
    TooOld(RsyncVersion),
    Unrecognised(String),
    NotSuccessful(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::OpenRsync => write!(formatter, "openrsync is not supported"),
            Refusal::TooOld(version) => {
                write!(formatter, "version {version} is older than 3.1")
            }
            Refusal::Unrecognised(line) => {
                write!(
                    formatter,
                    "`--version` printed {line:?}, not an rsync version line"
                )
            }
            Refusal::NotSuccessful(status) => {
                write!(formatter, "`--version` ended with {status}")
            }
        }
    }
}

/// The version an rsync reports about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsyncVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub protocol: u32,
}

impl fmt::Display for RsyncVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Directories searched after `PATH`, where Homebrew and a manual install
/// put rsync on macOS.
const FIXED_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// An rsync executable that passed the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rsync {
    path: PathBuf,
    version: RsyncVersion,
}

impl Rsync {
    /// Runs `<path> --version` and accepts the executable when it reports
    /// rsync 3.1 or later. openrsync, the macOS default, is refused.
    pub fn probe(path: &Path) -> Result<Rsync, RsyncError> {
        let mut command = scrub_environment(Command::new(path), inherited_names());
        command.arg("--version").stdin(Stdio::null());
        let output = command.output().map_err(|source| RsyncError::Spawn {
            path: path.to_path_buf(),
            source,
        })?;
        let refuse = |reason| RsyncError::Refused {
            path: path.to_path_buf(),
            reason,
        };
        if !output.status.success() {
            return Err(refuse(Refusal::NotSuccessful(output.status.to_string())));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let first_line = text.lines().next().unwrap_or_default();
        let version = parse_version_line(first_line).map_err(refuse)?;
        Ok(Rsync {
            path: path.to_path_buf(),
            version,
        })
    }

    /// Finds the rsync `remote add` stores in `machine.rsync`: the
    /// configured path when there is one, else the first of the `PATH`
    /// directories, `/opt/homebrew/bin` and `/usr/local/bin` that holds an
    /// rsync passing the probe.
    pub fn resolve(configured: Option<&Path>, path_var: &OsStr) -> Result<Rsync, RsyncError> {
        resolve_in(configured, &search_dirs(path_var))
    }

    /// The rsync a run uses: the stored `machine.rsync`, probed again
    /// because the executable may have changed since `remote add`.
    pub fn from_machine(machine: &MachineConfig) -> Result<Rsync, RsyncError> {
        let path = machine.rsync.as_deref().ok_or(RsyncError::NotResolved)?;
        require_absolute(path)?;
        Rsync::probe(path)
    }

    /// The absolute path to store in `machine.rsync`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn version(&self) -> RsyncVersion {
        self.version
    }

    /// Prepares one rsync call from `source` to `destination`. Both are
    /// passed as given, so the caller escapes a remote path (`host:path`).
    pub fn transfer(
        &self,
        settings: &TransferSettings,
        source: &OsStr,
        destination: &OsStr,
    ) -> Transfer {
        let mut args: Vec<OsString> = [
            "--stats",
            "--no-human-readable",
            "--no-whole-file",
            "--ignore-times",
            &format!("--timeout={}", settings.timeout_seconds),
            "-e",
        ]
        .map(OsString::from)
        .into();
        args.push(join_rsh(&settings.ssh_command));
        if let Some(remote_rsync) = &settings.remote_rsync {
            args.push(format!("--rsync-path={remote_rsync}").into());
        }
        if settings.resumable {
            args.push("--partial-dir=.antiq-partial".into());
        }
        args.extend([
            "--".into(),
            source.to_os_string(),
            destination.to_os_string(),
        ]);
        Transfer {
            program: self.path.clone(),
            args,
        }
    }
}

/// What a transfer needs besides the paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSettings {
    /// The ssh command rsync runs as its remote shell, program first.
    pub ssh_command: Vec<OsString>,
    /// `--timeout`, the seconds without any data after which rsync gives up.
    pub timeout_seconds: u32,
    /// `--rsync-path`, the rsync to run on the remote.
    pub remote_rsync: Option<String>,
    /// Keep an interrupted file in `.antiq-partial` so that the next call
    /// resumes it. Archive uploads set this.
    pub resumable: bool,
}

impl TransferSettings {
    pub fn for_machine(
        machine: &MachineConfig,
        ssh_command: Vec<OsString>,
        resumable: bool,
    ) -> TransferSettings {
        TransferSettings {
            ssh_command,
            timeout_seconds: machine.rsync_timeout,
            remote_rsync: machine.remote_rsync.clone(),
            resumable,
        }
    }
}

/// One prepared rsync call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Transfer {
    /// The command to run, with the environment of
    /// [`scrub_environment`].
    pub fn command(&self) -> Command {
        self.build(Command::new(&self.program), inherited_names())
    }

    fn build(&self, base: Command, inherited: impl IntoIterator<Item = OsString>) -> Command {
        let mut command = scrub_environment(base, inherited);
        command.args(&self.args);
        command
    }

    /// Runs the call. Any status but zero is a failure, 23 (partial
    /// transfer), 24 (files vanished) and 30 (timeout) included.
    pub fn run(&self) -> Result<TransferStats, RsyncError> {
        let mut command = self.command();
        command.stdin(Stdio::null());
        let output = command.output().map_err(|source| RsyncError::Spawn {
            path: self.program.clone(),
            source,
        })?;
        if !output.status.success() {
            return Err(RsyncError::Failed {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(parse_stats(&String::from_utf8_lossy(&output.stdout)))
    }
}

/// The byte counts of rsync's `--stats` block. A count the output does not
/// give in the expected form is `None`: unknown, not zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferStats {
    pub literal_bytes: Option<u64>,
    pub matched_bytes: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub bytes_received: Option<u64>,
}

/// Reads the counts out of the `--stats` text under `LC_ALL=C` and
/// `--no-human-readable`, where a count is plain digits. A line that is
/// missing, or whose number has another form (a thousands separator, a
/// decimal point), leaves its count unknown instead of guessing.
fn parse_stats(output: &str) -> TransferStats {
    let count = |label: &str| {
        output.lines().find_map(|line| {
            let number = line.strip_prefix(label)?.strip_prefix(": ")?;
            let number = number.strip_suffix(" bytes").unwrap_or(number);
            if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            number.parse().ok()
        })
    };
    TransferStats {
        literal_bytes: count("Literal data"),
        matched_bytes: count("Matched data"),
        bytes_sent: count("Total bytes sent"),
        bytes_received: count("Total bytes received"),
    }
}

/// The value of rsync's `-e` option for an ssh command.
///
/// rsync splits that value itself: parts are separated by spaces, single
/// and double quotes keep spaces in a part, backslashes mean nothing, and a
/// quote doubled inside a part quoted with the same quote stands for that
/// quote. Quoting every part in single quotes and doubling the single quotes
/// inside it therefore carries any argument, an empty one included.
fn join_rsh(args: &[OsString]) -> OsString {
    let mut joined = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            joined.push(b' ');
        }
        joined.push(b'\'');
        for &byte in arg.as_bytes() {
            joined.push(byte);
            if byte == b'\'' {
                joined.push(b'\'');
            }
        }
        joined.push(b'\'');
    }
    OsString::from_vec(joined)
}

/// `machine.rsync` is stored as an absolute path so that a run does not
/// depend on the `PATH` it starts with.
fn require_absolute(path: &Path) -> Result<(), RsyncError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(RsyncError::RelativePath(path.to_path_buf()))
    }
}

/// The directories to look for `rsync` in: the absolute entries of
/// `path_var`, then the fixed ones, each once.
fn search_dirs(path_var: &OsStr) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in env::split_paths(path_var).chain(FIXED_DIRS.map(PathBuf::from)) {
        // A relative or empty `PATH` entry means the current directory,
        // which differs from run to run.
        if dir.is_absolute() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

fn resolve_in(configured: Option<&Path>, dirs: &[PathBuf]) -> Result<Rsync, RsyncError> {
    // A path the user configured is the user's choice: when it fails the
    // probe the run reports that, instead of quietly using another rsync.
    if let Some(path) = configured {
        require_absolute(path)?;
        return Rsync::probe(path);
    }
    let mut refusals = Vec::new();
    for dir in dirs {
        let candidate = dir.join("rsync");
        match Rsync::probe(&candidate) {
            Ok(rsync) => return Ok(rsync),
            // Most directories on a `PATH` hold no rsync at all.
            Err(RsyncError::Spawn { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(RsyncError::Refused { reason, .. }) => {
                refusals.push((candidate, reason.to_string()));
            }
            Err(error) => refusals.push((candidate, error.to_string())),
        }
    }
    Err(RsyncError::NoneFound { refusals })
}

/// Reads the first line of `rsync --version`.
pub(crate) fn parse_version_line(line: &str) -> Result<RsyncVersion, Refusal> {
    if line.starts_with("openrsync:") {
        return Err(Refusal::OpenRsync);
    }
    let unrecognised = || Refusal::Unrecognised(line.to_string());
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let ["rsync", "version", release, "protocol", "version", protocol] = tokens.as_slice() else {
        return Err(unrecognised());
    };
    let protocol = protocol.parse().map_err(|_| unrecognised())?;
    let (major, minor, patch) = parse_release(release).ok_or_else(unrecognised)?;
    let version = RsyncVersion {
        major,
        minor,
        patch,
        protocol,
    };
    if (major, minor) < (3, 1) {
        return Err(Refusal::TooOld(version));
    }
    Ok(version)
}

/// `3.5.1`, `3.2` and `3.1.0pre1`: numbers separated by dots, text after
/// the digits of a component dropped, the patch level optional.
fn parse_release(release: &str) -> Option<(u32, u32, u32)> {
    let mut numbers = release.split('.').map(|component| {
        let end = component
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(component.len());
        component[..end].parse::<u32>().ok()
    });
    let major = numbers.next()??;
    let minor = numbers.next()??;
    let patch = numbers.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

fn inherited_names() -> impl Iterator<Item = OsString> {
    env::vars_os().map(|(name, _)| name)
}

/// Gives `command` the environment every antiq call of rsync runs with: the
/// C locale, which fixes the digits and labels of the statistics, and none
/// of the variables that change what rsync does or which files it skips.
/// `LC_ALL` overrides `LANG`, `LANGUAGE` and every other `LC_*` variable.
/// `inherited` names the variables the process has, so that a test can
/// supply them.
fn scrub_environment(
    mut command: Command,
    inherited: impl IntoIterator<Item = OsString>,
) -> Command {
    command.env("LC_ALL", "C");
    for name in inherited {
        let bytes = name.as_bytes();
        if bytes.starts_with(b"RSYNC_") || bytes == b"CVSIGNORE" {
            command.env_remove(&name);
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    /// A directory holding a `rsync` that is the fixture script, with the
    /// answers the script reads beside it. The fixture is linked, never
    /// copied: a test process that writes an executable can see it fail to
    /// start with "text file busy" while another test forks.
    struct Fake {
        dir: TempDir,
    }

    impl Fake {
        fn new(version: &str) -> Fake {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-rsync.sh");
            symlink(fixture, dir.path().join("rsync")).expect("the link is new");
            fs::write(dir.path().join("version"), version).expect("the directory is writable");
            Fake { dir }
        }

        fn dir(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("rsync")
        }

        fn version_exit(&self, code: i32) {
            fs::write(self.dir.path().join("version_exit"), code.to_string())
                .expect("the directory is writable");
        }

        fn read(&self, name: &str) -> Vec<u8> {
            fs::read(self.dir.path().join(name)).expect("the fixture logged the call")
        }
    }

    const RSYNC_3_5_1: &str =
        "rsync  version 3.5.1  protocol version 33\nCopyright (C) 1996-2026\n";
    const OPENRSYNC: &str = "openrsync: protocol version 29\nrsync version 2.6.9 compatible\n";

    fn probed(text: &str) -> Result<Rsync, RsyncError> {
        let fake = Fake::new(text);
        Rsync::probe(&fake.path())
    }

    fn refusal(text: &str) -> Refusal {
        match probed(text).expect_err("the probe refuses") {
            RsyncError::Refused { reason, .. } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn version(major: u32, minor: u32, patch: u32, protocol: u32) -> RsyncVersion {
        RsyncVersion {
            major,
            minor,
            patch,
            protocol,
        }
    }

    #[test]
    fn probe_accepts_the_recorded_rsync_3_5_1_line() {
        let rsync = probed(RSYNC_3_5_1).expect("3.5.1 passes");
        assert_eq!(rsync.version(), version(3, 5, 1, 33));
    }

    #[test]
    fn probe_keeps_the_path_it_was_given() {
        let fake = Fake::new(RSYNC_3_5_1);
        let rsync = Rsync::probe(&fake.path()).expect("3.5.1 passes");
        assert_eq!(rsync.path(), fake.path());
    }

    #[test]
    fn probe_accepts_3_1_0_and_a_pre_release_suffix() {
        let rsync = probed("rsync  version 3.1.0pre1  protocol version 31\n").expect("3.1 passes");
        assert_eq!(rsync.version(), version(3, 1, 0, 31));
    }

    #[test]
    fn probe_accepts_a_version_without_a_patch_level() {
        let rsync = probed("rsync  version 3.2  protocol version 31\n").expect("3.2 passes");
        assert_eq!(rsync.version(), version(3, 2, 0, 31));
    }

    #[test]
    fn probe_refuses_openrsync() {
        assert_eq!(refusal(OPENRSYNC), Refusal::OpenRsync);
    }

    #[test]
    fn probe_refuses_3_0_9() {
        assert_eq!(
            refusal("rsync  version 3.0.9  protocol version 30\n"),
            Refusal::TooOld(version(3, 0, 9, 30))
        );
    }

    #[test]
    fn probe_refuses_2_6_9_in_the_rsync_form() {
        assert_eq!(
            refusal("rsync  version 2.6.9  protocol version 29\n"),
            Refusal::TooOld(version(2, 6, 9, 29))
        );
    }

    #[test]
    fn probe_refuses_unrecognised_output() {
        assert_eq!(
            refusal("hello\n"),
            Refusal::Unrecognised("hello".to_string())
        );
    }

    #[test]
    fn probe_refuses_a_version_line_without_a_protocol() {
        assert_eq!(
            refusal("rsync  version 3.5.1\n"),
            Refusal::Unrecognised("rsync  version 3.5.1".to_string())
        );
    }

    #[test]
    fn probe_refuses_a_version_that_is_not_a_number() {
        assert_eq!(
            refusal("rsync  version three  protocol version 33\n"),
            Refusal::Unrecognised("rsync  version three  protocol version 33".to_string())
        );
    }

    #[test]
    fn probe_refuses_empty_output() {
        assert_eq!(refusal(""), Refusal::Unrecognised(String::new()));
    }

    #[test]
    fn probe_refuses_a_failing_version_call() {
        let fake = Fake::new(RSYNC_3_5_1);
        fake.version_exit(3);
        match Rsync::probe(&fake.path()).expect_err("the probe refuses") {
            RsyncError::Refused {
                reason: Refusal::NotSuccessful(status),
                ..
            } => assert!(status.contains('3'), "{status}"),
            other => panic!("expected a failed version call, got {other:?}"),
        }
    }

    #[test]
    fn probe_reports_a_missing_executable() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let missing = dir.path().join("rsync");
        match Rsync::probe(&missing).expect_err("nothing to run") {
            RsyncError::Spawn { path, .. } => assert_eq!(path, missing),
            other => panic!("expected a spawn error, got {other:?}"),
        }
    }

    #[test]
    fn probe_runs_version_with_one_argument_in_a_scrubbed_environment() {
        let fake = Fake::new(RSYNC_3_5_1);
        Rsync::probe(&fake.path()).expect("3.5.1 passes");
        assert_eq!(fake.read("calls.log"), b"version\0--version\0");
        let env = String::from_utf8(fake.read("env.log")).expect("text");
        assert_eq!(env, "LC_ALL=C\n");
    }

    #[test]
    fn environment_sets_lc_all_c_and_removes_rsync_variables_and_cvsignore() {
        let inherited = [
            "RSYNC_PASSWORD",
            "RSYNC_RSH",
            "CVSIGNORE",
            "HOME",
            "rsync_x",
        ];
        let command =
            scrub_environment(Command::new("rsync"), inherited.iter().map(OsString::from));
        let edits: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let mut expected = vec![
            ("CVSIGNORE".to_string(), None),
            ("LC_ALL".to_string(), Some("C".to_string())),
            ("RSYNC_PASSWORD".to_string(), None),
            ("RSYNC_RSH".to_string(), None),
        ];
        let mut edits = edits;
        edits.sort();
        expected.sort();
        assert_eq!(edits, expected);
    }

    #[test]
    fn the_fake_sees_no_rsync_variables_and_lc_all_c() {
        let fake = Fake::new(RSYNC_3_5_1);
        let mut base = Command::new(fake.path());
        base.arg("--version")
            .env("RSYNC_PASSWORD", "secret")
            .env("CVSIGNORE", "*")
            .env("LC_ALL", "de_DE.UTF-8");
        let inherited = ["RSYNC_PASSWORD", "CVSIGNORE"].map(OsString::from);
        let output = scrub_environment(base, inherited)
            .output()
            .expect("the fixture runs");
        assert!(output.status.success());
        let env = String::from_utf8(fake.read("env.log")).expect("text");
        assert_eq!(env, "LC_ALL=C\n");
    }

    #[test]
    fn resolve_prefers_the_configured_path() {
        let configured = Fake::new(RSYNC_3_5_1);
        let searched = Fake::new("rsync  version 3.2.7  protocol version 32\n");
        let rsync = resolve_in(Some(&configured.path()), &[searched.dir()]).expect("it resolves");
        assert_eq!(rsync.path(), configured.path());
    }

    #[test]
    fn resolve_does_not_fall_back_when_the_configured_path_fails() {
        let configured = Fake::new(OPENRSYNC);
        let searched = Fake::new(RSYNC_3_5_1);
        match resolve_in(Some(&configured.path()), &[searched.dir()]).expect_err("it is refused") {
            RsyncError::Refused { path, reason } => {
                assert_eq!(path, configured.path());
                assert_eq!(reason, Refusal::OpenRsync);
            }
            other => panic!("expected the configured path's refusal, got {other:?}"),
        }
    }

    #[test]
    fn resolve_refuses_a_relative_configured_path() {
        match resolve_in(Some(Path::new("bin/rsync")), &[]).expect_err("it is refused") {
            error @ RsyncError::RelativePath(_) => {
                assert!(error.to_string().contains("machine.rsync"), "{error}");
            }
            other => panic!("expected a relative path error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_takes_the_first_directory_that_passes() {
        let openrsync = Fake::new(OPENRSYNC);
        let first = Fake::new(RSYNC_3_5_1);
        let second = Fake::new("rsync  version 3.2.7  protocol version 32\n");
        let dirs = [openrsync.dir(), first.dir(), second.dir()];
        let rsync = resolve_in(None, &dirs).expect("it resolves");
        assert_eq!(rsync.path(), first.path());
        assert_eq!(rsync.version(), version(3, 5, 1, 33));
    }

    #[test]
    fn resolve_skips_a_directory_without_rsync() {
        let empty = tempfile::tempdir().expect("a temporary directory");
        let fake = Fake::new(RSYNC_3_5_1);
        let dirs = [empty.path().to_path_buf(), fake.dir()];
        let rsync = resolve_in(None, &dirs).expect("it resolves");
        assert_eq!(rsync.path(), fake.path());
    }

    #[test]
    fn resolve_reports_every_refusal_when_none_passes() {
        let openrsync = Fake::new(OPENRSYNC);
        let old = Fake::new("rsync  version 3.0.9  protocol version 30\n");
        let dirs = [openrsync.dir(), old.dir()];
        match resolve_in(None, &dirs).expect_err("none passes") {
            RsyncError::NoneFound { refusals } => {
                assert_eq!(refusals.len(), 2);
                assert_eq!(refusals[0].0, openrsync.path());
                assert!(refusals[0].1.contains("openrsync"), "{refusals:?}");
                assert_eq!(refusals[1].0, old.path());
                assert!(refusals[1].1.contains("3.0.9"), "{refusals:?}");
            }
            other => panic!("expected none found, got {other:?}"),
        }
    }

    #[test]
    fn resolve_reports_a_candidate_that_cannot_be_started() {
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let dir = tempfile::tempdir().expect("a temporary directory");
        let candidate = dir.path().join("rsync");
        fs::write(&candidate, "#!/bin/sh\n").expect("the directory is writable");
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644))
            .expect("the file is ours");
        match resolve_in(None, &[dir.path().to_path_buf()]).expect_err("none passes") {
            RsyncError::NoneFound { refusals } => {
                assert_eq!(refusals.len(), 1);
                assert_eq!(refusals[0].0, candidate);
                assert!(refusals[0].1.contains("cannot run"), "{refusals:?}");
            }
            other => panic!("expected none found, got {other:?}"),
        }
    }

    #[test]
    fn the_messages_name_the_path_and_the_reason() {
        let none_found = RsyncError::NoneFound {
            refusals: vec![
                (
                    PathBuf::from("/usr/bin/rsync"),
                    "openrsync is not supported".to_string(),
                ),
                (PathBuf::from("/opt/rsync"), "cannot run".to_string()),
            ],
        };
        assert_eq!(
            none_found.to_string(),
            "no rsync 3.1 or later found; /usr/bin/rsync: openrsync is not supported; /opt/rsync: cannot run"
        );
        let unrecognised = Refusal::Unrecognised("hello".to_string());
        assert_eq!(
            unrecognised.to_string(),
            "`--version` printed \"hello\", not an rsync version line"
        );
        let failed = Refusal::NotSuccessful("exit status: 3".to_string());
        assert_eq!(failed.to_string(), "`--version` ended with exit status: 3");
    }

    #[test]
    fn search_dirs_appends_the_fixed_directories_and_skips_relative_entries() {
        let dirs = search_dirs(OsStr::new("/usr/bin::relative/bin:/opt/tools"));
        assert_eq!(
            dirs,
            [
                PathBuf::from("/usr/bin"),
                PathBuf::from("/opt/tools"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ]
        );
    }

    #[test]
    fn search_dirs_lists_a_fixed_directory_once() {
        let dirs = search_dirs(OsStr::new("/usr/local/bin:/usr/bin"));
        assert_eq!(
            dirs,
            [
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/opt/homebrew/bin"),
            ]
        );
    }

    #[test]
    fn resolve_searches_the_path_variable_first() {
        let fake = Fake::new(RSYNC_3_5_1);
        let rsync = Rsync::resolve(None, fake.dir().as_os_str()).expect("it resolves");
        assert_eq!(rsync.path(), fake.path());
    }

    #[test]
    fn from_machine_without_a_path_is_not_resolved() {
        let error = Rsync::from_machine(&MachineConfig::default()).expect_err("no path");
        assert!(matches!(error, RsyncError::NotResolved), "{error:?}");
    }

    // =========================================================
    // The command line
    // =========================================================

    fn names(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn settings(ssh: &[&str]) -> TransferSettings {
        TransferSettings {
            ssh_command: names(ssh),
            timeout_seconds: 13,
            remote_rsync: None,
            resumable: false,
        }
    }

    fn argv(transfer: &Transfer) -> Vec<String> {
        transfer
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn prepared(settings: &TransferSettings) -> Transfer {
        let fake = Fake::new(RSYNC_3_5_1);
        let rsync = Rsync::probe(&fake.path()).expect("3.5.1 passes");
        // Only the argv is inspected; the fake's directory is gone by then.
        rsync.transfer(
            settings,
            OsStr::new("/src/a.tar"),
            OsStr::new("host:/dest/a"),
        )
    }

    #[test]
    fn argv_has_the_fixed_flags_in_order_and_none_that_overwrite_or_delete() {
        let transfer = prepared(&settings(&["ssh", "-o", "BatchMode=yes"]));
        assert_eq!(
            argv(&transfer),
            [
                "--stats",
                "--no-human-readable",
                "--no-whole-file",
                "--ignore-times",
                "--timeout=13",
                "-e",
                "'ssh' '-o' 'BatchMode=yes'",
                "--",
                "/src/a.tar",
                "host:/dest/a",
            ]
        );
    }

    #[test]
    fn the_command_runs_the_probed_path_with_the_argv() {
        let fake = Fake::new(RSYNC_3_5_1);
        let rsync = Rsync::probe(&fake.path()).expect("3.5.1 passes");
        let transfer = rsync.transfer(&settings(&["ssh"]), OsStr::new("a"), OsStr::new("b"));
        let command = transfer.command();
        assert_eq!(command.get_program(), fake.path().as_os_str());
        let args: Vec<&OsStr> = command.get_args().collect();
        let expected: Vec<&OsStr> = transfer.args.iter().map(OsString::as_os_str).collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn argv_passes_rsync_path_when_configured_and_not_otherwise() {
        let mut with = settings(&["ssh"]);
        with.remote_rsync = Some("/opt/homebrew/bin/rsync".to_string());
        let with = argv(&prepared(&with));
        assert_eq!(with[0], "--stats");
        assert_eq!(with[6], "'ssh'");
        assert_eq!(with[7], "--rsync-path=/opt/homebrew/bin/rsync");
        let without = argv(&prepared(&settings(&["ssh"])));
        assert_eq!(without[0], "--stats");
        assert!(
            !without.iter().any(|arg| arg.starts_with("--rsync-path")),
            "{without:?}"
        );
    }

    #[test]
    fn argv_has_the_partial_dir_when_resumable_and_not_otherwise() {
        let mut resumable = settings(&["ssh"]);
        resumable.resumable = true;
        let resumable = argv(&prepared(&resumable));
        assert_eq!(resumable[0], "--stats");
        assert_eq!(resumable[7], "--partial-dir=.antiq-partial");
        let plain = argv(&prepared(&settings(&["ssh"])));
        assert_eq!(plain[0], "--stats");
        assert!(
            !plain.iter().any(|arg| arg.starts_with("--partial")),
            "{plain:?}"
        );
    }

    #[test]
    fn both_optional_flags_come_after_the_remote_shell_and_before_the_paths() {
        let mut both = settings(&["ssh"]);
        both.resumable = true;
        both.remote_rsync = Some("rsync".to_string());
        let both = argv(&prepared(&both));
        assert_eq!(
            both[7..],
            [
                "--rsync-path=rsync",
                "--partial-dir=.antiq-partial",
                "--",
                "/src/a.tar",
                "host:/dest/a",
            ]
        );
    }

    #[test]
    fn the_settings_come_from_the_machine_config() {
        let machine = MachineConfig {
            rsync_timeout: 42,
            remote_rsync: Some("/usr/local/bin/rsync".to_string()),
            ..MachineConfig::default()
        };
        let settings = TransferSettings::for_machine(&machine, names(&["ssh"]), true);
        assert_eq!(
            settings,
            TransferSettings {
                ssh_command: names(&["ssh"]),
                timeout_seconds: 42,
                remote_rsync: Some("/usr/local/bin/rsync".to_string()),
                resumable: true,
            }
        );
    }

    #[test]
    fn the_default_machine_config_gives_the_default_timeout() {
        let settings =
            TransferSettings::for_machine(&MachineConfig::default(), names(&["ssh"]), false);
        assert_eq!(settings.timeout_seconds, 300);
        assert_eq!(settings.remote_rsync, None);
    }

    // =========================================================
    // The -e value
    // =========================================================

    #[test]
    fn join_rsh_quotes_every_argument() {
        let args = names(&[
            "ssh",
            "a b",
            "it's",
            "say \"hi\"",
            "back\\slash",
            "tab\there",
            "new\nline",
            "",
        ]);
        assert_eq!(
            join_rsh(&args),
            OsString::from(
                "'ssh' 'a b' 'it''s' 'say \"hi\"' 'back\\slash' 'tab\there' 'new\nline' ''"
            )
        );
    }

    #[test]
    fn join_rsh_keeps_bytes_that_are_not_utf8() {
        let args = vec![OsString::from_vec(vec![b'a', 0xff, b'\'', b'b'])];
        assert_eq!(
            join_rsh(&args).as_bytes(),
            [b'\'', b'a', 0xff, b'\'', b'\'', b'b', b'\'']
        );
    }

    /// rsync is the authority on how it splits its `-e` value: the fixture
    /// stands in for ssh, records the arguments rsync gives it and fails the
    /// connection.
    #[test]
    #[ignore = "needs rsync 3.1 or later at /opt/homebrew/bin/rsync"]
    fn rsh_round_trips_through_real_rsync() {
        let real = Rsync::probe(Path::new(REAL_RSYNC)).unwrap_or_else(|error| {
            panic!("the round trip needs rsync 3.1 or later at {REAL_RSYNC}: {error}")
        });
        let shell = Fake::new("");
        let hard = [
            "-o",
            "ProxyCommand=nc -x 'proxy' %h %p",
            "it's",
            "say \"hi\"",
            "''",
            "back\\slash",
            "tab\there",
            "new\nline",
            "",
            "two  spaces",
        ];
        let mut ssh = vec![shell.path().into_os_string()];
        ssh.extend(names(&hard));
        let transfer = real.transfer(
            &settings_with(ssh),
            OsStr::new("/dev/null"),
            OsStr::new("remote-host:/dest/a"),
        );
        let error = transfer.run().expect_err("the stand-in ssh fails");
        assert!(matches!(error, RsyncError::Failed { .. }), "{error:?}");
        let log = shell.read("calls.log");
        let mut recorded: Vec<&[u8]> = log.split(|&byte| byte == 0).collect();
        recorded.pop();
        assert_eq!(recorded[0], b"transfer");
        let expected: Vec<&[u8]> = hard.iter().map(|arg| arg.as_bytes()).collect();
        assert_eq!(recorded[1..=hard.len()], expected[..]);
    }

    fn settings_with(ssh_command: Vec<OsString>) -> TransferSettings {
        TransferSettings {
            ssh_command,
            timeout_seconds: 13,
            remote_rsync: None,
            resumable: false,
        }
    }

    const REAL_RSYNC: &str = "/opt/homebrew/bin/rsync";

    // =========================================================
    // Statistics
    // =========================================================

    /// The block rsync 3.5.1 printed for a push of a 2,568 byte file under
    /// `LC_ALL=C` with `--no-human-readable`.
    const STATS_3_5_1: &str = "\nNumber of files: 1 (reg: 1)\n\
Number of created files: 1 (reg: 1)\n\
Number of deleted files: 0\n\
Number of regular files transferred: 1\n\
Total file size: 2568 bytes\n\
Total transferred file size: 2568 bytes\n\
Literal data: 2568 bytes\n\
Number of 4 KiB logical blocks touched: 1\n\
Matched data: 0 bytes\n\
File list size: 0\n\
File list generation time: 0.001 seconds\n\
File list transfer time: 0.000 seconds\n\
Total bytes sent: 2652\n\
Total bytes received: 35\n\
\n\
sent 2652 bytes  received 35 bytes  5374.00 bytes/sec\n\
total size is 2568  speedup is 0.96\n";

    #[test]
    fn reads_literal_data_1408_bytes() {
        let stats = parse_stats("Literal data: 1408 bytes\n");
        assert_eq!(stats.literal_bytes, Some(1408));
        assert_eq!(stats.matched_bytes, None);
        assert_eq!(stats.bytes_sent, None);
        assert_eq!(stats.bytes_received, None);
    }

    #[test]
    fn reads_the_recorded_3_5_1_block() {
        assert_eq!(
            parse_stats(STATS_3_5_1),
            TransferStats {
                literal_bytes: Some(2568),
                matched_bytes: Some(0),
                bytes_sent: Some(2652),
                bytes_received: Some(35),
            }
        );
    }

    #[test]
    fn a_missing_line_is_unknown_not_zero() {
        let without_matched = STATS_3_5_1.replace("Matched data: 0 bytes\n", "");
        let stats = parse_stats(&without_matched);
        assert_eq!(stats.matched_bytes, None);
        assert_eq!(stats.literal_bytes, Some(2568));
    }

    #[test]
    fn no_output_is_all_unknown() {
        assert_eq!(parse_stats(""), TransferStats::default());
    }

    #[test]
    fn a_separator_in_a_number_is_unknown() {
        for text in [
            "Literal data: 1.408 bytes\n",
            "Literal data: 1,408 bytes\n",
            "Literal data: bytes\n",
            "Literal data: 99999999999999999999999 bytes\n",
        ] {
            assert_eq!(parse_stats(text).literal_bytes, None, "{text}");
        }
    }

    #[test]
    fn a_label_must_match_the_whole_line_start() {
        let stats = parse_stats("Total literal data: 5 bytes\nTotal bytes sent : 5\n");
        assert_eq!(stats, TransferStats::default());
    }

    // =========================================================
    // Running
    // =========================================================

    fn run_fake(configure: impl FnOnce(&Fake)) -> (Fake, Result<TransferStats, RsyncError>) {
        let fake = Fake::new(RSYNC_3_5_1);
        configure(&fake);
        let rsync = Rsync::probe(&fake.path()).expect("3.5.1 passes");
        let mut settings = settings(&["ssh", "-p", "2222"]);
        settings.resumable = true;
        let result = rsync
            .transfer(
                &settings,
                OsStr::new("/src/a.tar"),
                OsStr::new("host:/dest/a"),
            )
            .run();
        (fake, result)
    }

    fn write(fake: &Fake, name: &str, text: &str) {
        fs::write(fake.dir().join(name), text).expect("the directory is writable");
    }

    #[test]
    fn a_successful_transfer_returns_the_stats() {
        let (_, result) = run_fake(|fake| write(fake, "stdout", STATS_3_5_1));
        assert_eq!(
            result.expect("exit 0 succeeds"),
            TransferStats {
                literal_bytes: Some(2568),
                matched_bytes: Some(0),
                bytes_sent: Some(2652),
                bytes_received: Some(35),
            }
        );
    }

    #[test]
    fn exit_0_without_stats_is_ok_with_unknown_stats() {
        let (_, result) = run_fake(|_| ());
        assert_eq!(result.expect("exit 0 succeeds"), TransferStats::default());
    }

    #[test]
    fn the_fake_receives_the_argv_and_lc_all_c() {
        let (fake, result) = run_fake(|_| ());
        result.expect("exit 0 succeeds");
        let log = fake.read("calls.log");
        let mut recorded: Vec<&str> = std::str::from_utf8(&log)
            .expect("text")
            .split('\0')
            .collect();
        recorded.pop();
        assert_eq!(
            recorded,
            [
                "version",
                "--version",
                "transfer",
                "--stats",
                "--no-human-readable",
                "--no-whole-file",
                "--ignore-times",
                "--timeout=13",
                "-e",
                "'ssh' '-p' '2222'",
                "--partial-dir=.antiq-partial",
                "--",
                "/src/a.tar",
                "host:/dest/a",
            ]
        );
        let env = String::from_utf8(fake.read("env.log")).expect("text");
        assert_eq!(env, "LC_ALL=C\nLC_ALL=C\n");
    }

    fn failure(code: &str, message: &str) -> RsyncError {
        let (_, result) = run_fake(|fake| {
            write(fake, "exit", code);
            write(fake, "stderr", message);
            write(fake, "stdout", STATS_3_5_1);
        });
        result.expect_err("a non-zero exit fails")
    }

    #[test]
    fn exit_23_fails_the_transfer() {
        match failure("23", "some files could not be transferred\n") {
            RsyncError::Failed { code, stderr } => {
                assert_eq!(code, Some(23));
                assert_eq!(stderr, "some files could not be transferred");
            }
            other => panic!("expected a failed transfer, got {other:?}"),
        }
    }

    #[test]
    fn exit_24_fails_the_transfer() {
        match failure("24", "some files vanished\n") {
            RsyncError::Failed { code, stderr } => {
                assert_eq!(code, Some(24));
                assert_eq!(stderr, "some files vanished");
            }
            other => panic!("expected a failed transfer, got {other:?}"),
        }
    }

    #[test]
    fn exit_30_fails_the_transfer() {
        match failure("30", "io timeout\n") {
            RsyncError::Failed { code, stderr } => {
                assert_eq!(code, Some(30));
                assert_eq!(stderr, "io timeout");
            }
            other => panic!("expected a failed transfer, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_message_names_the_code_and_what_rsync_said() {
        let error = failure("30", "io timeout\n");
        assert_eq!(
            error.to_string(),
            "rsync failed with exit code 30: io timeout"
        );
    }

    #[test]
    fn a_failure_without_output_names_only_the_code() {
        let error = RsyncError::Failed {
            code: Some(23),
            stderr: String::new(),
        };
        assert_eq!(error.to_string(), "rsync failed with exit code 23");
    }

    #[test]
    fn a_signal_death_fails_the_transfer() {
        let (_, result) = run_fake(|fake| write(fake, "signal", ""));
        match result.expect_err("a killed rsync fails") {
            error @ RsyncError::Failed { code: None, .. } => {
                assert_eq!(error.to_string(), "rsync was killed by a signal");
            }
            other => panic!("expected a failed transfer, got {other:?}"),
        }
    }

    #[test]
    fn a_vanished_program_fails_with_spawn() {
        let (fake, _) = run_fake(|_| ());
        let rsync = Rsync::probe(&fake.path()).expect("it is still there");
        fs::remove_file(fake.path()).expect("the link is ours");
        let result = rsync
            .transfer(&settings(&["ssh"]), OsStr::new("a"), OsStr::new("b"))
            .run();
        assert!(
            matches!(result, Err(RsyncError::Spawn { .. })),
            "{result:?}"
        );
    }

    /// A copy on this machine with the real rsync: the flags are accepted
    /// and the stats come out in the form the parser reads.
    #[test]
    #[ignore = "needs rsync 3.1 or later at /opt/homebrew/bin/rsync"]
    fn real_rsync_copies_a_file_and_reports_its_stats() {
        let real = Rsync::probe(Path::new(REAL_RSYNC)).unwrap_or_else(|error| {
            panic!("this test needs rsync 3.1 or later at {REAL_RSYNC}: {error}")
        });
        let dir = tempfile::tempdir().expect("a temporary directory");
        let source = dir.path().join("a.tar");
        fs::write(&source, vec![b'x'; 5000]).expect("the directory is writable");
        let destination = dir.path().join("b.tar");
        let mut settings = settings(&["ssh"]);
        settings.resumable = true;
        settings.remote_rsync = Some(REAL_RSYNC.to_string());
        let stats = real
            .transfer(&settings, source.as_os_str(), destination.as_os_str())
            .run()
            .expect("a local copy succeeds");
        assert_eq!(stats.literal_bytes, Some(5000));
        assert_eq!(stats.matched_bytes, Some(0));
        assert!(
            stats.bytes_sent.is_some_and(|sent| sent >= 5000),
            "{stats:?}"
        );
        assert!(stats.bytes_received.is_some(), "{stats:?}");
        assert_eq!(fs::read(&destination).expect("copied"), vec![b'x'; 5000]);
    }

    #[test]
    fn from_machine_refuses_a_relative_path() {
        let machine = MachineConfig {
            rsync: Some(PathBuf::from("rsync")),
            ..MachineConfig::default()
        };
        let error = Rsync::from_machine(&machine).expect_err("it is refused");
        assert!(matches!(error, RsyncError::RelativePath(_)), "{error:?}");
    }

    #[test]
    fn from_machine_probes_the_stored_path() {
        let fake = Fake::new(RSYNC_3_5_1);
        let machine = MachineConfig {
            rsync: Some(fake.path()),
            ..MachineConfig::default()
        };
        assert_eq!(
            Rsync::from_machine(&machine).expect("it passes").path(),
            fake.path()
        );
        fs::write(fake.dir().join("version"), OPENRSYNC).expect("the directory is writable");
        let error = Rsync::from_machine(&machine).expect_err("it is refused now");
        assert!(
            matches!(
                error,
                RsyncError::Refused {
                    reason: Refusal::OpenRsync,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_source_with_the_size_and_time_of_the_destination_still_replaces_it() {
        let rsync = Rsync::resolve(None, &std::env::var_os("PATH").expect("PATH"))
            .expect("these tests need an rsync 3.1 or later");
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        fs::write(&source, "new bytes").expect("source");
        fs::write(&destination, "old bytes").expect("destination");
        let same_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for path in [&source, &destination] {
            fs::File::options()
                .write(true)
                .open(path)
                .expect("open")
                .set_modified(same_time)
                .expect("mtime");
        }
        let settings = TransferSettings {
            ssh_command: vec!["ssh".into()],
            timeout_seconds: 30,
            remote_rsync: None,
            resumable: false,
        };
        rsync
            .transfer(&settings, source.as_os_str(), destination.as_os_str())
            .run()
            .expect("rsync runs");
        assert_eq!(fs::read(&destination).expect("destination"), b"new bytes");
    }
}
