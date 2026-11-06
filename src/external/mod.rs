// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Running external programs: one runner that streams what a program
//! prints to the caller while it runs, writes all of it into the log of the
//! run, and returns what the program left.

pub mod log;
pub mod summary;

use std::borrow::Cow;
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub use log::{Logs, RunLog};

/// What [`run`] hands the segments of the standard output to: it returns
/// what the standard output of the outcome keeps of a segment.
pub type Segments<'a> = dyn FnMut(&[u8]) -> Option<Cow<[u8]>> + 'a;

/// What a finished call left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The exit status; `None` when a signal ended the program.
    pub code: Option<i32>,
    /// The signal that ended the program, if one did.
    pub signal: Option<i32>,
    pub duration: Duration,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The log of the run, when it is being written.
    pub log: Option<PathBuf>,
}

impl Outcome {
    /// An outcome that took no time and has no log.
    pub fn finished(
        code: Option<i32>,
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
    ) -> Outcome {
        Outcome {
            code,
            signal: None,
            duration: Duration::ZERO,
            stdout: stdout.into(),
            stderr: stderr.into(),
            log: None,
        }
    }
}

/// Runs `command` to its end and returns what it left, whatever the exit
/// status.
///
/// The standard output goes to `on_segment` while the program still runs. A
/// segment ends after a newline, which it keeps, or before a carriage
/// return, which starts the next segment: the progress updates a program
/// writes begin with one, so each reaches `on_segment` as soon as the update
/// after it starts, also in the middle of a large file. What `on_segment`
/// returns is appended to the standard output of the outcome, `None` leaves
/// the segment out.
///
/// Standard error is read on a thread of its own: a full pipe would
/// otherwise stop the program while this one waits for stdout.
///
/// Everything is written to `log`, a line at a time, each stamped with the
/// time its first byte arrived. A segment waits for its end before it is
/// logged, so it can be written after a later line of the other stream.
pub fn run(mut command: Command, log: &RunLog, on_segment: &mut Segments) -> io::Result<Outcome> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let started = Instant::now();
    let call = log.start_call(&command_line(&command));
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr was piped"));
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));

    let (stdout_bytes, stderr_bytes, read_failure) = std::thread::scope(|scope| {
        let drain = scope.spawn(|| {
            let mut bytes = Vec::new();
            let mut line = Vec::new();
            let failure = loop {
                line.clear();
                match stderr.read_until(b'\n', &mut line) {
                    Ok(0) => break None,
                    Ok(_) => {
                        call.stderr(Instant::now(), &line);
                        bytes.extend_from_slice(&line);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => break Some(error),
                }
            };
            (bytes, failure)
        });

        let mut bytes = Vec::new();
        let mut segment: Vec<u8> = Vec::new();
        let mut first_byte: Option<Instant> = None;
        let mut failure = None;
        let mut hand_over =
            |segment: &mut Vec<u8>, first_byte: &mut Option<Instant>, bytes: &mut Vec<u8>| {
                let at = first_byte.take().unwrap_or_else(Instant::now);
                if let Some(kept) = on_segment(segment) {
                    bytes.extend_from_slice(&kept);
                }
                call.stdout(at, segment);
                segment.clear();
            };
        loop {
            let buffer = match stdout.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    failure = Some(error);
                    // The program blocks on the pipe nobody reads, and so
                    // would the join of the stderr thread below.
                    let _ = child.kill();
                    break;
                }
            };
            if buffer.is_empty() {
                break;
            }
            first_byte.get_or_insert_with(Instant::now);
            // Up to the next byte that ends or starts a segment.
            let at = buffer.iter().position(|byte| matches!(byte, b'\n' | b'\r'));
            match at {
                None => {
                    segment.extend_from_slice(buffer);
                    let length = buffer.len();
                    stdout.consume(length);
                }
                Some(at) if buffer[at] == b'\n' => {
                    segment.extend_from_slice(&buffer[..=at]);
                    stdout.consume(at + 1);
                    hand_over(&mut segment, &mut first_byte, &mut bytes);
                }
                Some(at) => {
                    segment.extend_from_slice(&buffer[..at]);
                    stdout.consume(at + 1);
                    if !segment.is_empty() {
                        hand_over(&mut segment, &mut first_byte, &mut bytes);
                    }
                    segment.push(b'\r');
                    first_byte = Some(Instant::now());
                }
            }
        }
        if !segment.is_empty() {
            hand_over(&mut segment, &mut first_byte, &mut bytes);
        }
        let (err_bytes, err_read) = drain.join().expect("the stderr reader does not panic");
        (bytes, err_bytes, failure.or(err_read))
    });
    if let Some(cause) = read_failure {
        let _ = child.kill();
        let _ = child.wait();
        return Err(cause);
    }
    let status = child.wait()?;
    let duration = started.elapsed();
    call.end(status.code(), status.signal(), duration);
    Ok(Outcome {
        code: status.code(),
        signal: status.signal(),
        duration,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        log: log.path(),
    })
}

