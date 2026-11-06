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

/// One action of a dry run. A terminal gets the magenta tag
/// `○ would <verb>` and `rest` behind it. A pipe keeps `Would <verb>
/// <rest>`, the line scripts read.
pub fn line(env: &Env, verb: &str, rest: &Rich) -> String {
    if !env.rich {
        return format!("Would {verb} {}", rest.to_plain());
    }
    let symbols = Symbols::for_env(env);
    let tag = paint(
        env,
        theme::MAGENTA.bold(),
        &format!("{} would {verb}", symbols.dry_run),
    );
    let rest = flow(env, rest, Style::new(), usize::MAX).join(" ");
    format!("{tag}  {rest}")
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
