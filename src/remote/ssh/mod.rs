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
use crate::remote::rsync::{Rsync, RsyncError, RsyncVersion, TransferSettings, parse_version_line};
use crate::report::Warning;
use classify::{Failure, decided_failure};
use jiff::Timestamp;
use script::{EXISTS_STATUS, Script};
use session::{Output, Session, SessionError, SessionSettings, Target};
use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
/// The file the operation names is not there.
const FILE_MISSING: i32 = 44;
/// The upload on the NAS does not have the size of the local file.
const SIZE_MISMATCH: i32 = 45;
/// The final name of an upload holds the very bytes that were to be put.
const SAME_BYTES: i32 = 46;
/// The final name of an upload holds other bytes, or is no regular file.
const OTHER_BYTES: i32 = 47;

/// Lists the directory `$1/$2` as one `@entry <name> <d|f> <size> <mtime>`
/// frame without a body per entry. Names with a byte outside
/// `[A-Za-z0-9._%-]` are left out since they could not be told apart in a
/// frame header. The three globs cover the dot names that interrupted
/// uploads leave.
const LIST_DIR: &str = r#"_dir="$1/$2"
[ -d "$_dir" ] || exit 0
antiq_searchable "$_dir" || exit 1
for _f in "$_dir"/* "$_dir"/.[!.]* "$_dir"/..?*; do
    [ -e "$_f" ] || [ -h "$_f" ] || continue
    _n=${_f##*/}
    case $_n in *[!A-Za-z0-9._%-]*) continue ;; esac
    if [ -d "$_f" ] && [ ! -h "$_f" ]; then _k=d; else _k=f; fi
    _st=$(antiq_stat "$_f")
    _rc=$?
    [ "$_rc" -eq 43 ] && exit 43
    [ "$_rc" -eq 0 ] || continue
    printf '@entry %s %s %s %s 0\n\n' "$_n" "$_k" "${_st%% *}" "${_st#* }"
done
"#;

const LIST_OBJECTS: &str = r#"[ ! -d "$1/objects" ] || antiq_searchable "$1/objects" || exit 1
for _d in "$1"/objects/*; do
    [ -d "$_d" ] || continue
    _n=${_d##*/}
    case $_n in *[!A-Za-z0-9]*) continue ;; esac
    printf '@object %s 0\n\n' "$_n"
done
"#;

/// The head of the metadata pull; the `case` arms that name the highest
/// known version of each object follow, then [`PULL_TAIL`].
const PULL_HEAD: &str = r#"[ ! -d "$1/objects" ] || antiq_searchable "$1/objects" || exit 1
for _d in "$1"/objects/*; do
    [ -d "$_d" ] || continue
    _o=${_d##*/}
    case $_o in *[!A-Za-z0-9]*) continue ;; esac
    case $_o in
"#;

/// Of each object the `meta.<digits>` files without a leading zero that
/// are above the known number. A number is compared by its length first and
/// by `sort` among equal lengths, so the shell does no arithmetic and a
/// name too long for a number is not an error. The result is a
/// prefilter: antiq applies its own version rules to what comes back.
const PULL_TAIL: &str = r#"        *) _h=0 ;;
    esac
    antiq_searchable "$_d" || exit 1
    for _f in "$_d"/meta.[1-9]*; do
        [ -f "$_f" ] || continue
        _n=${_f##*/}
        _v=${_n#meta.}
        case $_v in *[!0-9]*) continue ;; esac
        if [ "${#_v}" -lt "${#_h}" ]; then continue; fi
        if [ "${#_v}" -eq "${#_h}" ]; then
            [ "$_v" = "$_h" ] && continue
            [ "$(printf '%s\n%s\n' "$_h" "$_v" | sort | tail -n 1)" = "$_v" ] || continue
        fi
        antiq_emit meta "$_f" "$_o" "$_n" || exit $?
    done
done
"#;

const READ_FILE: &str = r#"_f="$1/objects/$2/$3"
[ -e "$_f" ] || exit 44
antiq_emit file "$_f" || exit $?
"#;

const CHECKSUM: &str = r#"_f="$1/objects/$2/$3"
[ -e "$_f" ] || exit 44
if [ ! -f "$_f" ]; then
    printf '%s is not a regular file\n' "$_f" >&2
    exit 1
fi
_h=$(antiq_sha256 "$_f")
_rc=$?
[ "$_rc" -eq 0 ] || exit "$_rc"
printf '@sum %s 0\n\n' "$_h"
"#;

/// Makes the object's directory and points `$1` at the file `$3` in it, so
/// that the exclusive-create text which follows works on `$1`.
const CREATE_EXCLUSIVE_HEAD: &str = r#"mkdir -p "$1/objects/$2" || exit 1
set -- "$1/objects/$2/$3"
"#;

/// Writes `$4` to the temp `$3` and checks that `$5` bytes arrived. The
/// size check is what notices a shell that cut the value short.
const WRITE_TEMP: &str = r#"mkdir -p "$1/objects/$2" || exit 1
printf '%s' "$4" > "$1/objects/$2/$3" || exit 1
_st=$(antiq_stat "$1/objects/$2/$3")
_rc=$?
[ "$_rc" -eq 0 ] || exit "$_rc"
if [ "${_st%% *}" != "$5" ]; then
    printf 'wrote %s bytes instead of %s\n' "${_st%% *}" "$5" >&2
    exit 45
fi
"#;

/// Moves the temp `$3` to the name `$4` without replacing anything. The
/// name is looked at before `ln` runs and again when `ln` fails: a name
/// that is there is 17, any other failure of `ln` is 48 with its message.
const RENAME_INTO_PLACE: &str = r#"_t="$1/objects/$2/$3"
_f="$1/objects/$2/$4"
[ -e "$_t" ] || [ -h "$_t" ] || exit 44
if [ -e "$_f" ] || [ -h "$_f" ]; then exit 17; fi
if ! _e=$(ln "$_t" "$_f" 2>&1); then
    if [ -e "$_f" ] || [ -h "$_f" ]; then exit 17; fi
    printf '%s\n' "$_e" >&2
    exit 48
fi
rm -f "$_t" || exit 1
"#;

/// Removes the file `$3` of the object, or of its `.antiq-partial`
/// directory when `$4` is given.
const DELETE: &str = r#"rm -f "$1/objects/$2${4:+/.antiq-partial}/$3" || exit 1
"#;

/// Shell functions of the upload scripts.
///
/// - `antiq_taken FILE SHA` ends with [`SAME_BYTES`] when FILE is a regular
///   file with the SHA-256 SHA and with [`OTHER_BYTES`] for anything else.
/// - `antiq_taken_temp TEMP FILE SHA` is that, and removes TEMP when the
///   bytes are the same, since the upload is then already in place.
const UPLOAD_FUNCTIONS: &str = r#"antiq_taken() {
    if [ ! -f "$1" ] || [ -h "$1" ]; then return 47; fi
    _taken=$(antiq_sha256 "$1") || return $?
    [ "$_taken" = "$2" ] && return 46
    return 47
}
antiq_taken_temp() {
    antiq_taken "$2" "$3"
    _rc=$?
    [ "$_rc" -eq 46 ] && rm -f "$1"
    return "$_rc"
}
"#;

/// Before the transfer, with `$3` the temp name, `$4` the final name, `$5`
/// the SHA-256 and `$6` the seed or nothing. A taken final name ends the
/// script with [`SAME_BYTES`] or [`OTHER_BYTES`]. A seed that is a regular
/// file and no symlink is hard-linked to the temp name when that is free,
/// so that rsync finds the old package next to the new one. A seed that
/// does not link only costs the transfer of the whole file.
const UPLOAD_PREPARE: &str = r#"_d="$1/objects/$2"
mkdir -p "$_d" || exit 1
_f="$_d/$4"
if [ -e "$_f" ] || [ -h "$_f" ]; then
    antiq_taken "$_f" "$5"
    exit $?
fi
if [ -n "$6" ]; then
    _s="$_d/$6"
    _t="$_d/$3"
    if [ -f "$_s" ] && [ ! -h "$_s" ] && [ ! -e "$_t" ] && [ ! -h "$_t" ]; then
        ln "$_s" "$_t" 2>/dev/null
    fi
fi
exit 0
"#;

/// After the transfer, with `$6` the size of the local file. The size is
/// checked after a `sync`, the final name is a hard link that is made only
/// when the name is free, and the temp name goes once the name is in
/// place.
const UPLOAD_FINISH: &str = r#"_d="$1/objects/$2"
_t="$_d/$3"
_f="$_d/$4"
sync || exit 1
_st=$(antiq_stat "$_t")
_rc=$?
[ "$_rc" -eq 0 ] || exit "$_rc"
if [ "${_st%% *}" != "$6" ]; then
    printf 'the upload has %s bytes on the NAS instead of %s\n' "${_st%% *}" "$6" >&2
    exit 45
fi
if [ -e "$_f" ] || [ -h "$_f" ]; then
    antiq_taken_temp "$_t" "$_f" "$5"
    exit $?
fi
if ! _e=$(ln "$_t" "$_f" 2>&1); then
    if [ -e "$_f" ] || [ -h "$_f" ]; then
        antiq_taken_temp "$_t" "$_f" "$5"
        exit $?
    fi
    printf '%s\n' "$_e" >&2
    exit 48
fi
rm -f "$_t" || exit 1
sync || exit 1
"#;

/// The file `$3` of the object `$2` is there to be fetched.
const FETCH_CHECK: &str = r#"[ -f "$1/objects/$2/$3" ] || exit 44
"#;

/// Shell functions every script of the remote starts with. They use
/// only POSIX `sh` and tools every NAS has.
///
/// - `antiq_stat_flavour` sets `_flavour` to `gnu`, `bsd` or `none`, by
///   which form of `stat` works.
/// - `antiq_stat FILE` prints `<size> <mtime>`; status 43 when `stat`
///   knows neither the GNU nor the BSD form.
/// - `antiq_sha256 [FILE]` prints the SHA-256 as 64 lower-case hex digits.
///   The tool is the first of `sha256sum`, `shasum`, `sha256` and
///   `openssl` that hashes `abc` from a pipe to the known digest, since
///   tools differ in what they print and a NAS may carry a broken one;
///   status 42 when none does. `_tool` names that tool. FILE is hashed by
///   that tool alone: a file it cannot read or a result of the wrong
///   shape is status 1 with the tool's message, as the fault is the file's
///   and not the NAS's.
/// - `antiq_root_kind ROOT` sets `_root_kind` to `dir`, `missing` (absent
///   below a directory that can be searched) or `unusable`, and
///   `_root_why` to the reason for `unusable`.
/// - `antiq_searchable DIR` fails with a message unless DIR can be read
///   and searched. A glob over a directory that cannot be read matches
///   nothing, and the listing would look empty.
/// - `antiq_emit KIND FILE [ARG...]` prints `@KIND ARG... <length>`, the
///   file and a newline.
/// - `antiq_root_check ROOT` ends the script unless ROOT is a directory
///   with a readable marker, and prints the marker as a frame.
pub(crate) const PRELUDE: &str = r#"antiq_stat_flavour() {
    if stat -c '%s' / >/dev/null 2>&1; then
        _flavour=gnu
    elif stat -f '%z' / >/dev/null 2>&1; then
        _flavour=bsd
    else
        _flavour=none
    fi
}
antiq_stat() {
    antiq_stat_flavour
    case $_flavour in
    gnu) stat -c '%s %Y' "$1" ;;
    bsd) stat -f '%z %m' "$1" ;;
    *) return 43 ;;
    esac
}
antiq_sha256() {
    _found=
    for _tool in 'sha256sum' 'shasum -a 256' 'sha256 -q' 'openssl dgst -sha256 -r'; do
        _hash=$(printf abc | $_tool 2>/dev/null) || continue
        _hash=${_hash%% *}
        if [ "$_hash" = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad ]; then
            _found=1
            break
        fi
    done
    [ -n "$_found" ] || return 42
    if [ "$#" -eq 0 ]; then
        printf '%s
' "$_hash"
        return 0
    fi
    _hash=$($_tool "$1") || return 1
    _hash=${_hash%% *}
    case $_hash in *[!0-9a-f]*) _hash= ;; esac
    if [ "${#_hash}" -ne 64 ]; then
        printf 'cannot hash %s
' "$1" >&2
        return 1
    fi
    printf '%s
' "$_hash"
}
antiq_searchable() {
    [ -r "$1" ] && [ -x "$1" ] && return 0
    printf 'cannot read the directory %s\n' "$1" >&2
    return 1
}
antiq_emit() {
    _kind=$1
    _file=$2
    shift 2
    _stat=$(antiq_stat "$_file")
    _rc=$?
    [ "$_rc" -eq 0 ] || return "$_rc"
    printf '@%s' "$_kind"
    for _arg in "$@"; do
        printf ' %s' "$_arg"
    done
    printf ' %s\n' "${_stat%% *}"
    cat "$_file" || return 1
    printf '\n'
}
antiq_root_kind() {
    _root_kind=dir
    _root_why=
    [ -d "$1" ] && return 0
    _near=$1
    while [ ! -e "$_near" ] && [ ! -h "$_near" ]; do
        _near=$(dirname -- "$_near")
    done
    if [ "$_near" = "$1" ]; then
        _root_kind=unusable
        _root_why="$1 is not a directory"
    elif [ -d "$_near" ] && [ -x "$_near" ]; then
        _root_kind=missing
    else
        _root_kind=unusable
        _root_why="$_near cannot be searched or is not a directory"
    fi
}
antiq_root_check() {
    antiq_root_kind "$1"
    case $_root_kind in
    missing) exit 40 ;;
    unusable)
        printf '%s
' "$_root_why" >&2
        exit 49
        ;;
    esac
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

/// Points where a test replaces a step of the production behaviour.
#[derive(Clone)]
pub(crate) struct SshSeams {
    /// The script text that creates a file and fails when it exists, in
    /// place of [`script::EXCLUSIVE_CREATE`].
    pub exclusive_create: &'static str,
    /// Runs after the checks of a transfer and before rsync starts.
    pub before_transfer: Option<Hook>,
    /// Runs after rsync ended well and before the far side is asked to
    /// finish the upload.
    pub after_transfer: Option<Hook>,
    /// The program that stands in for `ssh`.
    pub program: Option<PathBuf>,
    /// The directory of the control socket and the master's log.
    pub control_dir: Option<PathBuf>,
}

