// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` command, and the code `unpin` shares with it.

use crate::cli::cmd::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, PieceArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::ops::pin::{Direction, PinReport, Pinned, run as pin_piece};
use antiq::report::lock_note;
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &PieceArgs, out: &mut Output) -> anyhow::Result<u8> {
    run_direction(ctx, args, out, Direction::Pin)
}

/// The directory that relative piece references start at: the canonical
/// form of `-C`, which is the directory the collection is found from, else
/// the current directory.
fn reference_directory(directory: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    match directory {
        Some(directory) => std::fs::canonicalize(directory)
            .with_context(|| format!("resolve {}", directory.display())),
        None => std::env::current_dir().context("find the current directory"),
    }
}

pub(super) fn run_direction(
    ctx: &Context,
    args: &PieceArgs,
    out: &mut Output,
    direction: Direction,
) -> anyhow::Result<u8> {
    let name = match direction {
        Direction::Pin => "pin",
        Direction::Unpin => "unpin",
    };
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let cwd = reference_directory(ctx.directory.as_ref())?;
    let pinned = pin_piece(
        &collection,
        &SystemClock,
        &cwd,
        &args.piece,
        direction,
        args.dry_run,
    )
    .with_context(|| format!("{name} the piece"))?;

    match pinned {
        Pinned::Done(report) => {
            if ctx.json {
                out.json(Some(name), &report).context("print the result")?;
            } else {
                out.line(&sentence(&report, direction))
                    .context("print the result")?;
                for warning in &report.warnings {
                    out.note(&format!("note: {}", sanitize(&warning.message)))
                        .context("print the notes")?;
                }
            }
        }
        Pinned::Planned {
            plan,
            notes,
            holder,
        } => {
            if ctx.json {
                out.json(Some(name), &plan.dry_run())
                    .context("print the plan")?;
            } else {
                for line in plan.lines() {
                    out.line(&sanitize(&line)).context("print the plan")?;
                }
                let held = holder.as_ref().map(lock_note);
                for note in notes.iter().chain(held.iter()) {
                    out.note(&format!("note: {}", sanitize(&note.message)))
                        .context("print the notes")?;
                }
            }
        }
    }
    Ok(0)
}

/// What a finished run says.
fn sentence(report: &PinReport, direction: Direction) -> String {
    let path = sanitize(&report.path.to_string());
    match (direction, report.changed) {
        (Direction::Pin, true) => format!("Pinned {path}"),
        (Direction::Pin, false) => format!("{path} is already pinned"),
        (Direction::Unpin, true) => format!("Unpinned {path}"),
        (Direction::Unpin, false) => format!("{path} is not pinned"),
    }
}
