// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Console and JSON output. Data goes to stdout, messages to stderr.
//!
//! Every function returns an `io::Result`, so a failed write reaches the
//! caller as an error instead of a panic. A closed pipe is the exception:
//! it is what `antiq status | head` ends with, so a stream whose reader is
//! gone silently drops what is written to it from then on.

use crate::cli::console::block::Block;
use crate::cli::console::dryrun;
use crate::cli::console::env::Env;
use crate::cli::console::render::{Part, render_blocks, render_parts};
use crate::cli::console::symbols::Symbols;
use crate::cli::notice::Notice;
use antiq::error::ErrorKind;
use antiq::shown::{escape_controls, first_raw_character};
use serde::Serialize;
use serde_json::{Map, Value};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

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

    /// Writes a line of text. A name from the user's files reaches it in the
    /// shown form of `antiq::shown`, so a raw control character in `line` is
    /// a value that skipped the rule; a debug build stops there.
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        debug_assert!(
            first_raw_character(line).is_none(),
            "a raw {:?} reached the output in {line:?}",
            first_raw_character(line)
        );
        self.write_line_bytes(line.as_bytes())
    }

    /// Writes a line of JSON. The structured fields carry names as they are,
    /// so JSON is no text line in the sense of `write_line`.
    fn write_json_line(&mut self, line: &str) -> io::Result<()> {
        self.write_line_bytes(line.as_bytes())
    }

    /// Writes `bytes` as they are and flushes.
    fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let written = self
            .writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush());
        self.close_if_gone(written)
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
        self.close_if_gone(written)
    }

    /// A write that failed because the reader is gone closes the sink and
    /// the run goes on without output. Any other failure is the caller's.
    fn close_if_gone(&mut self, written: io::Result<()>) -> io::Result<()> {
        match written {
            Err(error) if is_gone(&error) => {
                self.closed = true;
                Ok(())
            }
            other => other,
        }
    }
}

/// Whether a write failed because nobody reads the stream any more: a
/// closed pipe (`BrokenPipe`), or a terminal that was closed under the run
/// (`EIO`).
fn is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::BrokenPipe
        || error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())
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
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<&'a str>,
    #[serde(flatten)]
    details: &'a Map<String, Value>,
}

/// What the JSON error object of a failed run says.
pub struct ErrorReport<'a> {
    pub kind: ErrorKind,
    /// The code of the diagnostic that explains the error, if one does.
    pub code: Option<String>,
    pub message: &'a str,
    /// The help of that diagnostic.
    pub fix: Option<String>,
    pub details: &'a Map<String, Value>,
}

/// The width a console is laid out for when nothing says otherwise.
/// How data on stdout is laid out and coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub width: usize,
    pub color: bool,
    /// Whether the stream is a terminal. Output that differs between a
    /// terminal and a pipe asks this, not `color`, which `NO_COLOR` turns
    /// off on a terminal too.
    pub terminal: bool,
    /// Whether `…` and `·` may be written.
    pub utf8: bool,
}

impl Style {
    /// The style of a stream whose environment is `env`. Tables take
    /// their colour from the same detection as blocks, so `NO_COLOR`,
    /// `CLICOLOR=0` and a `TERM` that cannot show colour switch both off.
    pub fn from_env(env: &Env) -> Style {
        Style {
            width: env.width,
            color: env.color,
            terminal: env.rich,
            utf8: env.utf8,
        }
    }

    /// The glyphs this style may write.
    pub fn symbols(&self) -> Symbols {
        Symbols::for_utf8(self.utf8)
    }
}

/// The columns a terminal reports for itself, `None` when the stream is
/// no terminal or reports none.
pub fn terminal_columns(stream: impl AsFd) -> Option<u16> {
    let size = rustix::termios::tcgetwinsize(stream).ok()?;
    (size.ws_col > 0).then_some(size.ws_col)
}

/// The columns and rows of the terminal behind `stream`, `None` when it is
/// no terminal or reports zero columns or zero rows.
fn window_size(stream: impl AsFd) -> Option<(usize, usize)> {
    let size = rustix::termios::tcgetwinsize(stream).ok()?;
    (size.ws_col > 0 && size.ws_row > 0)
        .then_some((usize::from(size.ws_col), usize::from(size.ws_row)))
}

