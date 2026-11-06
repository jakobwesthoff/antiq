// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SSH remote.
//!
//! Every operation is one script run on the NAS through the session
//! (`script.rs`), and every script starts by checking the root and the
//! marker, as the path remote's `require` does. Scripts answer in frames
//! when they return bytes.

pub mod classify;
pub mod script;
pub mod session;

use super::marker::{self, Marker};
use super::{
    Assessment, Created, KnownVersions, MetaFile, Placement, Reachability, Remote, RemoteError,
    RemoteFile, Transfer, Upload,
};
use crate::config::{MachineConfig, RemoteConfig};
use crate::id::ObjectId;
use crate::report::Warning;
use jiff::Timestamp;
use script::Script;
use session::{Output, Session, SessionError, SessionSettings, Target};
use std::path::Path;

/// Where an ssh remote lives: the parts of its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub root: String,
}

// =========================================================
// Status codes of the scripts
// =========================================================

// All below 255, which is ssh's own. 17 is `script::EXISTS_STATUS`.

/// The root does not exist below a directory that can be searched.
const ROOT_MISSING: i32 = 40;
const MARKER_MISSING: i32 = 41;
const NO_HASH_TOOL: i32 = 42;
const NO_STAT: i32 = 43;
/// The root is not a directory, or a directory above it cannot be
/// searched.
const ROOT_UNUSABLE: i32 = 49;
const MARKER_UNREADABLE: i32 = 50;

/// Shell functions every script of the remote starts with. They use
/// only POSIX `sh` and tools every NAS has.
///
/// - `antiq_stat FILE` prints `<size> <mtime>`; status 43 when `stat`
///   knows neither the GNU nor the BSD form.
/// - `antiq_sha256 FILE` prints the SHA-256 as 64 lower-case hex digits
///   from the first tool of `sha256sum`, `shasum`, `sha256` and `openssl`
///   that gives exactly that; status 42 when none does. Tools differ in
///   what they print and a NAS may carry a broken one, so only a result
///   of the right shape counts.
/// - `antiq_emit KIND FILE` prints `@KIND <length>`, the file and a
///   newline.
/// - `antiq_root_check ROOT` ends the script unless ROOT is a directory
///   with a readable marker, and prints the marker as a frame.
pub(crate) const PRELUDE: &str = r#"antiq_stat() {
    if stat -c '%s' / >/dev/null 2>&1; then
        stat -c '%s %Y' "$1"
    elif stat -f '%z' / >/dev/null 2>&1; then
        stat -f '%z %m' "$1"
    else
        return 43
    fi
}
antiq_sha256() {
    for _tool in 'sha256sum' 'shasum -a 256' 'sha256 -q' 'openssl dgst -sha256 -r'; do
        _hash=$($_tool "$1" 2>/dev/null) || continue
        _hash=${_hash%% *}
        case $_hash in *[!0-9a-f]*) continue ;; esac
        if [ "${#_hash}" -eq 64 ]; then
            printf '%s
' "$_hash"
            return 0
        fi
    done
    return 42
}
antiq_emit() {
    _stat=$(antiq_stat "$2")
    _rc=$?
    [ "$_rc" -eq 0 ] || return "$_rc"
    printf '@%s %s
' "$1" "${_stat%% *}"
    cat "$2" || return 1
    printf '
'
}
antiq_root_check() {
    if [ ! -d "$1" ]; then
        _near=$1
        while [ ! -e "$_near" ] && [ ! -h "$_near" ]; do
            _near=$(dirname -- "$_near")
        done
        if [ "$_near" != "$1" ] && [ -d "$_near" ] && [ -x "$_near" ]; then
            exit 40
        fi
        if [ "$_near" = "$1" ]; then
            printf '%s is not a directory
' "$1" >&2
        else
            printf '%s cannot be searched or is not a directory
' "$_near" >&2
        fi
        exit 49
    fi
    _marker="$1/antiq-remote.json"
    if [ ! -e "$_marker" ] && [ ! -h "$_marker" ]; then
        exit 41
    fi
    antiq_emit marker "$_marker"
    _rc=$?
    [ "$_rc" -eq 43 ] && exit 43
    [ "$_rc" -eq 0 ] || exit 50
}
"#;

/// The remote on a NAS reached over ssh.
pub struct SshRemote {
    session: Session,
    host: String,
    root: String,
    id: ObjectId,
    #[allow(dead_code)] // read by the transfers
    machine: MachineConfig,
}

/// What a script answered after the root check passed.
struct Answer {
    /// The remote clock, for a script made with a clock.
    clock: Option<i64>,
    /// The frames after the marker frame.
    #[allow(dead_code)] // read by the operations that return data
    frames: Vec<Frame>,
}

