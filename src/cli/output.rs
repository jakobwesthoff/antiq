// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Console and JSON output. Data goes to stdout, messages to stderr.
//!
//! Every function returns an `io::Result`, so a failed write reaches the
//! caller as an error instead of a panic. A closed pipe is the exception:
//! it is what `antiq status | head` ends with, so a stream whose reader is
//! gone silently drops what is written to it from then on.

use antiq::error::ErrorKind;
use serde::Serialize;
use serde_json::{Map, Value};
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;

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
        self.write_line_bytes(line.as_bytes())
    }

    fn write_line_bytes(&mut self, line: &[u8]) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let written = self
            .writer
            .write_all(line)
            .and_then(|()| self.writer.write_all(b"\n"))
            .and_then(|()| self.writer.flush());
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

/// The width a console is laid out for when nothing says otherwise.
const DEFAULT_WIDTH: usize = 80;

/// How data on stdout is laid out and coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub width: usize,
    pub color: bool,
    /// Whether the stream is a terminal. Output that differs between a
    /// terminal and a pipe asks this, not `color`, which `NO_COLOR` turns
    /// off on a terminal too.
    pub terminal: bool,
}

impl Style {
    /// The style for a stream. `columns` is what the terminal reports, if
    /// the stream is one. Without a terminal the width is 80 and nothing
    /// is coloured; a terminal that reports no columns gets width 80. A
    /// non-empty `NO_COLOR` turns colour off (`no_color`).
    pub fn for_terminal(columns: Option<u16>, no_color: bool, is_tty: bool) -> Style {
        if !is_tty {
            return Style {
                width: DEFAULT_WIDTH,
                color: false,
                terminal: false,
            };
        }
        Style {
            width: columns
                .filter(|columns| *columns > 0)
                .map_or(DEFAULT_WIDTH, usize::from),
            color: !no_color,
            terminal: true,
        }
    }

    /// The style of `stream`, asking the environment for `NO_COLOR`.
    pub fn of(stream: impl AsFd) -> Style {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let is_tty = rustix::termios::isatty(&stream);
        let columns = if is_tty {
            terminal_columns(&stream)
        } else {
            None
        };
        Style::for_terminal(columns, no_color, is_tty)
    }
}

/// The columns a terminal reports for itself, `None` when the stream is
/// no terminal or reports none.
pub fn terminal_columns(stream: impl AsFd) -> Option<u16> {
    let size = rustix::termios::tcgetwinsize(stream).ok()?;
    (size.ws_col > 0).then_some(size.ws_col)
}

