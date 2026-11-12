// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Blocks as lines of text, in the rich form of a terminal and the plain
//! form of a pipe.

use super::block::{Block, Kind, Para};
use super::env::Env;
use super::rich::{Rich, Role, Word};
use super::symbols::Symbols;
use super::theme;
use anstyle::Style;
use antiq::shown::escape_controls;
use unicode_width::UnicodeWidthStr;

/// Prose is never wrapped wider than this, however wide the terminal is.
const MAX_WIDTH: usize = 100;

/// Below this width wrapping is off: a narrow column of fragments reads
/// worse than lines the terminal wraps itself.
const MIN_WRAP_WIDTH: usize = 40;

/// `blocks` one after the other. A terminal gets an empty line between
/// blocks; a pipe gets the plain lines of the blocks that have one.
pub fn render_blocks(blocks: &[Block], env: &Env) -> Vec<String> {
    let mut lines = Vec::new();
    for block in blocks {
        let rendered = if env.rich {
            block.render(env)
        } else {
            block.render_plain()
        };
        if rendered.is_empty() {
            continue;
        }
        if env.rich && !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(rendered);
    }
    lines
}

/// A piece of a group of output that belongs together, such as an error,
/// the table it refers to and the info after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Block(Box<Block>),
    /// Lines that only a terminal shows, such as a caption and a table. A
    /// pipe gets the data in the plain lines of the blocks.
    Terminal(Vec<String>),
}

/// `parts` one after the other, an empty line between them on a terminal.
pub fn render_parts(parts: &[Part], env: &Env) -> Vec<String> {
    let mut lines = Vec::new();
    for part in parts {
        let rendered = match part {
            Part::Block(block) => render_blocks(std::slice::from_ref(&**block), env),
            Part::Terminal(shown) if env.rich => shown.clone(),
            Part::Terminal(_) => Vec::new(),
        };
        if rendered.is_empty() {
            continue;
        }
        if env.rich && !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(rendered);
    }
    lines
}

pub(in crate::cli) fn paint(env: &Env, style: Style, text: &str) -> String {
    if env.color && !text.is_empty() {
        format!("{style}{text}{style:#}")
    } else {
        text.to_string()
    }
}

fn role_style(role: Role, base: Style) -> Style {
    match role {
        Role::Plain => base,
        Role::Strong => theme::BOLD,
    }
}

fn word_width(word: &Word) -> usize {
    word.iter().map(|(_, text)| text.width()).sum()
}

/// Fills lines with words up to `width` columns. A word wider than the
/// line stands alone on one.
fn wrap(words: &[Word], width: usize) -> Vec<Vec<&Word>> {
    let mut lines: Vec<Vec<&Word>> = Vec::new();
    let mut used = 0;
    for word in words {
        let columns = word_width(word);
        match lines.last_mut() {
            Some(line) if used + 1 + columns <= width => {
                line.push(word);
                used += 1 + columns;
            }
            _ => {
                lines.push(vec![word]);
                used = columns;
            }
        }
    }
    lines
}

/// The painted lines of `text` wrapped to `width` columns.
pub(in crate::cli) fn flow(env: &Env, text: &Rich, base: Style, width: usize) -> Vec<String> {
    wrap(&text.words(), width)
        .iter()
        .map(|line| paint_line(env, line, base))
        .collect()
}

/// `words` joined by single spaces, with one escape sequence around each
/// run of the same style. A space between two styles is not styled.
fn paint_line(env: &Env, words: &[&Word], base: Style) -> String {
    let mut runs: Vec<(Style, String)> = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let mut gap = index > 0;
        for (role, text) in word.iter() {
            let style = role_style(*role, base);
            match runs.last_mut() {
                Some((last, run)) if *last == style => {
                    if gap {
                        run.push(' ');
                    }
                    run.push_str(text);
                }
                _ => {
                    if gap {
                        runs.push((Style::new(), " ".to_string()));
                    }
                    runs.push((style, text.clone()));
                }
            }
            gap = false;
        }
    }
    runs.iter()
        .map(|(style, text)| paint(env, *style, text))
        .collect()
}

