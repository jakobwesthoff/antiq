// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scripts run on the far side.
//!
//! Every remote command is `/bin/sh -s` with the script on standard input,
//! so the login shell of the NAS parses nothing but that fixed command.
//! Values travel as positional parameters, quoted here.
//!
//! The script is one brace group. `sh -s` parses a whole compound command
//! before it runs it, so a stream that ends early (a dying connection)
//! is a syntax error and runs nothing. A flat script would run its last,
//! partly received line. A body must not read standard input, which is
//! the script itself, and must not exit with status 255, which the
//! session reads as ssh's own failure.

/// Why a script cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    #[error("a value contains a NUL byte, which a shell cannot hold")]
    Nul,
}

/// Quotes `value` as one POSIX shell word: single quotes around it, each
/// single quote inside written as `'\''`. Any byte but NUL can be quoted
/// this way, newlines included.
pub fn quote(value: &[u8]) -> Result<Vec<u8>, ScriptError> {
    if value.contains(&0) {
        return Err(ScriptError::Nul);
    }
    let mut quoted = Vec::with_capacity(value.len() + 2);
    quoted.push(b'\'');
    for &byte in value {
        if byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(byte);
        }
    }
    quoted.push(b'\'');
    Ok(quoted)
}

/// Exit status of [`EXCLUSIVE_CREATE`] when its target exists.
pub const EXISTS_STATUS: i32 = 17;

/// Script body that creates the file named by `$1` and fails when
/// something is there already. Exit status 0 means created,
/// [`EXISTS_STATUS`] means an entry exists (a directory and a dangling
/// symlink included), 1 means any other failure.
///
/// `set -C` makes the redirection refuse an existing regular file. It runs
/// in a subshell since a failed redirection of the special builtin `:`
/// ends a POSIX shell. A FIFO or device at the target is not covered; the
/// names antiq creates are its own.
pub const EXCLUSIVE_CREATE: &str = r#"if ( set -C; : > "$1" ) 2>/dev/null; then
    exit 0
elif [ -e "$1" ] || [ -h "$1" ]; then
    exit 17
else
    exit 1
fi
"#;

/// A script and the values it runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    body: String,
    args: Vec<Vec<u8>>,
    clock: bool,
}

impl Script {
    pub fn new(body: &str) -> Script {
        Script {
            body: body.to_string(),
            args: Vec::new(),
            clock: false,
        }
    }

    pub fn arg(mut self, value: impl AsRef<[u8]>) -> Script {
        self.args.push(value.as_ref().to_vec());
        self
    }

    pub fn with_clock(mut self) -> Script {
        self.clock = true;
        self
    }

