// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The log of a run: everything the external programs of one antiq run
//! printed, with the time each line arrived.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use sha2::{Digest, Sha256};
use zstd::stream::write::Encoder;

/// How many logs of one collection are kept.
pub const KEPT: usize = 10;

/// The ending of a log file: zstd-compressed text, which a person reads
/// with `zstd -dc <path> | less`.
pub const SUFFIX: &str = ".log.zst";

/// How long output may wait in the compressor. A run that is killed loses at
/// most this much of its stdout; stderr lines and the ends of calls are
/// flushed at once.
const FLUSH_AFTER: Duration = Duration::from_secs(1);

/// How many characters of the collection's folder name go into the name of
/// its log directory.
const NAME_LIMIT: usize = 64;

/// Where the logs live: `~/Library/Logs/antiq` on macOS, where Console.app
/// shows them, else `antiq` in the state directory. `None` when the root
/// would not be an absolute path, which would put the logs relative to the
/// working directory, possibly inside a collection.
pub fn log_root(home: Option<&Path>, state_dir: Option<&Path>, macos: bool) -> Option<PathBuf> {
    let base = if macos {
        home?.join("Library").join("Logs")
    } else {
        state_dir?.to_path_buf()
    };
    let root = base.join("antiq");
    root.is_absolute().then_some(root)
}

/// The directory that holds the logs of every collection.
#[derive(Debug, Clone)]
pub struct Logs {
    /// The root, or why there is none.
    root: Result<PathBuf, String>,
}

impl Logs {
    /// The platform's log directory.
    pub fn platform() -> Logs {
        let dirs = directories::BaseDirs::new();
        let root = log_root(
            dirs.as_ref().map(|dirs| dirs.home_dir()),
            dirs.as_ref().and_then(|dirs| dirs.state_dir()),
            cfg!(target_os = "macos"),
        );
        Logs {
            root: root
                .ok_or_else(|| "there is no absolute home directory to keep logs in".to_string()),
        }
    }

    /// The logs below `root`.
    pub fn at(root: impl Into<PathBuf>) -> Logs {
        Logs {
            root: Ok(root.into()),
        }
    }

    /// The log of one run of `command` on the collection at
    /// `collection_root`. No file exists before the first external program
    /// starts.
    pub fn open(&self, command: &str, collection_root: &Path, now: Timestamp) -> RunLog {
        let state = match &self.root {
            Ok(root) => State {
                target: Some(Target {
                    directory: root.join(directory_name(collection_root)),
                    command: command.to_string(),
                    now,
                }),
                file: None,
                last_flush: Instant::now(),
                path: None,
                warning: None,
            },
            Err(reason) => State::unwritten(Some(format!("no log was written: {reason}"))),
        };
        RunLog {
            state: Mutex::new(state),
        }
    }
}

/// `<folder name>-<8 hex digits of the hash of the canonical path>`, so that
/// a person recognises the collection and two collections that share a
/// folder name do not share a directory.
fn directory_name(collection_root: &Path) -> String {
    let canonical =
        fs::canonicalize(collection_root).unwrap_or_else(|_| collection_root.to_path_buf());
    let folder = canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string());
    let mut name: String = folder
        .chars()
        .map(|character| {
            if character.is_control() {
                '_'
            } else {
                character
            }
        })
        .take(NAME_LIMIT)
        .collect();
    // A leading dot would hide the directory.
    if name.starts_with('.') {
        name.replace_range(0..1, "_");
    }
    let digest = Sha256::digest(canonical.as_os_str().as_bytes());
    let hash: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{name}-{hash}")
}

/// The log of one antiq run.
pub struct RunLog {
    state: Mutex<State>,
}

impl std::fmt::Debug for RunLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunLog")
            .field("path", &self.path())
            .finish()
    }
}

struct State {
    /// Where the file will be created; `None` once it was tried or when
    /// nothing is to be written.
    target: Option<Target>,
    file: Option<Encoder<'static, File>>,
    last_flush: Instant,
    path: Option<PathBuf>,
    warning: Option<String>,
}

#[derive(Debug)]
struct Target {
    directory: PathBuf,
    command: String,
    now: Timestamp,
}

impl State {
    fn unwritten(warning: Option<String>) -> State {
        State {
            target: None,
            file: None,
            last_flush: Instant::now(),
            path: None,
            warning,
        }
    }

    /// Creates the file on first use. A failure turns the log off for the
    /// rest of the run.
    fn ensure_file(&mut self) {
        let Some(target) = self.target.take() else {
            return;
        };
        match create(&target) {
            Ok((file, path)) => match Encoder::new(file, 0) {
                Ok(encoder) => {
                    self.file = Some(encoder);
                    self.path = Some(path);
                }
                Err(error) => {
                    self.warning =
                        Some(format!("no log was written to {}: {error}", path.display()));
                }
            },
            Err(error) => {
                self.warning = Some(format!(
                    "no log was written to {}: {error}",
                    target.directory.display()
                ));
            }
        }
    }