/// How wide the text of a line may be after `indent` columns.
pub(in crate::cli) fn available(env: &Env, indent: usize) -> usize {
    if env.width < MIN_WRAP_WIDTH {
        usize::MAX
    } else {
        env.width.min(MAX_WIDTH).saturating_sub(indent)
    }
}

fn badge_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Error => "ERROR",
        Kind::Warning => "WARNING",
        Kind::Info => "INFO",
        Kind::Hint => "HINT",
        Kind::DryRun => "DRY RUN",
    }
}

fn badge_text(kind: Kind, symbols: &Symbols) -> String {
    let symbol = match kind {
        Kind::Error => symbols.error,
        Kind::Warning => symbols.warning,
        Kind::Info => symbols.info,
        Kind::Hint => symbols.hint,
        Kind::DryRun => symbols.dry_run,
    };
    format!(" {symbol} {} ", badge_word(kind))
}

/// The left edge of the lines of a block.
struct Bar {
    painted: String,
}

impl Bar {
    /// The bar with `rest` after it, or the bar alone for an empty rest.
    fn line(&self, rest: &str) -> String {
        if rest.is_empty() {
            self.painted.clone()
        } else {
            format!("{} {rest}", self.painted)
        }
    }
}

impl Block {
    /// The block as a terminal shows it, one string per line.
    pub fn render(&self, env: &Env) -> Vec<String> {
        let symbols = Symbols::for_env(env);
        let badge = badge_text(self.kind, &symbols);
        let bar = Bar {
            painted: paint(env, theme::bar(self.kind), symbols.bar),
        };
        let mut lines = Vec::new();

        match self.kind {
            Kind::Error => {
                lines.push(self.badge_row(env, &bar, &badge));
                for line in flow(
                    env,
                    &self.title.clone().capitalised(),
                    theme::BOLD,
                    available(env, 2),
                ) {
                    lines.push(bar.line(&line));
                }
                for line in &self.body {
                    lines.push(bar.line(&format!("  {line}")));
                }
                self.section(env, &bar, "How to fix it", &self.how, &mut lines);
                self.section(env, &bar, &self.said_title(), &self.said, &mut lines);
                let steps: Vec<Para> = self
                    .doing
                    .iter()
                    .map(|phrase| Para::Text(Rich::new().text(&format!("Attempting to {phrase}."))))
                    .collect();
                self.section(env, &bar, "What antiq was doing", &steps, &mut lines);
            }
            Kind::Hint | Kind::DryRun => {
                let hang = badge.width() + 1;
                let first = paint(env, theme::badge(self.kind), &badge);
                let body = flow(env, &self.title, Style::new(), available(env, hang));
                self.push_hanging(&mut lines, &first, hang, &body);
            }
            Kind::Warning | Kind::Info => {
                lines.push(self.badge_row(env, &bar, &badge));
                for line in flow(
                    env,
                    &self.title.clone().capitalised(),
                    theme::BOLD,
                    available(env, 2),
                ) {
                    lines.push(bar.line(&line));
                }
                // The sections of the block (listed items, explanation, fix)
                // are set apart by an empty bar line, and only the ones the
                // block has.
                let mut previous: Option<bool> = None;
                for paragraph in &self.text {
                    let listed = matches!(paragraph, Para::Pre(_));
                    if !matches!(paragraph, Para::Blank) {
                        if previous != Some(listed) {
                            lines.push(bar.line(""));
                        }
                        previous = Some(listed);
                    }
                    self.push_paragraph(env, &bar, paragraph, &mut lines);
                }
                if let Some(fix) = &self.fix {
                    lines.push(bar.line(""));
                    let arrow = paint(env, theme::bar(self.kind), symbols.hint);
                    // Later lines of the fix start under the text after the arrow.
                    let hang = symbols.hint.width() + 1;
                    let fixed = flow(env, fix, Style::new(), available(env, hang + 2));
                    for (index, line) in fixed.iter().enumerate() {
                        if index == 0 {
                            lines.push(bar.line(&format!("{arrow} {line}")));
                        } else {
                            lines.push(bar.line(&format!("{}{line}", " ".repeat(hang))));
                        }
                    }
                }
            }
        }
        lines
    }

