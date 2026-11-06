// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rsync executable.

use crate::config::MachineConfig;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
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
}

impl crate::error::HasKind for RsyncError {}

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
fn parse_version_line(line: &str) -> Result<RsyncVersion, Refusal> {
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
}
