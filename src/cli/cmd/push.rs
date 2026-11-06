// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `push` command: mirrors the collection to the target.

use super::progress;
use crate::cli::console::cards::{Count, items};
use crate::cli::console::env::Env;
use crate::cli::console::live::observed;
use crate::cli::console::render::{available, flow, paint};
use crate::cli::console::rich::Rich;
use crate::cli::console::symbols::Symbols;
use crate::cli::console::theme;
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::table::{pad, pad_left, sanitize};
use crate::cli::{Context, PushArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::push::{PushOutcome, PushReport, PushRequest, push, push_observed};
use antiq::progress::Operation;
use anyhow::Context as _;
use unicode_width::UnicodeWidthStr;

/// A piece is sent as the files it creates and then as the files it deletes.
pub(super) const COUNTS: &[(Operation, Count)] = &[
    (
        Operation::Storing,
        Count {
            text: |done, total| items(done, total, "created"),
            percent: true,
        },
    ),
    (
        Operation::Removing,
        Count {
            text: |done, total| items(done, total, "deleted"),
            percent: true,
        },
    ),
];

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
    // On a terminal the pieces show as cards while they are sent, and the
    // lines they leave behind replace the table of the report.
    let mut cards = None;
    let result = if progress::is_live(ctx, args.dry_run, out) {
        let target = collection.config().target.url.clone().unwrap_or_default();
        let header = format!(
            "Pushing to {}",
            paint(&out.stdout_env(), theme::BOLD, &sanitize(&target))
        );
        let cursor = out.stdout_env().cursor;
        let setup = progress::setup(
            COUNTS,
            ctx,
            out,
            Some(header),
            Box::new(|stamp, _| stamp.to_string()),
        );
        let run = observed(
            out,
            setup,
            |observer| {
                // The pre-pass that sizes the cards costs one more scan and
                // one more connection. Without cursor movement nothing
                // would draw them, so the pieces get their lines at the end.
                if cursor {
                    push_observed(&collection, &request, &SystemClock, observer)
                } else {
                    push(&collection, &request, &SystemClock)
                }
            },
            |result| match result {
                Ok(PushOutcome::Report(report)) => progress::push_pieces(report),
                _ => Vec::new(),
            },
        )
        .context("show the progress")?;
        cards = Some((run.cards, run.took));
        run.value
    } else {
        push(&collection, &request, &SystemClock)
    };
    let outcome = result
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
                let mut notices: Vec<Notice> = Vec::new();
                if let Some(reason) = &report.reason {
                    notices.push(Notice::Note(reason.clone()));
                }
                notices.extend(
                    report
                        .repairs
                        .iter()
                        .map(|repair| Notice::Note(format!("repaired: {}", repair.describe()))),
                );
                notices.extend(notice::warnings(&report.warnings));
                notices.extend(report.note_lines().into_iter().map(|line| {
                    Notice::Note(line.strip_prefix("note: ").unwrap_or(&line).to_string())
                }));
                let env = out.stdout_env();
                let shown = cards.as_ref().filter(|_| {
                    report.reason.is_none() && !report.dry_run && !report.changes.pieces.is_empty()
                });
                let lines = if let Some((cards, took)) = shown {
                    let target = report.initialized.then(|| sanitize(&report.target));
                    let summary = progress::push_summary(&report, target.as_deref(), *took);
                    let mut lines = vec![String::new(), cards.summary_line(&summary)];
                    if report.initialized {
                        lines.extend(marker_sentence(&env));
                    }
                    lines
                } else if env.rich && report.initialized && report.plan.is_none() {
                    claimed(&report, &env)
                } else {
                    report.lines()
                };
                if report.dry_run && report.reason.is_none() {
                    out.dry_plan(&lines, "pushed", "push to the target")
                        .context("print the result")?;
                } else {
                    for line in lines {
                        out.line(&line).context("print the result")?;
                    }
                }
                out.notices(&notices).context("print the notes")?;
            }
        }
    }
    Ok(0)
}

/// What the marker file of a claimed target is for.
fn marker_sentence(env: &Env) -> Vec<String> {
    let why = Rich::parse(
        "The marker file `.antiq-target` on the target now records this antiq collection and host, so another antiq collection cannot push there by mistake.",
    );
    flow(env, &why, anstyle::Style::new(), available(env, 0))
}

/// What a terminal gets after `push --init` has claimed the target: the
/// totals, what the marker is for, and a table of what was sent per piece.
fn claimed(report: &PushReport, env: &Env) -> Vec<String> {
    let counts = &report.changes;
    let mut rows: Vec<(String, [u64; 3])> = counts
        .pieces
        .iter()
        .map(|piece| {
            let c = &piece.counts;
            (
                sanitize(&piece.piece.to_string()),
                [c.created, c.updated, c.deleted],
            )
        })
        .collect();
    let other = [
        counts.other.created,
        counts.other.updated,
        counts.other.deleted,
    ];
    if other != [0; 3] {
        rows.push(("other files".to_string(), other));
    }
    let total = |column: usize| rows.iter().map(|(_, row)| row[column]).sum::<u64>();
    let headline = format!(
        "Claimed the push target {}: {} created, {} updated, {} deleted",
        sanitize(&report.target),
        total(0),
        total(1),
        total(2),
    );
    let done = paint(env, theme::GREEN, Symbols::for_env(env).done);
    let mut lines = vec![format!("{done} {}", paint(env, theme::BOLD, &headline))];

    lines.extend(marker_sentence(env));

    if rows.is_empty() {
        return lines;
    }
    const GAP: &str = "   ";
    const NUMBER: usize = 7;
    let name_width = rows
        .iter()
        .map(|(name, _)| name.width())
        .chain([5])
        .max()
        .expect("the header is a cell");
    lines.push(String::new());
    let header = format!(
        "{}{GAP}{}{GAP}{}{GAP}{}",
        pad("PIECE", name_width),
        pad_left("CREATED", NUMBER),
        pad_left("UPDATED", NUMBER),
        pad_left("DELETED", NUMBER)
    );
    lines.push(paint(env, theme::DIM, &header));
    // A zero is dim, so the numbers that did something stand out.
    let number = |count: u64| {
        let text = count.to_string();
        let fill = " ".repeat(NUMBER - text.len());
        if count == 0 {
            format!("{fill}{}", paint(env, theme::DIM, &text))
        } else {
            format!("{fill}{text}")
        }
    };
    for (name, row) in &rows {
        lines.push(format!(
            "{}{GAP}{}{GAP}{}{GAP}{}",
            pad(name, name_width),
            number(row[0]),
            number(row[1]),
            number(row[2])
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cards_of_a_push_count_what_it_created_and_what_it_deleted() {
        for (operation, word, count) in [
            (Operation::Storing, "storing", "1/4 created"),
            (Operation::Removing, "removing", "1/4 deleted"),
        ] {
            let [first, second] = progress::card_of(COUNTS, operation, 1, 4);
            assert!(first.ends_with(&format!("  {word}")), "{first:?}");
            assert!(second.ends_with(&format!("25%  {count}")), "{second:?}");
        }
    }
}