/// Where a run writes to.
pub struct Output {
    stdout: Sink,
    stderr: Sink,
    colored: bool,
    stdout_style: Style,
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
        .with_stdout_style(Style::of(io::stdout()))
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
            stdout_style: Style {
                width: DEFAULT_WIDTH,
                color: false,
                terminal: false,
            },
        }
    }

    /// The same output with `style` for what goes to stdout.
    pub fn with_stdout_style(mut self, style: Style) -> Output {
        self.stdout_style = style;
        self
    }

    /// How data on stdout is laid out and coloured.
    pub fn stdout_style(&self) -> Style {
        self.stdout_style
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

    /// Writes a line of a command's result to stdout.
    #[allow(dead_code, reason = "the commands print through it as they are built")]
    pub fn line(&mut self, text: &str) -> io::Result<()> {
        self.stdout.write_line(text)
    }

    /// Writes a line of a command's result to stdout as the bytes it is.
    /// For output that a shell reads back, such as a path.
    pub fn line_os(&mut self, text: &OsStr) -> io::Result<()> {
        self.stdout.write_line_bytes(text.as_bytes())
    }

    /// Writes a line of a message to the user to stderr.
    #[allow(dead_code, reason = "the commands print through it as they are built")]
    pub fn note(&mut self, text: &str) -> io::Result<()> {
        self.stderr.write_line(text)
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

        /// What was written to stdout, whether it is UTF-8 or not.
        pub fn stdout_bytes(&self) -> Vec<u8> {
            self.stdout
                .lock()
                .expect("no test panics while reading")
                .clone()
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
            .json_error(Some("archive"), ErrorKind::LockHeld, "held", &details)
            .expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "{\"format\":1,\"command\":\"archive\",\"error\":\
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
        out.json(Some("status"), &json!({}))
            .expect("a closed pipe is no error");
        out.json(Some("status"), &json!({}))
            .expect("a closed pipe is no error");
        assert_eq!(
            *flushes.lock().expect("no test panics while counting"),
            1,
            "nothing is written after the pipe closed"
        );
    }

    #[test]
    fn a_line_goes_to_stdout_and_a_note_to_stderr() {
        let capture = Capture::default();
        let mut out = capture.output(false);
        out.line("Created the collection")
            .expect("memory accepts writes");
        out.note("nothing else was done")
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "Created the collection\n");
        assert_eq!(capture.stderr(), "nothing else was done\n");
    }

    #[test]
    fn a_line_of_bytes_is_written_as_it_is() {
        let capture = Capture::default();
        let mut out = capture.output(false);
        out.line_os(OsStr::from_bytes(b"/work/100% done/a\xff"))
            .expect("memory accepts writes");
        assert_eq!(capture.stdout_bytes(), b"/work/100% done/a\xff\n");
        assert_eq!(capture.stderr(), "");
    }

    #[test]
    fn a_line_of_bytes_to_a_closed_pipe_is_dropped() {
        let (pipe, flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(pipe), Box::new(Vec::new()), false);
        out.line_os(OsStr::new("one"))
            .expect("a closed pipe is no error");
        out.line_os(OsStr::new("two"))
            .expect("a closed pipe is no error");
        assert_eq!(
            *flushes.lock().expect("no test panics while counting"),
            1,
            "nothing is written after the pipe closed"
        );
    }

    #[test]
    fn another_write_failure_of_a_line_of_bytes_is_an_error() {
        let (disk, _) = FailsOnFlush::new(io::ErrorKind::StorageFull);
        let mut out = Output::new(Box::new(disk), Box::new(Vec::new()), false);
        let error = out
            .line_os(OsStr::new("one"))
            .expect_err("a full disk is reported");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }

    #[test]
    fn a_line_to_a_closed_pipe_is_dropped() {
        let (pipe, flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(pipe), Box::new(Vec::new()), false);
        out.line("one").expect("a closed pipe is no error");
        out.line("two").expect("a closed pipe is no error");
        assert_eq!(
            *flushes.lock().expect("no test panics while counting"),
            1,
            "nothing is written after the pipe closed"
        );
    }

    #[test]
    fn a_note_to_a_closed_pipe_is_dropped() {
        let (pipe, flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(Vec::new()), Box::new(pipe), false);
        out.note("one").expect("a closed pipe is no error");
        out.note("two").expect("a closed pipe is no error");
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
            .json(Some("status"), &json!({}))
            .expect_err("a full disk is reported");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }

    #[test]
    fn without_a_terminal_the_width_is_80_and_nothing_is_coloured() {
        assert_eq!(
            Style::for_terminal(None, false, false),
            Style {
                width: 80,
                color: false,
                terminal: false
            }
        );
        // A size reported for a stream that is no terminal does not count.
        assert_eq!(
            Style::for_terminal(Some(200), false, false),
            Style {
                width: 80,
                color: false,
                terminal: false
            }
        );
    }

    #[test]
    fn a_terminal_gets_its_width_and_colour() {
        assert_eq!(
            Style::for_terminal(Some(120), false, true),
            Style {
                width: 120,
                color: true,
                terminal: true
            }
        );
    }

    #[test]
    fn a_terminal_that_reports_no_columns_gets_80() {
        assert_eq!(
            Style::for_terminal(Some(0), false, true),
            Style {
                width: 80,
                color: true,
                terminal: true
            }
        );
        assert_eq!(Style::for_terminal(None, false, true).width, 80);
    }

    #[test]
    fn no_color_turns_colour_off_on_a_terminal() {
        assert_eq!(
            Style::for_terminal(Some(100), true, true),
            Style {
                width: 100,
                color: false,
                terminal: true
            }
        );
    }

    /// A pseudo terminal whose window has `columns` columns.
    struct Pty {
        master: std::os::fd::OwnedFd,
        slave: std::os::fd::OwnedFd,
    }

    fn pty(columns: u16) -> Pty {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize {
            ws_row: 24,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: both descriptors are fresh results of `openpty`, owned
        // from here on, and the null pointers ask for no name or termios.
        let (master, slave) = unsafe {
            let made = libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &size as *const libc::winsize as *mut libc::winsize,
            );
            assert_eq!(made, 0, "a pseudo terminal can be opened");
            (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
        };
        Pty { master, slave }
    }

    #[test]
    fn a_terminal_reports_the_columns_of_its_window() {
        let pty = pty(132);
        assert_eq!(terminal_columns(&pty.slave), Some(132));
        let style = Style::of(&pty.slave);
        assert_eq!(style.width, 132);
        drop(pty.master);
    }

    #[test]
    fn a_window_of_zero_columns_is_unknown() {
        let pty = pty(0);
        assert_eq!(terminal_columns(&pty.slave), None);
        assert_eq!(Style::of(&pty.slave).width, 80);
        drop(pty.master);
    }

    #[test]
    fn a_pipe_and_dev_null_are_no_terminal() {
        let (reader, _writer) = io::pipe().expect("a pipe can be made");
        assert_eq!(terminal_columns(&reader), None);
        assert_eq!(
            Style::of(&reader),
            Style {
                width: 80,
                color: false,
                terminal: false
            }
        );
        let null = std::fs::File::open("/dev/null").expect("/dev/null opens");
        assert_eq!(terminal_columns(&null), None);
    }

    #[test]
    fn output_carries_the_style_of_its_stdout() {
        let capture = Capture::default();
        let style = Style {
            width: 120,
            color: true,
            terminal: true,
        };
        let out = capture.output(false).with_stdout_style(style);
        assert_eq!(out.stdout_style(), style);
        assert_eq!(capture.output(false).stdout_style().width, 80);
    }
}