/// Where a run writes to.
pub struct Output {
    stdout: Sink,
    stderr: Sink,
    stdout_style: Style,
    stdout_env: Env,
    stderr_env: Env,
    size: Size,
    /// The footer of a dry run, which `notices` writes after the notices.
    footer: Vec<String>,
}

/// Where the columns and rows of a terminal are read, each time `get` is
/// called.
#[derive(Clone)]
pub struct SizeSource(Arc<dyn Fn() -> Option<(usize, usize)> + Send + Sync>);

impl SizeSource {
    pub fn new(read: impl Fn() -> Option<(usize, usize)> + Send + Sync + 'static) -> SizeSource {
        SizeSource(Arc::new(read))
    }

    /// The columns and rows now, or `None` when the terminal reports none.
    pub fn get(&self) -> Option<(usize, usize)> {
        (self.0)()
    }
}

/// Where the size of the terminal comes from.
#[derive(Debug, Clone, Copy)]
enum Size {
    /// The window of the process's own stdout.
    Window,
    /// A size given by the caller, or none.
    Given(Option<(usize, usize)>),
}

impl Output {
    /// The process's own stdout and stderr, each laid out for what it is.
    pub fn stdio() -> Output {
        let stdout = Env::detect(&io::stdout());
        let mut output = Output::new(Box::new(io::stdout()), Box::new(io::stderr()))
            .with_stdout_style(Style::from_env(&stdout))
            .with_envs(stdout, Env::detect(&io::stderr()));
        output.size = Size::Window;
        output
    }

    pub fn new(stdout: Box<dyn Write + Send>, stderr: Box<dyn Write + Send>) -> Output {
        Output {
            stdout: Sink::new(stdout),
            stderr: Sink::new(stderr),
            stdout_style: Style::from_env(&Env::pipe()),
            stdout_env: Env::pipe(),
            stderr_env: Env::pipe(),
            size: Size::Given(None),
            footer: Vec::new(),
        }
    }

    /// The same output with the size that its size source reports.
    #[cfg(test)]
    pub fn with_stdout_size(mut self, size: Option<(usize, usize)>) -> Output {
        self.size = Size::Given(size);
        self
    }

    /// Reads the columns and rows of the terminal behind stdout whenever
    /// asked, `None` when it reports none. A window of zero columns or zero
    /// rows is unknown. The source does not borrow the output, so a driver
    /// that holds the output mutably still sees the window change.
    pub fn stdout_size_source(&self) -> SizeSource {
        let size = self.size;
        SizeSource::new(move || match size {
            Size::Given(size) => size,
            Size::Window => window_size(io::stdout()),
        })
    }