    /// The bytes to write to the standard input of `/bin/sh -s`.
    pub fn to_stdin(&self) -> Result<Vec<u8>, ScriptError> {
        // The remote may not accept the locale variables ssh sends, so the
        // script sets the one that makes the tools' output byte-exact.
        let mut stdin = b"{ LC_ALL=C; export LC_ALL\n".to_vec();
        if !self.args.is_empty() {
            stdin.extend_from_slice(b"set --");
            for arg in &self.args {
                stdin.push(b' ');
                stdin.extend_from_slice(&quote(arg)?);
            }
            stdin.push(b'\n');
        }
        if self.clock {
            stdin.extend_from_slice(b"date +%s\n");
        }
        stdin.extend_from_slice(self.body.as_bytes());
        if !self.body.ends_with('\n') {
            stdin.push(b'\n');
        }
        stdin.extend_from_slice(b"}\n");
        Ok(stdin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Runs `stdin` through a local `/bin/sh -s`, the way the far side
    /// runs it, in `dir`. Returns the exit status and stdout.
    fn run_sh(stdin: &[u8], dir: &std::path::Path) -> (i32, Vec<u8>) {
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

    fn echo_first_argument(value: &[u8]) -> Vec<u8> {
        let dir = tempfile::tempdir().expect("temp dir");
        let script = Script::new("printf '%s' \"$1\"").arg(value);
        let (status, out) = run_sh(&script.to_stdin().expect("builds"), dir.path());
        assert_eq!(status, 0);
        out
    }

    #[test]
    fn a_value_with_a_space_survives() {
        assert_eq!(echo_first_argument(b"a b"), b"a b");
    }

    #[test]
    fn a_value_with_a_single_quote_survives() {
        assert_eq!(echo_first_argument(b"it's"), b"it's");
    }

    #[test]
    fn a_value_with_a_newline_survives() {
        assert_eq!(echo_first_argument(b"a\nb"), b"a\nb");
    }

    #[test]
    fn a_value_with_shell_syntax_survives() {
        assert_eq!(
            echo_first_argument(b"$HOME `x` $(y) \\ \""),
            b"$HOME `x` $(y) \\ \""
        );
    }

    #[test]
    fn a_value_starting_with_a_dash_survives() {
        assert_eq!(echo_first_argument(b"-n"), b"-n");
    }

    #[test]
    fn an_empty_value_survives() {
        let dir = tempfile::tempdir().expect("temp dir");
        let script = Script::new("printf '%s' \"$#\"").arg("");
        let (_, out) = run_sh(&script.to_stdin().expect("builds"), dir.path());
        assert_eq!(out, b"1");
    }

    #[test]
    fn a_value_that_is_not_utf8_survives() {
        assert_eq!(echo_first_argument(b"a\xffb"), b"a\xffb");
    }

    #[test]
    fn a_nul_in_a_value_is_refused() {
        assert_eq!(quote(b"a\0b"), Err(ScriptError::Nul));
    }

    #[test]
    fn the_script_is_one_brace_group_with_the_values_first() {
        let script = Script::new("echo \"$1\"\n").arg("a").arg("b c");
        let text = String::from_utf8(script.to_stdin().expect("builds")).expect("UTF-8");
        assert_eq!(
            text,
            "{ LC_ALL=C; export LC_ALL\nset -- 'a' 'b c'\necho \"$1\"\n}\n"
        );
    }

    #[test]
    fn without_values_there_is_no_set_line() {
        let text =
            String::from_utf8(Script::new("true").to_stdin().expect("builds")).expect("UTF-8");
        assert_eq!(text, "{ LC_ALL=C; export LC_ALL\ntrue\n}\n");
    }

    #[test]
    fn the_clock_line_comes_before_the_body() {
        let text = String::from_utf8(
            Script::new("echo body")
                .with_clock()
                .to_stdin()
                .expect("builds"),
        )
        .expect("UTF-8");
        assert_eq!(text, "{ LC_ALL=C; export LC_ALL\ndate +%s\necho body\n}\n");
    }

    #[test]
    fn a_stream_cut_short_runs_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A prefix of this line, `touch ran-"$1"`, is a complete command
        // of its own that creates a different file than the whole line.
        let script = Script::new("touch ran-\"$1\"-done\n").arg("x");
        let full = script.to_stdin().expect("builds");
        // The last two bytes are the closing brace and its newline; a cut
        // after the brace alone still leaves a complete group.
        for cut in 0..full.len() - 1 {
            run_sh(&full[..cut], dir.path());
            assert!(
                std::fs::read_dir(dir.path())
                    .expect("read dir")
                    .next()
                    .is_none(),
                "a script cut at byte {cut} ran: {:?}",
                String::from_utf8_lossy(&full[..cut])
            );
        }
        run_sh(&full, dir.path());
        assert!(
            dir.path().join("ran-x-done").exists(),
            "the whole script runs"
        );
    }

    fn exclusive_create(path: &std::path::Path) -> i32 {
        let dir = tempfile::tempdir().expect("temp dir");
        let script = Script::new(EXCLUSIVE_CREATE).arg(path.as_os_str().as_encoded_bytes());
        run_sh(&script.to_stdin().expect("builds"), dir.path()).0
    }

    #[test]
    fn exclusive_create_creates_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("claim");
        assert_eq!(exclusive_create(&target), 0);
        assert!(target.exists());
    }

    #[test]
    fn exclusive_create_reports_an_existing_file_and_keeps_its_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("claim");
        std::fs::write(&target, b"mine").expect("write");
        assert_eq!(exclusive_create(&target), EXISTS_STATUS);
        assert_eq!(std::fs::read(&target).expect("read"), b"mine");
    }

    #[test]
    fn exclusive_create_reports_a_directory_as_existing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(exclusive_create(dir.path()), EXISTS_STATUS);
    }

    #[test]
    fn exclusive_create_reports_a_dangling_symlink_as_existing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("claim");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &target).expect("symlink");
        assert_eq!(exclusive_create(&target), EXISTS_STATUS);
        assert!(!dir.path().join("nowhere").exists());
    }

    #[test]
    fn exclusive_create_fails_with_1_in_a_missing_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(exclusive_create(&dir.path().join("no/claim")), 1);
    }
}