type Hook = std::sync::Arc<dyn Fn() + Send + Sync>;

impl Default for SshSeams {
    fn default() -> SshSeams {
        SshSeams {
            exclusive_create: script::EXCLUSIVE_CREATE,
            before_transfer: None,
            after_transfer: None,
            program: None,
            control_dir: None,
        }
    }
}

/// The remote on a NAS reached over ssh.
pub struct SshRemote {
    session: Session,
    host: String,
    root: String,
    id: ObjectId,
    machine: MachineConfig,
    seams: SshSeams,
    /// The rsync of this machine, probed at the first transfer.
    rsync: Mutex<Option<Rsync>>,
}

/// What a script answered after the root check passed.
struct Answer {
    /// The remote clock, for a script made with a clock.
    clock: Option<i64>,
    /// The frames after the marker frame.
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
        SshRemote::connect_with(target, remote, machine, SshSeams::default())
    }

    pub(crate) fn connect_with(
        target: &SshTarget,
        remote: &RemoteConfig,
        machine: &MachineConfig,
        seams: SshSeams,
    ) -> Result<SshRemote, RemoteError> {
        let mut ssh_target = Target::new(&target.host);
        ssh_target.port = target.port;
        ssh_target.user.clone_from(&target.user);
        let mut settings = SessionSettings::from_config(machine, remote, ssh_target);
        if let Some(program) = &seams.program {
            settings.program.clone_from(program);
        }
        if let Some(control_dir) = &seams.control_dir {
            settings.control_dir.clone_from(control_dir);
        }
        let session = Session::new(settings).map_err(map_session)?;
        let mut remote = SshRemote::with_session(
            session,
            target.root.clone(),
            remote.id,
            machine.clone(),
            target.host.clone(),
        );
        remote.seams = seams;
        Ok(remote)
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
            seams: SshSeams::default(),
            rsync: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_seams(mut self, seams: SshSeams) -> SshRemote {
        self.seams = seams;
        self
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
            ROOT_MISSING => return Err(self.root_missing()),
            MARKER_MISSING => return Err(self.marker_missing()),
            ROOT_UNUSABLE => return Err(self.root_unusable(&stderr)),
            MARKER_UNREADABLE => return Err(self.marker_unreadable()),
            NO_HASH_TOOL => return Err(self.no_hash_tool()),
            NO_STAT => return Err(self.no_stat()),
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
        match Marker::parse(bytes) {
            Ok(found) => self.marker_matches(&found),
            Err(error) => Err(self.marker_invalid(&error)),
        }
    }

    fn marker_matches(&self, found: &Marker) -> Result<(), RemoteError> {
        if found.id == self.id {
            return Ok(());
        }
        Err(RemoteError::Broken(format!(
            "{} holds the marker of another remote ({}), not {}",
            self.root, found.id, self.id
        )))
    }

    fn marker_invalid(&self, error: &marker::MarkerError) -> RemoteError {
        RemoteError::Broken(format!("{}/{}: {error}", self.root, marker::FILE_NAME))
    }

    fn root_missing(&self) -> RemoteError {
        RemoteError::Away {
            message: format!("{} does not exist on {}", self.root, self.host),
            overdue: false,
        }
    }

    fn marker_missing(&self) -> RemoteError {
        RemoteError::Broken(format!("{} holds no {}", self.root, marker::FILE_NAME))
    }

    fn root_unusable(&self, why: &str) -> RemoteError {
        RemoteError::Broken(format!("{}: {why}", self.root))
    }

    fn marker_unreadable(&self) -> RemoteError {
        RemoteError::Broken(format!(
            "{}/{} cannot be read",
            self.root,
            marker::FILE_NAME
        ))
    }

    fn no_hash_tool(&self) -> RemoteError {
        RemoteError::Broken(format!(
            "{} has no sha256sum, shasum, sha256 or openssl that computes a SHA-256",
            self.host
        ))
    }

    fn no_stat(&self) -> RemoteError {
        RemoteError::Broken(format!(
            "{} has no stat that prints size and modification time",
            self.host
        ))
    }

    fn conflict(&self, upload: &Upload<'_>) -> RemoteError {
        RemoteError::PayloadConflict {
            name: upload.final_name.to_string(),
        }
    }

    fn exec(&self, script: &Script) -> Result<Output, RemoteError> {
        self.session.run(script).map_err(map_session)
    }

    /// Runs a script that names one file of an object. A file that is not
    /// there is an Io error of kind `NotFound`.
    fn run_on_file(
        &self,
        body: &str,
        id: ObjectId,
        name: &str,
        action: &'static str,
    ) -> Result<Answer, RemoteError> {
        plain_name(name)?;
        let script = self.script(body).arg(id.to_string()).arg(name);
        let output = self.exec(&script)?;
        if output.status == FILE_MISSING {
            return Err(RemoteError::Io {
                action,
                path: format!("{}/objects/{id}/{name}", self.root),
                source: std::io::ErrorKind::NotFound.into(),
            });
        }
        self.answer(output, false, action)
    }

    fn entries(&self, id: ObjectId, subdir: &str) -> Result<Vec<RemoteFile>, RemoteError> {
        let script = self.script(LIST_DIR).arg(format!("objects/{id}{subdir}"));
        let answer = self.run(&script, false, "list")?;
        parse_entries(&answer.frames)
    }

    /// The rsync of this machine. It is probed once; a machine without a
    /// usable one fails here, before the first call to the NAS.
    fn rsync(&self) -> Result<Rsync, RemoteError> {
        let mut cached = self.rsync.lock().expect("no panic holds the rsync lock");
        if let Some(rsync) = &*cached {
            return Ok(rsync.clone());
        }
        let rsync = Rsync::from_machine(&self.machine)?;
        *cached = Some(rsync.clone());
        Ok(rsync)
    }

    /// Runs rsync between `source` and `destination` over the session's
    /// ssh command. A failure is told apart with the text the master wrote
    /// to its log during the transfer: a master that dies leaves rsync with
    /// nothing but "connection unexpectedly closed".
    fn transfer(
        &self,
        rsync: &Rsync,
        resumable: bool,
        source: OsString,
        destination: OsString,
    ) -> Result<crate::remote::rsync::TransferStats, RemoteError> {
        let settings =
            TransferSettings::for_machine(&self.machine, self.session.rsh_argv(), resumable);
        let log = self.session.log_path();
        let logged = std::fs::metadata(log).map(|meta| meta.len()).unwrap_or(0);
        rsync
            .transfer(&settings, &source, &destination)
            .run()
            .map_err(|error| {
                let tail = std::fs::read(log)
                    .map(|bytes| bytes.into_iter().skip(logged as usize).collect::<Vec<u8>>())
                    .unwrap_or_default();
                classify_rsync_failure(error, &String::from_utf8_lossy(&tail))
            })
    }

    fn remote_path(&self, id: ObjectId, name: &str) -> OsString {
        format!("{}:{}/objects/{id}/{name}", self.host, self.root).into()
    }

    fn run_hook(hook: &Option<Hook>) {
        if let Some(hook) = hook {
            hook();
        }
    }

    fn check_root(&self, body: &str, action: &'static str) -> Result<(), RemoteError> {
        self.run(&self.script(body), false, action).map(|_| ())
    }
}

// =========================================================
// remote add and remote show
// =========================================================

/// The `ln` of a script failed, or made no regular file.
const LN_FAILED: i32 = 48;

/// Looks at the root and the tools and changes nothing. `$1` is the root
/// and `$2` the command that runs the rsync of the NAS. The answer is one
/// `@kind length` block per finding: `root` (`dir`, `missing` or
/// `unusable: <why>`), `parent` for a missing root, `marker`,
/// `marker_unreadable` or `marker_absent` for a directory, `layout` for
/// each name antiq writes in a remote that the directory holds, `rsync`
/// with the first line of its version or `rsync_failed` with its output,
/// `sha256` with the tool or `none`, and `stat` with the flavour or
/// `none`.
const PROBE: &str = r#"antiq_note() {
    printf '@%s %s
%s
' "$1" "${#2}" "$2"
}
antiq_root_kind "$1"
case $_root_kind in
dir) antiq_note root dir ;;
missing)
    antiq_note root missing
    if [ -d "$(dirname -- "$1")" ]; then antiq_note parent present; else antiq_note parent absent; fi
    ;;
*) antiq_note root "unusable: $_root_why" ;;
esac
if [ "$_root_kind" = dir ]; then
    _m="$1/antiq-remote.json"
    if [ -e "$_m" ] || [ -h "$_m" ]; then
        if _text=$(cat "$_m" 2>/dev/null); then
            antiq_note marker "$_text"
        else
            antiq_note marker_unreadable ""
        fi
    else
        antiq_note marker_absent ""
    fi
    for _f in "$1"/objects "$1"/.antiq-clock.*; do
        if [ -e "$_f" ] || [ -h "$_f" ]; then antiq_note layout "${_f##*/}"; fi
    done
fi
if _out=$(eval "$2 --version" 2>&1); then
    antiq_note rsync "${_out%%
*}"
else
    antiq_note rsync_failed "$_out"
fi
if antiq_sha256 >/dev/null; then antiq_note sha256 "${_tool%% *}"; else antiq_note sha256 none; fi
antiq_stat_flavour
antiq_note stat "$_flavour"
"#;

/// Makes the root, whose parent has to be there.
const MAKE_ROOT: &str = "mkdir \"$1\"\n";

/// Removes the root when it is empty; a root with anything in it stays.
const REMOVE_EMPTY_ROOT: &str = "rmdir \"$1\" 2>/dev/null\nexit 0\n";

/// Hard-links the probe file `$1` to `$2`. The link has to be a regular
/// file.
const LINK_PROBE: &str = r#"if ! _e=$(ln "$1" "$2" 2>&1); then
    printf '%s
' "$_e" >&2
    exit 48
fi
if [ ! -f "$2" ] || [ -h "$2" ]; then
    echo "the link is not a regular file" >&2
    exit 48
fi
"#;

const REMOVE_PROBE: &str = "rm -f \"$1\" \"$2\"\nexit 0\n";

/// Writes the marker `$2` to the temp name `$3` in the root `$1`, flushes
/// it, links it to the name `$4` when that is free, removes the temp and
/// flushes again.
const WRITE_MARKER: &str = r#"_t="$1/$3"
_m="$1/$4"
if ! printf '%s' "$2" > "$_t"; then
    rm -f "$_t"
    exit 1
fi
if ! sync; then
    rm -f "$_t"
    exit 1
fi
if [ -e "$_m" ] || [ -h "$_m" ]; then
    rm -f "$_t"
    echo "$4 appeared in the meantime" >&2
    exit 1
fi
if ! _e=$(ln "$_t" "$_m" 2>&1); then
    rm -f "$_t"
    printf '%s
' "$_e" >&2
    exit 48
fi
rm -f "$_t" || exit 1
sync || exit 1
"#;

/// What the probe script found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Probe {
    pub root: RootFound,
    pub marker: MarkerFound,
    /// A name antiq writes in a remote that the root holds.
    pub layout_name: Option<String>,
    /// The first line of the NAS rsync's `--version`, or what it printed
    /// when it failed.
    pub rsync: Result<String, String>,
    /// The first hash tool that gave the digest of `abc`.
    pub sha256_tool: Option<String>,
    pub stat: Option<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RootFound {
    Dir,
    /// Absent below a directory that can be searched.
    Missing {
        parent_exists: bool,
    },
    Unusable(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MarkerFound {
    /// The root is no directory, so there is nothing to look at.
    NoRoot,
    Absent,
    Unreadable,
    Text(Vec<u8>),
}

/// What `remote add` needs of the NAS, proven by the probe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Tools {
    pub rsync: RsyncVersion,
    pub sha256_tool: String,
    pub stat: &'static str,
}

/// Why the writes of `remote add` do not prove the root usable.
#[derive(Debug)]
pub(crate) enum Unproven {
    ExclusiveCreate(String),
    HardLink(String),
    Remote(RemoteError),
}

impl From<RemoteError> for Unproven {
    fn from(error: RemoteError) -> Unproven {
        Unproven::Remote(error)
    }
}

impl Probe {
    fn parse(stdout: &[u8]) -> Result<Probe, RemoteError> {
        let mut root = None;
        let mut parent_exists = false;
        let mut marker = MarkerFound::NoRoot;
        let mut layout_name = None;
        let mut rsync = None;
        let mut sha256_tool = None;
        let mut stat = None;
        for frame in read_frames(stdout)? {
            let text = String::from_utf8_lossy(&frame.bytes).into_owned();
            match frame.kind.as_str() {
                "root" => root = Some(text),
                "parent" => parent_exists = text == "present",
                "marker" => marker = MarkerFound::Text(frame.bytes),
                "marker_unreadable" => marker = MarkerFound::Unreadable,
                "marker_absent" => marker = MarkerFound::Absent,
                "layout" => layout_name = layout_name.or(Some(text)),
                "rsync" => rsync = Some(Ok(text)),
                "rsync_failed" => rsync = Some(Err(text)),
                "sha256" => sha256_tool = Some(text).filter(|tool| tool != "none"),
                "stat" => {
                    stat = match text.as_str() {
                        "gnu" => Some("gnu"),
                        "bsd" => Some("bsd"),
                        _ => None,
                    };
                }
                _ => {}
            }
        }
        let root = match root.as_deref() {
            Some("dir") => RootFound::Dir,
            Some("missing") => RootFound::Missing { parent_exists },
            Some(unusable) => RootFound::Unusable(
                unusable
                    .strip_prefix("unusable: ")
                    .unwrap_or(unusable)
                    .to_string(),
            ),
            None => return Err(malformed("the probe did not describe the root")),
        };
        Ok(Probe {
            root,
            marker,
            layout_name,
            rsync: rsync.ok_or_else(|| malformed("the probe did not run the rsync"))?,
            sha256_tool,
            stat,
        })
    }
}

/// The output of a script that failed, as a sentence.
fn failure_reason(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("the command ended with status {}", output.status)
    } else {
        stderr
    }
}

impl SshRemote {
    /// Brings the connection up for the work of `remote add`, which cannot
    /// use [`Remote::begin_phase`] since the root may not have a marker yet.
    pub(crate) fn begin_connection(&self) -> Result<(), RemoteError> {
        self.session.begin_phase().map_err(map_session)
    }

