// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `reconcile` command.

use crate::cli::cmd::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, ReconcileArgs};
use antiq::brake::BrakeInput;
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::ops::reconcile::{ReconcileReport, ReconcileRequest, Reconciled, reconcile};
use antiq::report::{CommandReport, lock_note};
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &ReconcileArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let request = ReconcileRequest {
        prune: args.prune,
        force: args.force,
        dry_run: args.dry_run,
    };
    let mut interaction = ctx.interaction();
    let brake = BrakeInput {
        force: args.force,
        interaction: interaction.as_mut(),
    };
    let reconciled = reconcile(
        &collection,
        &SystemClock,
        &antiq::remote::open,
        brake,
        request,
    )
    .context("reconcile the index")?;

    match reconciled {
        Reconciled::Done(mut report) => {
            let stop = report.stop.take();
            // A run that stopped at the remote tells it by the error alone
            // under `--json`, as the envelope is the one document it prints.
            if ctx.json && stop.is_none() {
                out.json(Some("reconcile"), &report)
                    .context("print the result")?;
            } else if !ctx.json {
                print_report(&report, out)?;
            }
            if let Some(stop) = stop {
                return Err(anyhow::Error::new(stop).context("use the remote"));
            }
            Ok(if report.partial() { 3 } else { 0 })
        }
        Reconciled::Planned {
            plan,
            notes,
            holder,
            stop,
        } => {
            if ctx.json && stop.is_none() {
                out.json(Some("reconcile"), &plan.dry_run())
                    .context("print the plan")?;
            } else if !ctx.json {
                for line in plan.lines() {
                    out.line(&sanitize(&line)).context("print the plan")?;
                }
                let held = holder.as_ref().map(lock_note);
                for note in notes.iter().chain(held.iter()) {
                    out.note(&format!("note: {}", sanitize(&note.message)))
                        .context("print the notes")?;
                }
            }
            if let Some(stop) = stop {
                return Err(anyhow::Error::new(stop).context("use the remote"));
            }
            Ok(0)
        }
    }
}

/// The console form of a finished run.
fn print_report(report: &ReconcileReport, out: &mut Output) -> anyhow::Result<()> {
    out.line(&format!(
        "Rebuilt the index: {} written, {} removed",
        report.index.written, report.index.removed
    ))
    .context("print the result")?;
    if let Some(remote) = &report.remote {
        out.line(&format!(
            "Remote: {} pulled, {} removed",
            remote.pulled, remote.removed
        ))
        .context("print the result")?;
    }
    for piece in &report.marked {
        out.line(&format!(
            "Marked missing on the remote: {}",
            sanitize(&piece.path)
        ))
        .context("print the result")?;
    }
    for piece in &report.pruned {
        out.line(&format!("Dropped: {}", sanitize(&piece.path)))
            .context("print the result")?;
    }
    for piece in &report.unmarked {
        out.line(&format!(
            "Archive back on the remote: {}",
            sanitize(&piece.path)
        ))
        .context("print the result")?;
    }
    let skipped = report
        .index
        .skipped
        .iter()
        .map(|object| object.reason.clone());
    let warnings = report
        .warnings
        .iter()
        .map(|warning| warning.message.clone());
    let changed = report.changed_elsewhere.iter().map(|changed| {
        format!(
            "`{}` has version {} from {}, which this machine has not taken",
            changed.path, changed.version, changed.machine
        )
    });
    for note in warnings.chain(skipped).chain(changed) {
        out.note(&format!("note: {}", sanitize(&note)))
            .context("print the notes")?;
    }
    Ok(())
}
