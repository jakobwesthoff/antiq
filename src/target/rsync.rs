// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rsync executable.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::marker;
use super::url::TargetUrl;
use crate::config::TargetConfig;

/// Errors of finding and running rsync.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RsyncError {
    #[error("`[target] rsync` must be an absolute path, not {}", .0.display())]
    RelativePath(PathBuf),
    // The io error is in the text and not a source, so that a rendered chain
    // names it once.
    #[error("cannot run {}: {cause}", path.display())]
    Spawn { path: PathBuf, cause: io::Error },
    #[error("{} is refused: {reason}; set `[target] rsync` to an rsync 3.2.4 or newer", path.display())]
    Refused { path: PathBuf, reason: Refusal },
    #[error("no rsync 3.2.4 or later found{}; set `[target] rsync` to the path of one", refusals_text(.refusals))]
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
                write!(formatter, "version {version} is older than 3.2.4")
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
    major: u32,
    minor: u32,
    patch: u32,
    /// The release carried a suffix such as `pre1`: a build before the
    /// release of its number.
    pre_release: bool,
}

impl fmt::Display for RsyncVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if self.pre_release {
            write!(formatter, " (pre-release)")?;
        }
        Ok(())
    }
}

/// The oldest rsync `push` accepts: 3.2.4 added `--fsync`. The final
/// flag sorts a pre-release of a version below the version itself.
const FLOOR: (u32, u32, u32, bool) = (3, 2, 4, true);

/// What rsync is told to run as its remote shell: no prompt, and a
/// connection that dies is noticed.
const SSH_COMMAND: &str =
    "ssh -o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4";

/// Seconds without data after which rsync gives up, so that a stalled
/// connection cannot hold the lock for hours.
const TIMEOUT: &str = "--timeout=300";