/// One `@kind args... length` block of a script's answer.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    kind: String,
    args: Vec<String>,
    bytes: Vec<u8>,
}

fn malformed(why: &str) -> RemoteError {
    RemoteError::Broken(format!("the answer of the remote is malformed: {why}"))
}

/// Takes the frames off the front of `data`: the header line, `length`
/// bytes and a newline each.
fn read_frames(mut data: &[u8]) -> Result<Vec<Frame>, RemoteError> {
    let mut frames = Vec::new();
    while !data.is_empty() {
        let end = data
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or_else(|| malformed("a frame header has no end"))?;
        let header = std::str::from_utf8(&data[..end])
            .map_err(|_| malformed("a frame header is not UTF-8"))?;
        let mut words = header
            .strip_prefix('@')
            .ok_or_else(|| malformed("a frame header does not start with @"))?
            .split(' ');
        let kind = words.next().unwrap_or_default().to_string();
        let mut args: Vec<String> = words.map(String::from).collect();
        let length: usize = args
            .pop()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| malformed("a frame header has no length"))?;
        let body = &data[end + 1..];
        if body.len() <= length || body[length] != b'\n' {
            return Err(malformed("a frame is shorter than its length"));
        }
        frames.push(Frame {
            kind,
            args,
            bytes: body[..length].to_vec(),
        });
        data = &body[length + 1..];
    }
    Ok(frames)
}

fn map_session(error: SessionError) -> RemoteError {
    match error {
        SessionError::Away(message) => RemoteError::Away {
            message,
            overdue: false,
        },
        SessionError::Broken(message) => RemoteError::Broken(message),
        other => RemoteError::Session(other),
    }
}

impl SshRemote {
    /// Prepares the session of the remote the config attaches. Nothing is
    /// sent: the first operation connects.
    pub fn connect(
        target: &SshTarget,
        remote: &RemoteConfig,
        machine: &MachineConfig,
    ) -> Result<SshRemote, RemoteError> {
        let mut ssh_target = Target::new(&target.host);
        ssh_target.port = target.port;
        ssh_target.user.clone_from(&target.user);
        let settings = SessionSettings::from_config(machine, remote, ssh_target);
        let session = Session::new(settings).map_err(map_session)?;
        Ok(SshRemote::with_session(
            session,
            target.root.clone(),
            remote.id,
            machine.clone(),
            target.host.clone(),
        ))
    }

    pub(crate) fn with_session(
        session: Session,
        root: String,
        id: ObjectId,
        machine: MachineConfig,
        host: String,
    ) -> SshRemote {
        SshRemote {
            session,
            host,
            root,
            id,
            machine,
        }
    }

    /// A script that starts with the root check. `$1` is the root.
    fn script(&self, body: &str) -> Script {
        Script::new(&format!("{PRELUDE}\nantiq_root_check \"$1\"\n{body}")).arg(&self.root)
    }

    /// Runs `script` and turns what the root check found and the status
    /// into the error that says it. `action` names the operation in an Io
    /// error.
    fn run(
        &self,
        script: &Script,
        clock: bool,
        action: &'static str,
    ) -> Result<Answer, RemoteError> {
        let output = self.session.run(script).map_err(map_session)?;
        self.answer(output, clock, action)
    }

    fn answer(
        &self,
        output: Output,
        clock: bool,
        action: &'static str,
    ) -> Result<Answer, RemoteError> {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let root = &self.root;
        match output.status {
            0 => {}
            ROOT_MISSING => {
                return Err(RemoteError::Away {
                    message: format!("{root} does not exist on {}", self.host),
                    overdue: false,
                });
            }
            MARKER_MISSING => {
                return Err(RemoteError::Broken(format!(
                    "{root} holds no {}",
                    marker::FILE_NAME
                )));
            }
            ROOT_UNUSABLE => return Err(RemoteError::Broken(format!("{root}: {stderr}"))),
            MARKER_UNREADABLE => {
                return Err(RemoteError::Broken(format!(
                    "{root}/{} cannot be read",
                    marker::FILE_NAME
                )));
            }
            NO_HASH_TOOL => {
                return Err(RemoteError::Broken(format!(
                    "{} has no sha256sum, shasum, sha256 or openssl that computes a SHA-256",
                    self.host
                )));
            }
            NO_STAT => {
                return Err(RemoteError::Broken(format!(
                    "{} has no stat that prints size and modification time",
                    self.host
                )));
            }
            status => {
                let reason = if stderr.is_empty() {
                    format!("the command ended with status {status}")
                } else {
                    stderr
                };
                return Err(RemoteError::Io {
                    action,
                    path: root.clone(),
                    source: std::io::Error::other(reason),
                });
            }
        }
        let (clock, data) = if clock {
            let (clock, data) = output.clock_and_rest().map_err(map_session)?;
            (Some(clock), data)
        } else {
            (None, &output.stdout[..])
        };
        let mut frames = read_frames(data)?;
        if frames.first().map(|frame| frame.kind.as_str()) != Some("marker") {
            return Err(malformed("the marker is missing"));
        }
        let frame = frames.remove(0);
        self.check_marker(&frame.bytes)?;
        Ok(Answer { clock, frames })
    }

