// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Running the antiq binary on a pseudo terminal, the way a person at a
//! terminal runs it, and reading back what it drew.
//!
//! The child gets the terminal as its controlling terminal, in a session of
//! its own, so the developer's real terminal is never touched. What it writes
//! to the terminal goes through [`Screen`], a minimal terminal emulator that
//! knows the sequences the picker uses.
//!
//! Nothing here sleeps. [`Pty::wait_for`] returns as soon as a condition on
//! the screen holds and fails after [`WAIT_BOUND`].

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use unicode_width::UnicodeWidthChar;

use super::cmd::detached;

/// How long a wait for the screen or the exit of the child lasts before the
/// test fails. A run that is fine takes milliseconds.
pub const WAIT_BOUND: Duration = Duration::from_secs(20);

pub const ROWS: u16 = 24;
pub const COLS: u16 = 100;

pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";
pub const CTRL_F: &[u8] = b"\x06";
pub const CTRL_K: &[u8] = b"\x0b";
pub const CTRL_X: &[u8] = b"\x18";

// =========================================================
// The emulated screen
// =========================================================

type Grid = Vec<Vec<char>>;

fn blank_grid(rows: usize, cols: usize) -> Grid {
    vec![vec![' '; cols]; rows]
}

enum Parse {
    Ground,
    Escape,
    Csi(String),
    /// Inside an operating system command or another string sequence, up to
    /// its terminator.
    Skip,
}

/// What a terminal of [`ROWS`] by [`COLS`] shows after the bytes written to
/// it so far.
pub struct Screen {
    rows: usize,
    cols: usize,
    grid: Grid,
    /// The grid and cursor of the normal screen while the alternate screen is
    /// shown.
    saved: Option<(Grid, usize, usize)>,
    alt_entered: bool,
    row: usize,
    col: usize,
    parse: Parse,
    /// The start of a UTF-8 sequence the last write ended in.
    carry: Vec<u8>,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Screen {
        Screen {
            rows,
            cols,
            grid: blank_grid(rows, cols),
            saved: None,
            alt_entered: false,
            row: 0,
            col: 0,
            parse: Parse::Ground,
            carry: Vec::new(),
        }
    }

