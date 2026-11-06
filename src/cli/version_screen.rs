// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The screen `antiq --version` shows on a terminal: Archie's emblem with
//! the version and a few lines about antiq beside it. Anywhere else the
//! version is the one line clap prints.

use super::emblem::{self, Rendering, TerminalFacts, visible_width};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const MAGENTA: &str = "\x1b[35m";
const GOLD: &str = "\x1b[38;2;251;213;59m";

/// Columns between the emblem and the text.
const GAP: usize = 3;

/// The project page.
const URL: &str = "https://antiq.westhoffswelt.de";

/// What the text beside the emblem says.
pub struct About<'a> {
    pub version: &'a str,
    pub tagline: &'a str,
    pub url: &'a str,
}

impl About<'_> {
    /// This crate: its version, its description and the project page.
    pub fn of_antiq() -> About<'static> {
        About {
            version: env!("CARGO_PKG_VERSION"),
            tagline: env!("CARGO_PKG_DESCRIPTION"),
            url: URL,
        }
    }

    /// The lines beside the emblem, with their colour codes.
    fn text(&self) -> Vec<String> {
        let About {
            version,
            tagline,
            url,
        } = self;
        vec![
            format!("{BOLD}{MAGENTA}antiq{RESET}  {BOLD}{version}{RESET}"),
            String::new(),
            format!("{DIM}{tagline}{RESET}"),
            String::new(),
            format!(
                "{GOLD}{ITALIC}Archie{RESET}{DIM}{ITALIC}, pangolin idol of obsidian and gold,{RESET}"
            ),
            format!("{DIM}{ITALIC}keeps watch over your collection.{RESET}"),
            String::new(),
            format!("{DIM}{url}{RESET}"),
        ]
    }

    /// Columns of the widest line of the text.
    fn text_width(&self) -> usize {
        self.text()
            .iter()
            .map(|line| visible_width(line))
            .max()
            .unwrap_or(0)
    }

    /// The screen for a terminal described by `facts`, or `None` when the
    /// terminal should get the plain version.
    pub fn screen(&self, facts: &TerminalFacts) -> Option<Vec<String>> {
        let rendering = emblem::for_terminal(facts, GAP + self.text_width())?;
        Some(self.beside(rendering))
    }

    /// The rows of `rendering` with the text to the right of them, centred
    /// on the rows. A row without text ends where the emblem ends.
    fn beside(&self, rendering: &Rendering) -> Vec<String> {
        let text = self.text();
        let top = rendering.height().saturating_sub(text.len()) / 2;
        rendering
            .lines
            .iter()
            .enumerate()
            .map(
                |(row, line)| match row.checked_sub(top).and_then(|i| text.get(i)) {
                    Some(text) if !text.is_empty() => {
                        format!("{line}{RESET}{}{text}", " ".repeat(GAP))
                    }
                    _ => line.clone(),
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::emblem::Glyphs;

    fn about() -> About<'static> {
        About {
            version: "1.2.3",
            tagline: "Keeps things",
            url: "https://example.org",
        }
    }

    fn emblem_of(width: usize, height: usize) -> Rendering {
        let row = format!("\x1b[38;2;9;9;9m{}\x1b[0m", "#".repeat(width));
        Rendering::new(52, Glyphs::Block, &vec![row; height].join("\n"))
    }

    fn plain(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn the_text_is_centred_on_the_rows_of_the_emblem() {
        let rows = about().beside(&emblem_of(10, 22));
        assert_eq!(rows.len(), 22);
        let shown: Vec<String> = rows.iter().map(|row| plain(row)).collect();
        // Eight lines on 22 rows: seven rows above, seven below.
        assert!(shown[..7].iter().all(|row| row == &"#".repeat(10)));
        assert_eq!(shown[7], "##########   antiq  1.2.3");
        assert_eq!(shown[8], "##########");
        assert_eq!(shown[9], "##########   Keeps things");
        assert_eq!(
            shown[11],
            "##########   Archie, pangolin idol of obsidian and gold,"
        );
        assert_eq!(shown[12], "##########   keeps watch over your collection.");
        assert_eq!(shown[14], "##########   https://example.org");
        assert!(shown[15..].iter().all(|row| row == &"#".repeat(10)));
    }

    #[test]
    fn the_emblem_is_reset_before_the_gap_and_the_text_carries_its_codes() {
        let rows = about().beside(&emblem_of(4, 22));
        assert_eq!(
            rows[7],
            "\x1b[38;2;9;9;9m####\x1b[0m\x1b[0m   \x1b[1m\x1b[35mantiq\x1b[0m  \x1b[1m1.2.3\x1b[0m"
        );
        assert_eq!(
            rows[9],
            "\x1b[38;2;9;9;9m####\x1b[0m\x1b[0m   \x1b[2mKeeps things\x1b[0m"
        );
        assert!(rows[11].ends_with(
            "\x1b[38;2;251;213;59m\x1b[3mArchie\x1b[0m\x1b[2m\x1b[3m, pangolin idol of obsidian and gold,\x1b[0m"
        ));
        assert!(rows[12].ends_with("\x1b[2m\x1b[3mkeeps watch over your collection.\x1b[0m"));
        assert!(rows[14].ends_with("\x1b[2mhttps://example.org\x1b[0m"));
        // A row without text is the emblem's row, unchanged.
        assert_eq!(rows[0], "\x1b[38;2;9;9;9m####\x1b[0m");
    }

    #[test]
    fn an_emblem_shorter_than_the_text_puts_the_text_at_the_top() {
        let rows = about().beside(&emblem_of(4, 5));
        assert_eq!(rows.len(), 5);
        assert!(plain(&rows[0]).ends_with("antiq  1.2.3"));
    }

    #[test]
    fn the_text_is_as_wide_as_its_widest_line() {
        assert_eq!(
            about().text_width(),
            "Archie, pangolin idol of obsidian and gold,".len()
        );
    }

    #[test]
    fn this_crates_text_names_its_version_and_description() {
        let about = About::of_antiq();
        assert_eq!(about.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(about.tagline, "Your idle folders, kept like antiques");
    }

    #[test]
    fn the_url_is_the_one_of_the_project_page() {
        let config = include_str!("../../docs/pages/config.yaml");
        let line = config
            .lines()
            .find_map(|line| line.strip_prefix("url: "))
            .expect("the page config names its url");
        assert_eq!(line.trim(), URL);
    }

    #[test]
    fn the_screen_of_a_terminal_that_cannot_show_it_is_none() {
        let facts = TerminalFacts {
            terminal: false,
            color: true,
            truecolor: true,
            columns: 200,
            rows: 60,
            cell: None,
            glyphs: Glyphs::Block,
        };
        assert_eq!(About::of_antiq().screen(&facts), None);
    }

    #[test]
    fn the_screen_of_a_roomy_terminal_has_the_rows_of_the_emblem() {
        let facts = TerminalFacts {
            terminal: true,
            color: true,
            truecolor: true,
            columns: 120,
            rows: 40,
            cell: None,
            glyphs: Glyphs::Block,
        };
        let rows = About::of_antiq()
            .screen(&facts)
            .expect("room for the emblem");
        assert_eq!(rows.len(), 22);
        assert!(plain(&rows[7]).ends_with(concat!("antiq  ", env!("CARGO_PKG_VERSION"))));
    }
}
