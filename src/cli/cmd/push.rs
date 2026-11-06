// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `push` command: mirrors the collection to the target.

use crate::cli::output::Output;
use crate::cli::{Context, PushArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::push::{PushOutcome, PushRequest, push};
use anyhow::Context as _;

pub fn run(ctx: &Context, args: &PushArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let request = PushRequest {
        init: args.init,
        force: args.force,
        if_reachable: args.if_reachable,
        dry_run: args.dry_run,
    };
    let outcome = push(&collection, &request, &SystemClock)
        .map_err(antiq::Error::from)
        .context("push the antiq collection")?;

    match outcome {
        PushOutcome::HeldDryRun(held) => {
            if ctx.json {
                out.json(Some("push"), &held).context("print the result")?;
            } else {
                for line in held.lines() {
                    out.line(&line).context("print the result")?;
                }
            }
        }
        PushOutcome::Report(report) => {
            if ctx.json {
                out.json(Some("push"), &*report)
                    .context("print the result")?;
            } else {
                // An unreachable target is not a failure under
                // `--if-reachable`, so its reason is a message and stdout
                // stays empty.
                if let Some(reason) = &report.reason {
                    out.note(&format!("note: {reason}"))
                        .context("print the result")?;
                }
                for repair in &report.repairs {
                    out.note(&format!("note: repaired: {}", repair.describe()))
                        .context("print the result")?;
                }
                for warning in &report.warnings {
                    out.note(&format!("note: {}", warning.message))
                        .context("print the result")?;
                }
                for note in report.note_lines() {
                    out.note(&note).context("print the result")?;
                }
                for line in report.lines() {
                    out.line(&line).context("print the result")?;
                }
            }
        }
    }
    Ok(0)
}