/// What the mirror leaves out, anchored at the collection root: antiq's
/// own scratch and lock files, the target's marker (which also keeps
/// `--delete` away from it), and the files macOS puts at the root of a
/// volume.
const MIRROR_EXCLUDES: [&str; 11] = [
    "/.antiq-target",
    "/.antiq/tmp/",
    "/.antiq/trash/",
    "/.antiq/doomed/",
    "/.antiq/lock",
    "/.DS_Store",
    "/.Spotlight-V100/",
    "/.fseventsd/",
    "/.DocumentRevisions-V100/",
    "/.TemporaryItems/",
    "/.Trashes/",
];

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
    /// rsync 3.2.4 or later. openrsync, the macOS default, is refused.
    pub(crate) fn probe(path: &Path) -> Result<Rsync, RsyncError> {
        let mut command = scrub_environment(Command::new(path), inherited_names());
        command.arg("--version").stdin(Stdio::null());
        let output = command.output().map_err(|cause| RsyncError::Spawn {
            path: path.to_path_buf(),
            cause,
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

    /// Finds the rsync `push` runs: the
    /// configured path when there is one, else the first of the `PATH`
    /// directories, `/opt/homebrew/bin` and `/usr/local/bin` that holds an
    /// rsync passing the probe.
    pub fn resolve(configured: Option<&Path>, path_var: &OsStr) -> Result<Rsync, RsyncError> {
        resolve_in(configured, &search_dirs(path_var))
    }

    /// The absolute path of the rsync that passed the probe.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn version(&self) -> RsyncVersion {
        self.version
    }

    /// The call that mirrors the collection to the target. A dry run adds
    /// `-n` and nothing else.
    pub fn mirror(
        &self,
        options: &Options,
        collection: &Path,
        target: &TargetUrl,
        dry_run: bool,
    ) -> Call {
        let mut args: Vec<OsString> = Vec::new();
        if dry_run {
            args.push("-n".into());
        }
        args.extend(
            [
                "-a",
                "-H",
                "-x",
                "--delete",
                "--delete-delay",
                "--itemize-changes",
            ]
            .map(OsString::from),
        );
        args.extend(MIRROR_EXCLUDES.map(|name| OsString::from(format!("--exclude={name}"))));
        self.finish(
            args,
            options,
            true,
            [
                with_trailing_slash(collection.as_os_str()),
                target.dir_arg(),
            ],
        )
    }

    /// The call that lists the target directory.
    pub fn list(&self, options: &Options, target: &TargetUrl) -> Call {
        self.finish(
            vec!["--list-only".into()],
            options,
            false,
            [target.dir_arg()],
        )
    }

    /// The call that copies the target's marker into the directory `into`.
    pub fn fetch_marker(&self, options: &Options, target: &TargetUrl, into: &Path) -> Call {
        let mut marker = target.dir_arg();
        marker.push(marker::FILE_NAME);
        // rsync skips a larger file without an error, which the pre-check
        // reads as "the fetch left no file". The bound matches the limit
        // the marker is read with.
        self.finish(
            vec!["--max-size=64K".into()],
            options,
            true,
            [marker, with_trailing_slash(into.as_os_str())],
        )
    }

    /// The call that sends the content of the directory `from`, which holds
    /// only the marker, to the target. Without `-a`, `-p` and `-t` the
    /// target's root keeps its mode and time.
    pub fn send_marker(&self, options: &Options, from: &Path, target: &TargetUrl) -> Call {
        self.finish(
            vec!["-r".into(), "--chmod=F644".into()],
            options,
            true,
            [with_trailing_slash(from.as_os_str()), target.dir_arg()],
        )
    }

    /// Appends what every call shares, in the order of the mirror: the
    /// timeout, `--fsync`, `--rsync-path` and the ssh command, then the
    /// user's `rsync_args` so that they can override antiq's options, then
    /// the paths.
    fn finish(
        &self,
        mut args: Vec<OsString>,
        options: &Options,
        fsync: bool,
        paths: impl IntoIterator<Item = OsString>,
    ) -> Call {
        args.push(TIMEOUT.into());
        if fsync && options.fsync {
            args.push("--fsync".into());
        }
        if let Some(rsync_path) = &options.rsync_path {
            args.push(format!("--rsync-path={rsync_path}").into());
        }
        args.push("-e".into());
        args.push(SSH_COMMAND.into());
        args.extend(options.rsync_args.iter().map(OsString::from));
        args.extend(paths);
        Call {
            program: self.path.clone(),
            args,
        }
    }
}

/// `path` with one `/` at the end, so that rsync works on the content of
/// the directory.
fn with_trailing_slash(path: &OsStr) -> OsString {
    let mut arg = path.to_os_string();
    if !arg.as_encoded_bytes().ends_with(b"/") {
        arg.push("/");
    }
    arg
}

/// The `[target]` keys that shape every rsync call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Pass `--fsync`.
    pub fsync: bool,
    /// `--rsync-path`, the rsync to run on the target.
    pub rsync_path: Option<String>,
    /// Options that follow antiq's own, verbatim.
    pub rsync_args: Vec<String>,
}

impl Options {
    pub fn from_config(target: &TargetConfig) -> Options {
        Options {
            fsync: target.fsync,
            rsync_path: target.rsync_path.clone(),
            rsync_args: target.rsync_args.clone(),
        }
    }
}

/// The short options of rsync that take a value; the rest of their cluster,
/// or else the next argument, is that value.
const SHORT_OPTIONS_WITH_VALUE: &str = "BefMT";

/// The first option of `rsync_args` that makes rsync quiet, if there is one.
///
/// A quiet rsync prints no `--list-only` listing, which the pre-check of an
/// ssh target could not tell from an empty directory.
pub fn quiet_option(args: &[String]) -> Option<&str> {
    let mut value_follows = false;
    for arg in args {
        if std::mem::take(&mut value_follows) {
            continue;
        }
        if arg == "--quiet" {
            return Some(arg);
        }
        let Some(letters) = arg.strip_prefix('-') else {
            continue;
        };
        if letters.is_empty() || !letters.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            continue;
        }
        for (index, letter) in letters.char_indices() {
            if letter == 'q' {
                return Some(arg);
            }
            if SHORT_OPTIONS_WITH_VALUE.contains(letter) {
                value_follows = index + 1 == letters.len();
                break;
            }
        }
    }
    None
}

/// One prepared rsync call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    program: PathBuf,
    args: Vec<OsString>,
}

