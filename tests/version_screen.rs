// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq --version`: Archie's screen on a terminal that can show it, the
//! plain version line everywhere else.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::Stdio;
use std::time::Duration;

use support::cmd::Antiq;

const PLAIN: &str = concat!("antiq ", env!("CARGO_PKG_VERSION"), "\n");

/// The text of a rendering file, as the binary embeds it.
fn rendering(name: &str) -> String {
    let path = format!(
        "{}/assets/emblem/terminal/{name}.ansi",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// What `antiq` wrote to a pseudo-terminal of `columns` by `rows` cells and
/// `pixels` pixels (width, height), with `envs` added to a terminal that
/// draws 24-bit colour.
fn on_terminal(
    args: &[&str],
    (columns, rows): (u16, u16),
    pixels: (u16, u16),
    envs: &[(&str, &str)],
) -> String {
    let (mut master, mut slave) = (0, 0);
    let mut window = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: pixels.0,
        ws_ypixel: pixels.1,
    };
    // SAFETY: both out-pointers refer to live integers, the name and termios
    // arguments are null, and `window` lives through the call.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut window,
        )
    };
    assert_eq!(opened, 0, "openpty failed");
    // SAFETY: `openpty` returned two new descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut master = File::from(master);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        // The read ends once the command, the only holder of the slave,
        // is gone, with end of file or with an I/O error on some systems.
        while let Ok(count) = master.read(&mut chunk) {
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        bytes
    });
    let mut antiq = Antiq::new()
        .args(args)
        .env("COLORTERM", "truecolor")
        .env_remove("ZELLIJ")
        .stdout(Stdio::from(slave));
    for (key, value) in envs {
        antiq = antiq.env(key, value);
    }
    let run = antiq
        .run_within(Duration::from_secs(20))
        .expect("the command finishes on a terminal");
    assert_eq!(run.code(), 0, "{}", run.stderr);
    String::from_utf8(reader.join().expect("the reader does not panic"))
        .expect("the output is UTF-8")
        .replace("\r\n", "\n")
}

/// Whether `screen` shows `rendering` in its leading rows.
fn shows(screen: &str, rendering: &str) -> bool {
    screen
        .lines()
        .zip(rendering.lines())
        .all(|(row, emblem)| row.starts_with(emblem))
}

fn plain_text(screen: &str) -> String {
    let mut text = String::new();
    let mut chars = screen.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            text.push(character);
        }
    }
    text
}

#[test]
fn a_pipe_gets_the_plain_version_for_both_flags() {
    for flag in ["--version", "-V"] {
        let run = Antiq::new().arg(flag).run();
        assert_eq!(run.code(), 0, "{flag}");
        assert_eq!(run.stdout, PLAIN, "{flag}");
        assert_eq!(run.stderr, "", "{flag}");
    }
}

#[test]
fn a_pipe_gets_the_plain_version_whatever_the_environment_claims() {
    let run = Antiq::new()
        .arg("--version")
        .env("COLORTERM", "truecolor")
        .env("CLICOLOR_FORCE", "1")
        .run();
    assert_eq!(run.stdout, PLAIN);
}

#[test]
fn a_terminal_gets_archie_with_the_version_beside_him() {
    // 120 columns of 13 by 25 pixels on 40 rows: the shape of 0.52.
    let screen = on_terminal(&["--version"], (120, 40), (1560, 1000), &[]);
    assert!(
        shows(&screen, &rendering("archie-r052.block")),
        "{screen:?}"
    );
    let text = plain_text(&screen);
    assert!(
        text.contains(&format!("antiq  {}", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    assert!(text.contains("Your idle folders, kept like antiques"));
    assert!(text.contains("Archie, pangolin idol of obsidian and gold,"));
    assert!(text.contains("https://antiq.westhoffswelt.de"));
    assert_eq!(screen.lines().count(), 22);
}

#[test]
fn the_cell_shape_of_the_terminal_picks_the_rendering() {
    // 15 by 39 pixels a cell: 0.38, nearest to 0.39.
    let narrow = on_terminal(&["-V"], (120, 40), (1800, 1560), &[]);
    assert!(
        shows(&narrow, &rendering("archie-r039.block")),
        "{narrow:?}"
    );
}

#[test]
fn a_terminal_that_reports_no_pixels_gets_the_rendering_of_0_52() {
    let screen = on_terminal(&["-V"], (120, 40), (0, 0), &[]);
    assert!(
        shows(&screen, &rendering("archie-r052.block")),
        "{screen:?}"
    );
}

#[test]
fn a_terminal_known_to_draw_sextants_gets_them() {
    let screen = on_terminal(
        &["-V"],
        (120, 40),
        (1560, 1000),
        &[("TERM_PROGRAM", "ghostty")],
    );
    assert!(
        shows(&screen, &rendering("archie-r052.sextant")),
        "{screen:?}"
    );
}

#[test]
fn a_terminal_without_the_room_gets_the_plain_version() {
    for size in [(60, 40), (120, 22)] {
        let screen = on_terminal(&["-V"], size, (0, 0), &[]);
        assert_eq!(plain_text(&screen), PLAIN, "{size:?}");
    }
}

#[test]
fn a_terminal_without_colour_or_24_bit_colour_gets_the_plain_version() {
    for (name, envs) in [
        ("no colour", &[("NO_COLOR", "1")][..]),
        ("no 24-bit colour", &[("COLORTERM", "")][..]),
    ] {
        let screen = on_terminal(&["-V"], (120, 40), (0, 0), envs);
        assert_eq!(plain_text(&screen), PLAIN, "{name}");
    }
}

#[test]
fn json_gets_the_plain_version_on_a_terminal() {
    let screen = on_terminal(&["--json", "--version"], (120, 40), (0, 0), &[]);
    assert_eq!(plain_text(&screen), PLAIN);
}