    /// Writes `bytes` to stdout as they are, such as the escape sequences
    /// that redraw a region of lines.
    pub fn stdout_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdout.write_raw(bytes)
    }

    /// The same output with the environments of its two streams.
    pub fn with_envs(mut self, stdout: Env, stderr: Env) -> Output {
        self.stdout_env = stdout;
        self.stderr_env = stderr;
        self
    }

    /// How blocks and symbols on stdout are rendered.
    pub fn stdout_env(&self) -> Env {
        self.stdout_env
    }

    /// How blocks and symbols on stderr are rendered.
    pub fn stderr_env(&self) -> Env {
        self.stderr_env
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
        self.stdout.write_json_line(&line)
    }

    /// Writes the error object of a failed run to stdout.
    pub fn json_error(&mut self, command: Option<&str>, error: &ErrorReport) -> io::Result<()> {
        let payload = ErrorPayload {
            error: ErrorBody {
                kind: error.kind.as_str(),
                code: error.code.as_deref(),
                message: error.message,
                fix: error.fix.as_deref(),
                details: error.details,
            },
        };
        self.json(command, &payload)
    }

    /// Writes a line of a command's result to stdout.
    pub fn line(&mut self, text: &str) -> io::Result<()> {
        self.stdout.write_line(text)
    }

    /// Writes a line of a message to the user to stderr.
    pub fn note(&mut self, text: &str) -> io::Result<()> {
        self.stderr.write_line(text)
    }

    /// Writes an info block that belongs to a command's result to stdout.
    /// Only a terminal gets it, after an empty line; a pipe gets nothing.
    pub fn info_result(&mut self, info: &Block) -> io::Result<()> {
        self.result_block(info)
    }

    /// Writes an info block that stands for the result itself, without an
    /// empty line before it. Only a terminal gets it; the answer says
    /// whether it was written.
    pub fn info_only(&mut self, info: &Block) -> io::Result<bool> {
        if !self.stdout_env.rich {
            return Ok(false);
        }
        for line in render_blocks(std::slice::from_ref(info), &self.stdout_env) {
            self.stdout.write_line(&line)?;
        }
        Ok(true)
    }

    /// Writes a hint that belongs to a command's result to stdout, under
    /// the same rule as `info_result`.
    pub fn hint_result(&mut self, hint: &Block) -> io::Result<()> {
        self.result_block(hint)
    }

    fn result_block(&mut self, block: &Block) -> io::Result<()> {
        if !self.stdout_env.rich {
            return Ok(());
        }
        self.stdout.write_line("")?;
        for line in render_blocks(std::slice::from_ref(block), &self.stdout_env) {
            self.stdout.write_line(&line)?;
        }
        Ok(())
    }

    /// Writes blocks of any kind to stderr: the rich form on a terminal,
    /// the `error:` and `warning:` lines on a pipe. Info and hint blocks
    /// exist only on a terminal.
    pub fn blocks(&mut self, blocks: &[Block]) -> io::Result<()> {
        for line in render_blocks(blocks, &self.stderr_env) {
            self.stderr.write_line(&line)?;
        }
        Ok(())
    }

    /// Writes a group of blocks and tables that belong together to stderr
    /// as one unit, such as an error with the table of the matches it
    /// names. A pipe gets the plain lines of the blocks and none of the rest.
    pub fn group(&mut self, parts: &[Part]) -> io::Result<()> {
        for line in render_parts(parts, &self.stderr_env) {
            self.stderr.write_line(&line)?;
        }
        Ok(())
    }

    /// Writes blocks that belong to a command's result to stdout, after an
    /// empty line. Only a terminal gets them; a pipe gets nothing.
    pub fn result_blocks(&mut self, blocks: &[Block]) -> io::Result<()> {
        if !self.stdout_env.rich || blocks.is_empty() {
            return Ok(());
        }
        self.stdout.write_line("")?;
        for line in render_blocks(blocks, &self.stdout_env) {
            self.stdout.write_line(&line)?;
        }
        Ok(())
    }

    /// Writes the warnings and notes of a run to stderr, after its result.
    /// A terminal gets an empty line first and a block each; a pipe gets
    /// the `warning:` and `note:` lines.
    pub fn notices(&mut self, notices: &[Notice]) -> io::Result<()> {
        self.notices_only(notices)?;
        for line in std::mem::take(&mut self.footer) {
            self.stdout.write_line(&line)?;
        }
        Ok(())
    }

    fn notices_only(&mut self, notices: &[Notice]) -> io::Result<()> {
        if notices.is_empty() {
            return Ok(());
        }
        if self.stderr_env.rich {
            self.stderr.write_line("")?;
        }
        let mut pending: Vec<Block> = Vec::new();
        for notice in notices {
            match notice.block() {
                Some(block) => pending.push(block),
                None => {
                    self.blocks(&std::mem::take(&mut pending))?;
                    if let Some(line) = notice.line() {
                        self.stderr.write_line(&line)?;
                    }
                }
            }
        }
        self.blocks(&pending)
    }

    /// Writes the lines of a dry run to stdout and says how many of them
    /// were actions. A `Would <verb> <rest>` line gets the tag on a
    /// terminal, with the rests lined up; any other line is written as it
    /// is. A `Would skip` line is tagged but is no action: running again
    /// would skip it too.
    ///
    /// The lines name paths from the user's files. A control character in
    /// one is escaped here, before the line is split into verb and rest, so
    /// that a name cannot end a line or forge another action.
    pub fn dry_lines(&mut self, lines: &[String]) -> io::Result<usize> {
        let lines: Vec<String> = lines.iter().map(|line| escape_controls(line)).collect();
        let width = lines
            .iter()
            .filter_map(|line| dryrun::split(line))
            .map(|(verb, _)| verb.width())
            .max()
            .unwrap_or(0);
        let mut actions = 0;
        for line in &lines {
            let Some((verb, rest)) = dryrun::split(line) else {
                self.stdout.write_line(line)?;
                continue;
            };
            if verb != "skip" {
                actions += 1;
            }
            let shown = dryrun::line(&self.stdout_env, verb, width, rest);
            self.stdout.write_line(&shown)?;
        }
        Ok(actions)
    }

    /// Writes the lines of a dry run and, when any of them is an action,
    /// the footer. `nothing_was` and `rerun` complete the footer's
    /// sentence, see [`Output::dry_footer`].
    pub fn dry_plan(&mut self, lines: &[String], nothing_was: &str, rerun: &str) -> io::Result<()> {
        self.dry_plan_explained(lines, None, nothing_was, rerun)
    }

    /// As [`Output::dry_plan`], with an info block between the lines and
    /// the footer that says what the command does. Only a terminal gets
    /// it, and only when the plan has an action.
    pub fn dry_plan_explained(
        &mut self,
        lines: &[String],
        info: Option<&Block>,
        nothing_was: &str,
        rerun: &str,
    ) -> io::Result<()> {
        if self.dry_lines(lines)? > 0 {
            if let Some(info) = info {
                self.info_result(info)?;
            }
            self.dry_footer(nothing_was, rerun)?;
        }
        Ok(())
    }

    /// Sets the closing lines of a dry run, none for a pipe. They are the
    /// last thing a run prints, so `notices` writes them to stdout after the
    /// notices, and every command that ends a dry run with this footer calls
    /// `notices` last, with no notices if it has none.
    pub fn dry_footer(&mut self, nothing_was: &str, rerun: &str) -> io::Result<()> {
        self.footer = dryrun::footer(&self.stdout_env, nothing_was, rerun);
        Ok(())
    }
}

