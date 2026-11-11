// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq skill`: the agent skill compiled into the binary, so an agent
//! reads the skill of the antiq it runs, and `--install` writes it into a
//! skills directory. The skill needs no collection.

use crate::cli::notice;
use crate::cli::output::Output;
use crate::cli::skill_age;
use crate::cli::{Context, SkillArgs};
use antiq::report::Warning;
use anyhow::Context as _;
use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};

/// The skill's core in the Agent Skills format: every rule an agent needs
/// for every task.
pub const SKILL: &str = include_str!("../../../skills/antiq/SKILL.md");

/// The skill's parts by the name `antiq skill <part>` takes. Each is
/// `references/<name>.md` beside `SKILL.md`, which names it at the step
/// that needs it.
pub const PARTS: &[(&str, &str)] = &[
    (
        "mirroring",
        include_str!("../../../skills/antiq/references/mirroring.md"),
    ),
    (
        "import",
        include_str!("../../../skills/antiq/references/import.md"),
    ),
    (
        "damage",
        include_str!("../../../skills/antiq/references/damage.md"),
    ),
];

/// The names `antiq skill <part>` accepts, for the argument parser.
pub fn part_names() -> impl Iterator<Item = &'static str> {
    PARTS.iter().map(|(name, _)| *name)
}

pub fn run(ctx: &Context, args: &SkillArgs, out: &mut Output) -> anyhow::Result<u8> {
    match &args.install {
        Some(dir) => install(ctx, &ctx.start.join(dir), out),
        None => print(ctx, args.part.as_deref(), out),
    }
}

// =========================================================
// Printing
// =========================================================

#[derive(Serialize)]
struct Printed<'a> {
    part: Option<&'a str>,
    skill: &'a str,
    warnings: Vec<Warning>,
}

fn print(ctx: &Context, part: Option<&str>, out: &mut Output) -> anyhow::Result<u8> {
    let text = match part {
        None => SKILL,
        Some(name) => PARTS
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, text)| *text)
            .expect("the argument parser accepts only the names of parts"),
    };
    // An agent that reads the skill this way may also have an older copy
    // installed, which its runner loads in its place in later sessions.
    let warnings: Vec<Warning> = skill_age::outdated_copies(&ctx.start).into_iter().collect();
    if ctx.json {
        out.json(
            Some("skill"),
            &Printed {
                part,
                skill: text,
                warnings,
            },
        )
        .context("print the skill")?;
    } else {
        // The skill is Markdown for an agent to read as it is, so it goes
        // out unwrapped and uncoloured, also on a terminal.
        out.stdout_raw(text.as_bytes()).context("print the skill")?;
        out.notices(&notice::warnings(&warnings))
            .context("print the warnings")?;
    }
    Ok(0)
}

// =========================================================
// Installing
// =========================================================

#[derive(Serialize)]
struct Installed {
    installed: Vec<String>,
}

/// Writes the skill into `root/antiq`. `root` is a skills root such as
/// `.claude/skills`; each skill lives in a directory named after it.
fn install(ctx: &Context, root: &Path, out: &mut Output) -> anyhow::Result<u8> {
    let dir = root.join("antiq");
    let mut written = Vec::new();
    write_skill(&dir, &mut written, |file| {
        // Each file is named once it is written, so a failure half way
        // leaves a pipe naming what was written.
        if ctx.json {
            Ok(())
        } else {
            out.line(&format!("installed {}", file.display()))
        }
    })
    .with_context(|| format!("install the skill into {}", dir.display()))?;
    if ctx.json {
        let installed = written
            .iter()
            .map(|file| file.display().to_string())
            .collect();
        out.json(Some("skill"), &Installed { installed })
            .context("print the installed files")?;
    }
    Ok(0)
}

/// Writes the core and every part into `dir`, pushing each file onto
/// `written` and handing it to `announce` once it is written. A failure
/// names the path it happened at, since the message of the JSON error
/// carries no context lines.
fn write_skill(
    dir: &Path,
    written: &mut Vec<PathBuf>,
    mut announce: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let at = |path: &Path| {
        let path = path.display().to_string();
        move |error: io::Error| io::Error::new(error.kind(), format!("{path}: {error}"))
    };
    std::fs::create_dir_all(dir).map_err(at(dir))?;
    let core = dir.join("SKILL.md");
    std::fs::write(&core, SKILL).map_err(at(&core))?;
    announce(&core)?;
    written.push(core);

    // antiq owns `references/`: it is replaced whole, so a part an older
    // antiq wrote and this one no longer ships disappears. Any other file
    // in `dir` stays.
    let references = dir.join("references");
    match std::fs::remove_dir_all(&references) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(at(&references)(error));
        }
        _ => {}
    }
    std::fs::create_dir(&references).map_err(at(&references))?;
    for (name, text) in PARTS {
        let file = references.join(format!("{name}.md"));
        std::fs::write(&file, text).map_err(at(&file))?;
        announce(&file)?;
        written.push(file);
    }
    Ok(())
}