/// The command as a person would type it, for the log.
fn command_line(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|word| shell_quote(&word.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `word` in single quotes unless it is plain, so that a shell reads it as
/// one word.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::log::tests::read_log;
    use super::*;
    use std::cell::Cell;
    use std::fs;

    fn script(text: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(text);
        command
    }

    fn keep_all(segment: &[u8]) -> Option<Cow<'_, [u8]>> {
        Some(segment.into())
    }

    fn logged(root: &std::path::Path) -> RunLog {
        Logs::at(root).open(
            "push",
            std::path::Path::new("/nonexistent/c"),
            "2026-11-06T20:14:03Z".parse().expect("a timestamp"),
        )
    }

    /// The offset in seconds of the first logged line of `stream` that
    /// holds `text`.
    fn offset(log: &str, stream: &str, text: &str) -> f64 {
        let line = log
            .lines()
            .find(|line| line.contains(&format!("s {stream} ")) && line.contains(text))
            .unwrap_or_else(|| panic!("no {stream} line with {text:?} in\n{log}"));
        line[1..line.find('s').expect("an offset")]
            .parse()
            .expect("a number")
    }

    #[test]
    fn what_the_program_prints_reaches_the_caller_before_the_program_ends() {
        let root = tempfile::tempdir().expect("a temp dir");
        let flag = root.path().join("seen");
        // The program ends only once the caller has seen its first line, and
        // gives up after five seconds so that a broken runner fails the
        // test instead of hanging it.
        let command = script(&format!(
            "echo first; i=0; while [ ! -e '{0}' ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done; [ -e '{0}' ]",
            flag.display()
        ));

        let outcome = run(command, &RunLog::discard(), &mut |segment| {
            if segment == b"first\n" {
                fs::write(&flag, "").expect("writable");
            }
            Some(segment.into())
        })
        .expect("the program runs");

        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"first\n");
    }

    #[test]
    fn a_carriage_return_starts_a_new_segment() {
        let segments = std::cell::RefCell::new(Vec::new());
        run(
            script("printf 'a\\rb\\rc\\n'"),
            &RunLog::discard(),
            &mut |segment| {
                segments.borrow_mut().push(segment.to_vec());
                None
            },
        )
        .expect("the program runs");
        assert_eq!(
            segments.into_inner(),
            [b"a".to_vec(), b"\rb".to_vec(), b"\rc\n".to_vec()]
        );
    }

    #[test]
    fn the_caller_decides_what_the_outcome_keeps_of_stdout() {
        let outcome = run(
            script("echo keep; echo drop"),
            &RunLog::discard(),
            &mut |segment| (segment == b"keep\n").then(|| segment.into()),
        )
        .expect("the program runs");
        assert_eq!(outcome.stdout, b"keep\n");
    }

    #[test]
    fn both_streams_land_in_the_log_with_the_end_of_the_call() {
        let root = tempfile::tempdir().expect("a temp dir");
        let log = logged(root.path());

        let outcome = run(
            script("echo to-out; echo to-err >&2; exit 23"),
            &log,
            &mut keep_all,
        )
        .expect("the program runs");

        assert_eq!(outcome.code, Some(23));
        assert_eq!(outcome.stderr, b"to-err\n");
        let path = outcome.log.expect("a log was written");
        drop(log);
        let text = read_log(&path);
        assert!(
            text.contains("# command: sh -c 'echo to-out; echo to-err >&2; exit 23'"),
            "{text}"
        );
        assert!(text.contains("s out to-out\n"), "{text}");
        assert!(text.contains("s err to-err\n"), "{text}");
        assert!(text.contains("# exit 23\n# duration: "), "{text}");
    }

    #[test]
    fn a_signal_is_reported_and_logged() {
        let root = tempfile::tempdir().expect("a temp dir");
        let log = logged(root.path());

        let outcome = run(script("kill -KILL $$"), &log, &mut keep_all).expect("it runs");

        assert_eq!((outcome.code, outcome.signal), (None, Some(9)));
        let path = log.path().expect("a path");
        drop(log);
        assert!(read_log(&path).contains("# signal 9\n"));
    }

    #[test]
    fn a_progress_update_is_stamped_with_its_own_time_not_the_time_of_the_next_output() {
        let root = tempfile::tempdir().expect("a temp dir");
        let log = logged(root.path());

        run(
            script("printf '\\rA'; sleep 0.3; echo E >&2"),
            &log,
            &mut keep_all,
        )
        .expect("the program runs");

        let path = log.path().expect("a path");
        drop(log);
        let text = read_log(&path);
        assert!(
            offset(&text, "out", "A") < offset(&text, "err", "E"),
            "{text}"
        );
    }

    #[test]
    fn a_program_that_does_not_start_is_an_error_and_leaves_no_log() {
        let root = tempfile::tempdir().expect("a temp dir");
        let log = logged(root.path());

        let result = run(Command::new("/nonexistent/program"), &log, &mut keep_all);

        assert!(result.is_err());
        assert_eq!(log.path(), None);
        assert!(!root.path().join("anything").exists());
        assert_eq!(fs::read_dir(root.path()).expect("a dir").count(), 0);
    }

    #[test]
    fn a_discarded_log_leaves_the_outcome_without_a_path() {
        let outcome = run(script("true"), &RunLog::discard(), &mut keep_all).expect("it runs");
        assert_eq!(outcome.log, None);
    }

    #[test]
    fn a_word_with_a_quote_or_a_space_is_quoted_in_the_log() {
        assert_eq!(shell_quote("--exclude=/a"), "--exclude=/a");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn the_stderr_lines_all_reach_the_outcome_when_stdout_is_idle() {
        let calls = Cell::new(0);
        let outcome = run(
            script("for i in 1 2 3; do echo \"line $i\" >&2; done"),
            &RunLog::discard(),
            &mut |_| {
                calls.set(calls.get() + 1);
                None
            },
        )
        .expect("the program runs");
        assert_eq!(outcome.stderr, b"line 1\nline 2\nline 3\n");
        assert_eq!(calls.get(), 0);
    }
}