    /// Writes a line. With `urgent` it also leaves the compressor now, so
    /// that a killed run still leaves it behind in a readable file.
    fn write_line(&mut self, text: &str, urgent: bool) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let flush = urgent || self.last_flush.elapsed() >= FLUSH_AFTER;
        let written =
            writeln!(file, "{text}").and_then(|()| if flush { file.flush() } else { Ok(()) });
        if flush {
            self.last_flush = Instant::now();
        }
        if let Err(error) = written {
            self.file = None;
            self.warning = Some(format!(
                "the log stopped at {}: {error}",
                self.path.as_deref().unwrap_or(Path::new("?")).display()
            ));
        }
    }
}

/// The compressed stream gets its end frame when the run is over.
impl Drop for State {
    fn drop(&mut self) {
        if let Some(encoder) = self.file.take() {
            let _ = encoder.finish();
        }
    }
}

impl RunLog {
    /// A log that writes nothing and has nothing to report.
    pub fn discard() -> RunLog {
        RunLog {
            state: Mutex::new(State::unwritten(None)),
        }
    }

    /// The file, once an external program started.
    pub fn path(&self) -> Option<PathBuf> {
        self.lock().path.clone()
    }

    /// Why the log is incomplete or missing, when it is.
    pub fn warning(&self) -> Option<String> {
        self.lock().warning.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("a log writer does not panic")
    }

    /// Starts the record of one call. The file is created here, so a
    /// program that fails to start leaves no trace in it.
    pub fn start_call(&self, command_line: &str) -> CallLog<'_> {
        let mut state = self.lock();
        state.ensure_file();
        state.write_line(
            &format!(
                "\n# command: {command_line}\n# started: {}",
                jiff::Zoned::now()
            ),
            true,
        );
        CallLog {
            log: self,
            began: Instant::now(),
        }
    }
}

/// The record of one call in a [`RunLog`].
pub struct CallLog<'a> {
    log: &'a RunLog,
    began: Instant,
}

impl CallLog<'_> {
    /// A piece of stdout. `at` is when its first byte arrived.
    pub fn stdout(&self, at: Instant, bytes: &[u8]) {
        self.line(at, "out", bytes, false);
    }

    /// A line of stderr.
    pub fn stderr(&self, at: Instant, bytes: &[u8]) {
        self.line(at, "err", bytes, true);
    }

    fn line(&self, at: Instant, stream: &str, bytes: &[u8], urgent: bool) {
        let offset = at.saturating_duration_since(self.began);
        let text = escape(bytes.strip_suffix(b"\n").unwrap_or(bytes));
        self.log.lock().write_line(
            &format!("+{:.3}s {stream} {text}", offset.as_secs_f64()),
            urgent,
        );
    }

    /// The end of the call.
    pub fn end(&self, code: Option<i32>, signal: Option<i32>, duration: Duration) {
        let how = match (code, signal) {
            (Some(code), _) => format!("exit {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "unknown end".to_string(),
        };
        self.log.lock().write_line(
            &format!("# {how}\n# duration: {:.3}s", duration.as_secs_f64()),
            true,
        );
    }
}

/// `bytes` as one line of text: control characters other than the tab are
/// written out, so that a progress line with carriage returns stays one line
/// of the log.
fn escape(bytes: &[u8]) -> String {
    let mut text = String::new();
    for character in String::from_utf8_lossy(bytes).chars() {
        match character {
            '\t' => text.push('\t'),
            '\r' => text.push_str("\\r"),
            '\n' => text.push_str("\\n"),
            character if character.is_control() => {
                text.push_str(&format!("\\x{:02x}", character as u32));
            }
            character => text.push(character),
        }
    }
    text
}

/// Creates the directory (private to the user: the log lists every path of
/// the collection) and a new file, then prunes the directory.
fn create(target: &Target) -> io::Result<(File, PathBuf)> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&target.directory)?;
    let stamp = target
        .now
        .to_zoned(TimeZone::system())
        .strftime("%Y-%m-%dT%H-%M-%S")
        .to_string();
    let mut attempt = 1u32;
    let (file, path) = loop {
        let name = if attempt == 1 {
            format!("{}-{stamp}{SUFFIX}", target.command)
        } else {
            format!("{}-{stamp}-{attempt}{SUFFIX}", target.command)
        };
        let path = target.directory.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => break (file, path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => attempt += 1,
            Err(error) => return Err(error),
        }
    };
    prune(&target.directory);
    Ok((file, path))
}