    /// The row that holds the badge alone. Without colour the badge has no
    /// background to set it apart, so the bar of the block stands in front
    /// of it and the row reads as part of the block.
    fn badge_row(&self, env: &Env, bar: &Bar, badge: &str) -> String {
        if env.color {
            paint(env, theme::badge(self.kind), badge)
        } else {
            bar.line(badge.trim())
        }
    }

    /// `body` lines after `first`, the later ones indented by `hang`.
    fn push_hanging(&self, lines: &mut Vec<String>, first: &str, hang: usize, body: &[String]) {
        for (index, line) in body.iter().enumerate() {
            if index == 0 {
                lines.push(format!("{first} {line}"));
            } else {
                lines.push(format!("{}{line}", " ".repeat(hang)));
            }
        }
        if body.is_empty() {
            lines.push(first.trim_end().to_string());
        }
    }

    fn section(
        &self,
        env: &Env,
        bar: &Bar,
        heading: &str,
        paragraphs: &[Para],
        lines: &mut Vec<String>,
    ) {
        if paragraphs.is_empty() {
            return;
        }
        lines.push(bar.line(""));
        lines.push(bar.line(&paint(env, theme::label(self.kind), heading)));
        for paragraph in paragraphs {
            self.push_paragraph(env, bar, paragraph, lines);
        }
    }

    fn push_paragraph(&self, env: &Env, bar: &Bar, paragraph: &Para, lines: &mut Vec<String>) {
        match paragraph {
            Para::Text(text) => {
                for line in flow(env, text, Style::new(), available(env, 4)) {
                    lines.push(bar.line(&format!("  {line}")));
                }
            }
            Para::Pre(line) => lines.push(bar.line(&format!(
                "  {}",
                paint(env, theme::BOLD, &escape_controls(line))
            ))),
            Para::Blank => lines.push(bar.line("")),
        }
    }

    fn said_title(&self) -> String {
        match &self.said_by {
            Some(program) => format!("What {program} said"),
            None => "Run log".to_string(),
        }
    }

    fn said_label(&self) -> String {
        match &self.said_by {
            Some(program) => format!("{program} said"),
            None => "log".to_string(),
        }
    }

    fn plain_title(&self) -> String {
        self.message
            .clone()
            .unwrap_or_else(|| self.title.to_plain())
    }

    /// The block as a pipe on stderr gets it: lines a script can grep.
    /// Info and hint exist only for a person at a terminal.
    pub fn render_plain(&self) -> Vec<String> {
        if let Some(line) = &self.pipe {
            return vec![line.clone()];
        }
        let mut lines = Vec::new();
        match self.kind {
            Kind::Error => {
                lines.push(format!("error: {}", self.plain_title()));
                lines.extend(self.body.iter().map(|line| format!("  {line}")));
                labelled(&mut lines, "fix", &self.how);
                labelled(&mut lines, &self.said_label(), &self.said);
                for candidate in &self.candidates {
                    lines.push(format!("    candidate: {candidate}"));
                }
                for phrase in &self.doing {
                    lines.push(format!("  while attempting to {phrase}"));
                }
            }
            Kind::Warning => {
                lines.push(format!("warning: {}", self.plain_title()));
                for paragraph in &self.text {
                    if let Some(text) = plain_paragraph(paragraph) {
                        lines.push(format!("  {text}"));
                    }
                }
                if let Some(fix) = &self.fix {
                    lines.push(format!("  fix: {}", fix.to_plain()));
                }
            }
            Kind::Info | Kind::Hint | Kind::DryRun => {}
        }
        lines
    }
}

fn plain_paragraph(paragraph: &Para) -> Option<String> {
    match paragraph {
        Para::Text(text) => Some(text.to_plain()),
        Para::Pre(line) => Some(escape_controls(line)),
        Para::Blank => None,
    }
}

/// The paragraphs of a section behind `label:`, the later ones aligned
/// under the first.
fn labelled(lines: &mut Vec<String>, label: &str, paragraphs: &[Para]) {
    let indent = " ".repeat(label.len() + 4);
    for (index, text) in paragraphs.iter().filter_map(plain_paragraph).enumerate() {
        if index == 0 {
            lines.push(format!("  {label}: {text}"));
        } else {
            lines.push(format!("{indent}{text}"));
        }
    }
}
