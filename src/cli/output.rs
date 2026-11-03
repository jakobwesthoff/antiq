// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Console and JSON output. Data goes to stdout, messages to stderr.
//!
//! Every function returns an `io::Result`, so a failed write reaches the
//! caller as an error instead of a panic. A closed pipe is the exception:
//! it is what `antiq find | head` ends with, so a stream whose reader is
//! gone silently drops what is written to it from then on.

use antiq::error::ErrorKind;
use serde::Serialize;
use serde_json::{Map, Value};
use std::io::{self, IsTerminal, Write};

/// The format number of the JSON envelope.
const ENVELOPE_FORMAT: u32 = 1;

/// One output stream that stops writing once its reader is gone.
struct Sink {
    writer: Box<dyn Write + Send>,
    closed: bool,
}

impl Sink {
    fn new(writer: Box<dyn Write + Send>) -> Sink {
        Sink {
            writer,
            closed: false,
        }
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let written = writeln!(self.writer, "{line}").and_then(|()| self.writer.flush());
        match written {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(())
            }
            other => other,
        }
    }
}

/// `{"format": 1, "command": ..., <payload keys>}`. The command is `null`
/// when a command line failed to parse before naming one.
#[derive(Serialize)]
struct Envelope<'a, P: Serialize> {
    format: u32,
    command: Option<&'a str>,
    #[serde(flatten)]
    payload: &'a P,
}

#[derive(Serialize)]
struct ErrorPayload<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    kind: &'a str,
    message: &'a str,
    #[serde(flatten)]
    details: &'a Map<String, Value>,
}

/// Where a run writes to.
pub struct Output {
    stdout: Sink,
    stderr: Sink,
    colored: bool,
}

impl Output {
    /// The process's own stdout and stderr. Colour is used when stderr is
    /// a terminal.
    pub fn stdio() -> Output {
        Output::new(
            Box::new(io::stdout()),
            Box::new(io::stderr()),
            io::stderr().is_terminal(),
        )
    }

    pub fn new(
        stdout: Box<dyn Write + Send>,
        stderr: Box<dyn Write + Send>,
        colored: bool,
    ) -> Output {
        Output {
            stdout: Sink::new(stdout),
            stderr: Sink::new(stderr),
            colored,
        }
    }

    /// Writes `payload` as the JSON object of `command` to stdout.
    pub fn json<P: Serialize>(&mut self, command: Option<&str>, payload: &P) -> io::Result<()> {
        let envelope = Envelope {
            format: ENVELOPE_FORMAT,
            command,
            payload,
        };
        let line = serde_json::to_string(&envelope)
            .expect("an envelope of plain data and string keys always serialises");
        self.stdout.write_line(&line)
    }

    /// Writes the error object of a failed run to stdout.
    pub fn json_error(
        &mut self,
        command: Option<&str>,
        kind: ErrorKind,
        message: &str,
        details: &Map<String, Value>,
    ) -> io::Result<()> {
        let payload = ErrorPayload {
            error: ErrorBody {
                kind: kind.as_str(),
                message,
                details,
            },
        };
        self.json(command, &payload)
    }