    /// The lines of the screen without their trailing blanks.
    pub fn lines(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|row| {
                let line: String = row.iter().filter(|&&c| c != '\0').collect();
                line.trim_end().to_string()
            })
            .collect()
    }

    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.lines().iter().any(|line| line.contains(needle))
    }

    /// Whether the child ever switched to the alternate screen.
    pub fn alt_entered(&self) -> bool {
        self.alt_entered
    }

    /// Whether the child is on the alternate screen now.
    pub fn on_alt(&self) -> bool {
        self.saved.is_some()
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        self.grid = blank_grid(rows, cols);
        self.saved = self.saved.as_ref().map(|_| (blank_grid(rows, cols), 0, 0));
        self.row = 0;
        self.col = 0;
    }

    fn apply(&mut self, bytes: &[u8]) {
        let mut buffer = std::mem::take(&mut self.carry);
        buffer.extend_from_slice(bytes);
        let mut rest = buffer.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    text.chars().for_each(|c| self.char(c));
                    return;
                }
                Err(error) => {
                    let (valid, tail) = rest.split_at(error.valid_up_to());
                    std::str::from_utf8(valid)
                        .expect("the prefix is valid")
                        .chars()
                        .for_each(|c| self.char(c));
                    match error.error_len() {
                        // The sequence is cut off by the end of this write.
                        None => {
                            self.carry = tail.to_vec();
                            return;
                        }
                        Some(len) => {
                            self.char('\u{fffd}');
                            rest = &tail[len..];
                        }
                    }
                }
            }
        }
    }

    fn char(&mut self, c: char) {
        match std::mem::replace(&mut self.parse, Parse::Ground) {
            Parse::Ground => self.ground(c),
            Parse::Escape => match c {
                '[' => self.parse = Parse::Csi(String::new()),
                ']' | 'P' | '_' | '^' => self.parse = Parse::Skip,
                _ => {}
            },
            Parse::Csi(mut params) => {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    self.csi(&params, c);
                } else {
                    params.push(c);
                    self.parse = Parse::Csi(params);
                }
            }
            Parse::Skip => {
                if c != '\u{7}' && c != '\u{1b}' {
                    self.parse = Parse::Skip;
                } else if c == '\u{1b}' {
                    self.parse = Parse::Escape;
                }
            }
        }
    }

    fn ground(&mut self, c: char) {
        match c {
            '\u{1b}' => self.parse = Parse::Escape,
            '\r' => self.col = 0,
            '\n' => self.line_feed(),
            '\u{8}' => self.col = self.col.saturating_sub(1),
            c if c.is_control() => {}
            c => {
                let width = c.width().unwrap_or(0);
                if width == 0 {
                    return;
                }
                if self.col + width > self.cols {
                    self.col = 0;
                    self.line_feed();
                }
                self.grid[self.row][self.col] = c;
                if width == 2 {
                    self.grid[self.row][self.col + 1] = '\0';
                }
                self.col += width;
            }
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 == self.rows {
            self.grid.remove(0);
            self.grid.push(vec![' '; self.cols]);
        } else {
            self.row += 1;
        }
    }

    fn csi(&mut self, params: &str, final_byte: char) {
        let numbers = |defaults: usize| -> Vec<usize> {
            params
                .split(';')
                .map(|part| part.parse().unwrap_or(defaults))
                .collect()
        };
        match final_byte {
            'H' | 'f' => {
                let at = numbers(1);
                self.row = at[0].max(1).min(self.rows) - 1;
                self.col = at.get(1).copied().unwrap_or(1).max(1).min(self.cols) - 1;
            }
            'A' => self.row = self.row.saturating_sub(numbers(1)[0].max(1)),
            'B' => self.row = (self.row + numbers(1)[0].max(1)).min(self.rows - 1),
            'C' => self.col = (self.col + numbers(1)[0].max(1)).min(self.cols - 1),
            'D' => self.col = self.col.saturating_sub(numbers(1)[0].max(1)),
            'K' => {
                let (from, to) = match numbers(0)[0] {
                    1 => (0, self.col + 1),
                    2 => (0, self.cols),
                    _ => (self.col, self.cols),
                };
                self.grid[self.row][from..to].fill(' ');
            }
            'J' => match numbers(0)[0] {
                2 | 3 => self.grid = blank_grid(self.rows, self.cols),
                _ => {
                    self.grid[self.row][self.col..].fill(' ');
                    for row in &mut self.grid[self.row + 1..] {
                        row.fill(' ');
                    }
                }
            },
            'h' if params == "?1049" => {
                self.alt_entered = true;
                if self.saved.is_none() {
                    let normal =
                        std::mem::replace(&mut self.grid, blank_grid(self.rows, self.cols));
                    self.saved = Some((normal, self.row, self.col));
                }
            }
            'l' if params == "?1049" => {
                if let Some((normal, row, col)) = self.saved.take() {
                    self.grid = normal;
                    self.row = row;
                    self.col = col;
                }
            }
            // Colours, cursor visibility and the rest change nothing the
            // tests read.
            _ => {}
        }
    }
}

// =========================================================
// Cutting the byte stream into frames
// =========================================================

/// The picker brackets every frame in a synchronized update. The screen is
/// brought up to date only at frame ends, so a test never sees half a frame.
const FRAME_BEGIN: &[u8] = b"\x1b[?2026h";
const FRAME_END: &[u8] = b"\x1b[?2026l";

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The number of bytes at the end of `bytes` that are the start of
/// [`FRAME_BEGIN`] and may still become it.
fn partial_begin(bytes: &[u8]) -> usize {
    (1..FRAME_BEGIN.len())
        .rev()
        .find(|&len| bytes.ends_with(&FRAME_BEGIN[..len]))
        .unwrap_or(0)
}

// =========================================================
// The session
// =========================================================

/// Where the child's standard streams point.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Nothing for stdin, a pipe the test reads for stdout and stderr.
    Pipe,
    /// The pseudo terminal.
    Terminal,
}

pub struct Options {
    pub stdin: Stream,
    pub stdout: Stream,
}

impl Options {
    /// The way a person runs it with nothing redirected but the output of a
    /// command substitution: stdin from nowhere, output in pipes.
    pub fn piped() -> Options {
        Options {
            stdin: Stream::Pipe,
            stdout: Stream::Pipe,
        }
    }
}

