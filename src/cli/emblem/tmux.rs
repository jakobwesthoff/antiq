// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What tmux knows about the terminal a pane is shown on.
//!
//! Inside tmux the environment and the window size describe tmux, not the
//! terminal a client attached with. tmux knows that terminal: its answer to
//! XTVERSION, which names the terminal, and the size of its cells in pixels.
//! One `tmux display-message` asks for both.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long tmux gets to answer. It answers at once or not at all.
const ANSWER_WITHIN: Duration = Duration::from_millis(500);

/// What tmux is asked, for `display-message -p`: the client terminal's
/// answer to XTVERSION and the width and height of its cells in pixels. The
/// answer to XTVERSION holds blanks, so tabs keep the fields apart.
const FORMAT: &str = "#{client_termtype}\t#{client_cell_width}\t#{client_cell_height}";

/// What tmux said about the client of the pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientTerminal {
    /// The client terminal's answer to XTVERSION, such as `ghostty 1.3.2`.
    /// `None` when it gave none.
    pub termtype: Option<String>,
    /// Width and height of one cell in pixels, `None` when tmux knows no
    /// size.
    pub cell_pixels: Option<(u32, u32)>,
}

/// Reads what tmux printed for [`FORMAT`]. A field that is missing, empty or
/// not a positive number counts as unknown.
pub fn parse(answer: &str) -> ClientTerminal {
    let mut fields = answer.trim_end_matches('\n').split('\t');
    let termtype = fields
        .next()
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let mut pixels = || fields.next().and_then(|text| text.parse::<u32>().ok());
    let cell_pixels = pixels().zip(pixels()).filter(|(w, h)| *w > 0 && *h > 0);
    ClientTerminal {
        termtype,
        cell_pixels,
    }
}

/// Asks tmux about the client of the pane this process runs in. Any failure
/// is an unknown answer: no tmux program, an error, no answer in time.
pub fn ask() -> Option<ClientTerminal> {
    ask_program("tmux")
}

/// `ask` of the tmux that `program` names.
fn ask_program(program: impl AsRef<std::ffi::OsStr>) -> Option<ClientTerminal> {
    let mut child = Command::new(program)
        .args(["display-message", "-p", FORMAT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None if started.elapsed() >= ANSWER_WITHIN => {
                // A tmux that never answers is not waited for again.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    if !status.success() {
        return None;
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
    Some(parse(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_names_the_terminal_and_its_cell() {
        assert_eq!(
            parse("ghostty 1.3.2-mouse-fix\t15\t39\n"),
            ClientTerminal {
                termtype: Some("ghostty 1.3.2-mouse-fix".into()),
                cell_pixels: Some((15, 39)),
            }
        );
    }

    #[test]
    fn a_client_that_gave_no_answer_has_no_termtype() {
        let got = parse("\t8\t16\n");
        assert_eq!(got.termtype, None);
        assert_eq!(got.cell_pixels, Some((8, 16)));
    }

    #[test]
    fn a_cell_of_zero_or_a_garbled_field_is_unknown() {
        for answer in [
            "x\t0\t0\n",
            "x\t\t\n",
            "x\tabc\t39\n",
            "x\t15\n",
            "x\n",
            "",
            "x\t-1\t3\n",
        ] {
            assert_eq!(parse(answer).cell_pixels, None, "{answer:?}");
        }
    }

    /// A script that stands in for tmux, in a directory of its own.
    fn fake_tmux(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let path = dir.path().join("tmux");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the script is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the script becomes executable");
        (dir, path)
    }

    #[test]
    fn tmux_is_asked_for_the_format_and_its_answer_is_read() {
        let (_dir, path) = fake_tmux(
            r#"[ "$1 $2" = "display-message -p" ] || exit 3
printf 'ghostty 1.3.2\t15\t39\n'"#,
        );
        assert_eq!(
            ask_program(&path),
            Some(ClientTerminal {
                termtype: Some("ghostty 1.3.2".into()),
                cell_pixels: Some((15, 39)),
            })
        );
    }

    #[test]
    fn a_tmux_that_fails_or_is_missing_gives_no_answer() {
        let (_dir, path) = fake_tmux("exit 1");
        assert_eq!(ask_program(&path), None);
        assert_eq!(ask_program("/nonexistent/tmux"), None);
    }

    #[test]
    fn a_tmux_that_does_not_answer_in_time_is_given_up_on() {
        let (_dir, path) = fake_tmux("exec sleep 30");
        let started = Instant::now();
        assert_eq!(ask_program(&path), None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_blank_inside_the_termtype_stays() {
        assert_eq!(
            parse("iTerm2 3.6.7\t0\t0").termtype.as_deref(),
            Some("iTerm2 3.6.7")
        );
    }
}
