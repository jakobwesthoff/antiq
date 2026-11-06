// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ssh executable and its master connection.

use crate::config::{MachineConfig, RemoteConfig};
use crate::remote::ssh::script::ScriptError;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// The longest ControlPath antiq uses. `ssh -M` binds the path plus a
/// suffix of 17 characters, and macOS allows 104 bytes for a socket path.
pub const MAX_CONTROL_PATH: usize = 86;

/// Why a session cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The host cannot be reached from here right now.
    #[error("the remote is away: {0}")]
    Away(String),
    /// The host answers but refuses, or ssh failed in a way not recognised.
    #[error("the remote is broken: {0}")]
    Broken(String),
    #[error("the ssh control path {path} is {len} bytes, more than the {MAX_CONTROL_PATH} allowed")]
    ControlPathTooLong { path: PathBuf, len: usize },
    #[error("the ssh control directory {path} {reason}")]
    ControlDir { path: PathBuf, reason: String },
    #[error(transparent)]
    Script(#[from] ScriptError),
    #[error("cannot {what}: {source}")]
    Io {
        what: &'static str,
        source: std::io::Error,
    },
}

/// Where to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Target {
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
}

impl Target {
    pub fn new(host: impl Into<String>) -> Target {
        Target {
            host: host.into(),
            port: None,
            user: None,
        }
    }

    pub fn port(mut self, port: u16) -> Target {
        self.port = Some(port);
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Target {
        self.user = Some(user.into());
        self
    }
}

/// Everything a session needs to know before it connects.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionSettings {
    /// The ssh program; `ssh` from `PATH` unless a test says otherwise.
    pub program: PathBuf,
    pub target: Target,
    pub host_key_alias: Option<String>,
    pub connect_timeout: u32,
    pub server_alive_interval: u32,
    pub server_alive_count: u32,
    pub control_persist: u32,
    pub control_dir: PathBuf,
}

/// `/tmp/antiq-<uid>`, the per-user directory of the control sockets.
pub fn default_control_dir() -> PathBuf {
    PathBuf::from(format!(
        "/tmp/antiq-{}",
        rustix::process::geteuid().as_raw()
    ))
}

impl SessionSettings {
    /// The machine defaults of `config.rs` for `target`.
    pub fn new(target: Target) -> SessionSettings {
        let machine = MachineConfig::default();
        SessionSettings {
            program: PathBuf::from("ssh"),
            target,
            host_key_alias: None,
            connect_timeout: machine.connect_timeout,
            server_alive_interval: machine.server_alive_interval,
            server_alive_count: machine.server_alive_count,
            control_persist: machine.control_persist,
            control_dir: default_control_dir(),
        }
    }

    /// The settings of this machine and of the attached remote.
    pub fn from_config(
        machine: &MachineConfig,
        remote: &RemoteConfig,
        target: Target,
    ) -> SessionSettings {
        SessionSettings {
            host_key_alias: remote.host_key_alias.clone(),
            connect_timeout: machine.connect_timeout,
            server_alive_interval: machine.server_alive_interval,
            server_alive_count: machine.server_alive_count,
            control_persist: machine.control_persist,
            ..SessionSettings::new(target)
        }
    }
}

/// How commands reach the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Over the run's master connection.
    Master,
    /// Each command opens its own connection.
    Direct,
}

/// One run's connection to the remote.
#[derive(Debug)]
pub struct Session {
    settings: SessionSettings,
    control_path: PathBuf,
    log_path: PathBuf,
    mode: Mutex<Mode>,
}

static NEXT_SESSION: AtomicU32 = AtomicU32::new(0);

