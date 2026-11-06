// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lines of a dry run (`-n`): a tag in front of each action and a
//! footer that says nothing happened.

use super::env::Env;
use super::render::{available, flow, paint};
use super::rich::Rich;
use super::symbols::Symbols;
use super::theme;
use anstyle::Style;
use unicode_width::UnicodeWidthStr;

/// Verbs of two words, which `split` keeps together.
const TWO_WORD_VERBS: &[&str] = &["leave out"];

/// Splits a `Would <verb> <rest>` line, the form the library and the
/// commands write a dry run's actions in. `None` for any other line.
pub fn split(line: &str) -> Option<(&str, &str)> {
    let action = line.strip_prefix("Would ")?;
    for verb in TWO_WORD_VERBS {
        if let Some(rest) = action.strip_prefix(verb).and_then(|r| r.strip_prefix(' ')) {
            return Some((verb, rest));
        }
    }
    Some(action.split_once(' ').unwrap_or((action, "")))
}

/// One action of a dry run. A terminal gets the magenta tag
/// `○ would <verb>` and `rest` behind it, the verb padded to
/// `verb_width` columns so that the rests of several actions line up. A
/// pipe keeps `Would <verb> <rest>`, the line scripts read. `rest` is
/// written as it is, as the spaces of a path in it are part of the path.
pub fn line(env: &Env, verb: &str, verb_width: usize, rest: &str) -> String {
    if !env.rich {
        return format!("Would {verb} {rest}");
    }
    let symbols = Symbols::for_env(env);
    let tag = paint(
        env,
        theme::MAGENTA.bold(),
        &format!("{} would {verb}", symbols.dry_run),
    );
    let fill = " ".repeat(verb_width.saturating_sub(verb.width()));
    format!("{tag}{fill}  {rest}")
}

/// The closing lines of a dry run: an empty line, then "Dry run: nothing
/// was `nothing_was`. Run again without -n to `rerun`." A pipe gets
/// none, since the "Would" lines already say it. `rerun` is markup as
/// `Rich::parse` reads it.
pub fn footer(env: &Env, nothing_was: &str, rerun: &str) -> Vec<String> {
    if !env.rich {
        return Vec::new();
    }
    let label = "Dry run";
    let hang = label.width() + 2;
    let body = Rich::parse(&format!(
        "nothing was {nothing_was}. Run again without `-n` to {rerun}."
    ));
    let mut lines = vec![String::new()];
    for (index, text) in flow(env, &body, Style::new(), available(env, hang))
        .iter()
        .enumerate()
    {
        if index == 0 {
            let head = paint(env, theme::MAGENTA.bold(), label);
            lines.push(format!("{head}: {text}"));
        } else {
            lines.push(format!("{}{text}", " ".repeat(hang)));
        }
    }
    lines
}