#[cfg(test)]
pub mod testing {
    use super::{Env, Output};
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
        pub fn output(&self) -> Output {
            Output::new(
                Box::new(Buffer(self.stdout.clone())),
                Box::new(Buffer(self.stderr.clone())),
            )
        }

        /// Output whose streams have the given environments.
        pub fn output_with(&self, stdout: Env, stderr: Env) -> Output {
            self.output().with_envs(stdout, stderr)
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
    use crate::cli::console::rich::Rich;
    use antiq::report::Warning;
    use serde_json::json;

    #[test]
    fn an_envelope_starts_with_format_and_command_then_the_payload() {
        let capture = Capture::default();
        let mut out = capture.output();
        out.json(Some("status"), &json!({"pieces": [], "remote": "none"}))
            .expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "{\"format\":1,\"command\":\"status\",\"pieces\":[],\"remote\":\"none\"}\n"
        );
    }

    fn terminal() -> Env {
        Env::new(true, false, true, 80)
    }

    #[test]
    fn an_info_result_follows_an_empty_line_on_a_terminal_stdout() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.info_result(&Block::info(Rich::parse("Nothing was deleted.")))
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "\n▌ ℹ INFO\n▌ Nothing was deleted.\n");
        assert_eq!(capture.stderr(), "");
    }

    #[test]
    fn an_info_result_and_a_hint_result_are_dropped_on_a_stdout_pipe() {
        let capture = Capture::default();
        let mut out = capture.output_with(Env::pipe(), terminal());
        out.info_result(&Block::info(Rich::parse("Nothing was deleted.")))
            .expect("memory accepts writes");
        out.hint_result(&Block::hint(Rich::parse("Run `antiq status`.")))
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "");
        assert_eq!(capture.stderr(), "", "the stderr terminal does not matter");
    }

    #[test]
    fn a_hint_result_is_one_inline_line_on_a_terminal_stdout() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.hint_result(&Block::hint(Rich::parse("Run `antiq status`.")))
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "\n → HINT  Run antiq status.\n");
    }

    #[test]
    fn result_blocks_follow_an_empty_line_on_a_terminal_stdout_and_vanish_on_a_pipe() {
        let blocks = [
            Block::warning(Rich::parse("One")),
            Block::warning(Rich::parse("Two")),
        ];
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.result_blocks(&blocks).expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "\n▌ ⚠ WARNING\n▌ One\n\n▌ ⚠ WARNING\n▌ Two\n"
        );
        assert_eq!(capture.stderr(), "");

        let capture = Capture::default();
        let mut out = capture.output_with(Env::pipe(), terminal());
        out.result_blocks(&blocks).expect("memory accepts writes");
        assert_eq!(capture.stdout(), "");
    }

    #[test]
    fn notices_on_a_stderr_terminal_follow_an_empty_line_and_are_blocks() {
        let capture = Capture::default();
        let mut out = capture.output_with(Env::pipe(), terminal());
        out.notices(&[
            Notice::Warning(Warning::new("Clock skew")),
            Notice::Note("repaired: x".into()),
        ])
        .expect("memory accepts writes");
        assert_eq!(
            capture.stderr(),
            "\n▌ ⚠ WARNING\n▌ Clock skew\n\n▌ ℹ INFO\n▌ Repaired: x\n"
        );
        assert_eq!(capture.stdout(), "");
    }

    #[test]
    fn notices_on_a_stderr_pipe_are_warning_and_note_lines() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.notices(&[
            Notice::Warning(Warning::new("Clock skew")),
            Notice::Note("repaired: x".into()),
        ])
        .expect("memory accepts writes");
        assert_eq!(capture.stderr(), "warning: Clock skew\nnote: repaired: x\n");
        assert_eq!(capture.stdout(), "");
    }

    #[test]
    fn a_line_among_the_notices_keeps_its_place() {
        let capture = Capture::default();
        let mut out = capture.output_with(Env::pipe(), terminal());
        out.notices(&[
            Notice::Line("error: could not delete x".into()),
            Notice::Warning(Warning::new("Clock skew")),
            Notice::Line("error: could not delete y".into()),
        ])
        .expect("memory accepts writes");
        assert_eq!(
            capture.stderr(),
            "\nerror: could not delete x\n▌ ⚠ WARNING\n▌ Clock skew\nerror: could not delete y\n"
        );
    }

    #[test]
    fn no_notices_write_nothing_not_even_an_empty_line() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), terminal());
        out.notices(&[]).expect("memory accepts writes");
        assert_eq!(capture.stderr(), "");
    }

    #[test]
    fn blocks_on_a_stderr_terminal_are_rich_and_separated_by_an_empty_line() {
        let capture = Capture::default();
        let mut out = capture.output_with(Env::pipe(), terminal());
        out.blocks(&[
            Block::warning(Rich::parse("Clock skew")),
            Block::info(Rich::parse("Nothing was pushed.")),
        ])
        .expect("memory accepts writes");
        assert_eq!(
            capture.stderr(),
            "▌ ⚠ WARNING\n▌ Clock skew\n\n▌ ℹ INFO\n▌ Nothing was pushed.\n"
        );
        assert_eq!(capture.stdout(), "");
    }

    #[test]
    fn blocks_on_a_stderr_pipe_keep_only_the_plain_lines_of_errors_and_warnings() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.blocks(&[
            Block::warning(Rich::parse("Clock skew")),
            Block::info(Rich::parse("Nothing was pushed.")),
        ])
        .expect("memory accepts writes");
        assert_eq!(capture.stderr(), "warning: Clock skew\n");
        assert_eq!(capture.stdout(), "");
    }

    #[test]
    fn a_dry_run_on_a_terminal_gets_the_tag_and_the_footer() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.dry_lines(&["Would archive projects/scanner".to_string()])
            .expect("memory accepts writes");
        out.dry_footer("archived", "archive these pieces")
            .expect("memory accepts writes");
        out.notices(&[]).expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "○ would archive  projects/scanner\n\n\x20○ DRY RUN  Nothing was archived. \
             Run again without -n to archive these pieces.\n"
        );
    }

    #[test]
    fn a_plan_that_only_skips_has_no_footer() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        let skip = "Would skip projects/scanner (version 2026-11-04): 5 MB needed, 2 MB free";
        out.dry_plan(&[skip.to_string()], "unearthed", "unearth the piece")
            .expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "○ would skip  projects/scanner (version 2026-11-04): 5 MB needed, 2 MB free\n"
        );
    }

    #[test]
    fn a_plan_with_an_action_next_to_a_skip_has_the_footer() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        let lines = [
            "Would skip a (version v): 5 MB needed, 2 MB free".to_string(),
            "Would unearth b".to_string(),
        ];
        out.dry_plan(&lines, "unearthed", "unearth the piece")
            .expect("memory accepts writes");
        out.notices(&[]).expect("memory accepts writes");
        assert!(
            capture
                .stdout()
                .contains("○ DRY RUN  Nothing was unearthed."),
            "{:?}",
            capture.stdout()
        );
    }

    #[test]
    fn a_dry_run_line_keeps_the_spaces_of_a_path() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.dry_lines(&["Would archive notes/two  spaces /and trailing ".to_string()])
            .expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "○ would archive  notes/two  spaces /and trailing \n"
        );
    }

    #[test]
    fn styling_and_the_tab_of_a_pipe_table_pass_the_check_of_a_line() {
        let capture = Capture::default();
        let mut out = capture.output();
        out.line("\x1b[1mpiece\x1b[0m\tstamp")
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "\x1b[1mpiece\x1b[0m\tstamp\n");
    }

    #[test]
    fn json_with_a_name_of_the_escaped_class_is_no_text_line_and_passes() {
        let capture = Capture::default();
        let mut out = capture.output();
        out.json(
            Some("find"),
            &serde_json::json!({"piece": "a\u{202e}b\u{85}"}),
        )
        .expect("memory accepts writes");
        assert!(
            capture.stdout().contains("a\u{202e}b\u{85}"),
            "{:?}",
            capture.stdout()
        );
    }

    #[test]
    fn a_control_character_in_a_dry_run_line_is_escaped_on_a_pipe() {
        let capture = Capture::default();
        let mut out = capture.output_with(Env::pipe(), Env::pipe());
        let actions = out
            .dry_lines(&["Would archive a\nWould archive b\u{1b}[0m".to_string()])
            .expect("memory accepts writes");
        assert_eq!(actions, 1);
        assert_eq!(
            capture.stdout(),
            "Would archive a%0AWould archive b%1B[0m\n"
        );
    }

    #[test]
    fn a_control_character_in_a_dry_run_line_is_escaped_on_a_terminal() {
        let capture = Capture::default();
        let mut out = capture.output_with(terminal(), Env::pipe());
        out.dry_lines(&["Would archive a\nWould archive b".to_string()])
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "○ would archive  a%0AWould archive b\n");
    }

    #[test]
    fn a_dry_run_on_a_stdout_pipe_keeps_the_would_line_and_has_no_footer() {
        let capture = Capture::default();
        let mut out = capture.output_with(Env::pipe(), terminal());
        out.dry_lines(&["Would archive projects/scanner".to_string()])
            .expect("memory accepts writes");
        out.dry_footer("archived", "archive these pieces")
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "Would archive projects/scanner\n");
    }

    #[test]
    fn each_stream_has_its_own_environment() {
        let capture = Capture::default();
        let out = capture.output();
        assert_eq!(out.stdout_env(), Env::pipe());
        assert_eq!(out.stderr_env(), Env::pipe());
        let terminal = Env::new(true, false, true, 100);
        let out = out.with_envs(terminal, Env::pipe());
        assert_eq!(out.stdout_env(), terminal);
        assert_eq!(out.stderr_env(), Env::pipe());
    }

    #[test]
    fn an_envelope_without_a_command_says_null() {
        let capture = Capture::default();
        capture
            .output()
            .json(None, &json!({}))
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "{\"format\":1,\"command\":null}\n");
    }

    #[test]
    fn an_error_object_carries_kind_code_message_fix_and_details() {
        let capture = Capture::default();
        let mut details = Map::new();
        details.insert("pid".into(), json!(4242));
        capture
            .output()
            .json_error(
                Some("archive"),
                &ErrorReport {
                    kind: ErrorKind::LockHeld,
                    code: Some("antiq::lock::held".into()),
                    message: "held",
                    fix: Some("wait".into()),
                    details: &details,
                },
            )
            .expect("memory accepts writes");
        assert_eq!(
            capture.stdout(),
            "{\"format\":1,\"command\":\"archive\",\"error\":\
             {\"kind\":\"lock_held\",\"code\":\"antiq::lock::held\",\
             \"message\":\"held\",\"fix\":\"wait\",\"pid\":4242}}\n"
        );
        assert_eq!(capture.stderr(), "");
    }

    #[test]
    fn a_closed_pipe_drops_the_output_without_an_error() {
        let (pipe, flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(pipe), Box::new(Vec::new()));
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
        let mut out = capture.output();
        out.line("Created the collection")
            .expect("memory accepts writes");
        out.note("nothing else was done")
            .expect("memory accepts writes");
        assert_eq!(capture.stdout(), "Created the collection\n");
        assert_eq!(capture.stderr(), "nothing else was done\n");
    }

    #[test]
    fn a_line_to_a_closed_pipe_is_dropped() {
        let (pipe, flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(pipe), Box::new(Vec::new()));
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
        let mut out = Output::new(Box::new(Vec::new()), Box::new(pipe));
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
        let mut out = Output::new(Box::new(disk), Box::new(Vec::new()));
        let error = out
            .json(Some("status"), &json!({}))
            .expect_err("a full disk is reported");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }

    #[test]
    fn a_style_takes_everything_from_the_environment() {
        assert_eq!(
            Style::from_env(&Env::new(true, true, true, 120)),
            Style {
                width: 120,
                color: true,
                terminal: true,
                utf8: true
            }
        );
        assert_eq!(
            Style::from_env(&Env::new(true, false, false, 100)),
            Style {
                width: 100,
                color: false,
                terminal: true,
                utf8: false
            }
        );
    }

    #[test]
    fn a_pipe_is_80_columns_without_colour() {
        assert_eq!(
            Style::from_env(&Env::pipe()),
            Style {
                width: 80,
                color: false,
                terminal: false,
                utf8: true
            }
        );
    }

    #[test]
    fn a_style_without_utf8_has_the_ascii_symbols() {
        let ascii = Style::from_env(&Env::new(true, true, false, 80));
        assert_eq!(ascii.symbols().ellipsis, "...");
        assert_eq!(ascii.symbols().separator, "-");
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
        drop(pty.master);
    }

    #[test]
    fn a_terminal_reports_its_columns_and_rows() {
        let pty = pty(132);
        assert_eq!(window_size(&pty.slave), Some((132, 24)));
        drop(pty.master);
    }

    #[test]
    fn a_window_of_zero_columns_has_no_size() {
        let pty = pty(0);
        assert_eq!(window_size(&pty.slave), None);
        drop(pty.master);
    }

    #[test]
    fn a_pipe_has_no_size_and_a_given_size_is_reported_as_it_is() {
        let (reader, _writer) = io::pipe().expect("a pipe can be made");
        assert_eq!(window_size(&reader), None);
        let capture = Capture::default();
        assert_eq!(capture.output().stdout_size_source().get(), None);
        assert_eq!(
            capture
                .output()
                .with_stdout_size(Some((90, 30)))
                .stdout_size_source()
                .get(),
            Some((90, 30))
        );
    }

    /// A terminal that was closed under the run: every write fails with `EIO`.
    struct DeadTerminal;

    impl Write for DeadTerminal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(5))
        }
    }

    #[test]
    fn raw_bytes_to_a_closed_pipe_or_a_dead_terminal_are_dropped() {
        let (stream, _flushes) = FailsOnFlush::new(io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(stream), Box::new(Vec::new()));
        out.stdout_raw(b"x").expect("a closed pipe is no error");
        let mut out = Output::new(Box::new(DeadTerminal), Box::new(Vec::new()));
        out.stdout_raw(b"x").expect("a dead terminal is no error");
        out.stdout_raw(b"y").expect("and stays closed");
    }

    #[test]
    fn a_line_to_a_dead_terminal_is_dropped_like_a_raw_write() {
        let mut out = Output::new(Box::new(DeadTerminal), Box::new(Vec::new()));
        out.line("x")
            .expect("a dead terminal loses the line, not the result of the run");
        out.line("y").expect("and stays closed");
    }

    #[test]
    fn another_failure_of_a_raw_write_is_an_error() {
        let (stream, _flushes) = FailsOnFlush::new(io::ErrorKind::PermissionDenied);
        let mut out = Output::new(Box::new(stream), Box::new(Vec::new()));
        assert!(out.stdout_raw(b"x").is_err());
    }

    #[test]
    fn a_window_of_zero_columns_is_unknown() {
        let pty = pty(0);
        assert_eq!(terminal_columns(&pty.slave), None);
        drop(pty.master);
    }

    #[test]
    fn a_pipe_and_dev_null_are_no_terminal() {
        let (reader, _writer) = io::pipe().expect("a pipe can be made");
        assert_eq!(terminal_columns(&reader), None);
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
            utf8: true,
        };
        let out = capture.output().with_stdout_style(style);
        assert_eq!(out.stdout_style(), style);
        assert_eq!(capture.output().stdout_style().width, 80);
    }
}