/// `<dir>/<pid>-<n>.sock`. The pid tells the cleanup of a later run
/// whether the owner is gone; `n` keeps two sessions of one process apart.
fn control_path_for(dir: &Path, pid: u32, n: u32) -> Result<PathBuf, SessionError> {
    let path = dir.join(format!("{pid}-{n}.sock"));
    let len = path.as_os_str().len();
    if len > MAX_CONTROL_PATH {
        return Err(SessionError::ControlPathTooLong { path, len });
    }
    Ok(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Directory,
    Symlink,
    Other,
}

fn kind_of(file_type: std::fs::FileType) -> Kind {
    if file_type.is_dir() {
        Kind::Directory
    } else if file_type.is_symlink() {
        Kind::Symlink
    } else {
        Kind::Other
    }
}

/// The control directory holds sockets that carry the user's commands, so
/// it must belong to the user alone: a directory (not a link to one) owned
/// by `uid` with mode 0700. `/tmp` is shared, so another user can create a
/// directory of the same name first.
fn verify_dir(
    path: &Path,
    kind: Kind,
    owner: u32,
    mode: u32,
    uid: u32,
) -> Result<(), SessionError> {
    let problem = match kind {
        Kind::Symlink => Some("is a symbolic link".to_string()),
        Kind::Other => Some("is not a directory".to_string()),
        Kind::Directory if owner != uid => {
            Some(format!("is owned by uid {owner}, not by uid {uid}"))
        }
        Kind::Directory if mode & 0o777 != 0o700 => {
            Some(format!("has mode {:04o}, not 0700", mode & 0o777))
        }
        Kind::Directory => None,
    };
    match problem {
        Some(reason) => Err(SessionError::ControlDir {
            path: path.to_path_buf(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Creates the control directory with mode 0700 when it is missing, then
/// verifies it, so that a directory made by someone else is never used.
fn prepare_dir(dir: &Path, uid: u32) -> Result<(), SessionError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(SessionError::Io {
                what: "create the ssh control directory",
                source,
            });
        }
    }
    let metadata = std::fs::symlink_metadata(dir).map_err(|source| SessionError::Io {
        what: "look at the ssh control directory",
        source,
    })?;
    verify_dir(
        dir,
        kind_of(metadata.file_type()),
        metadata.uid(),
        metadata.mode(),
        uid,
    )
}

/// Whether a process with this pid exists. A process of another user
/// answers `EPERM` and counts as existing.
fn is_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// `(pid, extension)` of a name of the form `<pid>-<n>.sock` or
/// `<pid>-<n>.log`.
fn parse_run_name(name: &str) -> Option<(u32, &str)> {
    let (stem, extension) = name.rsplit_once('.')?;
    if extension != "sock" && extension != "log" {
        return None;
    }
    let (pid, n) = stem.split_once('-')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(pid) || !digits(n) {
        return None;
    }
    Some((pid.parse().ok()?, extension))
}

/// Removes a socket or a log, whichever `path` is meant to be, and leaves
/// anything else (a regular file named like a socket, a symlink) alone.
fn remove_run_file(path: &Path, extension: &str) -> Result<(), SessionError> {
    use std::os::unix::fs::FileTypeExt;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    let file_type = metadata.file_type();
    let expected = if extension == "sock" {
        file_type.is_socket()
    } else {
        file_type.is_file()
    };
    if !expected {
        return Ok(());
    }
    std::fs::remove_file(path).map_err(|source| SessionError::Io {
        what: "remove a leftover ssh control file",
        source,
    })
}

/// Removes the sockets and logs of runs whose process is gone. A killed
/// run leaves its socket behind; its master stops by itself after
/// `ControlPersist`.
fn remove_stale(dir: &Path, is_alive: &dyn Fn(u32) -> bool) -> Result<(), SessionError> {
    let entries = std::fs::read_dir(dir).map_err(|source| SessionError::Io {
        what: "list the ssh control directory",
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SessionError::Io {
            what: "list the ssh control directory",
            source,
        })?;
        let name = entry.file_name();
        let Some((pid, extension)) = name.to_str().and_then(parse_run_name) else {
            continue;
        };
        if !is_alive(pid) {
            remove_run_file(&entry.path(), extension)?;
        }
    }
    Ok(())
}

impl Session {
    /// Checks the settings and prepares the control directory. Starts no
    /// process.
    pub fn new(settings: SessionSettings) -> Result<Session, SessionError> {
        Session::open(settings, NEXT_SESSION.fetch_add(1, Ordering::Relaxed))
    }

    fn open(settings: SessionSettings, n: u32) -> Result<Session, SessionError> {
        let control_path = control_path_for(&settings.control_dir, std::process::id(), n)?;
        let log_path = control_path.with_extension("log");
        prepare_dir(&settings.control_dir, rustix::process::geteuid().as_raw())?;
        remove_stale(&settings.control_dir, &is_alive)?;
        // A killed run of an earlier process with the same pid may have
        // left a master that still answers here; this run must not adopt it.
        remove_run_file(&control_path, "sock")?;
        remove_run_file(&log_path, "log")?;
        Ok(Session {
            settings,
            control_path,
            log_path,
            mode: Mutex::new(Mode::Direct),
        })
    }

    /// The log of this run's master. A master has no stderr once it
    /// detaches, so its messages, the keepalive timeout among them, land
    /// here.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The socket of this run's master.
    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    /// ssh's command line for rsync's `-e`, program first. rsync appends
    /// the host and its own remote command.
    pub fn rsh_argv(&self) -> Vec<OsString> {
        let mut argv = vec![self.settings.program.clone().into_os_string()];
        argv.extend(self.common_args());
        argv.extend(self.command_mode_args());
        argv
    }

    /// Options of every ssh call. The user's `~/.ssh/config` is read as
    /// usual, so the settings that would hide ssh's error text or start
    /// forwardings and local commands are overridden here: the first value
    /// ssh obtains for an option wins, and the command line comes first.
    fn common_args(&self) -> Vec<OsString> {
        let settings = &self.settings;
        let mut args: Vec<OsString> = Vec::new();
        let mut option = |text: String| {
            args.push("-o".into());
            args.push(text.into());
        };
        option("BatchMode=yes".to_string());
        option(format!("ConnectTimeout={}", settings.connect_timeout));
        option(format!(
            "ServerAliveInterval={}",
            settings.server_alive_interval
        ));
        option(format!(
            "ServerAliveCountMax={}",
            settings.server_alive_count
        ));
        option("LogLevel=INFO".to_string());
        option("ClearAllForwardings=yes".to_string());
        option("PermitLocalCommand=no".to_string());
        if let Some(alias) = &settings.host_key_alias {
            option(format!("HostKeyAlias={alias}"));
        }
        if let Some(port) = settings.target.port {
            args.push("-p".into());
            args.push(port.to_string().into());
        }
        if let Some(user) = &settings.target.user {
            args.push("-l".into());
            args.push(user.into());
        }
        args
    }

    /// What a call that runs a script adds: no terminal, no configured
    /// remote command, and the way to reach the remote.
    fn command_mode_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["-T".into(), "-o".into(), "RemoteCommand=none".into()];
        match *self.mode.lock().expect("no panic holds the mode lock") {
            Mode::Master => {
                args.extend(["-S".into(), self.control_path.clone().into_os_string()]);
                args.extend(["-o".into(), "ControlMaster=no".into()]);
            }
            // ControlPath=none keeps a ControlMaster of the user's own
            // config from starting a master for this call.
            Mode::Direct => {
                args.extend(["-o".into(), "ControlMaster=no".into()]);
                args.extend(["-o".into(), "ControlPath=none".into()]);
            }
        }
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::UnixListener;

    fn not_root() {
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "these tests assume a non-root user"
        );
    }

    /// A control directory of its own, short enough for a socket path.
    fn short_dir() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("/tmp is writable");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("the directory is ours");
        dir
    }

    fn settings_in(dir: &Path) -> SessionSettings {
        let mut settings = SessionSettings::new(Target::new("nas"));
        settings.control_dir = dir.to_path_buf();
        settings
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().expect("UTF-8 arguments"))
            .collect()
    }

    // ----- settings -----

    #[test]
    fn the_default_settings_are_the_machine_defaults() {
        let settings = SessionSettings::new(Target::new("nas"));
        assert_eq!(settings.program, PathBuf::from("ssh"));
        assert_eq!(settings.connect_timeout, 30);
        assert_eq!(settings.server_alive_interval, 15);
        assert_eq!(settings.server_alive_count, 4);
        assert_eq!(settings.control_persist, 60);
        assert_eq!(settings.host_key_alias, None);
    }

    #[test]
    fn the_default_control_directory_is_per_user_in_tmp() {
        let uid = rustix::process::geteuid().as_raw();
        assert_eq!(
            default_control_dir(),
            PathBuf::from(format!("/tmp/antiq-{uid}"))
        );
    }

    #[test]
    fn settings_from_the_config_take_the_machine_values_and_the_alias() {
        let machine = MachineConfig {
            connect_timeout: 9,
            server_alive_interval: 10,
            server_alive_count: 11,
            control_persist: 12,
            ..MachineConfig::default()
        };
        let remote = RemoteConfig {
            url: "ssh://nas/srv".to_string(),
            id: crate::id::ObjectId::from_ulid(ulid::Ulid::from_parts(1, 1)),
            host_key_alias: Some("nas-alias".to_string()),
        };
        let settings = SessionSettings::from_config(&machine, &remote, Target::new("nas"));
        assert_eq!(settings.connect_timeout, 9);
        assert_eq!(settings.server_alive_interval, 10);
        assert_eq!(settings.server_alive_count, 11);
        assert_eq!(settings.control_persist, 12);
        assert_eq!(settings.host_key_alias.as_deref(), Some("nas-alias"));
    }

    #[test]
    fn a_target_collects_user_and_port() {
        let target = Target::new("nas").port(2222).user("admin");
        assert_eq!(target.host, "nas");
        assert_eq!(target.port, Some(2222));
        assert_eq!(target.user.as_deref(), Some("admin"));
    }

    // ----- control path -----

    fn path_of_len(len: usize) -> PathBuf {
        // "/tmp/" + a name of the length that makes the whole path `len`
        // once "<pid>-<n>.sock" is appended by `control_path_for`.
        let tail = format!("{}-{}.sock", 1, 0).len();
        PathBuf::from(format!("/tmp/{}", "d".repeat(len - 5 - 1 - tail)))
    }

    #[test]
    fn a_control_path_of_86_bytes_is_accepted() {
        let path = control_path_for(&path_of_len(86), 1, 0).expect("86 bytes fit");
        assert_eq!(path.as_os_str().len(), 86);
    }

    #[test]
    fn a_control_path_of_87_bytes_is_refused() {
        match control_path_for(&path_of_len(87), 1, 0) {
            Err(SessionError::ControlPathTooLong { len, .. }) => assert_eq!(len, 87),
            other => panic!("expected ControlPathTooLong, got {other:?}"),
        }
    }

    #[test]
    fn the_control_path_names_the_pid_and_the_session() {
        let path = control_path_for(Path::new("/tmp/antiq-501"), 4242, 7).expect("fits");
        assert_eq!(path, PathBuf::from("/tmp/antiq-501/4242-7.sock"));
    }

    #[test]
    fn the_master_log_sits_next_to_the_socket() {
        let dir = short_dir();
        let session = Session::open(settings_in(dir.path()), 5).expect("session");
        let pid = std::process::id();
        assert_eq!(session.log_path(), dir.path().join(format!("{pid}-5.log")));
        assert_eq!(
            session.control_path(),
            dir.path().join(format!("{pid}-5.sock"))
        );
    }

    #[test]
    fn two_sessions_in_one_process_get_different_paths() {
        let dir = short_dir();
        let first = Session::new(settings_in(dir.path())).expect("session");
        let second = Session::new(settings_in(dir.path())).expect("session");
        assert_ne!(first.control_path(), second.control_path());
    }

    #[test]
    fn a_session_with_a_too_long_path_creates_nothing() {
        let dir = short_dir();
        let long = dir.path().join("d".repeat(80));
        assert!(matches!(
            Session::new(settings_in(&long)),
            Err(SessionError::ControlPathTooLong { .. })
        ));
        assert!(!long.exists(), "the directory is not created");
    }

    // ----- control directory -----

    fn check(kind: Kind, owner: u32, mode: u32) -> Result<(), SessionError> {
        verify_dir(Path::new("/tmp/antiq-501"), kind, owner, mode, 501)
    }

    #[test]
    fn a_directory_of_the_user_with_mode_0700_is_accepted() {
        check(Kind::Directory, 501, 0o40700).expect("accepted");
    }

    #[test]
    fn a_directory_of_another_user_is_refused() {
        let error = check(Kind::Directory, 0, 0o40700).expect_err("refused");
        assert!(error.to_string().contains("owned by uid 0"), "{error}");
    }

    #[test]
    fn a_directory_open_to_the_group_is_refused() {
        let error = check(Kind::Directory, 501, 0o40770).expect_err("refused");
        assert!(error.to_string().contains("mode 0770"), "{error}");
    }

    #[test]
    fn a_directory_open_to_everyone_is_refused() {
        let error = check(Kind::Directory, 501, 0o40755).expect_err("refused");
        assert!(error.to_string().contains("mode 0755"), "{error}");
    }

    #[test]
    fn a_symlink_is_refused() {
        let error = check(Kind::Symlink, 501, 0o120700).expect_err("refused");
        assert!(error.to_string().contains("symbolic link"), "{error}");
    }

    #[test]
    fn a_file_is_refused() {
        let error = check(Kind::Other, 501, 0o100700).expect_err("refused");
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn a_missing_control_directory_is_created_with_mode_0700() {
        not_root();
        let parent = short_dir();
        let dir = parent.path().join("antiq");
        Session::new(settings_in(&dir)).expect("session");
        let mode = std::fs::metadata(&dir)
            .expect("created")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_control_directory_with_mode_0755_is_refused() {
        not_root();
        let parent = short_dir();
        let dir = parent.path().join("antiq");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&dir)
            .expect("create");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(matches!(
            Session::new(settings_in(&dir)),
            Err(SessionError::ControlDir { .. })
        ));
    }

    #[test]
    fn a_symlink_as_control_directory_is_refused() {
        not_root();
        let parent = short_dir();
        let real = parent.path().join("real");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&real)
            .expect("create");
        let link = parent.path().join("antiq");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        match Session::new(settings_in(&link)) {
            Err(SessionError::ControlDir { reason, .. }) => {
                assert!(reason.contains("symbolic link"), "{reason}")
            }
            other => panic!("expected ControlDir, got {other:?}"),
        }
    }

    #[test]
    fn a_control_directory_below_a_missing_parent_cannot_be_created() {
        let parent = short_dir();
        let dir = parent.path().join("missing/antiq");
        match Session::new(settings_in(&dir)) {
            Err(SessionError::Io { what, .. }) => assert!(what.contains("create"), "{what}"),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn a_regular_file_as_control_directory_is_refused() {
        not_root();
        let parent = short_dir();
        let file = parent.path().join("antiq");
        std::fs::write(&file, b"").expect("write");
        assert!(matches!(
            Session::new(settings_in(&file)),
            Err(SessionError::ControlDir { .. })
        ));
    }

    // ----- stale sockets -----

    fn bind(dir: &Path, name: &str) -> UnixListener {
        UnixListener::bind(dir.join(name)).expect("bind a socket")
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("UTF-8")
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_socket_and_log_of_a_dead_run_are_removed() {
        let dir = short_dir();
        drop(bind(dir.path(), "111-0.sock"));
        std::fs::write(dir.path().join("111-0.log"), b"x").expect("write");
        remove_stale(dir.path(), &|_| false).expect("cleanup");
        assert_eq!(names(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn the_socket_of_a_live_run_stays() {
        let dir = short_dir();
        drop(bind(dir.path(), "111-0.sock"));
        std::fs::write(dir.path().join("111-0.log"), b"x").expect("write");
        remove_stale(dir.path(), &|pid| pid == 111).expect("cleanup");
        assert_eq!(names(dir.path()), ["111-0.log", "111-0.sock"]);
    }

    #[test]
    fn a_regular_file_named_like_a_socket_stays() {
        let dir = short_dir();
        std::fs::write(dir.path().join("111-0.sock"), b"x").expect("write");
        remove_stale(dir.path(), &|_| false).expect("cleanup");
        assert_eq!(names(dir.path()), ["111-0.sock"]);
    }

    #[test]
    fn a_symlink_named_like_a_socket_stays() {
        let dir = short_dir();
        let listener = bind(dir.path(), "target");
        std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("111-0.sock"))
            .expect("symlink");
        remove_stale(dir.path(), &|_| false).expect("cleanup");
        assert_eq!(names(dir.path()), ["111-0.sock", "target"]);
        drop(listener);
    }

    #[test]
    fn other_names_stay() {
        let dir = short_dir();
        drop(bind(dir.path(), "abc-0.sock"));
        drop(bind(dir.path(), "111.sock"));
        drop(bind(dir.path(), "111-x.sock"));
        std::fs::write(dir.path().join("notes.txt"), b"x").expect("write");
        remove_stale(dir.path(), &|_| false).expect("cleanup");
        assert_eq!(
            names(dir.path()),
            ["111-x.sock", "111.sock", "abc-0.sock", "notes.txt"]
        );
    }

    #[test]
    fn a_new_session_removes_the_sockets_of_dead_runs() {
        let dir = short_dir();
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("true runs");
        let dead = child.id();
        child.wait().expect("true ends");
        drop(bind(dir.path(), &format!("{dead}-0.sock")));
        Session::new(settings_in(dir.path())).expect("session");
        assert_eq!(names(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_missing_directory_cannot_be_cleaned() {
        let parent = short_dir();
        let result = remove_stale(&parent.path().join("missing"), &|_| false);
        assert!(matches!(result, Err(SessionError::Io { .. })), "{result:?}");
    }

    #[test]
    fn a_stale_socket_that_cannot_be_removed_is_an_error() {
        not_root();
        let dir = short_dir();
        drop(bind(dir.path(), "111-0.sock"));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("chmod");
        let result = remove_stale(dir.path(), &|_| false);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod back so the directory can be removed");
        match result {
            Err(SessionError::Io { what, .. }) => assert!(what.contains("remove"), "{what}"),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn pids_that_no_process_can_have_are_not_alive() {
        assert!(!is_alive(0));
        assert!(!is_alive(u32::MAX));
    }

    #[test]
    fn a_pid_that_cannot_be_checked_counts_as_alive() {
        // pid 1 belongs to another user: kill(1, 0) answers EPERM.
        not_root();
        assert!(is_alive(1));
    }

    #[test]
    fn this_process_is_alive_and_a_waited_child_is_not() {
        assert!(is_alive(std::process::id()));
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("true runs");
        let pid = child.id();
        child.wait().expect("true ends");
        assert!(!is_alive(pid));
    }

    #[test]
    fn a_socket_at_the_sessions_own_path_is_removed() {
        // A killed run with the same pid can leave a master behind that
        // still answers on this path; the new run must not adopt it.
        let dir = short_dir();
        let name = format!("{}-900000", std::process::id());
        drop(bind(dir.path(), &format!("{name}.sock")));
        std::fs::write(dir.path().join(format!("{name}.log")), b"x").expect("write");
        let session = Session::open(settings_in(dir.path()), 900000).expect("session");
        assert!(!session.control_path().exists());
        assert!(!session.log_path.exists());
    }

    #[test]
    fn a_file_at_the_sessions_own_path_stays() {
        let dir = short_dir();
        let name = format!("{}-900001.sock", std::process::id());
        std::fs::write(dir.path().join(&name), b"mine").expect("write");
        Session::open(settings_in(dir.path()), 900001).expect("session");
        assert_eq!(names(dir.path()), [name]);
    }

    // ----- rsync's ssh command -----

    #[test]
    fn rsh_argv_in_master_mode_joins_the_master() {
        let dir = short_dir();
        let session = Session::new(settings_in(dir.path())).expect("session");
        *session.mode.lock().expect("lock") = Mode::Master;
        let path = session.control_path().to_str().expect("UTF-8").to_string();
        assert_eq!(
            strings(session.rsh_argv()),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=30",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
                "-o",
                "LogLevel=INFO",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "PermitLocalCommand=no",
                "-T",
                "-o",
                "RemoteCommand=none",
                "-S",
                &path,
                "-o",
                "ControlMaster=no",
            ]
        );
    }

    #[test]
    fn rsh_argv_in_direct_mode_turns_the_control_path_off() {
        let dir = short_dir();
        let session = Session::new(settings_in(dir.path())).expect("session");
        let argv = strings(session.rsh_argv());
        assert_eq!(
            &argv[argv.len() - 4..],
            ["-o", "ControlMaster=no", "-o", "ControlPath=none"]
        );
        assert!(!argv.contains(&"-S".to_string()));
    }

    #[test]
    fn rsh_argv_names_alias_port_and_user_when_set() {
        let dir = short_dir();
        let mut settings = settings_in(dir.path());
        settings.host_key_alias = Some("nas-alias".to_string());
        settings.target = Target::new("nas").port(2222).user("admin");
        let session = Session::new(settings).expect("session");
        let argv = strings(session.rsh_argv());
        let joined = argv.join(" ");
        assert!(joined.contains("-o HostKeyAlias=nas-alias"), "{joined}");
        assert!(joined.contains("-p 2222"), "{joined}");
        assert!(joined.contains("-l admin"), "{joined}");
        assert!(
            !joined.contains("nas "),
            "the host is rsync's to append: {joined}"
        );
    }

    #[test]
    fn rsh_argv_has_no_alias_port_or_user_by_default() {
        let dir = short_dir();
        let session = Session::new(settings_in(dir.path())).expect("session");
        let joined = strings(session.rsh_argv()).join(" ");
        assert!(!joined.contains("HostKeyAlias"), "{joined}");
        assert!(!joined.contains("-p "), "{joined}");
        assert!(!joined.contains("-l "), "{joined}");
    }

    #[test]
    fn the_program_is_the_first_word_of_rsh_argv() {
        let dir = short_dir();
        let mut settings = settings_in(dir.path());
        settings.program = PathBuf::from("/opt/ssh wrapper/ssh");
        let session = Session::new(settings).expect("session");
        assert_eq!(
            session.rsh_argv()[0],
            OsString::from("/opt/ssh wrapper/ssh")
        );
    }

    #[test]
    fn a_session_can_be_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Session>();
    }
}
