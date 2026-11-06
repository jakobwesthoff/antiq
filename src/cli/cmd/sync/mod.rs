// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `sync` command.

pub mod conflict;
pub mod live;
pub mod tiers;

use crate::cli::cmd::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, SyncArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::ops::sync::{SyncReport, Synced, sync};
use antiq::report::{CommandReport, lock_note};
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &SyncArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let synced = sync(&collection, &SystemClock, args.dry_run).context("sync with the remote")?;

    match synced {
        Synced::Done(report) => {
            if ctx.json {
                out.json(Some("sync"), &report)
                    .context("print the result")?;
            } else {
                for line in lines(&report) {
                    out.line(&sanitize(&line)).context("print the result")?;
                }
                let skipped = report.skipped.iter().map(|object| {
                    let place = object
                        .path
                        .as_ref()
                        .map_or_else(|| object.id.to_string(), ToString::to_string);
                    format!("{place}: {}", object.message)
                });
                let warnings = report
                    .warnings
                    .iter()
                    .map(|warning| warning.message.clone());
                for note in warnings.chain(skipped) {
                    out.note(&format!("note: {}", sanitize(&note)))
                        .context("print the notes")?;
                }
            }
            Ok(if report.partial() { 3 } else { 0 })
        }
        Synced::Planned {
            plan,
            notes,
            holder,
        } => {
            if ctx.json {
                out.json(Some("sync"), &plan.dry_run())
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
            Ok(0)
        }
    }
}

/// What a run did, one line per step; a run that did nothing says so.
fn lines(report: &SyncReport) -> Vec<String> {
    let mut lines = Vec::new();
    for pulled in &report.pulled {
        lines.push(format!(
            "Pulled version {} of {}",
            pulled.version, pulled.id
        ));
    }
    for id in &report.listings {
        lines.push(format!("Fetched the listing of {id}"));
    }
    if lines.is_empty() && report.skipped.is_empty() {
        lines.push("Nothing to sync.".to_string());
    }
    lines
}