    /// Writes what went wrong, then what the run was doing, to stderr.
    pub fn plain_error(&mut self, message: &str, contexts: &[String]) -> io::Result<()> {
        let label = if self.colored {
            "\x1b[1;31merror:\x1b[0m"
        } else {
            "error:"
        };
        self.stderr.write_line(&format!("{label} {message}"))?;
        for context in contexts {
            self.stderr
                .write_line(&format!("  while attempting to {context}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod testing {
    use super::Output;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    /// Output that lands in memory.
    #[derive(Clone, Default)]
    pub struct Capture {
        stdout: Arc<Mutex<Vec<u8>>>,
        stderr: Arc<Mutex<Vec<u8>>>,
    }

    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("no test panics while writing")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        pub fn output(&self, colored: bool) -> Output {
            Output::new(
                Box::new(Buffer(self.stdout.clone())),
                Box::new(Buffer(self.stderr.clone())),
                colored,
            )
        }

        pub fn stdout(&self) -> String {
            text(&self.stdout)
        }

        pub fn stderr(&self) -> String {
            text(&self.stderr)
        }
    }

    fn text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buffer.lock().expect("no test panics while reading").clone())
            .expect("the tests write UTF-8")
    }

    /// A stream that takes every write and fails every flush with `kind`.
    /// With `BrokenPipe` it is a stream whose reader has gone away. The
    /// counter says how often a flush was attempted.
    pub struct FailsOnFlush {
        kind: io::ErrorKind,
        flushes: Arc<Mutex<usize>>,
    }

    impl FailsOnFlush {
        pub fn new(kind: io::ErrorKind) -> (FailsOnFlush, Arc<Mutex<usize>>) {
            let flushes = Arc::new(Mutex::new(0));
            let stream = FailsOnFlush {
                kind,
                flushes: flushes.clone(),
            };
            (stream, flushes)
        }
    }

    impl Write for FailsOnFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            *self.flushes.lock().expect("no test panics while counting") += 1;
            Err(self.kind.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Capture, FailsOnFlush};
    use super::*;
    use serde_json::json;

    #[test]
    fn an_envelope_starts_with_format_and_command_then_the_payload() {
        let capture = Capture::default();
        let mut out = capture.output(false);
        out.json(Some("status"), &json!({"pieces": [], "remote": "none"}))
            .expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "{\"format\":1,\"command\":\"status\",\"pieces\":[],\"remote\":\"none\"}\n"
        );
    }

    #[test]
    fn an_envelope_without_a_command_says_null() {
        let capture = Capture::default();
        capture
            .output(false)
            .json(None, &json!({}))
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "{\"format\":1,\"command\":null}\n");
    }

    #[test]
    fn an_error_object_carries_kind_message_and_details() {
        let capture = Capture::default();
        let mut details = Map::new();
        details.insert("pid".into(), json!(4242));
        capture
            .output(false)
            .json_error(Some("sync"), ErrorKind::LockHeld, "held", &details)
            .expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "{\"format\":1,\"command\":\"sync\",\"error\":\
             {\"kind\":\"lock_held\",\"message\":\"held\",\"pid\":4242}}\n"
        );
        assert_eq!(capture.stderr(), "");
    }

    #[test]
    fn a_plain_error_lists_what_the_run_was_doing_on_stderr() {
        let capture = Capture::default();
        capture
            .output(false)
            .plain_error(
                "permission denied",
                &["read the config file".into(), "start the run".into()],
            )
            .expect("memory accepts writes");
        assert_eq!(
            capture.stderr(),
            "error: permission denied\n  \
             while attempting to read the config file\n  \
             while attempting to start the run\n"
        );
        assert_eq!(capture.stdout(), "");
    }

    #[test]
    fn colour_changes_only_the_label() {
        let capture = Capture::default();
        capture
            .output(true)
            .plain_error("broken", &[])
            .expect("memory accepts writes");
        assert_eq!(capture.stderr(), "\x1b[1;31merror:\x1b[0m broken\n");
    }

    #[test]
    fn a_closed_pipe_drops_the_output_without_an_error() {
        let (pipe, flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(pipe), Box::new(Vec::new()), false);
        out.json(Some("find"), &json!({}))
            .expect("a closed pipe is no error");
        out.json(Some("find"), &json!({}))
            .expect("a closed pipe is no error");
        assert_eq!(
            *flushes.lock().expect("no test panics while counting"),
            1,
            "nothing is written after the pipe closed"
        );
    }

    #[test]
    fn another_write_failure_is_an_error() {
        let (disk, _) = FailsOnFlush::new(io::ErrorKind::StorageFull);
        let mut out = Output::new(Box::new(disk), Box::new(Vec::new()), false);
        let error = out
            .json(Some("find"), &json!({}))
            .expect_err("a full disk is reported");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }
}