/// A child process on a pseudo terminal.
pub struct Pty {
    master: File,
    chunks: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
    screen: Screen,
    /// Bytes read but not yet applied: the frame being drawn.
    pending: Vec<u8>,
    child: Child,
    _home: TempDir,
}

impl Pty {
    /// Start `args` of the antiq binary on a new terminal, `-C root` first.
    pub fn start(root: &std::path::Path, args: &[&str], options: Options) -> Pty {
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the tests assume a user that is not root"
        );
        let home = tempfile::tempdir().expect("the temp dir is writable");
        let mut command = detached(env!("CARGO_BIN_EXE_antiq"));
        command
            .env("HOME", home.path())
            .arg("-C")
            .arg(root)
            .args(args);

        let master =
            rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
                .expect("a pseudo terminal opens");
        // A master the child inherited would keep the terminal open for as
        // long as it lives, and tests run in parallel and spawn children.
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)
            .expect("the master is close-on-exec");
        rustix::pty::grantpt(&master).expect("the slave is granted");
        rustix::pty::unlockpt(&master).expect("the slave is unlocked");
        let slave_path = rustix::pty::ptsname(&master, Vec::new()).expect("the slave has a name");
        let slave = open_slave(&slave_path);
        // macOS refuses the size on a master whose slave nobody has open.
        rustix::termios::tcsetwinsize(
            &slave,
            rustix::termios::Winsize {
                ws_row: ROWS,
                ws_col: COLS,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .expect("the window size is set");

        if options.stdin == Stream::Terminal {
            command.stdin(Stdio::from(clone_fd(&slave)));
        }
        match options.stdout {
            Stream::Terminal => {
                command.stdout(Stdio::from(clone_fd(&slave)));
                command.stderr(Stdio::from(clone_fd(&slave)));
            }
            Stream::Pipe => {
                command.stdout(Stdio::piped());
                command.stderr(Stdio::piped());
            }
        }
        // The session is made by `detached`; this runs after it, so the
        // terminal becomes the controlling terminal of that session.
        // SAFETY: the closure calls only `open` and `ioctl`, which are
        // async-signal-safe, on a path built before the fork.
        unsafe {
            let path: CString = slave_path.clone();
            command.pre_exec(move || {
                let tty = rustix::fs::open(
                    path.as_c_str(),
                    rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
                    rustix::fs::Mode::empty(),
                )?;
                rustix::process::ioctl_tiocsctty(&tty)?;
                // Left open on purpose: the terminal then has a holder for
                // as long as the child lives, even while the child has not
                // opened `/dev/tty` yet, and the reader sees the end of the
                // terminal only when the child is gone.
                let _ = tty.into_raw_fd();
                Ok(())
            });
        }
        let child = command.spawn().expect("the antiq binary starts");
        // The child holds the slave now. Keeping our copy would stop the
        // master from seeing the end of the terminal when the child exits.
        drop(slave);

        let (sender, chunks) = mpsc::channel();
        let mut source = File::from(clone_fd(&master));
        let reader = thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = source.read(&mut buffer) {
                if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        Pty {
            master: File::from(master),
            chunks,
            reader: Some(reader),
            screen: Screen::new(ROWS.into(), COLS.into()),
            pending: Vec::new(),
            child,
            _home: home,
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Type `keys`.
    pub fn send(&mut self, keys: &[u8]) {
        self.master
            .write_all(keys)
            .expect("the terminal takes keys");
    }

    /// Type `text`, a key at a time, each after the frame that answers the
    /// one before.
    pub fn type_text(&mut self, text: &str) {
        let mut typed = String::new();
        for c in text.chars() {
            typed.push(c);
            self.send(c.to_string().as_bytes());
            let prompt = format!("❯ {typed}");
            self.wait_for(&format!("the prompt {prompt:?}"), |screen| {
                screen.contains(&prompt)
            });
        }
    }

    /// Make the terminal `cols` wide and tell the child.
    pub fn resize(&mut self, cols: u16) {
        rustix::termios::tcsetwinsize(
            &self.master,
            rustix::termios::Winsize {
                ws_row: ROWS,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .expect("the window size is set");
        self.screen.resize(ROWS.into(), cols.into());
        let pid = i32::try_from(self.child.id()).expect("a process ID fits");
        // SAFETY: `kill` takes two integers.
        let sent = unsafe { libc::kill(pid, libc::SIGWINCH) };
        assert_eq!(sent, 0, "the child takes the signal");
    }

    /// Whether the child is still running.
    pub fn running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("the child can be asked")
            .is_none()
    }

    /// Read until `condition` holds for the screen at the end of a frame.
    /// Fails after [`WAIT_BOUND`], showing the last screen.
    pub fn wait_for(&mut self, what: &str, condition: impl Fn(&Screen) -> bool) {
        let deadline = Instant::now() + WAIT_BOUND;
        self.settle();
        while !condition(&self.screen) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.chunks.recv_timeout(left) {
                Ok(chunk) => {
                    self.pending.extend_from_slice(&chunk);
                    self.settle();
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    let child = self.describe_child();
                    let stderr = self.stderr_so_far();
                    panic!(
                        "gave up waiting for {what}; the child {child}; stderr {stderr:?}; \
                         the screen is\n{}\n",
                        self.screen.text()
                    )
                }
            }
        }
    }

    fn describe_child(&mut self) -> String {
        match self.child.try_wait().expect("the child can be asked") {
            Some(status) => format!("ended with {status}"),
            None => "is still running".to_string(),
        }
    }

    /// What the child wrote to stderr, when it has ended and the pipe is
    /// at its end.
    fn stderr_so_far(&mut self) -> String {
        let mut text = String::new();
        let ended = self.child.try_wait().ok().flatten().is_some();
        if let (true, Some(pipe)) = (ended, self.child.stderr.as_mut()) {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    }

    /// Apply the complete frames among the pending bytes.
    fn settle(&mut self) {
        while let Ok(chunk) = self.chunks.try_recv() {
            self.pending.extend_from_slice(&chunk);
        }
        loop {
            let Some(begin) = find(&self.pending, FRAME_BEGIN) else {
                let keep = partial_begin(&self.pending);
                let ready = self.pending.len() - keep;
                self.screen.apply(&self.pending[..ready]);
                self.pending.drain(..ready);
                return;
            };
            let after = begin + FRAME_BEGIN.len();
            match find(&self.pending[after..], FRAME_END) {
                Some(end) => {
                    let through = after + end + FRAME_END.len();
                    self.screen.apply(&self.pending[..through]);
                    self.pending.drain(..through);
                }
                None => {
                    self.screen.apply(&self.pending[..begin]);
                    self.pending.drain(..begin);
                    return;
                }
            }
        }
    }

    /// Wait for the child to end and collect what it wrote to its pipes.
    ///
    /// The reader thread ends when the last holder of the slave side is
    /// gone, which closes the channel and ends the wait without a delay.
    pub fn finish(mut self) -> Finished {
        let deadline = Instant::now() + WAIT_BOUND;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.chunks.recv_timeout(left) {
                Ok(chunk) => {
                    self.pending.extend_from_slice(&chunk);
                    self.settle();
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!(
                    "the child did not end; the screen is\n{}\n",
                    self.screen.text()
                ),
            }
        }
        let status = self.child.wait().expect("the child can be waited for");
        if let Some(reader) = self.reader.take() {
            reader.join().expect("the reader ends");
        }
        self.settle();
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            pipe.read_to_string(&mut stdout).expect("stdout is UTF-8");
        }
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_string(&mut stderr).expect("stderr is UTF-8");
        }
        Finished {
            status,
            stdout,
            stderr,
            screen: std::mem::replace(&mut self.screen, Screen::new(1, 1)),
        }
    }
}

impl Drop for Pty {
    /// The child is in a session and process group of its own, so killing
    /// the group leaves no antiq behind when a test fails.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            if let Ok(pid) = i32::try_from(self.child.id()) {
                // SAFETY: `kill` takes two integers.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            let _ = self.child.wait();
        }
    }
}

/// What a finished run left behind.
pub struct Finished {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub screen: Screen,
}

impl Finished {
    pub fn code(&self) -> i32 {
        self.status
            .code()
            .expect("the binary exits instead of dying on a signal")
    }
}

fn open_slave(path: &CString) -> File {
    let fd = rustix::fs::open(
        path.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .expect("the slave opens");
    File::from(fd)
}

fn clone_fd(fd: &impl std::os::fd::AsFd) -> OwnedFd {
    fd.as_fd()
        .try_clone_to_owned()
        .expect("the descriptor clones")
}