/// What a finished rsync call left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The exit status; `None` when a signal ended rsync.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Call {
    /// The arguments after the program.
    #[cfg(test)]
    pub(crate) fn args(&self) -> &[OsString] {
        &self.args
    }

    /// The command to run, with the environment of [`scrub_environment`].
    fn command(&self) -> Command {
        let mut command = scrub_environment(Command::new(&self.program), inherited_names());
        command.args(&self.args);
        command
    }

    /// Runs the call and returns what it left, whatever the exit status:
    /// the caller reads the status and the texts of 23, 24, 30 and 255.
    pub fn run(&self) -> Result<Outcome, RsyncError> {
        let mut command = self.command();
        command.stdin(Stdio::null());
        let output = command.output().map_err(|cause| RsyncError::Spawn {
            path: self.program.clone(),
            cause,
        })?;
        Ok(Outcome {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// `[target] rsync` is an absolute path so that a run does not
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
            Err(RsyncError::Spawn { cause, .. }) if cause.kind() == io::ErrorKind::NotFound => {}
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
    // The number has to parse, but nothing reads it afterwards.
    protocol.parse::<u32>().map_err(|_| unrecognised())?;
    let (major, minor, patch, pre_release) = parse_release(release).ok_or_else(unrecognised)?;
    let version = RsyncVersion {
        major,
        minor,
        patch,
        pre_release,
    };
    if (major, minor, patch, !pre_release) < FLOOR {
        return Err(Refusal::TooOld(version));
    }
    Ok(version)
}

/// `3.5.1`, `3.2` and `3.2.4pre1`: numbers separated by dots, the patch
/// level optional. Text after the digits of a component marks a
/// pre-release.
fn parse_release(release: &str) -> Option<(u32, u32, u32, bool)> {
    let mut pre_release = false;
    let mut numbers = release.split('.').map(|component| {
        let end = component
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(component.len());
        pre_release |= end < component.len();
        component[..end].parse::<u32>().ok()
    });
    let major = numbers.next()??;
    let minor = numbers.next()??;
    let patch = numbers.next().unwrap_or(Some(0))?;
    drop(numbers);
    Some((major, minor, patch, pre_release))
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
    use crate::testing::fake_rsync::FakeRsync;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const RSYNC_3_5_1: &str =
        "rsync  version 3.5.1  protocol version 33\nCopyright (C) 1996-2026\n";
    const OPENRSYNC: &str = "openrsync: protocol version 29\nrsync version 2.6.9 compatible\n";

    fn probed(text: &str) -> Result<Rsync, RsyncError> {
        let fake = FakeRsync::new(text);
        Rsync::probe(&fake.path())
    }

    fn refusal(text: &str) -> Refusal {
        match probed(text).expect_err("the probe refuses") {
            RsyncError::Refused { reason, .. } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn version(major: u32, minor: u32, patch: u32) -> RsyncVersion {
        RsyncVersion {
            major,
            minor,
            patch,
            pre_release: false,
        }
    }

    fn line(release: &str) -> String {
        format!("rsync  version {release}  protocol version 31\n")
    }

    #[test]
    fn probe_accepts_the_recorded_rsync_3_5_1_line() {
        let rsync = probed(RSYNC_3_5_1).expect("3.5.1 passes");
        assert_eq!(rsync.version(), version(3, 5, 1));
    }

    #[test]
    fn probe_keeps_the_path_it_was_given() {
        let fake = FakeRsync::new(RSYNC_3_5_1);
        let rsync = Rsync::probe(&fake.path()).expect("3.5.1 passes");
        assert_eq!(rsync.path(), fake.path());
    }

    #[test]
    fn probe_accepts_3_2_4_and_newer() {
        for release in ["3.2.4", "3.2.5", "3.3.0", "3.10.0", "4.0.0", "3.2.5pre1"] {
            probed(&line(release)).unwrap_or_else(|error| panic!("{release}: {error}"));
        }
    }

    #[test]
    fn probe_refuses_versions_below_3_2_4() {
        for release in ["3.2.3", "3.1.0", "3.1.3", "3.2", "3.0.9"] {
            assert!(
                matches!(refusal(&line(release)), Refusal::TooOld(_)),
                "{release}"
            );
        }
    }

    #[test]
    fn probe_refuses_a_pre_release_of_3_2_4() {
        let refused = refusal(&line("3.2.4pre1"));
        assert!(matches!(refused, Refusal::TooOld(found) if found.pre_release));
        assert_eq!(
            refused.to_string(),
            "version 3.2.4 (pre-release) is older than 3.2.4"
        );
    }

    #[test]
    fn a_version_without_a_patch_level_reads_as_patch_zero() {
        match refusal(&line("3.2")) {
            Refusal::TooOld(found) => assert_eq!(found, version(3, 2, 0)),
            other => panic!("expected a refusal for being too old, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_rsync_is_reported_with_its_path_and_the_config_key() {
        let fake = FakeRsync::new(OPENRSYNC);
        let error = Rsync::probe(&fake.path()).expect_err("openrsync is refused");
        let text = error.to_string();
        assert!(text.contains(&fake.path().display().to_string()), "{text}");
        assert!(text.contains("openrsync"), "{text}");
        assert!(text.contains("`[target] rsync`"), "{text}");
        assert!(text.contains("3.2.4"), "{text}");
        let old = FakeRsync::new(&line("3.1.0"));
        let text = Rsync::probe(&old.path()).expect_err("3.1.0").to_string();
        assert!(
            text.contains("3.1.0") && text.contains("`[target] rsync`"),
            "{text}"
        );
    }

    #[test]
    fn probe_refuses_openrsync() {
        assert_eq!(refusal(OPENRSYNC), Refusal::OpenRsync);
    }

    #[test]
    fn probe_refuses_3_0_9() {
        assert_eq!(
            refusal("rsync  version 3.0.9  protocol version 30\n"),
            Refusal::TooOld(version(3, 0, 9))
        );
    }

    #[test]
    fn probe_refuses_2_6_9_in_the_rsync_form() {
        assert_eq!(
            refusal("rsync  version 2.6.9  protocol version 29\n"),
            Refusal::TooOld(version(2, 6, 9))
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
        let fake = FakeRsync::new(RSYNC_3_5_1);
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
    fn a_spawn_error_names_the_io_error_once_in_its_rendered_chain() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let error = Rsync::probe(&dir.path().join("rsync")).expect_err("nothing to run");

        let mut rendered = error.to_string();
        let mut link = std::error::Error::source(&error);
        while let Some(cause) = link {
            rendered.push_str(&format!(": {cause}"));
            link = cause.source();
        }

        assert_eq!(rendered.matches("No such file").count(), 1, "{rendered}");
    }

    #[test]
    fn probe_runs_version_with_one_argument_in_a_scrubbed_environment() {
        let fake = FakeRsync::new(RSYNC_3_5_1);
        Rsync::probe(&fake.path()).expect("3.5.1 passes");
        assert_eq!(fake.calls(), [["version", "--version"]]);
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
        let fake = FakeRsync::new(RSYNC_3_5_1);
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
        let configured = FakeRsync::new(RSYNC_3_5_1);
        let searched = FakeRsync::new("rsync  version 3.2.7  protocol version 32\n");
        let rsync = resolve_in(Some(&configured.path()), &[searched.dir()]).expect("it resolves");
        assert_eq!(rsync.path(), configured.path());
    }

    #[test]
    fn resolve_does_not_fall_back_when_the_configured_path_fails() {
        let configured = FakeRsync::new(OPENRSYNC);
        let searched = FakeRsync::new(RSYNC_3_5_1);
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
                assert!(error.to_string().contains("`[target] rsync`"), "{error}");
            }
            other => panic!("expected a relative path error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_takes_the_first_directory_that_passes() {
        let openrsync = FakeRsync::new(OPENRSYNC);
        let first = FakeRsync::new(RSYNC_3_5_1);
        let second = FakeRsync::new("rsync  version 3.2.7  protocol version 32\n");
        let dirs = [openrsync.dir(), first.dir(), second.dir()];
        let rsync = resolve_in(None, &dirs).expect("it resolves");
        assert_eq!(rsync.path(), first.path());
        assert_eq!(rsync.version(), version(3, 5, 1));
    }

    #[test]
    fn resolve_skips_a_directory_without_rsync() {
        let empty = tempfile::tempdir().expect("a temporary directory");
        let fake = FakeRsync::new(RSYNC_3_5_1);
        let dirs = [empty.path().to_path_buf(), fake.dir()];
        let rsync = resolve_in(None, &dirs).expect("it resolves");
        assert_eq!(rsync.path(), fake.path());
    }

    #[test]
    fn resolve_reports_every_refusal_when_none_passes() {
        let openrsync = FakeRsync::new(OPENRSYNC);
        let old = FakeRsync::new("rsync  version 3.0.9  protocol version 30\n");
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
            "no rsync 3.2.4 or later found; /usr/bin/rsync: openrsync is not supported; /opt/rsync: cannot run; set `[target] rsync` to the path of one"
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
        let fake = FakeRsync::new(RSYNC_3_5_1);
        let rsync = Rsync::resolve(None, fake.dir().as_os_str()).expect("it resolves");
        assert_eq!(rsync.path(), fake.path());
    }

    // =========================================================
    // The calls
    // =========================================================

    const SSH: &str = "ssh -o BatchMode=yes -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=4";

    fn options() -> Options {
        Options {
            fsync: true,
            rsync_path: None,
            rsync_args: Vec::new(),
        }
    }

    fn nas() -> TargetUrl {
        TargetUrl::parse("nas.local:/volume1/antiq").expect("a target")
    }

    /// An rsync that passed the probe; the fake stays alive with it.
    fn probed_fake() -> (FakeRsync, Rsync) {
        let fake = FakeRsync::new(RSYNC_3_5_1);
        let rsync = Rsync::probe(&fake.path()).expect("3.5.1 passes");
        (fake, rsync)
    }

    fn argv(call: &Call) -> Vec<String> {
        call.args()
            .iter()
            .map(|arg| arg.to_str().expect("text").to_string())
            .collect()
    }

    /// The mirror argument list of the spec, up to the option group that
    /// follows the excludes.
    fn mirror_head() -> Vec<&'static str> {
        vec![
            "-a",
            "-H",
            "-x",
            "--delete",
            "--delete-delay",
            "--itemize-changes",
            "--exclude=/.antiq-target",
            "--exclude=/.antiq/tmp/",
            "--exclude=/.antiq/trash/",
            "--exclude=/.antiq/doomed/",
            "--exclude=/.antiq/lock",
            "--exclude=/.DS_Store",
            "--exclude=/.Spotlight-V100/",
            "--exclude=/.fseventsd/",
            "--exclude=/.DocumentRevisions-V100/",
            "--exclude=/.TemporaryItems/",
            "--exclude=/.Trashes/",
            "--timeout=300",
        ]
    }

    #[test]
    fn the_mirror_argv_is_the_one_of_the_spec() {
        let (_fake, rsync) = probed_fake();
        let call = rsync.mirror(
            &options(),
            Path::new("/Users/jakob/projects"),
            &nas(),
            false,
        );
        let mut expected = mirror_head();
        expected.extend([
            "--fsync",
            "-e",
            SSH,
            "/Users/jakob/projects/",
            "nas.local:/volume1/antiq/",
        ]);
        assert_eq!(argv(&call), expected);
    }

    #[test]
    fn the_mirror_drops_fsync_when_the_config_says_so() {
        let (_fake, rsync) = probed_fake();
        let options = Options {
            fsync: false,
            ..options()
        };
        let call = rsync.mirror(&options, Path::new("/c"), &nas(), false);
        let mut expected = mirror_head();
        expected.extend(["-e", SSH, "/c/", "nas.local:/volume1/antiq/"]);
        assert_eq!(argv(&call), expected);
    }

    #[test]
    fn rsync_path_sits_between_fsync_and_the_ssh_command() {
        let (_fake, rsync) = probed_fake();
        let options = Options {
            rsync_path: Some("/usr/local/bin/rsync".into()),
            ..options()
        };
        let call = rsync.mirror(&options, Path::new("/c"), &nas(), false);
        let mut expected = mirror_head();
        expected.extend([
            "--fsync",
            "--rsync-path=/usr/local/bin/rsync",
            "-e",
            SSH,
            "/c/",
            "nas.local:/volume1/antiq/",
        ]);
        assert_eq!(argv(&call), expected);
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn q_alone_or_in_a_cluster_and_quiet_make_rsync_quiet() {
        for list in [
            &["-q"][..],
            &["--quiet"],
            &["--bwlimit=100", "-aq"],
            &["-qa"],
            &["-vqz"],
            &["-Hxq"],
        ] {
            let owned = args(list);
            let found = quiet_option(&owned);
            assert!(found.is_some(), "{list:?}");
            assert!(list.contains(&found.expect("found")), "{list:?}");
        }
    }

    #[test]
    fn a_q_that_is_the_value_of_an_option_or_no_option_at_all_is_no_quiet() {
        for list in [
            &["-eq"][..],
            &["-aeq"],
            &["-Bq"],
            &["-fq"],
            &["-Mq"],
            &["-Tq"],
            &["-e", "ssh -q"],
            &["--bwlimit", "100"],
            &["-e", "-q"],
            &["-ae", "-q"],
            &["--rsync-path=quiet"],
            &["--no-perms", "-a", "-vv"],
            &["--exclude=-q"],
            &["-"],
            &["-q1"],
            &[],
        ] {
            assert_eq!(quiet_option(&args(list)), None, "{list:?}");
        }
    }

    #[test]
    fn rsync_args_follow_antiqs_options_and_precede_the_paths() {
        let (_fake, rsync) = probed_fake();
        let options = Options {
            rsync_args: vec!["--no-perms".into(), "-e".into(), "ssh -p 2222".into()],
            ..options()
        };
        let call = rsync.mirror(&options, Path::new("/c"), &nas(), false);
        let mut expected = mirror_head();
        expected.extend([
            "--fsync",
            "-e",
            SSH,
            "--no-perms",
            "-e",
            "ssh -p 2222",
            "/c/",
            "nas.local:/volume1/antiq/",
        ]);
        assert_eq!(argv(&call), expected);
    }

    #[test]
    fn the_dry_run_is_the_mirror_with_a_leading_n() {
        let (_fake, rsync) = probed_fake();
        let real = rsync.mirror(&options(), Path::new("/c"), &nas(), false);
        let dry = rsync.mirror(&options(), Path::new("/c"), &nas(), true);
        let mut expected = vec!["-n".to_string()];
        expected.extend(argv(&real));
        assert_eq!(argv(&dry), expected);
    }

    #[test]
    fn a_path_target_and_a_collection_with_a_slash_get_one_trailing_slash() {
        let (_fake, rsync) = probed_fake();
        let target = TargetUrl::parse("/Volumes/My Share/antiq").expect("a target");
        let call = rsync.mirror(&options(), Path::new("/Users/jakob/p/"), &target, false);
        let args = argv(&call);
        assert_eq!(args[args.len() - 2], "/Users/jakob/p/");
        assert_eq!(args[args.len() - 1], "/Volumes/My Share/antiq/");
    }

    #[test]
    fn the_list_argv_has_no_fsync() {
        let (_fake, rsync) = probed_fake();
        let options = Options {
            rsync_path: Some("/r".into()),
            rsync_args: vec!["--bwlimit=5".into()],
            ..options()
        };
        let call = rsync.list(&options, &nas());
        assert_eq!(
            argv(&call),
            [
                "--list-only",
                "--timeout=300",
                "--rsync-path=/r",
                "-e",
                SSH,
                "--bwlimit=5",
                "nas.local:/volume1/antiq/"
            ]
        );
    }

    #[test]
    fn the_marker_fetch_argv_names_the_marker_and_the_directory() {
        let (_fake, rsync) = probed_fake();
        let call = rsync.fetch_marker(&options(), &nas(), Path::new("/c/.antiq/tmp/x"));
        assert_eq!(
            argv(&call),
            [
                "--max-size=64K",
                "--timeout=300",
                "--fsync",
                "-e",
                SSH,
                "nas.local:/volume1/antiq/.antiq-target",
                "/c/.antiq/tmp/x/"
            ]
        );
    }

    #[test]
    fn the_marker_send_argv_is_recursive_without_archive_options() {
        let (_fake, rsync) = probed_fake();
        let call = rsync.send_marker(&options(), Path::new("/c/.antiq/tmp/x"), &nas());
        let args = argv(&call);
        assert_eq!(
            args,
            [
                "-r",
                "--chmod=F644",
                "--timeout=300",
                "--fsync",
                "-e",
                SSH,
                "/c/.antiq/tmp/x/",
                "nas.local:/volume1/antiq/"
            ]
        );
        for forbidden in ["-a", "-p", "-t", "--delete"] {
            assert!(!args.iter().any(|arg| arg == forbidden), "{forbidden}");
        }
    }

    #[test]
    fn the_options_come_from_the_target_config() {
        let config = TargetConfig {
            fsync: false,
            rsync_path: Some("/r".into()),
            rsync_args: vec!["--x".into()],
            ..TargetConfig::default()
        };
        assert_eq!(
            Options::from_config(&config),
            Options {
                fsync: false,
                rsync_path: Some("/r".into()),
                rsync_args: vec!["--x".into()],
            }
        );
    }

    // =========================================================
    // Running
    // =========================================================

    fn run_with(fake: &FakeRsync, rsync: &Rsync) -> Outcome {
        rsync
            .list(&options(), &nas())
            .run()
            .unwrap_or_else(|error| panic!("{error} ({})", fake.path().display()))
    }

    #[test]
    fn exit_0_returns_the_output() {
        let (fake, rsync) = probed_fake();
        fake.answer_all("listing\n", "", 0);
        let outcome = run_with(&fake, &rsync);
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"listing\n");
    }

    #[test]
    fn exit_23_returns_stdout_and_stderr() {
        let (fake, rsync) = probed_fake();
        fake.answer_all(
            ">f+++++++++ a\n",
            "IO error encountered -- skipping file deletion\n",
            23,
        );
        let outcome = run_with(&fake, &rsync);
        assert_eq!(outcome.code, Some(23));
        assert_eq!(outcome.stdout, b">f+++++++++ a\n");
        assert_eq!(
            outcome.stderr,
            b"IO error encountered -- skipping file deletion\n"
        );
    }

    #[test]
    fn exit_24_returns_what_rsync_printed() {
        let (fake, rsync) = probed_fake();
        fake.answer_all("", "file has vanished: \"/c/a\"\n", 24);
        let outcome = run_with(&fake, &rsync);
        assert_eq!(outcome.code, Some(24));
        assert_eq!(outcome.stderr, b"file has vanished: \"/c/a\"\n");
    }

    #[test]
    fn exit_255_returns_the_ssh_line() {
        let (fake, rsync) = probed_fake();
        fake.answer_all("", "ssh: Could not resolve hostname nas\n", 255);
        let outcome = run_with(&fake, &rsync);
        assert_eq!(outcome.code, Some(255));
        assert_eq!(outcome.stderr, b"ssh: Could not resolve hostname nas\n");
    }

    #[test]
    fn a_signal_gives_no_exit_code() {
        let (fake, rsync) = probed_fake();
        fake.kill_on(1);
        assert_eq!(run_with(&fake, &rsync).code, None);
    }

    #[test]
    fn each_call_of_a_run_gets_its_own_answer() {
        let (fake, rsync) = probed_fake();
        fake.answer(1, "", "rsync: change_dir failed (2)\n", 23);
        fake.answer(2, "", "", 0);
        assert_eq!(run_with(&fake, &rsync).code, Some(23));
        assert_eq!(run_with(&fake, &rsync).code, Some(0));
    }

    #[test]
    fn a_program_that_cannot_start_is_a_spawn_error() {
        let call = Call {
            program: "/nonexistent/rsync".into(),
            args: Vec::new(),
        };
        assert!(matches!(call.run(), Err(RsyncError::Spawn { .. })));
    }

    #[test]
    fn a_call_runs_the_probed_program_with_its_argv_and_lc_all_c() {
        let (fake, rsync) = probed_fake();
        let call = rsync.list(&options(), &nas());
        call.run().expect("the fake runs");
        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "the probe and the call");
        assert_eq!(calls[1][0], "transfer");
        assert_eq!(calls[1][1..], argv(&call)[..]);
        let env = String::from_utf8(fake.read("env.log")).expect("text");
        assert_eq!(env, "LC_ALL=C\nLC_ALL=C\n");
    }

    // =========================================================
    // Real rsync
    // =========================================================

    fn real_rsync() -> Rsync {
        Rsync::resolve(None, &env::var_os("PATH").expect("PATH is set")).expect(
            "these tests need rsync 3.2.4 or newer on PATH, /opt/homebrew/bin or /usr/local/bin",
        )
    }

    fn put(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("the directory is writable");
        fs::write(path, content).expect("the directory is writable");
    }

    fn run_ok(call: &Call) -> Outcome {
        let outcome = call.run().expect("rsync starts");
        assert_eq!(
            outcome.code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        outcome
    }

    #[test]
    fn real_rsync_mirrors_with_hard_links_and_the_excludes() {
        use std::os::unix::fs::MetadataExt;
        let rsync = real_rsync();
        let dir = tempfile::tempdir().expect("a temporary directory");
        let collection = dir.path().join("collection");
        let target = dir.path().join("target");
        put(&collection, "piece/file", "content");
        fs::hard_link(collection.join("piece/file"), collection.join("piece/twin"))
            .expect("hard links work here");
        put(&collection, "piece/.DS_Store", "inside");
        put(&collection, ".DS_Store", "root");
        put(&collection, ".antiq/tmp/scratch", "x");
        put(&collection, ".antiq/trash/old", "x");
        put(&collection, ".antiq/lock", "");
        put(&collection, ".antiq/pieces/p/1.tar.zst", "tar");
        put(&target, ".antiq-target", "marker");
        put(&target, "stale", "gone soon");
        let url = TargetUrl::Path {
            root: target.clone(),
        };

        let outcome = run_ok(&rsync.mirror(&options(), &collection, &url, false));

        let listing = String::from_utf8_lossy(&outcome.stdout);
        assert!(listing.contains("*deleting"), "{listing}");
        assert_eq!(
            fs::metadata(target.join("piece/file"))
                .expect("copied")
                .ino(),
            fs::metadata(target.join("piece/twin"))
                .expect("copied")
                .ino(),
            "-H keeps the hard link"
        );
        assert!(target.join("piece/.DS_Store").exists());
        assert!(target.join(".antiq/pieces/p/1.tar.zst").exists());
        for left_out in [
            ".DS_Store",
            ".antiq/tmp",
            ".antiq/trash",
            ".antiq/lock",
            "stale",
        ] {
            assert!(!target.join(left_out).exists(), "{left_out}");
        }
        assert_eq!(
            fs::read_to_string(target.join(".antiq-target")).expect("kept"),
            "marker"
        );
    }

    #[test]
    fn real_rsync_changes_nothing_in_a_dry_run_and_itemizes() {
        let rsync = real_rsync();
        let dir = tempfile::tempdir().expect("a temporary directory");
        let collection = dir.path().join("collection");
        let target = dir.path().join("target");
        put(&collection, "new", "content");
        put(&target, "stale", "x");
        let url = TargetUrl::Path {
            root: target.clone(),
        };

        let outcome = run_ok(&rsync.mirror(&options(), &collection, &url, true));

        let listing = String::from_utf8_lossy(&outcome.stdout);
        assert!(listing.contains("new"), "{listing}");
        assert!(listing.contains("*deleting"), "{listing}");
        assert!(!target.join("new").exists());
        assert!(target.join("stale").exists());
    }

    #[test]
    fn real_rsync_sends_lists_and_fetches_the_marker() {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let rsync = real_rsync();
        let dir = tempfile::tempdir().expect("a temporary directory");
        let target = dir.path().join("target");
        fs::create_dir(&target).expect("the directory is new");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o750)).expect("ours");
        let outgoing = dir.path().join("outgoing");
        let incoming = dir.path().join("incoming");
        put(&outgoing, ".antiq-target", "{\"format\":1}\n");
        fs::create_dir(&incoming).expect("the directory is new");
        let long_ago = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(outgoing.join(".antiq-target"), long_ago).expect("ours");
        let url = TargetUrl::Path {
            root: target.clone(),
        };

        run_ok(&rsync.send_marker(&options(), &outgoing, &url));

        let sent = fs::metadata(target.join(".antiq-target")).expect("sent");
        assert_eq!(sent.mode() & !0o644 & 0o777, 0, "{:o}", sent.mode());
        assert_ne!(sent.mode() & 0o400, 0);
        assert_ne!(sent.mtime(), 1_000_000_000, "-t is not passed");
        assert_eq!(
            fs::metadata(&target).expect("kept").mode() & 0o777,
            0o750,
            "the root keeps its mode"
        );
        let listing = run_ok(&rsync.list(&options(), &url));
        assert!(
            String::from_utf8_lossy(&listing.stdout).contains(".antiq-target"),
            "{}",
            String::from_utf8_lossy(&listing.stdout)
        );
        run_ok(&rsync.fetch_marker(&options(), &url, &incoming));
        assert_eq!(
            fs::read(incoming.join(".antiq-target")).expect("fetched"),
            b"{\"format\":1}\n"
        );
    }
}