    /// The same remote under the ULID its marker names.
    pub(crate) fn with_id(mut self, id: ObjectId) -> SshRemote {
        self.id = id;
        self
    }

    /// The command that runs the rsync of the NAS.
    fn remote_rsync_command(&self) -> &str {
        self.machine.remote_rsync.as_deref().unwrap_or("rsync")
    }

    /// Looks at the root and the tools in one read-only script.
    pub(crate) fn probe(&self) -> Result<Probe, RemoteError> {
        let script = Script::new(&format!("{PRELUDE}\n{PROBE}"))
            .arg(&self.root)
            .arg(self.remote_rsync_command());
        let output = self.exec(&script)?;
        if output.status != 0 {
            return Err(RemoteError::Io {
                action: "probe",
                path: self.root.clone(),
                source: std::io::Error::other(failure_reason(&output)),
            });
        }
        Probe::parse(&output.stdout)
    }

    /// The marker of a root that has one. `Ok(None)` is a directory
    /// without a marker; a root that is missing is away, and a root that
    /// cannot be used or a marker that cannot be read is broken.
    pub(crate) fn marker_in(&self, probe: &Probe) -> Result<Option<Marker>, RemoteError> {
        match &probe.root {
            RootFound::Missing { .. } => return Err(self.root_missing()),
            RootFound::Unusable(why) => return Err(self.root_unusable(why)),
            RootFound::Dir => {}
        }
        match &probe.marker {
            MarkerFound::NoRoot | MarkerFound::Absent => Ok(None),
            MarkerFound::Unreadable => Err(self.marker_unreadable()),
            MarkerFound::Text(bytes) => Marker::parse(bytes)
                .map(Some)
                .map_err(|error| self.marker_invalid(&error)),
        }
    }

    /// Whether the probe found the remote this value stands for. Every
    /// answer of [`SshRemote::marker_in`] and of the marker check is away
    /// or broken, so nothing else can go wrong here.
    fn judge(&self, probe: &Probe) -> Assessment {
        let verdict = self.marker_in(probe).and_then(|found| match found {
            Some(found) => self.marker_matches(&found),
            None => Err(self.marker_missing()),
        });
        match verdict {
            Ok(()) => Assessment::new(Reachability::Reachable, ""),
            Err(RemoteError::Away { message, .. }) => Assessment::new(Reachability::Away, message),
            Err(other) => Assessment::new(Reachability::Broken, other.to_string()),
        }
    }

    /// The tools `remote add` needs on the NAS, or the reason one is not
    /// good enough.
    pub(crate) fn tools_of(&self, probe: &Probe) -> Result<Tools, RemoteError> {
        let command = self.remote_rsync_command();
        let rsync = match &probe.rsync {
            Ok(line) => parse_version_line(line).map_err(|refusal| {
                RemoteError::Broken(format!(
                    "the rsync on {} (`{command}`): {refusal}",
                    self.host
                ))
            })?,
            Err(output) => {
                return Err(RemoteError::Broken(format!(
                    "`{command} --version` does not run on {}: {output}",
                    self.host
                )));
            }
        };
        let sha256_tool = probe
            .sha256_tool
            .clone()
            .ok_or_else(|| self.no_hash_tool())?;
        let stat = probe.stat.ok_or_else(|| self.no_stat())?;
        Ok(Tools {
            rsync,
            sha256_tool,
            stat,
        })
    }

    /// What `remote show` reports: whether the remote this value stands
    /// for is reachable, and for a reachable one what was probed.
    pub(crate) fn show(
        &self,
    ) -> Result<(Assessment, serde_json::Map<String, serde_json::Value>), RemoteError> {
        use serde_json::Value;
        let mut probed = serde_json::Map::new();
        let probe = match self.probe() {
            Ok(probe) => probe,
            Err(RemoteError::Away { message, .. }) => {
                return Ok((Assessment::new(Reachability::Away, message), probed));
            }
            Err(RemoteError::Broken(message)) => {
                return Ok((Assessment::new(Reachability::Broken, message), probed));
            }
            Err(other) => return Err(other),
        };
        let assessment = self.judge(&probe);
        if assessment.reachability != Reachability::Reachable {
            return Ok((assessment, probed));
        }
        // A tool that is missing or too old shows as null: the state of
        // the remote does not depend on it.
        let text = |value: Option<String>| value.map_or(Value::Null, Value::String);
        let local = Rsync::from_machine(&self.machine).ok();
        let remote = probe
            .rsync
            .as_ref()
            .ok()
            .and_then(|line| parse_version_line(line).ok());
        probed.insert(
            "rsync".into(),
            text(local.map(|rsync| rsync.version().to_string())),
        );
        probed.insert(
            "remote_rsync".into(),
            text(remote.map(|version| version.to_string())),
        );
        probed.insert("sha256".into(), text(probe.sha256_tool.clone()));
        probed.insert("stat".into(), text(probe.stat.map(String::from)));
        Ok((assessment, probed))
    }

    /// Makes the root, whose parent has to exist.
    pub(crate) fn make_root(&self) -> Result<(), RemoteError> {
        let output = self.exec(&Script::new(MAKE_ROOT).arg(&self.root))?;
        if output.status == 0 {
            return Ok(());
        }
        Err(RemoteError::Io {
            action: "create",
            path: self.root.clone(),
            source: std::io::Error::other(failure_reason(&output)),
        })
    }

    /// Removes a root a refused `remote add` made. A root with anything in
    /// it stays, and whatever the removal says does not change why the
    /// run was refused.
    pub(crate) fn remove_made_root(&self) {
        let _ = self.exec(&Script::new(REMOVE_EMPTY_ROOT).arg(&self.root));
    }

    /// Proves that the root takes exclusive creation and hard links, with
    /// a file called `probe` in it. The probe file and its link are
    /// removed again.
    pub(crate) fn prove_creation(&self, probe: &str) -> Result<(), Unproven> {
        let path = format!("{}/{probe}", self.root);
        let link = format!("{path}.link");
        let created = self.exec(&Script::new(self.seams.exclusive_create).arg(&path))?;
        if created.status != 0 {
            return Err(Unproven::ExclusiveCreate(failure_reason(&created)));
        }
        let proof = self.prove_probe(&path, &link);
        let _ = self.exec(&Script::new(REMOVE_PROBE).arg(&path).arg(&link));
        proof
    }

    fn prove_probe(&self, path: &str, link: &str) -> Result<(), Unproven> {
        let again = self.exec(&Script::new(self.seams.exclusive_create).arg(path))?;
        match again.status {
            EXISTS_STATUS => {}
            0 => {
                return Err(Unproven::ExclusiveCreate(
                    "a second create of the same file succeeded".into(),
                ));
            }
            _ => return Err(Unproven::ExclusiveCreate(failure_reason(&again))),
        }
        let linked = self.exec(&Script::new(LINK_PROBE).arg(path).arg(link))?;
        if linked.status != 0 {
            return Err(Unproven::HardLink(format!(
                "cannot link the probe file: {}",
                failure_reason(&linked)
            )));
        }
        Ok(())
    }

    /// Writes the marker of this remote under the temp name `temp`.
    pub(crate) fn write_marker(&self, temp: &str) -> Result<(), Unproven> {
        let script = Script::new(WRITE_MARKER)
            .arg(&self.root)
            .arg(Marker::new(self.id).to_bytes())
            .arg(temp)
            .arg(marker::FILE_NAME);
        let output = self.exec(&script)?;
        match output.status {
            0 => Ok(()),
            LN_FAILED => Err(Unproven::HardLink(format!(
                "cannot link {} into place: {}",
                marker::FILE_NAME,
                failure_reason(&output)
            ))),
            _ => Err(Unproven::Remote(RemoteError::Io {
                action: "write",
                path: format!("{}/{}", self.root, marker::FILE_NAME),
                source: std::io::Error::other(failure_reason(&output)),
            })),
        }
    }
}

/// Every name the remote is given is one component of an object's
/// directory. The names come from the metadata of other machines, so a
/// name that reaches outside the directory is refused. Only the
/// characters a listing shows (`LIST_DIR`) are accepted: rsync before
/// 3.2.4 hands the remote path to the far shell and later versions expand
/// globs in it, so a space, `;`, `$`, `*` or a leading `-` in a name must
/// never get that far. A leading `-` is refused too, as the shell and
/// rsync would read it as an option.
fn plain_name(name: &str) -> Result<(), RemoteError> {
    let listable = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'-'));
    if name.is_empty() || name == "." || name == ".." || name.starts_with('-') || !listable {
        return Err(RemoteError::InvalidName { name: name.into() });
    }
    Ok(())
}

fn number<T: std::str::FromStr>(text: &str) -> Result<T, RemoteError> {
    text.parse().map_err(|_| malformed("a number is not one"))
}

