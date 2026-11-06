// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `reconcile` command.

use crate::cli::cmd::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, ReconcileArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::ops::reconcile::{Reconciled, reconcile};
use antiq::report::{CommandReport, lock_note};
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &ReconcileArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let reconciled = reconcile(&collection, &SystemClock, args.prune, args.dry_run)
        .context("reconcile the index")?;

    match reconciled {
        Reconciled::Done(report) => {
            if ctx.json {
                out.json(Some("reconcile"), &report)
                    .context("print the result")?;
            } else {
                out.line(&format!(
                    "Rebuilt the index: {} written, {} removed",
                    report.index.written, report.index.removed
                ))
                .context("print the result")?;
                let skipped = report.index.skipped.iter().map(|object| &object.reason);
                let warnings = report.warnings.iter().map(|warning| &warning.message);
                for note in warnings.chain(skipped) {
                    out.note(&format!("note: {}", sanitize(note)))
                        .context("print the notes")?;
                }
            }
            Ok(if report.partial() { 3 } else { 0 })
        }
        Reconciled::Planned {
            plan,
            notes,
            holder,
        } => {
            if ctx.json {
                out.json(Some("reconcile"), &plan.dry_run())
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