/// Deletes all but the [`KEPT`] newest logs of the directory. Newest means
/// the modification time, with the name as tie-break: a name carries local
/// time and the command, neither of which orders runs across a time zone
/// change or two commands. A failure to delete leaves the file.
fn prune(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(SUFFIX) {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            metadata
                .is_file()
                .then(|| (metadata.modified().ok(), name, path))
                .and_then(|(modified, name, path)| Some((modified?, name, path)))
        })
        .collect();
    logs.sort_by(|left, right| (&right.0, &right.1).cmp(&(&left.0, &left.1)));
    for (_, _, path) in logs.into_iter().skip(KEPT) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn now() -> Timestamp {
        "2026-11-06T20:14:03Z".parse().expect("a valid timestamp")
    }

    /// The text of a finished log.
    pub(crate) fn read_log(path: &Path) -> String {
        let bytes = fs::read(path).expect("the log is readable");
        String::from_utf8(zstd::decode_all(bytes.as_slice()).expect("a complete frame"))
            .expect("the log is UTF-8")
    }

    fn files(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("the directory exists")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The one directory below `root`, which belongs to the one collection.
    fn only_directory(root: &Path) -> PathBuf {
        let mut names = files(root);
        assert_eq!(names.len(), 1, "{names:?}");
        root.join(names.remove(0))
    }

    fn write_once(logs: &Logs, collection: &Path) -> PathBuf {
        let log = logs.open("push", collection, now());
        log.start_call("rsync --version");
        log.path().expect("the log was created")
    }

    #[test]
    fn macos_keeps_logs_in_the_library_and_other_systems_in_the_state_directory() {
        let home = Path::new("/Users/ann");
        let state = Path::new("/home/ann/.local/state");
        assert_eq!(
            log_root(Some(home), None, true),
            Some(PathBuf::from("/Users/ann/Library/Logs/antiq"))
        );
        assert_eq!(
            log_root(Some(home), Some(state), false),
            Some(PathBuf::from("/home/ann/.local/state/antiq"))
        );
    }

    #[test]
    fn a_missing_or_relative_root_is_no_root() {
        assert_eq!(log_root(None, None, true), None);
        assert_eq!(log_root(Some(Path::new("home")), None, true), None);
        assert_eq!(log_root(None, Some(Path::new("state")), false), None);
    }

    #[test]
    fn a_collection_directory_has_the_folder_name_and_a_hash_of_the_path() {
        let first = directory_name(Path::new("/nonexistent/one/Photos"));
        let second = directory_name(Path::new("/nonexistent/two/Photos"));
        assert!(first.starts_with("Photos-"), "{first}");
        assert!(second.starts_with("Photos-"), "{second}");
        assert_ne!(first, second);
        assert_eq!(first.len(), "Photos-".len() + 8);
    }

    #[test]
    fn a_folder_name_is_kept_but_for_control_characters_a_leading_dot_and_its_length() {
        assert!(directory_name(Path::new("/x/Bücher")).starts_with("Bücher-"));
        assert!(directory_name(Path::new("/x/.hidden")).starts_with("_hidden-"));
        assert!(directory_name(Path::new("/x/a\nb")).starts_with("a_b-"));
        let long = format!("/x/{}", "é".repeat(200));
        assert_eq!(directory_name(Path::new(&long)).chars().count(), 64 + 1 + 8);
        assert!(directory_name(Path::new("/")).starts_with("root-"));
    }

    #[test]
    fn nothing_is_created_before_a_program_starts() {
        let root = tempfile::tempdir().expect("a temp dir");
        let logs = Logs::at(root.path().join("logs"));
        let log = logs.open("push", Path::new("/c"), now());
        assert_eq!(log.path(), None);
        assert!(!root.path().join("logs").exists());
    }

    #[test]
    fn a_log_is_private_to_the_user_and_named_after_the_command_and_the_time() {
        let root = tempfile::tempdir().expect("a temp dir");
        let logs = Logs::at(root.path().join("logs"));
        let path = write_once(&logs, Path::new("/c"));
        let name = path.file_name().expect("a name").to_string_lossy();
        assert!(name.starts_with("push-2026-11-0"), "{name}");
        assert!(name.ends_with(SUFFIX), "{name}");
        let mode = |path: &Path| fs::metadata(path).expect("exists").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("a parent")), 0o700);
    }

    #[test]
    fn two_runs_in_the_same_second_get_two_files() {
        let root = tempfile::tempdir().expect("a temp dir");
        let logs = Logs::at(root.path());
        let first = write_once(&logs, Path::new("/c"));
        let second = write_once(&logs, Path::new("/c"));
        assert_ne!(first, second);
        assert!(
            second.to_string_lossy().ends_with("-2.log.zst"),
            "{second:?}"
        );
    }

    #[test]
    fn the_eleventh_log_deletes_the_oldest_of_its_collection_only() {
        let root = tempfile::tempdir().expect("a temp dir");
        let logs = Logs::at(root.path());
        let other = write_once(&logs, Path::new("/nonexistent/other"));
        let note = other.with_file_name("notes.txt");
        fs::write(&note, "not a log").expect("writable");
        let mut ours = Vec::new();
        for index in 0..KEPT {
            let path = write_once(&logs, Path::new("/nonexistent/ours"));
            // Past times, so that the next real log is the newest.
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(1_600_000_000 + index as i64, 0),
            )
            .expect("settable");
            ours.push(path);
        }
        assert!(ours.iter().all(|path| path.exists()));

        let newest = write_once(&logs, Path::new("/nonexistent/ours"));

        assert!(!ours[0].exists(), "the oldest is deleted");
        assert!(ours[1..].iter().all(|path| path.exists()));
        assert!(newest.exists());
        assert!(other.exists(), "another collection's log stays");
        assert!(note.exists(), "a file that is no log stays");
    }

    #[test]
    fn a_log_that_cannot_be_created_is_a_warning_and_nothing_else() {
        let root = tempfile::tempdir().expect("a temp dir");
        let blocker = root.path().join("blocker");
        fs::write(&blocker, "a file where the directory should go").expect("writable");
        let logs = Logs::at(blocker.join("logs"));
        let log = logs.open("push", Path::new("/c"), now());

        let call = log.start_call("rsync --version");
        call.stdout(Instant::now(), b"text");
        call.end(Some(0), None, Duration::ZERO);

        assert_eq!(log.path(), None);
        let warning = log.warning().expect("a warning");
        assert!(warning.starts_with("no log was written to"), "{warning}");
    }

    #[test]
    fn a_platform_without_a_root_gives_a_disabled_log_with_a_warning() {
        let logs = Logs {
            root: Err("there is no home".into()),
        };
        let log = logs.open("push", Path::new("/c"), now());
        log.start_call("rsync --version");
        assert_eq!(log.path(), None);
        assert_eq!(
            log.warning().as_deref(),
            Some("no log was written: there is no home")
        );
    }

    #[test]
    fn a_discarded_log_writes_nothing_and_reports_nothing() {
        let log = RunLog::discard();
        let call = log.start_call("rsync --version");
        call.end(Some(0), None, Duration::ZERO);
        assert_eq!(log.path(), None);
        assert_eq!(log.warning(), None);
    }

    #[test]
    fn a_run_that_is_killed_leaves_its_flushed_lines_readable() {
        let root = tempfile::tempdir().expect("a temp dir");
        let logs = Logs::at(root.path());
        let log = logs.open("push", Path::new("/c"), now());
        let call = log.start_call("rsync -a");
        call.stderr(Instant::now(), b"rsync: before the kill\n");

        // The log is neither finished nor dropped, as in a killed process.
        let bytes = fs::read(log.path().expect("a path")).expect("readable");
        let mut text = Vec::new();
        let mut decoder = zstd::stream::read::Decoder::new(bytes.as_slice()).expect("a frame");
        let _ = std::io::Read::read_to_end(&mut decoder, &mut text);
        let text = String::from_utf8(text).expect("UTF-8");

        assert!(text.contains("err rsync: before the kill"), "{text}");
    }

    #[test]
    fn a_call_is_recorded_with_its_command_lines_streams_and_end() {
        let root = tempfile::tempdir().expect("a temp dir");
        let logs = Logs::at(root.path());
        let log = logs.open("push", Path::new("/c"), now());
        let call = log.start_call("rsync -a 'a b'");
        call.stdout(Instant::now(), b"\rprogress\n");
        call.stderr(Instant::now(), b"rsync: oops\n");
        call.end(None, Some(9), Duration::from_millis(1500));
        let path = log.path().expect("a path");
        drop(log);

        let text = read_log(&path);
        assert!(
            text.contains("# command: rsync -a 'a b'\n# started: "),
            "{text}"
        );
        assert!(text.contains("s out \\rprogress\n"), "{text}");
        assert!(text.contains("s err rsync: oops\n"), "{text}");
        assert!(text.contains("# signal 9\n# duration: 1.500s\n"), "{text}");
        assert!(text.lines().any(|line| line.starts_with("+0.0")), "{text}");
    }

    #[test]
    fn the_directory_of_a_collection_is_below_the_root() {
        let root = tempfile::tempdir().expect("a temp dir");
        let logs = Logs::at(root.path());
        write_once(&logs, Path::new("/nonexistent/Photos"));
        let directory = only_directory(root.path());
        assert!(
            directory
                .file_name()
                .expect("a name")
                .to_string_lossy()
                .starts_with("Photos-")
        );
    }
}