fn checksum_of(frames: &[Frame]) -> Result<String, RemoteError> {
    let sum = frames
        .first()
        .and_then(|frame| frame.args.first())
        .filter(|sum| {
            sum.len() == 64 && sum.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
        .ok_or_else(|| malformed("the checksum is not a SHA-256"))?;
    Ok(sum.clone())
}

fn parse_entries(frames: &[Frame]) -> Result<Vec<RemoteFile>, RemoteError> {
    frames
        .iter()
        .map(|frame| match frame.args.as_slice() {
            [name, kind, size, mtime] if frame.kind == "entry" => Ok(RemoteFile {
                name: name.clone(),
                size: number(size)?,
                modified: Timestamp::from_second(number(mtime)?)
                    .map_err(|_| malformed("a time is out of range"))?,
                is_dir: kind == "d",
            }),
            _ => Err(malformed("an entry has the wrong form")),
        })
        .collect()
}

/// The versions of the frames that are above what is known. The far side
/// filtered by name already; the version rules are applied here.
fn parse_meta(frames: Vec<Frame>, known: &KnownVersions) -> Result<Vec<MetaFile>, RemoteError> {
    let mut files = Vec::new();
    for frame in frames {
        let [object, name] = frame.args.as_slice() else {
            return Err(malformed("a metadata frame has the wrong form"));
        };
        let (Some(id), Some(number)) =
            (canonical_object(object), crate::store::version_number(name))
        else {
            continue;
        };
        if number > known.highest(id) {
            files.push(MetaFile {
                id,
                number: NonZeroU32::new(number).expect("a version number is above zero"),
                bytes: frame.bytes,
            });
        }
    }
    Ok(files)
}

/// The bytes of the first frame, which must be a file.
fn file_bytes(frames: Vec<Frame>) -> Result<Vec<u8>, RemoteError> {
    match frames.into_iter().next() {
        Some(frame) if frame.kind == "file" => Ok(frame.bytes),
        _ => Err(malformed("the file is missing from the answer")),
    }
}

/// The object a frame names, if it is written in the canonical form: a
/// directory of another spelling is not one of antiq's.
fn canonical_object(text: &str) -> Option<ObjectId> {
    let id: ObjectId = text.parse().ok()?;
    (id.to_string() == text).then_some(id)
}

/// A local path as rsync reads it: a relative one gets `./`, since rsync
/// takes a name with a colon for `host:path`. Joining an absolute path
/// to `.` gives that path.
fn local_arg(path: &Path) -> OsString {
    Path::new(".").join(path).into_os_string()
}

/// What a failed rsync means for the remote. ssh's own lines, in rsync's
/// stderr or in the master's log, tell away from broken the way they do
/// for a command. Codes 23, 24 and 11 are rsync's own findings about
/// files and are a failed transfer whatever else was printed. Without
/// such a line, 255 is broken and every other code, a timeout (30), a
/// signal and a failure to start rsync included, is a failed transfer.
fn classify_rsync_failure(error: RsyncError, master_log: &str) -> RemoteError {
    let RsyncError::Failed {
        code: Some(code),
        stderr,
    } = &error
    else {
        return RemoteError::Transfer(error.to_string());
    };
    if matches!(code, 23 | 24 | 11) {
        return RemoteError::Transfer(error.to_string());
    }
    let decided =
        decided_failure(stderr.as_bytes()).or_else(|| decided_failure(master_log.as_bytes()));
    match decided {
        Some(Failure::Away(message)) => RemoteError::Away {
            message,
            overdue: false,
        },
        Some(Failure::Broken(message)) => RemoteError::Broken(message),
        None if *code == 255 => RemoteError::Broken(error.to_string()),
        None => RemoteError::Transfer(error.to_string()),
    }
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

    fn pull_metadata(&self, known: &KnownVersions) -> Result<Vec<MetaFile>, RemoteError> {
        let mut body = String::from(PULL_HEAD);
        for (id, highest) in known.iter() {
            body.push_str(&format!("        {id}) _h={highest} ;;\n"));
        }
        body.push_str(PULL_TAIL);
        let answer = self.run(&self.script(&body), false, "pull metadata of")?;
        parse_meta(answer.frames, known)
    }
    fn list_objects(&self) -> Result<Vec<ObjectId>, RemoteError> {
        let answer = self.run(&self.script(LIST_OBJECTS), false, "list")?;
        let mut objects: Vec<ObjectId> = answer
            .frames
            .iter()
            .filter_map(|frame| frame.args.first().and_then(|name| canonical_object(name)))
            .collect();
        objects.sort();
        Ok(objects)
    }
    fn list_dir(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
        self.entries(id, "")
    }
    fn list_partial(&self, id: ObjectId) -> Result<Vec<RemoteFile>, RemoteError> {
        self.entries(id, "/.antiq-partial")
    }
    fn read_file(&self, id: ObjectId, name: &str) -> Result<Vec<u8>, RemoteError> {
        let answer = self.run_on_file(READ_FILE, id, name, "read")?;
        file_bytes(answer.frames)
    }
    fn get_payload(&self, id: ObjectId, name: &str, to: &Path) -> Result<Transfer, RemoteError> {
        plain_name(name)?;
        let rsync = self.rsync()?;
        self.run_on_file(FETCH_CHECK, id, name, "fetch")?;
        Self::run_hook(&self.seams.before_transfer);
        let stats = self.transfer(&rsync, false, self.remote_path(id, name), local_arg(to))?;
        let bytes = match stats.literal_bytes {
            Some(bytes) => bytes,
            None => std::fs::metadata(to).map(|meta| meta.len()).unwrap_or(0),
        };
        Ok(Transfer {
            bytes,
            already_present: false,
        })
    }
    fn create_exclusive(&self, id: ObjectId, name: &str) -> Result<Created, RemoteError> {
        plain_name(name)?;
        let body = format!("{CREATE_EXCLUSIVE_HEAD}{}", self.seams.exclusive_create);
        let script = self.script(&body).arg(id.to_string()).arg(name);
        let output = self.exec(&script)?;
        if output.status == EXISTS_STATUS {
            return Ok(Created::AlreadyExists);
        }
        self.answer(output, false, "create")?;
        Ok(Created::Yes)
    }
    fn write_temp(&self, id: ObjectId, temp: &str, bytes: &[u8]) -> Result<(), RemoteError> {
        plain_name(temp)?;
        let script = self
            .script(WRITE_TEMP)
            .arg(id.to_string())
            .arg(temp)
            .arg(bytes)
            .arg(bytes.len().to_string());
        self.run(&script, false, "write")?;
        Ok(())
    }
    fn rename_into_place(
        &self,
        id: ObjectId,
        temp: &str,
        name: &str,
    ) -> Result<Placement, RemoteError> {
        plain_name(temp)?;
        plain_name(name)?;
        let script = self
            .script(RENAME_INTO_PLACE)
            .arg(id.to_string())
            .arg(temp)
            .arg(name);
        let output = self.exec(&script)?;
        match output.status {
            EXISTS_STATUS => return Ok(Placement::TargetExists),
            FILE_MISSING => {
                return Err(RemoteError::Io {
                    action: "rename",
                    path: format!("{}/objects/{id}/{temp}", self.root),
                    source: std::io::ErrorKind::NotFound.into(),
                });
            }
            _ => {}
        }
        self.answer(output, false, "rename")?;
        Ok(Placement::Placed)
    }
    fn put_payload(&self, upload: &Upload<'_>) -> Result<Transfer, RemoteError> {
        plain_name(upload.temp)?;
        plain_name(upload.final_name)?;
        if let Some(seed) = upload.seed {
            plain_name(seed)?;
        }
        let id = upload.id;
        let size = std::fs::metadata(upload.source)
            .map_err(|source| RemoteError::Io {
                action: "read",
                path: upload.source.display().to_string(),
                source,
            })?
            .len();
        let rsync = self.rsync()?;
        let already_present = Transfer {
            bytes: 0,
            already_present: true,
        };

        let prepare = self
            .script(&format!("{UPLOAD_FUNCTIONS}{UPLOAD_PREPARE}"))
            .arg(id.to_string())
            .arg(upload.temp)
            .arg(upload.final_name)
            .arg(upload.sha256)
            .arg(upload.seed.unwrap_or(""));
        let output = self.exec(&prepare)?;
        match output.status {
            SAME_BYTES => return Ok(already_present),
            OTHER_BYTES => return Err(self.conflict(upload)),
            _ => {}
        }
        self.answer(output, false, "prepare the upload of")?;

        Self::run_hook(&self.seams.before_transfer);
        let stats = self.transfer(
            &rsync,
            upload.seed.is_none(),
            local_arg(upload.source),
            self.remote_path(id, upload.temp),
        )?;
        Self::run_hook(&self.seams.after_transfer);

        let finish = self
            .script(&format!("{UPLOAD_FUNCTIONS}{UPLOAD_FINISH}"))
            .arg(id.to_string())
            .arg(upload.temp)
            .arg(upload.final_name)
            .arg(upload.sha256)
            .arg(size.to_string());
        let output = self.exec(&finish)?;
        match output.status {
            SAME_BYTES => return Ok(already_present),
            OTHER_BYTES => return Err(self.conflict(upload)),
            SIZE_MISMATCH => {
                let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(RemoteError::Transfer(reason));
            }
            _ => {}
        }
        self.answer(output, false, "finish the upload of")?;
        Ok(Transfer {
            bytes: stats.literal_bytes.unwrap_or(size),
            already_present: false,
        })
    }
    fn delete(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
        plain_name(name)?;
        let script = self.script(DELETE).arg(id.to_string()).arg(name);
        self.run(&script, false, "delete").map(|_| ())
    }
    fn delete_partial(&self, id: ObjectId, name: &str) -> Result<(), RemoteError> {
        plain_name(name)?;
        let script = self
            .script(DELETE)
            .arg(id.to_string())
            .arg(name)
            .arg("partial");
        self.run(&script, false, "delete").map(|_| ())
    }
    fn checksum(&self, id: ObjectId, name: &str) -> Result<String, RemoteError> {
        let answer = self.run_on_file(CHECKSUM, id, name, "hash")?;
        checksum_of(&answer.frames)
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
    fn a_file_is_hashed_by_the_tool_the_proof_chose_and_by_no_other() {
        let dir = bare_path(&["sh", "cat"]);
        tool(
            dir.path(),
            "sha256sum",
            &format!("[ \"$#\" -eq 0 ] && {{ echo '{ABC}  -'; exit 0; }}; exit 1"),
        );
        tool(dir.path(), "shasum", &format!("echo '{ABC}  x'"));
        let (status, out) = run_with_only(dir.path(), "antiq_sha256 abc");
        assert_eq!((status, out.as_str()), (1, ""));
    }

    #[test]
    fn a_file_result_of_the_wrong_shape_is_status_1_and_not_a_next_tool() {
        let dir = bare_path(&["sh", "cat"]);
        tool(
            dir.path(),
            "sha256sum",
            &format!("[ \"$#\" -eq 0 ] && {{ echo '{ABC}  -'; exit 0; }}; echo junk"),
        );
        tool(dir.path(), "shasum", &format!("echo '{ABC}  x'"));
        let (status, out) = run_with_only(dir.path(), "antiq_sha256 abc");
        assert_eq!((status, out.as_str()), (1, ""));
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

    // ----- reading -----

    const OBJECT: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

    fn object() -> ObjectId {
        OBJECT.parse().expect("a ULID")
    }

    impl Far {
        /// The directory of an object on the far side, made.
        fn object_dir(&self, id: &str) -> PathBuf {
            let dir = self.root().join("objects").join(id);
            std::fs::create_dir_all(&dir).expect("object directory");
            dir
        }

        /// How many commands the shim ran.
        fn commands(&self) -> usize {
            std::fs::read_to_string(self.state.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("/bin/sh -s"))
                .count()
        }
    }

    #[test]
    fn the_objects_are_the_canonical_ulid_directories_in_order() {
        let far = Far::new();
        let second = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        for name in [
            OBJECT,
            second,
            "01arz3ndektsv4rrffq69g5fav",
            "not-an-id",
            ".hidden",
        ] {
            far.object_dir(name);
        }
        std::fs::write(
            far.root().join("objects/01BX5ZZKBKACTAV9WEVGEMMVRY"),
            "a file",
        )
        .expect("file");
        let found = far.remote().list_objects().expect("listed");
        let expected: Vec<ObjectId> = [second, OBJECT]
            .iter()
            .map(|text| text.parse().expect("a ULID"))
            .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_remote_without_objects_has_none() {
        let far = Far::new();
        assert_eq!(far.remote().list_objects().expect("listed"), vec![]);
    }

    fn set_mtime(path: &Path, seconds: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .or_else(|_| std::fs::File::open(path))
            .expect("open");
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
            .expect("mtime");
    }

    #[test]
    fn a_directory_lists_names_sizes_times_and_kinds() {
        let far = Far::new();
        let dir = far.object_dir(OBJECT);
        std::fs::write(dir.join("meta.1"), "12345").expect("file");
        set_mtime(&dir.join("meta.1"), 1_700_000_000);
        std::fs::create_dir(dir.join(".antiq-partial")).expect("dir");
        let mut found = far.remote().list_dir(object()).expect("listed");
        found.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<(&str, u64, bool)> = found
            .iter()
            .map(|file| (file.name.as_str(), file.size, file.is_dir))
            .collect();
        assert_eq!(
            names,
            [(".antiq-partial", names[0].1, true), ("meta.1", 5, false)]
        );
        assert_eq!(found[1].modified.as_second(), 1_700_000_000);
    }

    #[test]
    fn dot_names_of_interrupted_work_are_listed() {
        let far = Far::new();
        let dir = far.object_dir(OBJECT);
        for name in [".x.laptop.part", ".x.laptop.part.Ab12Cd", ".y", "..z"] {
            std::fs::write(dir.join(name), "d").expect("file");
        }
        let mut names: Vec<String> = far
            .remote()
            .list_dir(object())
            .expect("listed")
            .into_iter()
            .map(|file| file.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["..z", ".x.laptop.part", ".x.laptop.part.Ab12Cd", ".y"]
        );
    }

    #[test]
    fn an_empty_or_absent_directory_lists_nothing() {
        let far = Far::new();
        assert_eq!(far.remote().list_dir(object()).expect("absent"), vec![]);
        far.object_dir(OBJECT);
        assert_eq!(far.remote().list_dir(object()).expect("empty"), vec![]);
    }

    #[test]
    fn a_name_with_a_tab_or_a_space_is_not_listed() {
        let far = Far::new();
        let dir = far.object_dir(OBJECT);
        for name in ["tab\tname", "a b", "good"] {
            std::fs::write(dir.join(name), "d").expect("file");
        }
        let names: Vec<String> = far
            .remote()
            .list_dir(object())
            .expect("listed")
            .into_iter()
            .map(|file| file.name)
            .collect();
        assert_eq!(names, ["good"]);
    }

    #[test]
    fn the_partial_directory_lists_its_files_dot_names_included() {
        let far = Far::new();
        let partial = far.object_dir(OBJECT).join(".antiq-partial");
        std::fs::create_dir(&partial).expect("dir");
        std::fs::write(partial.join(".temp.part"), "abc").expect("file");
        std::fs::write(partial.join("plain"), "abcd").expect("file");
        let mut found = far.remote().list_partial(object()).expect("listed");
        found.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<(&str, u64)> = found
            .iter()
            .map(|file| (file.name.as_str(), file.size))
            .collect();
        assert_eq!(names, [(".temp.part", 3), ("plain", 4)]);
        assert_eq!(
            far.remote()
                .list_partial("01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("id"))
                .expect("absent"),
            vec![]
        );
    }

    fn put_meta(far: &Far, id: &str, name: &str, bytes: &str) {
        std::fs::write(far.object_dir(id).join(name), bytes).expect("file");
    }

    #[test]
    fn only_versions_above_the_known_ones_are_pulled() {
        let far = Far::new();
        for (name, bytes) in [
            ("meta.1", "one"),
            ("meta.2", "two"),
            ("meta.10", "ten"),
            ("meta.9", "nine"),
            ("meta.2.claim", ""),
            ("meta.3.tmp.laptop", "t"),
            ("meta.x", "x"),
            ("meta.01", "z"),
            ("meta.010", "z"),
            ("meta.4294967296", "big"),
            ("payload.tar", "p"),
        ] {
            put_meta(&far, OBJECT, name, bytes);
        }
        let mut known = KnownVersions::new();
        known.insert(object(), 2);
        let mut found = far.remote().pull_metadata(&known).expect("pulled");
        found.sort_by_key(|file| file.number);
        let versions: Vec<(u32, &[u8])> = found
            .iter()
            .map(|file| (file.number.get(), file.bytes.as_slice()))
            .collect();
        assert_eq!(versions, [(9, &b"nine"[..]), (10, &b"ten"[..])]);
        assert!(found.iter().all(|file| file.id == object()));
    }

    /// Runs `check` while `dir` cannot be listed (mode 0300), then gives the
    /// directory its permissions back. Does nothing when the directory stays
    /// listable, as for the superuser.
    fn with_unlistable(dir: &Path, check: impl FnOnce()) {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o300)).expect("chmod");
        if std::fs::read_dir(dir).is_err() {
            check();
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }

    #[test]
    fn an_object_directory_that_cannot_be_listed_is_an_io_error() {
        let far = Far::new();
        let dir = far.object_dir(OBJECT);
        with_unlistable(&dir, || {
            let error = far.remote().list_dir(object()).expect_err("unlistable");
            assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        });
    }

    #[test]
    fn the_objects_directory_that_cannot_be_listed_is_an_io_error_for_the_object_list() {
        let far = Far::new();
        far.object_dir(OBJECT);
        with_unlistable(&far.root().join("objects"), || {
            let error = far.remote().list_objects().expect_err("unlistable");
            assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        });
    }

    #[test]
    fn the_objects_directory_that_cannot_be_listed_is_an_io_error_for_the_pull() {
        let far = Far::new();
        put_meta(&far, OBJECT, "meta.1", "a");
        with_unlistable(&far.root().join("objects"), || {
            let error = far
                .remote()
                .pull_metadata(&KnownVersions::new())
                .expect_err("unlistable");
            assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        });
    }

    #[test]
    fn an_object_directory_that_cannot_be_listed_is_an_io_error_for_the_pull() {
        let far = Far::new();
        put_meta(&far, OBJECT, "meta.1", "a");
        with_unlistable(&far.object_dir(OBJECT), || {
            let error = far
                .remote()
                .pull_metadata(&KnownVersions::new())
                .expect_err("unlistable");
            assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        });
    }

    #[test]
    fn unknown_objects_are_pulled_from_version_one() {
        let far = Far::new();
        put_meta(&far, OBJECT, "meta.1", "a");
        put_meta(&far, "01ARZ3NDEKTSV4RRFFQ69G5FAV", "meta.1", "b\0c\n");
        put_meta(&far, "not-an-id", "meta.1", "z");
        let mut found = far
            .remote()
            .pull_metadata(&KnownVersions::new())
            .expect("pulled");
        found.sort_by_key(|file| file.bytes.clone());
        let bytes: Vec<&[u8]> = found.iter().map(|file| file.bytes.as_slice()).collect();
        assert_eq!(bytes, [&b"a"[..], &b"b\0c\n"[..]]);
    }

    #[test]
    fn five_hundred_objects_are_pulled_in_one_call() {
        let far = Far::new();
        for n in 0..500u64 {
            let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_000 + n, 7)).to_string();
            put_meta(&far, &id, "meta.1", "m");
        }
        let before = far.commands();
        let found = far
            .remote()
            .pull_metadata(&KnownVersions::new())
            .expect("pulled");
        assert_eq!(found.len(), 500);
        assert_eq!(far.commands() - before, 1);
    }

    #[test]
    fn a_small_file_is_read_whole() {
        let far = Far::new();
        let bytes: &[u8] = b"a\0b\n\nc\n";
        std::fs::write(far.object_dir(OBJECT).join("listing"), bytes).expect("file");
        assert_eq!(
            far.remote().read_file(object(), "listing").expect("read"),
            bytes
        );
    }

    #[test]
    fn an_empty_file_is_read_as_nothing() {
        let far = Far::new();
        std::fs::write(far.object_dir(OBJECT).join("empty"), "").expect("file");
        assert_eq!(
            far.remote().read_file(object(), "empty").expect("read"),
            b""
        );
    }

    #[test]
    fn a_file_that_is_not_there_is_not_found() {
        let far = Far::new();
        far.object_dir(OBJECT);
        let error = far
            .remote()
            .read_file(object(), "nothing")
            .expect_err("missing");
        match error {
            RemoteError::Io { ref source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound, "{error}")
            }
            other => panic!("expected an Io error, got {other}"),
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_io_error() {
        let far = Far::new();
        let file = far.object_dir(OBJECT).join("locked");
        std::fs::write(&file, "x").expect("file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let error = far
            .remote()
            .read_file(object(), "locked")
            .expect_err("unreadable");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
    }

    /// Takes all permissions from `path` and says whether that made it
    /// unreadable. The superuser reads it anyway, and a test that depends
    /// on the failure returns early then.
    fn make_unreadable(path: &Path) -> bool {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        std::fs::File::open(path).is_err()
    }

    #[test]
    fn the_checksum_of_an_unreadable_file_is_an_io_error_and_the_remote_stays_sound() {
        let far = Far::new();
        let file = far.object_dir(OBJECT).join("payload");
        std::fs::write(&file, "abc").expect("file");
        if !make_unreadable(&file) {
            return;
        }
        let error = far
            .remote()
            .checksum(object(), "payload")
            .expect_err("unreadable");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
    }

    #[test]
    fn the_checksum_of_a_directory_is_an_io_error() {
        let far = Far::new();
        std::fs::create_dir(far.object_dir(OBJECT).join("payload")).expect("directory");
        let error = far
            .remote()
            .checksum(object(), "payload")
            .expect_err("directory");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
    }

    #[test]
    fn a_put_over_an_unreadable_file_is_an_io_error() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        let source = far.local("pack.tar", b"x");
        let there = far_file(&far, "pack.tar");
        std::fs::write(&there, "other").expect("there already");
        if !make_unreadable(&there) {
            return;
        }
        let error = far
            .remote_with(&fake, SshSeams::default())
            .put_payload(&upload(&source, &sha_of(b"x")))
            .expect_err("unreadable");
        std::fs::set_permissions(&there, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
    }

    #[test]
    fn the_checksum_is_the_sha256_of_the_bytes_on_the_far_side() {
        let far = Far::new();
        std::fs::write(far.object_dir(OBJECT).join("payload"), "abc").expect("file");
        assert_eq!(
            far.remote().checksum(object(), "payload").expect("hashed"),
            ABC
        );
    }

    #[test]
    fn the_checksum_of_a_missing_file_is_not_found() {
        let far = Far::new();
        far.object_dir(OBJECT);
        let error = far
            .remote()
            .checksum(object(), "nothing")
            .expect_err("missing");
        assert!(
            matches!(error, RemoteError::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound),
            "{error}"
        );
    }

    #[test]
    fn the_checksum_without_a_hash_tool_is_broken() {
        let far = Far::new();
        std::fs::write(far.object_dir(OBJECT).join("payload"), "abc").expect("file");
        for tool in ["sha256sum", "shasum", "sha256", "openssl"] {
            far.wrap(tool, "exit 1");
        }
        let error = far
            .remote()
            .checksum(object(), "payload")
            .expect_err("no tool");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
    }

    #[test]
    fn a_checksum_that_is_no_hash_is_an_error_and_never_a_mismatch() {
        let far = Far::new();
        let output = Output {
            status: 0,
            stdout: b"@sum zzz 0\n\n".to_vec(),
            stderr: vec![],
        };
        let frames = read_frames(&output.stdout).expect("frames");
        let error = checksum_of(&frames).expect_err("junk");
        assert!(error.to_string().contains("SHA-256"), "{error}");
        let _ = far;
    }

    #[test]
    fn names_that_reach_outside_the_directory_are_refused_before_any_script_runs() {
        let far = Far::new();
        let remote = far.remote();
        for name in ["", ".", "..", "a/b", "../x", "nul\0byte"] {
            for result in [
                remote.read_file(object(), name).map(|_| ()),
                remote.checksum(object(), name).map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(RemoteError::InvalidName { .. })),
                    "{name:?}"
                );
            }
        }
        assert_eq!(far.commands(), 0);
    }

    #[test]
    fn a_frame_can_carry_arguments_from_the_prelude() {
        let work = tempfile::tempdir().expect("temp dir");
        std::fs::write(work.path().join("f"), "xy").expect("file");
        let (status, out) = run_prelude("antiq_emit meta f 01ARZ meta.3", work.path());
        assert_eq!((status, out.as_str()), (0, "@meta 01ARZ meta.3 2\nxy\n"));
    }

    fn frame(kind: &str, args: &[&str]) -> Frame {
        Frame {
            kind: kind.into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            bytes: vec![],
        }
    }

    #[test]
    fn an_entry_with_the_wrong_number_of_fields_is_malformed() {
        let error = parse_entries(&[frame("entry", &["a", "f", "1"])]).expect_err("short");
        assert!(error.to_string().contains("wrong form"), "{error}");
    }

    #[test]
    fn an_entry_with_too_many_fields_is_malformed() {
        let error = parse_entries(&[frame("entry", &["a", "f", "1", "2", "3"])]).expect_err("long");
        assert!(error.to_string().contains("wrong form"), "{error}");
    }

    #[test]
    fn a_frame_of_another_kind_is_no_entry() {
        let error = parse_entries(&[frame("object", &["a", "f", "1", "2"])]).expect_err("kind");
        assert!(error.to_string().contains("wrong form"), "{error}");
    }

    #[test]
    fn an_entry_with_a_size_that_is_no_number_is_malformed() {
        let error = parse_entries(&[frame("entry", &["a", "f", "big", "2"])]).expect_err("size");
        assert!(error.to_string().contains("not one"), "{error}");
    }

    #[test]
    fn an_entry_with_a_time_out_of_range_is_malformed() {
        let error = parse_entries(&[frame("entry", &["a", "f", "1", "99999999999999999"])])
            .expect_err("time");
        assert!(error.to_string().contains("out of range"), "{error}");
    }

    #[test]
    fn a_metadata_frame_without_object_and_name_is_malformed() {
        let error = parse_meta(vec![frame("meta", &["only-one"])], &KnownVersions::new())
            .expect_err("form");
        assert!(error.to_string().contains("wrong form"), "{error}");
    }

    #[test]
    fn an_answer_without_a_file_frame_is_malformed() {
        let error = file_bytes(vec![frame("sum", &[])]).expect_err("kind");
        assert!(error.to_string().contains("missing"), "{error}");
        let error = file_bytes(vec![]).expect_err("none");
        assert!(error.to_string().contains("missing"), "{error}");
    }

    #[test]
    fn a_checksum_of_the_wrong_length_is_malformed() {
        let short = &ABC[..63];
        let long = format!("{ABC}0");
        for sum in [short, "", long.as_str()] {
            let error = checksum_of(&[frame("sum", &[sum])]).expect_err("length");
            assert!(error.to_string().contains("SHA-256"), "{sum}: {error}");
        }
    }

    #[test]
    fn a_checksum_frame_without_a_hash_is_malformed() {
        assert!(checksum_of(&[]).is_err());
        assert!(checksum_of(&[frame("sum", &[])]).is_err());
        assert!(checksum_of(&[frame("sum", &[&ABC.to_uppercase()])]).is_err());
        assert_eq!(checksum_of(&[frame("sum", &[ABC])]).expect("hash"), ABC);
    }

    // ----- writing -----

    fn far_file(far: &Far, name: &str) -> PathBuf {
        far.object_dir(OBJECT).join(name)
    }

    #[test]
    fn the_first_exclusive_create_makes_the_file_and_the_second_keeps_it() {
        let far = Far::new();
        let remote = far.remote();
        assert_eq!(
            remote
                .create_exclusive(object(), "meta.2.claim")
                .expect("first"),
            Created::Yes
        );
        let claim = far_file(&far, "meta.2.claim");
        assert_eq!(std::fs::read(&claim).expect("claim"), b"");
        std::fs::write(&claim, "mine").expect("bytes");
        assert_eq!(
            remote
                .create_exclusive(object(), "meta.2.claim")
                .expect("second"),
            Created::AlreadyExists
        );
        assert_eq!(std::fs::read(&claim).expect("claim"), b"mine");
    }

    #[test]
    fn an_exclusive_create_runs_the_text_of_the_seam() {
        let far = Far::new();
        let seams = SshSeams {
            exclusive_create: "exit 17",
            ..SshSeams::default()
        };
        let remote = far.remote().with_seams(seams);
        assert_eq!(
            remote.create_exclusive(object(), "fresh").expect("answer"),
            Created::AlreadyExists
        );
        assert!(!far_file(&far, "fresh").exists());
    }

    #[test]
    fn a_failing_mkdir_on_the_far_side_is_an_io_error_of_kind_other() {
        let far = Far::new();
        far.wrap("mkdir", "echo 'mkdir: denied' >&2; exit 1");
        let error = far
            .remote()
            .create_exclusive(object(), "claim")
            .expect_err("mkdir fails");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        assert!(error.to_string().contains("mkdir: denied"), "{error}");
        assert_eq!(error.kind(), crate::error::ErrorKind::Other);
    }

    #[test]
    fn a_temp_file_holds_exactly_the_bytes_given() {
        let far = Far::new();
        let bytes: &[u8] = b"line\n'quoted' \"double\" $HOME `x` \\ \xff\n\n";
        far.remote()
            .write_temp(object(), ".meta.3.laptop.part", bytes)
            .expect("written");
        assert_eq!(
            std::fs::read(far_file(&far, ".meta.3.laptop.part")).expect("temp"),
            bytes
        );
    }

    #[test]
    fn a_temp_file_of_another_size_than_the_bytes_is_an_error() {
        let far = Far::new();
        far.wrap(
            "stat",
            r#"case "$2" in '%s %Y' | '%z %m') echo "1 0"; exit 0 ;; esac
exec /usr/bin/stat "$@""#,
        );
        let error = far
            .remote()
            .write_temp(object(), ".t.part", b"three")
            .expect_err("size");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        assert!(
            error.to_string().contains("wrote 1 bytes instead of 5"),
            "{error}"
        );
    }

    #[test]
    fn rewriting_a_temp_file_shortens_it() {
        let far = Far::new();
        let remote = far.remote();
        remote
            .write_temp(object(), ".t.part", b"long content")
            .expect("first");
        remote
            .write_temp(object(), ".t.part", b"short")
            .expect("second");
        assert_eq!(
            std::fs::read(far_file(&far, ".t.part")).expect("temp"),
            b"short"
        );
        remote.write_temp(object(), ".t.part", b"").expect("empty");
        assert_eq!(std::fs::read(far_file(&far, ".t.part")).expect("temp"), b"");
    }

    #[test]
    fn a_nul_byte_is_refused_before_any_script_runs() {
        let far = Far::new();
        let remote = far.remote();
        let error = remote
            .write_temp(object(), ".t.part", b"a\0b")
            .expect_err("NUL");
        assert!(matches!(error, RemoteError::Session(_)), "{error}");
        assert_eq!(far.commands(), 0);
    }

    #[test]
    fn a_temp_is_renamed_into_place_and_its_name_goes() {
        let far = Far::new();
        let temp = far_file(&far, ".meta.3.part");
        std::fs::write(&temp, "v3").expect("temp");
        let placed = far
            .remote()
            .rename_into_place(object(), ".meta.3.part", "meta.3")
            .expect("renamed");
        assert_eq!(placed, Placement::Placed);
        assert_eq!(
            std::fs::read(far_file(&far, "meta.3")).expect("final"),
            b"v3"
        );
        assert!(!temp.exists());
    }

    #[test]
    fn a_taken_name_is_left_alone_whatever_holds_it() {
        let far = Far::new();
        let remote = far.remote();
        let temp = far_file(&far, ".t.part");
        std::fs::write(&temp, "new").expect("temp");
        let final_name = far_file(&far, "taken");
        for taker in ["file", "directory", "dangling"] {
            match taker {
                "file" => std::fs::write(&final_name, "old").expect("file"),
                "directory" => std::fs::create_dir(&final_name).expect("directory"),
                _ => std::os::unix::fs::symlink("nowhere", &final_name).expect("symlink"),
            }
            let placed = remote
                .rename_into_place(object(), ".t.part", "taken")
                .expect("answer");
            assert_eq!(placed, Placement::TargetExists, "{taker}");
            assert_eq!(std::fs::read(&temp).expect("temp kept"), b"new", "{taker}");
            match taker {
                "file" => assert_eq!(std::fs::read(&final_name).expect("final"), b"old"),
                "directory" => assert!(final_name.is_dir()),
                _ => assert_eq!(
                    std::fs::read_link(&final_name).expect("link"),
                    Path::new("nowhere")
                ),
            }
            if final_name.is_dir() {
                std::fs::remove_dir(&final_name).expect("clean");
            } else {
                std::fs::remove_file(&final_name).expect("clean");
            }
        }
    }

    #[test]
    fn a_link_the_far_side_refuses_is_an_io_error_and_never_a_taken_name() {
        let far = Far::new();
        let temp = far_file(&far, ".t.part");
        std::fs::write(&temp, "new").expect("temp");
        far.wrap("ln", "echo 'ln: Operation not permitted' >&2; exit 1");
        let error = far
            .remote()
            .rename_into_place(object(), ".t.part", "meta.3")
            .expect_err("ln fails");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        assert!(
            error.to_string().contains("Operation not permitted"),
            "{error}"
        );
        assert_eq!(std::fs::read(&temp).expect("temp stays"), b"new");
        assert!(!far_file(&far, "meta.3").exists());
    }

    #[test]
    fn a_name_taken_while_the_link_is_made_is_a_taken_name() {
        let far = Far::new();
        let temp = far_file(&far, ".t.part");
        std::fs::write(&temp, "new").expect("temp");
        far.wrap(
            "ln",
            "echo rival > \"$2\"; echo 'ln: File exists' >&2; exit 1",
        );
        let placed = far
            .remote()
            .rename_into_place(object(), ".t.part", "meta.3")
            .expect("answer");
        assert_eq!(placed, Placement::TargetExists);
        assert_eq!(
            std::fs::read(far_file(&far, "meta.3")).expect("rival"),
            b"rival\n"
        );
        assert_eq!(std::fs::read(&temp).expect("temp kept"), b"new");
    }

    #[test]
    fn renaming_a_temp_that_is_not_there_creates_nothing() {
        let far = Far::new();
        far.object_dir(OBJECT);
        let error = far
            .remote()
            .rename_into_place(object(), ".nothing.part", "meta.3")
            .expect_err("missing");
        assert!(
            matches!(error, RemoteError::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound),
            "{error}"
        );
        assert!(!far_file(&far, "meta.3").exists());
    }

    #[test]
    fn deleting_removes_a_file_and_a_missing_file_counts_as_removed() {
        let far = Far::new();
        let remote = far.remote();
        let file = far_file(&far, "payload.old");
        std::fs::write(&file, "x").expect("file");
        remote.delete(object(), "payload.old").expect("deleted");
        assert!(!file.exists());
        remote.delete(object(), "payload.old").expect("again");
        remote
            .delete("01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("id"), "x")
            .expect("object without directory");
    }

    #[test]
    fn deleting_a_directory_is_an_io_error() {
        let far = Far::new();
        std::fs::create_dir(far_file(&far, "dir")).expect("dir");
        let error = far.remote().delete(object(), "dir").expect_err("directory");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        assert!(far_file(&far, "dir").is_dir());
    }

    #[test]
    fn a_partial_file_is_deleted_from_the_partial_directory_only() {
        let far = Far::new();
        let partial = far_file(&far, ".antiq-partial");
        std::fs::create_dir(&partial).expect("dir");
        std::fs::write(partial.join(".t.part"), "half").expect("partial");
        std::fs::write(far_file(&far, ".t.part"), "other").expect("object file");
        let remote = far.remote();
        remote.delete_partial(object(), ".t.part").expect("deleted");
        assert!(!partial.join(".t.part").exists());
        assert!(far_file(&far, ".t.part").exists());
        remote.delete_partial(object(), ".t.part").expect("again");
    }

    #[test]
    fn writing_refuses_names_that_reach_outside_the_directory() {
        let far = Far::new();
        let remote = far.remote();
        for name in ["", ".", "..", "a/b", "../x"] {
            assert!(matches!(
                remote.create_exclusive(object(), name),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.write_temp(object(), name, b"x"),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.rename_into_place(object(), name, "x"),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.rename_into_place(object(), "x", name),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.delete(object(), name),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.delete_partial(object(), name),
                Err(RemoteError::InvalidName { .. })
            ));
        }
        assert_eq!(far.commands(), 0);
    }

    /// Names the shell or rsync would read as more than a literal file name.
    const UNSAFE_NAMES: [&str; 8] = ["a b", "a;b", "$x", "a*b", "-x", "a?b", "a'b", "a\nb"];

    #[test]
    fn writing_refuses_names_outside_the_listing_alphabet() {
        let far = Far::new();
        let remote = far.remote();
        for name in UNSAFE_NAMES {
            assert!(matches!(
                remote.create_exclusive(object(), name),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.write_temp(object(), name, b"x"),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.delete(object(), name),
                Err(RemoteError::InvalidName { .. })
            ));
            assert!(matches!(
                remote.read_file(object(), name),
                Err(RemoteError::InvalidName { .. })
            ));
        }
        assert_eq!(far.commands(), 0);
    }

    #[test]
    fn a_get_refuses_names_outside_the_listing_alphabet() {
        let far = Far::new();
        let to = far.state.path().join("fetched.tar");
        let remote = far.remote_with_seams(SshSeams::default());
        for name in UNSAFE_NAMES {
            let error = remote.get_payload(object(), name, &to).expect_err("name");
            assert!(matches!(error, RemoteError::InvalidName { .. }), "{error}");
        }
        assert_eq!(far.commands(), 0);
    }

    #[test]
    fn a_put_refuses_temp_final_and_seed_names_outside_the_listing_alphabet() {
        let far = Far::new();
        let source = far.local("pack.tar", b"x");
        let sha = sha_of(b"x");
        let remote = far.remote_with_seams(SshSeams::default());
        for name in UNSAFE_NAMES {
            let bad_temp = Upload::new(object(), name, "pack.tar", &source, &sha);
            let bad_final = Upload::new(object(), ".t", name, &source, &sha);
            let bad_seed = Upload {
                seed: Some(name),
                ..Upload::new(object(), ".t", "pack.tar", &source, &sha)
            };
            for upload in [bad_temp, bad_final, bad_seed] {
                let error = remote.put_payload(&upload).expect_err("name");
                assert!(matches!(error, RemoteError::InvalidName { .. }), "{error}");
            }
        }
        assert_eq!(far.commands(), 0);
    }

    // ----- transfers -----

    #[test]
    fn rsync_reaches_the_far_side_through_the_shim() {
        use crate::remote::rsync::{Rsync, TransferSettings};
        let far = Far::new();
        let remote = far.remote_with_seams(SshSeams::default());
        let rsync = Rsync::from_machine(&remote.machine).expect("rsync");
        let settings =
            TransferSettings::for_machine(&remote.machine, remote.session.rsh_argv(), false);
        let source = far.state.path().join("local.bin");
        std::fs::write(&source, "carried").expect("source");
        let destination = far.object_dir(OBJECT).join("carried.bin");
        let target = format!("nas:{}", destination.display());
        rsync
            .transfer(&settings, source.as_os_str(), target.as_ref())
            .run()
            .expect("rsync runs through the shim");
        assert_eq!(std::fs::read(destination).expect("copied"), b"carried");
    }

    // ----- payload helpers -----

    /// Bytes that do not repeat, so that rsync finds a match only where
    /// two files share a stretch.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        crate::pack::hashing::hex(&Sha256::digest(bytes))
    }

    impl Far {
        fn local(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.state.path().join(name);
            std::fs::write(&path, bytes).expect("local file");
            path
        }

        fn remote_with(&self, rsync: &Path, seams: SshSeams) -> SshRemote {
            let machine = MachineConfig {
                rsync: Some(rsync.to_path_buf()),
                ..MachineConfig::default()
            };
            self.remote_of(machine, seams)
        }

        fn remote_of(&self, machine: MachineConfig, seams: SshSeams) -> SshRemote {
            SshRemote::with_session(
                self.session(),
                self.root().to_str().expect("UTF-8").to_string(),
                id(),
                machine,
                "nas".into(),
            )
            .with_seams(seams)
        }

        fn real_rsync() -> PathBuf {
            crate::remote::rsync::Rsync::resolve(None, &std::env::var_os("PATH").expect("PATH"))
                .expect("these tests need an rsync 3.1 or later")
                .path()
                .to_path_buf()
        }

        fn remote_with_seams(&self, seams: SshSeams) -> SshRemote {
            self.remote_with(&Far::real_rsync(), seams)
        }

        /// What the fake rsync was called with, one call per entry.
        fn fake_calls(fake: &Path) -> Vec<Vec<String>> {
            let raw = std::fs::read(fake.with_file_name("calls.log")).unwrap_or_default();
            raw.split(|&byte| byte == b'\n')
                .flat_map(|chunk| chunk.split(|&byte| byte == 0))
                .fold(Vec::<Vec<String>>::new(), |mut calls, word| {
                    match word {
                        b"version" | b"transfer" => {
                            calls.push(vec![String::from_utf8_lossy(word).into_owned()])
                        }
                        b"" => {}
                        word => calls
                            .last_mut()
                            .expect("a call starts with its mode")
                            .push(String::from_utf8_lossy(word).into_owned()),
                    }
                    calls
                })
        }
    }

    /// A directory with a fake rsync that passes the probe.
    fn fake_rsync() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-rsync.sh");
        let path = dir.path().join("rsync");
        std::os::unix::fs::symlink(fixture, &path).expect("link");
        std::fs::write(
            dir.path().join("version"),
            "rsync  version 3.5.1  protocol version 33\n",
        )
        .expect("version");
        (dir, path)
    }

    fn upload<'a>(source: &'a Path, sha: &'a str) -> Upload<'a> {
        Upload::new(object(), ".pack.laptop.part", "pack.tar", source, sha)
    }

    fn transfers(calls: &[Vec<String>]) -> usize {
        calls.iter().filter(|call| call[0] == "transfer").count()
    }

    // ----- put -----

    #[test]
    fn a_put_places_the_file_and_removes_the_temp() {
        let far = Far::new();
        let bytes = noise(300_000, 1);
        let source = far.local("pack.tar", &bytes);
        let sha = sha_of(&bytes);
        let moved = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&upload(&source, &sha))
            .expect("put");
        assert_eq!(moved.bytes, 300_000);
        assert!(!moved.already_present);
        assert_eq!(
            std::fs::read(far_file(&far, "pack.tar")).expect("final"),
            bytes
        );
        assert!(!far_file(&far, ".pack.laptop.part").exists());
    }

    #[test]
    fn a_put_of_a_name_that_holds_the_same_bytes_runs_no_transfer() {
        let far = Far::new();
        let (fake_dir, fake) = fake_rsync();
        let bytes = noise(1_000, 2);
        let source = far.local("pack.tar", &bytes);
        std::fs::write(far_file(&far, "pack.tar"), &bytes).expect("there already");
        let moved = far
            .remote_with(&fake, SshSeams::default())
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect("put");
        assert!(moved.already_present);
        assert_eq!(moved.bytes, 0);
        assert_eq!(transfers(&Far::fake_calls(&fake)), 0);
        drop(fake_dir);
    }

    #[test]
    fn a_put_onto_a_name_with_other_bytes_is_a_conflict_and_changes_nothing() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        let bytes = noise(1_000, 3);
        let source = far.local("pack.tar", &bytes);
        std::fs::write(far_file(&far, "pack.tar"), "other").expect("there already");
        let error = far
            .remote_with(&fake, SshSeams::default())
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect_err("conflict");
        assert!(
            matches!(error, RemoteError::PayloadConflict { .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read(far_file(&far, "pack.tar")).expect("kept"),
            b"other"
        );
        assert_eq!(transfers(&Far::fake_calls(&fake)), 0);
    }

    #[test]
    fn a_name_that_is_a_directory_is_a_conflict() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        let source = far.local("pack.tar", b"x");
        std::fs::create_dir(far_file(&far, "pack.tar")).expect("directory");
        let error = far
            .remote_with(&fake, SshSeams::default())
            .put_payload(&upload(&source, &sha_of(b"x")))
            .expect_err("conflict");
        assert!(
            matches!(error, RemoteError::PayloadConflict { .. }),
            "{error}"
        );
    }

    #[test]
    fn an_interrupted_upload_resumes_from_its_partial_file() {
        let far = Far::new();
        let bytes = noise(400_000, 4);
        let source = far.local("pack.tar", &bytes);
        let partial = far_file(&far, ".antiq-partial");
        std::fs::create_dir(&partial).expect("dir");
        std::fs::write(partial.join(".pack.laptop.part"), &bytes[..300_000]).expect("partial");
        let moved = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect("put");
        assert!(moved.bytes < 200_000, "literal bytes: {}", moved.bytes);
        assert_eq!(
            std::fs::read(far_file(&far, "pack.tar")).expect("final"),
            bytes
        );
    }

    #[test]
    fn a_seeded_upload_sends_only_what_differs_and_leaves_the_seed_alone() {
        let far = Far::new();
        let old = noise(400_000, 5);
        let mut new = old.clone();
        new[200_000..200_100].copy_from_slice(&noise(100, 6));
        std::fs::write(far_file(&far, "live.old"), &old).expect("seed");
        // rsync skips a file whose size and modification time match, and
        // the temp is a link to the seed.
        let source = far.local("live.new", &new);
        set_mtime(&source, 2_000_000_000);
        let sha = sha_of(&new);
        let with_seed = Upload {
            seed: Some("live.old"),
            ..Upload::new(object(), ".live.laptop.part", "live.new", &source, &sha)
        };
        let moved = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&with_seed)
            .expect("put");
        assert!(moved.bytes < 100_000, "literal bytes: {}", moved.bytes);
        assert_eq!(std::fs::read(far_file(&far, "live.new")).expect("new"), new);
        assert_eq!(std::fs::read(far_file(&far, "live.old")).expect("old"), old);
        assert!(!far_file(&far, ".live.laptop.part").exists());
    }

    #[test]
    fn a_missing_seed_sends_the_whole_file() {
        let far = Far::new();
        let bytes = noise(100_000, 7);
        let source = far.local("live.new", &bytes);
        let sha = sha_of(&bytes);
        let with_seed = Upload {
            seed: Some("live.gone"),
            ..Upload::new(object(), ".live.laptop.part", "live.new", &source, &sha)
        };
        let moved = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&with_seed)
            .expect("put");
        assert_eq!(moved.bytes, 100_000);
        assert_eq!(
            std::fs::read(far_file(&far, "live.new")).expect("new"),
            bytes
        );
    }

    #[test]
    fn a_seed_that_is_a_symlink_is_not_linked() {
        let far = Far::new();
        let bytes = noise(100_000, 8);
        std::fs::write(far.root().join("elsewhere"), &bytes).expect("target");
        std::os::unix::fs::symlink(far.root().join("elsewhere"), far_file(&far, "live.old"))
            .expect("symlink");
        let source = far.local("live.new", &bytes);
        let sha = sha_of(&bytes);
        let with_seed = Upload {
            seed: Some("live.old"),
            ..Upload::new(object(), ".live.laptop.part", "live.new", &source, &sha)
        };
        let moved = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&with_seed)
            .expect("put");
        assert_eq!(moved.bytes, 100_000);
    }

    #[test]
    fn an_upload_that_arrives_short_is_an_error_and_keeps_its_temp() {
        let far = Far::new();
        let bytes = noise(50_000, 9);
        let source = far.local("pack.tar", &bytes);
        let temp = far_file(&far, ".pack.laptop.part");
        let truncate = {
            let temp = temp.clone();
            std::sync::Arc::new(move || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&temp)
                    .expect("temp")
                    .set_len(10)
                    .expect("truncated");
            }) as Hook
        };
        let seams = SshSeams {
            after_transfer: Some(truncate),
            ..SshSeams::default()
        };
        let error = far
            .remote_with_seams(seams)
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect_err("short");
        assert!(matches!(error, RemoteError::Transfer(_)), "{error}");
        assert!(error.to_string().contains("10 bytes"), "{error}");
        assert!(!far_file(&far, "pack.tar").exists());
        assert!(temp.exists());
    }

    #[test]
    fn a_name_taken_during_the_upload_by_the_same_bytes_is_present() {
        let far = Far::new();
        let bytes = noise(20_000, 10);
        let source = far.local("pack.tar", &bytes);
        let place = {
            let target = far_file(&far, "pack.tar");
            let bytes = bytes.clone();
            std::sync::Arc::new(move || std::fs::write(&target, &bytes).expect("rival")) as Hook
        };
        let seams = SshSeams {
            after_transfer: Some(place),
            ..SshSeams::default()
        };
        let moved = far
            .remote_with_seams(seams)
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect("put");
        assert!(moved.already_present);
        assert_eq!(moved.bytes, 0);
        assert!(!far_file(&far, ".pack.laptop.part").exists());
    }

    #[test]
    fn a_name_taken_during_the_upload_by_other_bytes_is_a_conflict() {
        let far = Far::new();
        let bytes = noise(20_000, 11);
        let source = far.local("pack.tar", &bytes);
        let place = {
            let target = far_file(&far, "pack.tar");
            std::sync::Arc::new(move || std::fs::write(&target, "rival").expect("rival")) as Hook
        };
        let seams = SshSeams {
            after_transfer: Some(place),
            ..SshSeams::default()
        };
        let error = far
            .remote_with_seams(seams)
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect_err("conflict");
        assert!(
            matches!(error, RemoteError::PayloadConflict { .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read(far_file(&far, "pack.tar")).expect("rival"),
            b"rival"
        );
        assert!(far_file(&far, ".pack.laptop.part").exists());
    }

    #[test]
    fn the_far_side_syncs_before_and_after_the_final_name_appears() {
        let far = Far::new();
        let bytes = noise(20_000, 12);
        let source = far.local("pack.tar", &bytes);
        let log = far.state.path().join("sync.log");
        far.wrap(
            "sync",
            &format!(
                "d={dir}\n[ -e \"$d/.pack.laptop.part\" ] && t=temp || t=none\n[ -e \"$d/pack.tar\" ] && f=final || f=none\necho \"$t $f\" >> {log}",
                dir = far_file(&far, "").display(),
                log = log.display()
            ),
        );
        far.remote_with_seams(SshSeams::default())
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect("put");
        assert_eq!(
            std::fs::read_to_string(log).expect("log"),
            "temp none\nnone final\n"
        );
    }

    #[test]
    fn a_put_without_a_resolved_rsync_fails_before_any_ssh_call() {
        let far = Far::new();
        let source = far.local("pack.tar", b"x");
        let remote = far.remote();
        let error = remote
            .put_payload(&upload(&source, &sha_of(b"x")))
            .expect_err("no rsync");
        assert!(matches!(error, RemoteError::Rsync(_)), "{error}");
        assert_eq!(far.commands(), 0);
        far.object_dir(OBJECT);
        remote.list_dir(object()).expect("listing still works");
    }

    #[test]
    fn rsync_gets_a_relative_source_with_a_dot_slash_and_never_deletes() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        let remote = far.remote_with(&fake, SshSeams::default());
        let _ = remote.put_payload(&upload(Path::new("Cargo.toml"), &sha_of(b"x")));
        let calls = Far::fake_calls(&fake);
        let call = calls
            .iter()
            .find(|call| call[0] == "transfer")
            .expect("a transfer");
        assert!(call.contains(&"./Cargo.toml".to_string()), "{call:?}");
        assert!(
            call.last()
                .expect("destination")
                .ends_with("/.pack.laptop.part"),
            "{call:?}"
        );
        assert!(
            call.contains(&"--partial-dir=.antiq-partial".to_string()),
            "{call:?}"
        );
        for forbidden in ["--delete", "--inplace", "--append", "--remove-source-files"] {
            assert!(
                !call.iter().any(|arg| arg.starts_with(forbidden)),
                "{forbidden}"
            );
        }
    }

    #[test]
    fn a_seeded_upload_has_no_partial_directory() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        let remote = far.remote_with(&fake, SshSeams::default());
        let sha = sha_of(b"x");
        let with_seed = Upload {
            seed: Some("live.old"),
            ..upload(Path::new("Cargo.toml"), &sha)
        };
        let _ = remote.put_payload(&with_seed);
        let calls = Far::fake_calls(&fake);
        let call = calls
            .iter()
            .find(|call| call[0] == "transfer")
            .expect("a transfer");
        assert!(
            !call.iter().any(|arg| arg.starts_with("--partial-dir")),
            "{call:?}"
        );
    }

    // ----- get -----

    #[test]
    fn a_get_fetches_the_file_and_rewrites_an_existing_destination() {
        let far = Far::new();
        let bytes = noise(80_000, 13);
        std::fs::write(far_file(&far, "pack.tar"), &bytes).expect("remote file");
        let to = far.local("fetched.tar", b"stale and longer than nothing");
        let moved = far
            .remote_with_seams(SshSeams::default())
            .get_payload(object(), "pack.tar", &to)
            .expect("get");
        assert_eq!(std::fs::read(&to).expect("fetched"), bytes);
        assert!(moved.bytes > 0);
        assert!(!moved.already_present);
    }

    #[test]
    fn a_get_of_a_file_that_is_not_there_runs_no_transfer() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        far.object_dir(OBJECT);
        let to = far.state.path().join("fetched.tar");
        let error = far
            .remote_with(&fake, SshSeams::default())
            .get_payload(object(), "nothing.tar", &to)
            .expect_err("missing");
        assert!(
            matches!(error, RemoteError::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound),
            "{error}"
        );
        assert_eq!(transfers(&Far::fake_calls(&fake)), 0);
    }

    #[test]
    fn a_file_that_vanishes_before_the_get_starts_is_a_failed_transfer() {
        let far = Far::new();
        let file = far_file(&far, "pack.tar");
        std::fs::write(&file, "x").expect("remote file");
        let vanish =
            std::sync::Arc::new(move || std::fs::remove_file(&file).expect("removed")) as Hook;
        let seams = SshSeams {
            before_transfer: Some(vanish),
            ..SshSeams::default()
        };
        let to = far.state.path().join("fetched.tar");
        let error = far
            .remote_with_seams(seams)
            .get_payload(object(), "pack.tar", &to)
            .expect_err("vanished");
        assert!(matches!(error, RemoteError::Transfer(_)), "{error}");
    }

    #[test]
    fn a_get_refuses_names_that_reach_outside_the_directory() {
        let far = Far::new();
        let to = far.state.path().join("fetched.tar");
        let error = far
            .remote_with_seams(SshSeams::default())
            .get_payload(object(), "../x", &to)
            .expect_err("name");
        assert!(matches!(error, RemoteError::InvalidName { .. }), "{error}");
        assert_eq!(far.commands(), 0);
    }

    // ----- failures of rsync -----

    fn failed(code: Option<i32>, stderr: &str) -> crate::remote::rsync::RsyncError {
        crate::remote::rsync::RsyncError::Failed {
            code,
            stderr: stderr.into(),
        }
    }

    const CONNECT_REFUSED: &str =
        include_str!("../../../tests/fixtures/ssh-texts/connect-refused.txt");
    const PERMISSION_DENIED: &str =
        include_str!("../../../tests/fixtures/ssh-texts/permission-denied.txt");
    const KEEPALIVE: &str = include_str!("../../../tests/fixtures/ssh-texts/keepalive-timeout.txt");

    #[test]
    fn rsync_255_with_an_away_line_is_away() {
        let error = classify_rsync_failure(failed(Some(255), CONNECT_REFUSED), "");
        assert!(matches!(error, RemoteError::Away { .. }), "{error}");
    }

    #[test]
    fn rsync_255_with_a_broken_line_is_broken() {
        let error = classify_rsync_failure(failed(Some(255), PERMISSION_DENIED), "");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
    }

    #[test]
    fn rsync_255_with_a_keepalive_drop_in_the_master_log_is_away() {
        let error = classify_rsync_failure(
            failed(Some(255), "rsync: connection unexpectedly closed"),
            KEEPALIVE,
        );
        assert!(matches!(error, RemoteError::Away { .. }), "{error}");
    }

    #[test]
    fn rsync_255_without_a_recognised_line_is_broken() {
        let error = classify_rsync_failure(
            failed(Some(255), "rsync: connection unexpectedly closed"),
            "",
        );
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
    }

    #[test]
    fn rsync_12_with_a_decided_line_follows_the_line() {
        let error = classify_rsync_failure(failed(Some(12), CONNECT_REFUSED), "");
        assert!(matches!(error, RemoteError::Away { .. }), "{error}");
        let error = classify_rsync_failure(failed(Some(12), PERMISSION_DENIED), "");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
    }

    #[test]
    fn rsync_12_without_a_decided_line_is_a_failed_transfer() {
        let error = classify_rsync_failure(
            failed(Some(12), "rsync error: error in rsync protocol data stream"),
            "",
        );
        assert!(matches!(error, RemoteError::Transfer(_)), "{error}");
        assert!(error.to_string().contains("exit code 12"), "{error}");
    }

    #[test]
    fn rsync_30_is_a_failed_transfer() {
        let error = classify_rsync_failure(
            failed(Some(30), "rsync error: timeout in data send/receive"),
            "",
        );
        assert!(matches!(error, RemoteError::Transfer(_)), "{error}");
    }

    #[test]
    fn partial_transfers_and_vanished_files_are_failed_transfers_whatever_the_text_says() {
        for code in [23, 24, 11] {
            let error = classify_rsync_failure(failed(Some(code), CONNECT_REFUSED), KEEPALIVE);
            assert!(matches!(error, RemoteError::Transfer(_)), "{code}: {error}");
        }
    }

    #[test]
    fn rsync_killed_by_a_signal_is_a_failed_transfer() {
        let error = classify_rsync_failure(failed(None, ""), KEEPALIVE);
        assert!(matches!(error, RemoteError::Transfer(_)), "{error}");
    }

    #[test]
    fn rsync_that_does_not_start_is_a_failed_transfer() {
        let spawn = crate::remote::rsync::RsyncError::Spawn {
            path: "/nowhere/rsync".into(),
            source: std::io::Error::other("no such file"),
        };
        let error = classify_rsync_failure(spawn, "");
        assert!(matches!(error, RemoteError::Transfer(_)), "{error}");
    }

    #[test]
    fn a_master_that_drops_before_the_transfer_makes_the_put_away() {
        let far = Far::new();
        let bytes = noise(20_000, 14);
        let source = far.local("pack.tar", &bytes);
        let fault = far.state.path().join("fault");
        let drop_master = std::sync::Arc::new(move || {
            std::fs::write(&fault, "master-drop command\n").expect("fault")
        }) as Hook;
        let seams = SshSeams {
            before_transfer: Some(drop_master),
            ..SshSeams::default()
        };
        let remote = far.remote_with_seams(seams);
        remote.begin_phase().expect("phase");
        let error = remote
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect_err("dropped");
        assert!(matches!(error, RemoteError::Away { .. }), "{error}");
        assert!(!far_file(&far, "pack.tar").exists());
    }

    #[test]
    fn a_master_that_drops_after_the_transfer_leaves_the_temp_and_no_final_name() {
        let far = Far::new();
        let bytes = noise(20_000, 15);
        let source = far.local("pack.tar", &bytes);
        let fault = far.state.path().join("fault");
        let drop_master = std::sync::Arc::new(move || {
            std::fs::write(&fault, "master-drop command\n").expect("fault")
        }) as Hook;
        let seams = SshSeams {
            after_transfer: Some(drop_master),
            ..SshSeams::default()
        };
        let remote = far.remote_with_seams(seams);
        remote.begin_phase().expect("phase");
        let error = remote
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect_err("dropped");
        assert!(matches!(error, RemoteError::Away { .. }), "{error}");
        assert!(far_file(&far, ".pack.laptop.part").exists());
        assert!(!far_file(&far, "pack.tar").exists());
    }

    // ----- more transfer cases -----

    #[test]
    fn the_rsync_of_the_machine_is_probed_once() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        let remote = far.remote_with(&fake, SshSeams::default());
        let sha = sha_of(b"x");
        for _ in 0..2 {
            let _ = remote.put_payload(&upload(Path::new("Cargo.toml"), &sha));
        }
        let calls = Far::fake_calls(&fake);
        assert_eq!(calls.iter().filter(|call| call[0] == "version").count(), 1);
        assert_eq!(transfers(&calls), 2);
    }

    #[test]
    fn without_statistics_a_put_counts_the_size_of_the_file() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        let bytes = noise(1_234, 16);
        let source = far.local("pack.tar", &bytes);
        let place = {
            let temp = far_file(&far, ".pack.laptop.part");
            let bytes = bytes.clone();
            std::sync::Arc::new(move || std::fs::write(&temp, &bytes).expect("temp")) as Hook
        };
        let seams = SshSeams {
            after_transfer: Some(place),
            ..SshSeams::default()
        };
        let moved = far
            .remote_with(&fake, seams)
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect("put");
        assert_eq!(moved.bytes, 1_234);
    }

    #[test]
    fn without_statistics_a_get_counts_the_size_of_the_fetched_file() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        std::fs::write(far_file(&far, "pack.tar"), "x").expect("remote file");
        let to = far.local("fetched.tar", &noise(777, 17));
        let moved = far
            .remote_with(&fake, SshSeams::default())
            .get_payload(object(), "pack.tar", &to)
            .expect("get");
        assert_eq!(moved.bytes, 777);
    }

    #[test]
    fn a_final_link_the_far_side_refuses_is_an_io_error_and_keeps_the_temp() {
        let far = Far::new();
        let bytes = noise(5_000, 18);
        let source = far.local("pack.tar", &bytes);
        far.wrap("ln", "echo 'ln: Operation not permitted' >&2; exit 1");
        let error = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect_err("ln fails");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        assert!(
            error.to_string().contains("Operation not permitted"),
            "{error}"
        );
        assert!(far_file(&far, ".pack.laptop.part").exists());
        assert!(!far_file(&far, "pack.tar").exists());
    }

    #[test]
    fn a_name_taken_while_the_final_link_is_made_is_looked_at() {
        let far = Far::new();
        let bytes = noise(5_000, 19);
        let source = far.local("pack.tar", &bytes);
        far.wrap(
            "ln",
            "echo rival > \"$2\"; echo 'ln: File exists' >&2; exit 1",
        );
        let error = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&upload(&source, &sha_of(&bytes)))
            .expect_err("rival");
        assert!(
            matches!(error, RemoteError::PayloadConflict { .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read(far_file(&far, "pack.tar")).expect("rival"),
            b"rival\n"
        );
    }

    #[test]
    fn a_put_of_a_source_that_is_not_there_is_an_io_error_before_any_ssh_call() {
        let far = Far::new();
        let error = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&upload(Path::new("/nonexistent/pack.tar"), &sha_of(b"x")))
            .expect_err("no source");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
        assert_eq!(far.commands(), 0);
    }

    #[test]
    fn a_put_refuses_temp_final_and_seed_names_that_reach_outside_the_directory() {
        let far = Far::new();
        let source = far.local("pack.tar", b"x");
        let sha = sha_of(b"x");
        let remote = far.remote_with_seams(SshSeams::default());
        let bad_temp = Upload::new(object(), "../t", "pack.tar", &source, &sha);
        let bad_final = Upload::new(object(), ".t", "a/b", &source, &sha);
        let bad_seed = Upload {
            seed: Some(".."),
            ..Upload::new(object(), ".t", "pack.tar", &source, &sha)
        };
        for upload in [bad_temp, bad_final, bad_seed] {
            let error = remote.put_payload(&upload).expect_err("name");
            assert!(matches!(error, RemoteError::InvalidName { .. }), "{error}");
        }
        assert_eq!(far.commands(), 0);
    }

    #[test]
    fn what_the_master_logged_before_the_transfer_does_not_tell_how_it_failed() {
        let far = Far::new();
        let source = far.local("pack.tar", &noise(5_000, 20));
        // The NAS side of rsync ends at once without a word.
        let machine = MachineConfig {
            rsync: Some(Far::real_rsync()),
            remote_rsync: Some("/usr/bin/false".into()),
            ..MachineConfig::default()
        };
        let old_news = {
            let log_name = far.state.path().join("master.log");
            std::sync::Arc::new(move || {
                let log = std::fs::read_to_string(&log_name).expect("master log name");
                let mut log = std::fs::OpenOptions::new()
                    .append(true)
                    .open(log.trim())
                    .expect("master log");
                std::io::Write::write_all(&mut log, KEEPALIVE.as_bytes()).expect("logged");
            }) as Hook
        };
        let seams = SshSeams {
            before_transfer: Some(old_news),
            ..SshSeams::default()
        };
        let remote = far.remote_of(machine, seams);
        remote.begin_phase().expect("phase");
        let error = remote
            .put_payload(&upload(&source, &sha_of(b"x")))
            .expect_err("rsync fails");
        assert!(matches!(error, RemoteError::Transfer(_)), "{error}");
    }

    #[test]
    fn a_temp_name_that_is_a_directory_gets_no_seed_link_inside() {
        let far = Far::new();
        std::fs::write(far_file(&far, "live.old"), noise(5_000, 21)).expect("seed");
        let temp = far_file(&far, ".live.laptop.part");
        std::fs::create_dir(&temp).expect("directory");
        let source = far.local("live.new", &noise(5_000, 22));
        let sha = sha_of(b"x");
        let with_seed = Upload {
            seed: Some("live.old"),
            ..Upload::new(object(), ".live.laptop.part", "live.new", &source, &sha)
        };
        let _ = far
            .remote_with_seams(SshSeams::default())
            .put_payload(&with_seed);
        assert!(!temp.join("live.old").exists());
    }

    #[test]
    fn a_get_of_a_name_that_is_a_directory_is_not_found_and_runs_no_transfer() {
        let far = Far::new();
        let (_fake_dir, fake) = fake_rsync();
        std::fs::create_dir(far_file(&far, "dir")).expect("directory");
        let to = far.state.path().join("fetched");
        let error = far
            .remote_with(&fake, SshSeams::default())
            .get_payload(object(), "dir", &to)
            .expect_err("directory");
        assert!(
            matches!(error, RemoteError::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound),
            "{error}"
        );
        assert_eq!(transfers(&Far::fake_calls(&fake)), 0);
    }

    // ----- the tools of the prelude, as the probe of `remote add` uses them -----

    #[test]
    fn without_an_argument_only_the_digest_of_abc_counts() {
        let dir = bare_path(&["sh", "printf"]);
        tool(dir.path(), "sha256sum", &format!("echo {}", "0".repeat(64)));
        tool(dir.path(), "shasum", &format!("echo '{ABC}  -'"));
        let (status, out) = run_with_only(dir.path(), "antiq_sha256 >/dev/null; echo \"$_tool\"");
        assert_eq!((status, out.trim()), (0, "shasum -a 256"));
    }

    #[test]
    fn without_an_argument_and_without_a_matching_tool_the_status_is_42() {
        let dir = bare_path(&["sh", "printf"]);
        tool(dir.path(), "sha256sum", &format!("echo {}", "0".repeat(64)));
        let (status, _) = run_with_only(dir.path(), "antiq_sha256");
        assert_eq!(status, 42);
    }

    #[test]
    fn the_stat_flavour_is_gnu_bsd_or_none() {
        let cases = [
            ("case \"$1\" in -c) exit 0;; *) exit 1;; esac", "gnu"),
            ("case \"$1\" in -f) exit 0;; *) exit 1;; esac", "bsd"),
            ("exit 1", "none"),
        ];
        for (body, flavour) in cases {
            let dir = bare_path(&["sh"]);
            tool(dir.path(), "stat", body);
            let (status, out) = run_with_only(dir.path(), "antiq_stat_flavour; echo \"$_flavour\"");
            assert_eq!((status, out.trim()), (0, flavour));
        }
    }

    // ----- the probe of `remote add` -----

    #[test]
    fn the_probe_sees_the_marker_and_the_tools_of_a_good_root() {
        let far = Far::new();
        let probe = far.remote().probe().expect("a probe");
        assert_eq!(probe.root, RootFound::Dir);
        // The shell hands the marker over without its final newline.
        let file = std::fs::read(far.root().join(marker::FILE_NAME)).expect("the marker");
        assert_eq!(
            probe.marker,
            MarkerFound::Text(file.trim_ascii_end().to_vec())
        );
        assert_eq!(probe.layout_name, None);
        assert!(probe.sha256_tool.is_some());
        assert!(probe.stat.is_some());
        assert!(
            probe
                .rsync
                .as_ref()
                .is_ok_and(|line| line.starts_with("rsync")),
            "{:?}",
            probe.rsync
        );
    }

    #[test]
    fn the_probe_reads_the_rsync_command_from_the_machine() {
        let far = Far::new();
        let machine = MachineConfig {
            remote_rsync: Some("echo fake rsync words".into()),
            ..MachineConfig::default()
        };
        let probe = far
            .remote_of(machine, SshSeams::default())
            .probe()
            .expect("a probe");
        assert_eq!(probe.rsync, Ok("fake rsync words --version".to_string()));
    }

    #[test]
    fn the_probe_keeps_the_output_of_an_rsync_that_fails() {
        let far = Far::new();
        let machine = MachineConfig {
            remote_rsync: Some("/nonexistent/rsync".into()),
            ..MachineConfig::default()
        };
        let probe = far
            .remote_of(machine, SshSeams::default())
            .probe()
            .expect("a probe");
        let why = probe.rsync.expect_err("no rsync there");
        assert!(why.contains("/nonexistent/rsync"), "{why}");
    }

    #[test]
    fn the_probe_names_the_layout_names_a_root_holds() {
        let far = Far::new();
        std::fs::write(far.root().join(".antiq-clock.laptop"), "").expect("clock file");
        let probe = far.remote().probe().expect("a probe");
        assert_eq!(probe.layout_name.as_deref(), Some(".antiq-clock.laptop"));
        std::fs::create_dir(far.root().join("objects")).expect("objects");
        let probe = far.remote().probe().expect("a probe");
        assert_eq!(probe.layout_name.as_deref(), Some("objects"));
    }

    #[test]
    fn the_probe_tells_a_missing_root_with_and_without_its_parent() {
        let far = Far::new();
        let below = far.root().join("a");
        let probe = far.remote_at(&below).probe().expect("a probe");
        assert_eq!(
            probe.root,
            RootFound::Missing {
                parent_exists: true
            }
        );
        let probe = far.remote_at(&below.join("b")).probe().expect("a probe");
        assert_eq!(
            probe.root,
            RootFound::Missing {
                parent_exists: false
            }
        );
        assert_eq!(probe.marker, MarkerFound::NoRoot);
    }

    #[test]
    fn the_probe_tells_a_root_that_is_a_file_from_one_that_is_missing() {
        let far = Far::new();
        let file = far.root().join("plain");
        std::fs::write(&file, "x").expect("a file");
        let probe = far.remote_at(&file).probe().expect("a probe");
        assert!(
            matches!(&probe.root, RootFound::Unusable(why) if why.ends_with("is not a directory")),
            "{:?}",
            probe.root
        );
    }

    #[test]
    fn the_probe_tells_a_marker_that_cannot_be_read_from_none() {
        let far = Far::new();
        std::fs::remove_file(far.root().join(marker::FILE_NAME)).expect("no marker");
        let probe = far.remote().probe().expect("a probe");
        assert_eq!(probe.marker, MarkerFound::Absent);
        std::fs::create_dir(far.root().join(marker::FILE_NAME)).expect("a directory");
        let probe = far.remote().probe().expect("a probe");
        assert_eq!(probe.marker, MarkerFound::Unreadable);
    }

    #[test]
    fn a_probe_answer_ignores_a_kind_it_does_not_know() {
        let answer =
            b"@root 3\ndir\n@future 1\nx\n@rsync 5\nrsync\n@sha256 9\nsha256sum\n@stat 3\ngnu\n";
        let probe = Probe::parse(answer).expect("parses");
        assert_eq!(probe.root, RootFound::Dir);
        assert_eq!(probe.stat, Some("gnu"));
    }

    #[test]
    fn a_probe_answer_that_does_not_describe_the_root_is_malformed() {
        let error = Probe::parse(b"@rsync 5\nrsync\n").expect_err("no root");
        assert!(matches!(error, RemoteError::Broken(_)), "{error}");
    }

    /// An rsync command that ends the shell running the probe, as a
    /// script that is killed would.
    fn killing_machine() -> MachineConfig {
        MachineConfig {
            remote_rsync: Some("kill -9 $$ #".into()),
            ..MachineConfig::default()
        }
    }

    #[test]
    fn a_probe_script_that_dies_is_an_io_error() {
        let far = Far::new();
        let error = far
            .remote_of(killing_machine(), SshSeams::default())
            .probe()
            .expect_err("the script died");
        assert!(
            matches!(
                error,
                RemoteError::Io {
                    action: "probe",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn show_passes_on_an_error_that_is_neither_away_nor_broken() {
        let far = Far::new();
        let error = far
            .remote_of(killing_machine(), SshSeams::default())
            .show()
            .expect_err("the script died");
        assert!(matches!(error, RemoteError::Io { .. }), "{error}");
    }

    #[test]
    fn show_turns_a_refused_login_into_a_broken_remote() {
        let far = Far::new();
        far.fault("permission-denied");
        let (assessment, probed) = far.remote().show().expect("an assessment");
        assert_eq!(assessment.reachability, Reachability::Broken);
        assert!(probed.is_empty());
    }

    // ----- the writes of `remote add` -----

    #[test]
    fn making_a_root_below_a_missing_directory_is_an_io_error() {
        let far = Far::new();
        let error = far
            .remote_at(&far.root().join("a/b"))
            .make_root()
            .expect_err("no parent");
        assert!(
            matches!(
                error,
                RemoteError::Io {
                    action: "create",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_marker_that_appeared_meanwhile_is_not_replaced() {
        let far = Far::new();
        let before = std::fs::read(far.root().join(marker::FILE_NAME)).expect("marker");
        let error = far.remote().write_marker(".marker.tmp").expect_err("taken");
        assert!(
            matches!(&error, Unproven::Remote(RemoteError::Io { source, .. }) if source.to_string().contains("appeared")),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read(far.root().join(marker::FILE_NAME)).expect("marker"),
            before
        );
        assert!(!far.root().join(".marker.tmp").exists());
    }

    #[test]
    fn a_marker_cannot_be_written_into_a_root_that_is_read_only() {
        let far = Far::new();
        std::fs::remove_file(far.root().join(marker::FILE_NAME)).expect("no marker");
        std::fs::set_permissions(far.root(), std::fs::Permissions::from_mode(0o500))
            .expect("read only");
        assert!(
            std::fs::File::create(far.root().join("probe")).is_err(),
            "the precondition: the root cannot be written"
        );
        let error = far
            .remote()
            .write_marker(".marker.tmp")
            .expect_err("denied");
        std::fs::set_permissions(far.root(), std::fs::Permissions::from_mode(0o700))
            .expect("writable again");
        assert!(
            matches!(
                &error,
                Unproven::Remote(RemoteError::Io {
                    action: "write",
                    ..
                })
            ),
            "{error:?}"
        );
        assert_eq!(names_in(far.root()), Vec::<String>::new());
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .expect("listed")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn a_connection_lost_during_the_proof_is_the_error_of_the_connection() {
        let far = Far::new();
        far.fault("connect-refused");
        let error = far.remote().prove_creation(".probe").expect_err("away");
        assert!(
            matches!(&error, Unproven::Remote(RemoteError::Away { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_proof_leaves_no_probe_file_and_no_link() {
        let far = Far::new();
        far.remote().prove_creation(".probe").expect("proven");
        assert_eq!(names_in(far.root()), [marker::FILE_NAME]);
    }
}