    fn check_marker(&self, bytes: &[u8]) -> Result<(), RemoteError> {
        let root = &self.root;
        match Marker::parse(bytes) {
            Ok(found) if found.id == self.id => Ok(()),
            Ok(found) => Err(RemoteError::Broken(format!(
                "{root} holds the marker of another remote ({}), not {}",
                found.id, self.id
            ))),
            Err(error) => Err(RemoteError::Broken(format!(
                "{root}/{}: {error}",
                marker::FILE_NAME
            ))),
        }
    }

    fn check_root(&self, body: &str, action: &'static str) -> Result<(), RemoteError> {
        self.run(&self.script(body), false, action).map(|_| ())
    }
}

fn pending() -> RemoteError {
    RemoteError::Unsupported("this operation of the ssh remote".into())
}

impl Remote for SshRemote {
    fn reachability(&self) -> Result<Assessment, RemoteError> {
        match self.check_root("", "check") {
            Ok(()) => Ok(Assessment::new(Reachability::Reachable, "")),
            Err(RemoteError::Away { message, .. }) => {
                Ok(Assessment::new(Reachability::Away, message))
            }
            Err(RemoteError::Broken(message)) => Ok(Assessment::new(Reachability::Broken, message)),
            Err(other) => Err(other),
        }
    }

    fn begin_phase(&self) -> Result<(), RemoteError> {
        self.session.begin_phase().map_err(map_session)?;
        self.check_root("", "check")
    }

    fn clock(&self) -> Result<Timestamp, RemoteError> {
        let answer = self.run(&self.script("").with_clock(), true, "read the clock of")?;
        let seconds = answer.clock.expect("a script with a clock answers with it");
        Timestamp::from_second(seconds)
            .map_err(|_| malformed("the clock is outside the range of a timestamp"))
    }

    fn flush(&self) -> Result<(), RemoteError> {
        self.check_root("sync\n", "sync")
    }

    fn take_warnings(&self) -> Vec<Warning> {
        self.session.take_warnings()
    }

    fn finish(&self) {
        self.session.finish();
    }

    fn pull_metadata(&self, _known: &KnownVersions) -> Result<Vec<MetaFile>, RemoteError> {
        Err(pending())
    }
    fn list_objects(&self) -> Result<Vec<ObjectId>, RemoteError> {
        Err(pending())
    }
    fn list_dir(&self, _id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
        Err(pending())
    }
    fn list_partial(&self, _id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
        Err(pending())
    }
    fn read_file(&self, _id: ObjectId, _name: &str) -> Result<Vec<u8>, RemoteError> {
        Err(pending())
    }
    fn get_payload(&self, _id: ObjectId, _name: &str, _to: &Path) -> Result<Transfer, RemoteError> {
        Err(pending())
    }
    fn create_exclusive(&self, _id: ObjectId, _name: &str) -> Result<Created, RemoteError> {
        Err(pending())
    }
    fn write_temp(&self, _id: ObjectId, _temp: &str, _bytes: &[u8]) -> Result<(), RemoteError> {
        Err(pending())
    }
    fn rename_into_place(
        &self,
        _id: ObjectId,
        _temp: &str,
        _name: &str,
    ) -> Result<Placement, RemoteError> {
        Err(pending())
    }
    fn put_payload(&self, _upload: &Upload<'_>) -> Result<Transfer, RemoteError> {
        Err(pending())
    }
    fn delete(&self, _id: ObjectId, _name: &str) -> Result<(), RemoteError> {
        Err(pending())
    }
    fn delete_partial(&self, _id: ObjectId, _name: &str) -> Result<(), RemoteError> {
        Err(pending())
    }
    fn checksum(&self, _id: ObjectId, _name: &str) -> Result<String, RemoteError> {
        Err(pending())
    }
}

#[cfg(test)]
mod tests {
    use super::session::{Session, SessionError, SessionSettings, Target};
    use super::*;
    use crate::error::HasKind as _;
    use crate::remote::RemoteState;
    use crate::remote::marker::{self, Marker};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

    fn short_dir() -> TempDir {
        let dir = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("/tmp is writable");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("the directory is ours");
        dir
    }

    fn not_root() {
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "these tests assume a non-root user"
        );
    }

    /// The far side: a directory on this machine that the ssh shim runs
    /// its commands against, with the state directory of the shim.
    struct Far {
        state: TempDir,
        control: TempDir,
        root: TempDir,
    }

    impl Far {
        /// A far side whose root holds the marker of `REMOTE_ID`.
        fn new() -> Far {
            not_root();
            let perl = Command::new("perl").arg("-v").output();
            assert!(
                perl.is_ok_and(|output| output.status.success()),
                "the ssh shim needs perl to run a master process"
            );
            let far = Far {
                state: short_dir(),
                control: short_dir(),
                root: tempfile::tempdir().expect("temp dir"),
            };
            let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-shim.sh");
            std::os::unix::fs::symlink(script, far.state.path().join("ssh")).expect("symlink");
            far.write_marker(&Marker::new(id()));
            far
        }

        fn write_marker(&self, marker: &Marker) {
            std::fs::write(self.root().join(marker::FILE_NAME), marker.to_bytes())
                .expect("the marker is written");
        }

        fn root(&self) -> &Path {
            self.root.path()
        }

        fn session(&self) -> Session {
            let mut settings = SessionSettings::new(Target::new("nas"));
            settings.program = self.state.path().join("ssh");
            settings.control_dir = self.control.path().to_path_buf();
            Session::new(settings).expect("session")
        }

        fn remote_at(&self, root: &Path) -> SshRemote {
            SshRemote::with_session(
                self.session(),
                root.to_str().expect("UTF-8").to_string(),
                id(),
                MachineConfig::default(),
                "nas".into(),
            )
        }

        fn remote(&self) -> SshRemote {
            self.remote_at(self.root())
        }

        /// Puts a tool called `name` in front of the far side's `PATH`.
        /// `body` is the shell text of the wrapper.
        fn wrap(&self, name: &str, body: &str) {
            let dir = self.state.path().join("wrappers");
            std::fs::create_dir_all(&dir).expect("wrapper directory");
            let file = dir.join(name);
            std::fs::write(&file, format!("#!/bin/sh\n{body}\n")).expect("wrapper");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                .expect("wrapper is executable");
            let path = format!("{}:{}", dir.display(), std::env::var("PATH").expect("PATH"));
            std::fs::write(self.state.path().join("remote_path"), path).expect("remote_path");
        }

        /// `fault` is a line of the shim's `fault` file: a name and,
        /// optionally, `start` or `command`.
        fn fault(&self, fault: &str) {
            std::fs::write(self.state.path().join("fault"), format!("{fault}\n")).expect("fault");
        }
    }

    fn id() -> ObjectId {
        REMOTE_ID.parse().expect("a ULID")
    }

    /// Runs `stdin` through a local `/bin/sh -s` the way the far side
    /// does.
    fn run_sh(stdin: &[u8], dir: &Path) -> (i32, Vec<u8>) {
        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("/bin/sh starts");
        let mut pipe = child.stdin.take().expect("stdin is piped");
        let _ = pipe.write_all(stdin);
        drop(pipe);
        let out = child.wait_with_output().expect("sh finishes");
        (out.status.code().expect("sh exits"), out.stdout)
    }

    fn run_prelude(body: &str, dir: &Path) -> (i32, String) {
        let script = script::Script::new(&format!("{PRELUDE}\n{body}"));
        let (status, out) = run_sh(&script.to_stdin().expect("builds"), dir);
        (status, String::from_utf8(out).expect("UTF-8 output"))
    }

    fn assess(remote: &SshRemote) -> Assessment {
        remote.reachability().expect("an answer")
    }

    // ----- reachability of the root -----

    #[test]
    fn a_root_with_its_marker_is_reachable() {
        let far = Far::new();
        assert_eq!(assess(&far.remote()).reachability, Reachability::Reachable);
    }

    #[test]
    fn an_absent_root_is_away() {
        let far = Far::new();
        let remote = far.remote_at(&far.root().join("absent/below"));
        let found = assess(&remote);
        assert_eq!(found.reachability, Reachability::Away, "{}", found.reason);
        assert!(found.reason.contains("absent/below"), "{}", found.reason);
    }

    #[test]
    fn a_root_that_is_a_file_is_broken() {
        let far = Far::new();
        let file = far.root().join("plain");
        std::fs::write(&file, "x").expect("file");
        let found = assess(&far.remote_at(&file));
        assert_eq!(found.reachability, Reachability::Broken, "{}", found.reason);
        assert!(found.reason.contains("not a directory"), "{}", found.reason);
    }

    #[test]
    fn a_root_below_a_directory_that_cannot_be_searched_is_broken() {
        let far = Far::new();
        let locked = far.root().join("locked");
        std::fs::create_dir_all(locked.join("inner")).expect("dirs");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let found = assess(&far.remote_at(&locked.join("inner")));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_eq!(found.reachability, Reachability::Broken, "{}", found.reason);
        assert!(found.reason.contains("locked"), "{}", found.reason);
    }

    #[test]
    fn a_root_without_a_marker_is_broken() {
        let far = Far::new();
        std::fs::remove_file(far.root().join(marker::FILE_NAME)).expect("remove");
        let found = assess(&far.remote());
        assert_eq!(found.reachability, Reachability::Broken, "{}", found.reason);
        assert!(found.reason.contains(marker::FILE_NAME), "{}", found.reason);
    }

    #[test]
    fn a_marker_that_cannot_be_read_is_broken() {
        let far = Far::new();
        let marker = far.root().join(marker::FILE_NAME);
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let found = assess(&far.remote());
        assert_eq!(found.reachability, Reachability::Broken, "{}", found.reason);
        assert!(found.reason.contains("cannot be read"), "{}", found.reason);
    }

    #[test]
    fn the_marker_of_another_remote_is_broken() {
        let far = Far::new();
        let other: ObjectId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("a ULID");
        far.write_marker(&Marker::new(other));
        let found = assess(&far.remote());
        assert_eq!(found.reachability, Reachability::Broken, "{}", found.reason);
        assert!(found.reason.contains("another remote"), "{}", found.reason);
    }

    #[test]
    fn a_marker_of_a_newer_format_is_broken() {
        let far = Far::new();
        std::fs::write(
            far.root().join(marker::FILE_NAME),
            format!("{{\"format\":99,\"id\":\"{REMOTE_ID}\",\"layout\":1}}\n"),
        )
        .expect("marker");
        let found = assess(&far.remote());
        assert_eq!(found.reachability, Reachability::Broken, "{}", found.reason);
    }

    #[test]
    fn a_marker_that_is_no_json_is_broken() {
        let far = Far::new();
        std::fs::write(far.root().join(marker::FILE_NAME), "not json\n").expect("marker");
        let found = assess(&far.remote());
        assert_eq!(found.reachability, Reachability::Broken, "{}", found.reason);
    }

    #[test]
    fn a_host_that_refuses_the_connection_is_away() {
        let far = Far::new();
        far.fault("connect-refused");
        assert_eq!(assess(&far.remote()).reachability, Reachability::Away);
    }

    #[test]
    fn a_host_that_refuses_the_login_is_broken() {
        let far = Far::new();
        far.fault("permission-denied");
        assert_eq!(assess(&far.remote()).reachability, Reachability::Broken);
    }

    #[test]
    fn a_session_error_that_says_nothing_about_the_host_is_no_assessment() {
        let far = Far::new();
        let mut settings = SessionSettings::new(Target::new("nas"));
        settings.program = PathBuf::from("/nonexistent/ssh");
        settings.control_dir = far.control.path().to_path_buf();
        let remote = SshRemote::with_session(
            Session::new(settings).expect("session"),
            "/srv".into(),
            id(),
            MachineConfig::default(),
            "nas".into(),
        );
        let error = remote.reachability().expect_err("ssh cannot start");
        assert!(matches!(error, RemoteError::Session(_)), "{error}");
        assert_eq!(error.kind(), crate::error::ErrorKind::Other);
    }

    // ----- phases, clock, flush -----

    #[test]
    fn a_phase_on_an_absent_root_is_away() {
        let far = Far::new();
        let remote = far.remote_at(&far.root().join("absent"));
        let error = remote.begin_phase().expect_err("no root");
        assert!(
            matches!(error, RemoteError::Away { overdue: false, .. }),
            "{error}"
        );
        remote.finish();
    }

    #[test]
    fn a_phase_on_a_foreign_marker_is_broken() {
        let far = Far::new();
        far.write_marker(&Marker::new(
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("a ULID"),
        ));
        let remote = far.remote();
        let error = remote.begin_phase().expect_err("foreign");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
        remote.finish();
    }

    #[test]
    fn a_phase_on_a_good_root_goes_over_the_master() {
        let far = Far::new();
        let remote = far.remote();
        remote.begin_phase().expect("the phase begins");
        assert_eq!(remote.session.mode(), session::Mode::Master);
        remote.finish();
    }

    #[test]
    fn the_clock_is_the_far_sides_clock() {
        let far = Far::new();
        let remote = far.remote();
        let before = Timestamp::now().as_second();
        let clock = remote.clock().expect("the clock is read").as_second();
        let after = Timestamp::now().as_second();
        assert!((before..=after + 1).contains(&clock), "{clock}");
    }

    #[test]
    fn the_clock_of_a_root_without_marker_is_an_error() {
        let far = Far::new();
        std::fs::remove_file(far.root().join(marker::FILE_NAME)).expect("remove");
        assert!(matches!(far.remote().clock(), Err(RemoteError::Broken(_))));
    }

    #[test]
    fn flush_runs_sync_on_the_far_side() {
        let far = Far::new();
        let log = far.state.path().join("sync.log");
        far.wrap("sync", &format!("echo synced >> '{}'", log.display()));
        far.remote().flush().expect("flush");
        assert_eq!(std::fs::read_to_string(&log).expect("sync ran"), "synced\n");
    }

    #[test]
    fn flush_checks_the_root_before_it_syncs() {
        let far = Far::new();
        let log = far.state.path().join("sync.log");
        far.wrap("sync", &format!("echo synced >> '{}'", log.display()));
        std::fs::remove_file(far.root().join(marker::FILE_NAME)).expect("remove");
        assert!(far.remote().flush().is_err());
        assert!(!log.exists(), "sync did not run");
    }

    #[test]
    fn a_sync_that_fails_is_an_io_error_of_kind_other() {
        let far = Far::new();
        far.wrap("sync", "echo 'sync: broken' >&2; exit 1");
        let error = far.remote().flush().expect_err("sync fails");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        assert!(error.to_string().contains("sync: broken"), "{error}");
        assert_eq!(error.kind(), crate::error::ErrorKind::Other);
    }

    // ----- opening -----

    fn collection_with(url: &str) -> crate::collection::Collection {
        let mut config = crate::config::Config::new(
            crate::testing::tree::COLLECTION_ID
                .parse()
                .expect("the fixture ID is a ULID"),
        );
        config.machine.name = Some("laptop".into());
        config.remotes.push(RemoteConfig {
            url: url.into(),
            id: id(),
            host_key_alias: None,
        });
        crate::collection::Collection::new(PathBuf::from("/collection-for-tests"), config)
    }

    #[test]
    fn opening_an_ssh_url_connects_to_nothing() {
        let remote = crate::remote::open(&collection_with("ssh://nas/srv/antiq")).expect("opens");
        remote.finish();
    }

    #[test]
    fn a_remote_state_probe_of_a_host_that_ssh_would_read_as_an_option_is_broken() {
        let collection = collection_with("ssh://-oProxyCommand/srv/antiq");
        assert_eq!(crate::remote::probe_state(&collection), RemoteState::Broken);
    }

    // ----- session errors -----

    #[test]
    fn a_session_that_is_away_or_broken_is_a_remote_that_is() {
        assert!(matches!(
            map_session(SessionError::Away("down".into())),
            RemoteError::Away { ref message, overdue: false } if message == "down"
        ));
        assert!(matches!(
            map_session(SessionError::Broken("refused".into())),
            RemoteError::Broken(ref message) if message == "refused"
        ));
    }

    #[test]
    fn any_other_session_error_is_kind_other() {
        let error = map_session(SessionError::ControlDir {
            path: "/tmp/x".into(),
            reason: "is not ours".into(),
        });
        assert!(matches!(error, RemoteError::Session(_)), "{error}");
        assert_eq!(error.kind(), crate::error::ErrorKind::Other);
    }

    // ----- the shell prelude -----

    fn tool(dir: &Path, name: &str, body: &str) {
        let file = dir.join(name);
        std::fs::write(&file, format!("#!/bin/sh\n{body}\n")).expect("tool");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    /// A directory of tools that stand in for `PATH`, holding only `sh`
    /// helpers the prelude needs, so a hash tool can be left out.
    fn bare_path(tools: &[&str]) -> TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for name in tools {
            let found = ["/bin", "/usr/bin"]
                .iter()
                .map(|base| Path::new(base).join(name))
                .find(|candidate| candidate.exists())
                .expect("the tool exists");
            std::os::unix::fs::symlink(found, dir.path().join(name)).expect("symlink");
        }
        dir
    }

    fn run_with_only(path: &Path, body: &str) -> (i32, String) {
        let script = script::Script::new(&format!("{PRELUDE}\n{body}"));
        let work = tempfile::tempdir().expect("temp dir");
        std::fs::write(work.path().join("abc"), "abc").expect("file");
        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .current_dir(work.path())
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh starts");
        let mut pipe = child.stdin.take().expect("piped");
        let _ = pipe.write_all(&script.to_stdin().expect("builds"));
        drop(pipe);
        let out = child.wait_with_output().expect("finishes");
        (
            out.status.code().expect("exits"),
            String::from_utf8(out.stdout).expect("UTF-8"),
        )
    }

    #[test]
    fn each_hash_tool_alone_gives_the_sha256() {
        let cases: [(&str, &str); 4] = [
            ("sha256sum", &format!("echo '{ABC}  '\"$1\"")),
            ("shasum", &format!("echo '{ABC}  '\"$3\"")),
            ("sha256", &format!("echo '{ABC}'")),
            ("openssl", &format!("echo '{ABC} *'\"$4\"")),
        ];
        for (name, body) in cases {
            let dir = bare_path(&["sh", "cat"]);
            tool(dir.path(), name, body);
            let (status, out) = run_with_only(dir.path(), "antiq_sha256 abc");
            assert_eq!((status, out.trim()), (0, ABC), "{name}");
        }
    }

    #[test]
    fn a_hash_tool_that_fails_is_skipped_for_the_next() {
        let dir = bare_path(&["sh"]);
        tool(dir.path(), "sha256sum", "exit 1");
        tool(dir.path(), "shasum", &format!("echo '{ABC}  x'"));
        let (status, out) = run_with_only(dir.path(), "antiq_sha256 abc");
        assert_eq!((status, out.trim()), (0, ABC));
    }

    #[test]
    fn a_hash_tool_that_prints_junk_is_skipped_for_the_next() {
        let dir = bare_path(&["sh"]);
        tool(dir.path(), "sha256sum", "echo 'usage: sha256sum [FILE]'");
        tool(dir.path(), "shasum", &format!("echo '{ABC}  x'"));
        let (status, out) = run_with_only(dir.path(), "antiq_sha256 abc");
        assert_eq!((status, out.trim()), (0, ABC));
    }

    #[test]
    fn a_hash_in_upper_case_or_of_the_wrong_length_does_not_count() {
        let dir = bare_path(&["sh"]);
        tool(
            dir.path(),
            "sha256sum",
            &format!("echo '{}  x'", ABC.to_uppercase()),
        );
        tool(dir.path(), "shasum", &format!("echo '{}  x'", &ABC[1..]));
        let (status, _) = run_with_only(dir.path(), "antiq_sha256 abc");
        assert_eq!(status, 42);
    }

    #[test]
    fn without_a_hash_tool_the_status_is_42() {
        let dir = bare_path(&["sh"]);
        let (status, out) = run_with_only(dir.path(), "antiq_sha256 abc");
        assert_eq!((status, out.as_str()), (42, ""));
    }

    #[test]
    fn both_stat_flavours_give_size_and_modification_time() {
        let cases = [
            (
                "gnu",
                "case \"$1\" in -c) shift; echo '3 1700000000';; *) exit 1;; esac",
            ),
            (
                "bsd",
                "case \"$1\" in -f) shift; echo '3 1700000000';; *) exit 1;; esac",
            ),
        ];
        for (flavour, body) in cases {
            let dir = bare_path(&["sh"]);
            tool(dir.path(), "stat", body);
            let (status, out) = run_with_only(dir.path(), "antiq_stat abc");
            assert_eq!((status, out.trim()), (0, "3 1700000000"), "{flavour}");
        }
    }

    #[test]
    fn without_a_usable_stat_the_status_is_43() {
        let dir = bare_path(&["sh"]);
        tool(dir.path(), "stat", "exit 1");
        let (status, _) = run_with_only(dir.path(), "antiq_stat abc");
        assert_eq!(status, 43);
    }

    #[test]
    fn a_missing_file_is_no_missing_stat() {
        let work = tempfile::tempdir().expect("temp dir");
        let (status, _) = run_prelude("antiq_stat nothing-here", work.path());
        assert_ne!(status, 43);
        assert_ne!(status, 0);
    }

    #[test]
    fn a_frame_names_its_kind_and_the_length_of_the_file() {
        let work = tempfile::tempdir().expect("temp dir");
        std::fs::write(work.path().join("f"), "a\0b\n").expect("file");
        let (status, out) = run_prelude("antiq_emit blob f", work.path());
        assert_eq!((status, out.as_str()), (0, "@blob 4\na\0b\n\n"));
    }

    // ----- frames -----

    fn broken_reason(result: Result<Vec<Frame>, RemoteError>) -> String {
        match result {
            Err(RemoteError::Broken(reason)) => reason,
            other => panic!("expected a malformed answer, got {other:?}"),
        }
    }

    #[test]
    fn frames_carry_their_kind_arguments_and_bytes() {
        let frames =
            read_frames(b"@marker 2\n{}\n@file a.b 3\nx\ny\n@empty 0\n\n").expect("frames");
        assert_eq!(
            frames,
            vec![
                Frame {
                    kind: "marker".into(),
                    args: vec![],
                    bytes: b"{}".to_vec()
                },
                Frame {
                    kind: "file".into(),
                    args: vec!["a.b".into()],
                    bytes: b"x\ny".to_vec()
                },
                Frame {
                    kind: "empty".into(),
                    args: vec![],
                    bytes: vec![]
                },
            ]
        );
    }

    #[test]
    fn a_header_without_an_end_is_malformed() {
        assert!(broken_reason(read_frames(b"@marker 2")).contains("no end"));
    }

    #[test]
    fn a_header_that_is_not_utf8_is_malformed() {
        assert!(broken_reason(read_frames(b"@\xff 2\n..\n")).contains("UTF-8"));
    }

    #[test]
    fn a_header_without_the_at_sign_is_malformed() {
        assert!(broken_reason(read_frames(b"marker 2\nab\n")).contains("start with @"));
    }

    #[test]
    fn a_header_without_a_length_is_malformed() {
        assert!(broken_reason(read_frames(b"@marker\nab\n")).contains("no length"));
        assert!(broken_reason(read_frames(b"@marker x\nab\n")).contains("no length"));
    }

    #[test]
    fn a_frame_shorter_than_its_length_is_malformed() {
        assert!(broken_reason(read_frames(b"@marker 5\nab\n")).contains("shorter"));
    }

    #[test]
    fn a_frame_not_followed_by_a_newline_is_malformed() {
        assert!(broken_reason(read_frames(b"@marker 2\nabc")).contains("shorter"));
    }

    fn answered(stdout: &[u8], clock: bool) -> Result<Answer, RemoteError> {
        let far = Far::new();
        far.remote().answer(
            Output {
                status: 0,
                stdout: stdout.to_vec(),
                stderr: vec![],
            },
            clock,
            "check",
        )
    }

    #[test]
    fn an_answer_without_the_marker_frame_is_malformed() {
        let error = answered(b"@file x 1\na\n", false).err().expect("no marker");
        assert!(error.to_string().contains("marker is missing"), "{error}");
        let error = answered(b"", false).err().expect("empty");
        assert!(error.to_string().contains("marker is missing"), "{error}");
    }

    #[test]
    fn an_answer_whose_first_line_is_no_clock_is_a_session_problem() {
        let error = answered(b"noon\n", true).err().expect("no clock");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
    }

    #[test]
    fn a_clock_beyond_the_timestamp_range_is_malformed() {
        let far = Far::new();
        far.wrap("date", "echo 99999999999999999");
        let error = far.remote().clock().expect_err("out of range");
        assert!(error.to_string().contains("range"), "{error}");
    }

    #[test]
    fn a_status_the_script_does_not_use_has_its_number_in_the_error() {
        let far = Far::new();
        far.wrap("sync", "exit 7");
        let error = far.remote().flush().expect_err("sync fails");
        assert!(error.to_string().contains("status 7"), "{error}");
    }

    #[test]
    fn a_far_side_without_a_hash_tool_is_broken_and_names_the_host() {
        let far = Far::new();
        let output = Output {
            status: NO_HASH_TOOL,
            stdout: vec![],
            stderr: vec![],
        };
        let error = far
            .remote()
            .answer(output, false, "hash")
            .err()
            .expect("no tool");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
        assert!(
            error.to_string().contains("nas has no sha256sum"),
            "{error}"
        );
    }

    #[test]
    fn a_far_side_without_a_usable_stat_is_broken_and_names_the_host() {
        let far = Far::new();
        let output = Output {
            status: NO_STAT,
            stdout: vec![],
            stderr: vec![],
        };
        let error = far
            .remote()
            .answer(output, false, "stat")
            .err()
            .expect("no stat");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
        assert!(error.to_string().contains("nas has no stat"), "{error}");
    }

    #[test]
    fn what_the_session_warns_about_comes_out_of_the_remote() {
        let far = Far::new();
        let remote = far.remote();
        remote.begin_phase().expect("the phase begins");
        far.fault("session-refused command");
        remote.reachability().expect("the answer comes anyway");
        let warnings = remote.take_warnings();
        assert!(!warnings.is_empty());
        assert!(remote.take_warnings().is_empty(), "taken once");
        remote.finish();
    }
}
