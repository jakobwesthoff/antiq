// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `push` command: mirrors the collection to the target.

use super::progress::{self, Description};
use crate::cli::console::cards::Count;
use crate::cli::console::env::Env;
use crate::cli::console::render::{available, flow, paint};
use crate::cli::console::rich::Rich;
use crate::cli::console::symbols::Symbols;
use crate::cli::console::theme;
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::table::{numeric_header, pad, sanitize, thousands};
use crate::cli::{Context, PushArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::push::{PushOutcome, PushReport, PushRequest, push, push_observed};
use antiq::progress::Operation;
use anyhow::Context as _;
use unicode_width::UnicodeWidthStr;

/// The whole mirror is one item. It counts the entries rsync lists, against
/// the total rsync reports once it has one, and the entries it deletes.
pub(super) const COUNTS: &[(Operation, Count)] = &[
    (
        Operation::Storing,
        Count {
            text: entries,
            percent: false,
        },
    ),
    (
        Operation::Removing,
        Count {
            text: entries,
            percent: false,
        },
    ),
];

/// Fits `3 created, 1 deleted`.
const RESULT_WIDTH: usize = 20;

/// `2 of 3 entries` while rsync's total covers the count, `2 entries`
/// otherwise.
fn entries(done: u64, total: Option<u64>) -> String {
    match (total, done) {
        (Some(total), _) => format!("{} of {} entries", thousands(done), thousands(total)),
        (None, 1) => "1 entry".to_string(),
        (None, _) => format!("{} entries", thousands(done)),
    }
}

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
    // On a terminal that moves the cursor the whole mirror shows as one
    // card, and the table of the pieces replaces the report's lines.
    let mut description = Description::new(COUNTS);
    description.name_prefix = "Pushing to ";
    description.result_width = RESULT_WIDTH;
    let cursor = out.stdout_env().cursor;
    let (result, shown) = progress::watch(
        ctx,
        out,
        args.dry_run,
        description,
        |observer| {
            // Without cursor movement nothing would draw the card, so the
            // pieces get their lines at the end.
            if cursor {
                push_observed(&collection, &request, &SystemClock, observer)
            } else {
                push(&collection, &request, &SystemClock)
            }
        },
        || push(&collection, &request, &SystemClock),
        |result| match result {
            Ok(PushOutcome::Report(report)) if !cursor => progress::push_pieces(report),
            _ => Vec::new(),
        },
    )?;
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
                let shown = shown.as_ref().filter(|_| {
                    report.reason.is_none() && !report.dry_run && !report.changes.pieces.is_empty()
                });
                let lines = if let Some(shown) = shown {
                    let target = report.initialized.then(|| sanitize(&report.target));
                    let summary = progress::push_summary(&report, target.as_deref(), shown.took());
                    // The card is gone. A terminal without cursor movement
                    // has the lines of its pieces above already.
                    let mut lines = if cursor {
                        pieces_table(&change_rows(&report), &env)
                    } else {
                        Vec::new()
                    };
                    lines.extend([String::new(), shown.summary_line(&summary)]);
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

/// The changes of a push per piece, then the paths outside every piece.
fn change_rows(report: &PushReport) -> Vec<(String, [u64; 3])> {
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
    rows
}

/// A table of what was sent per piece: the columns PIECE, CREATED, UPDATED
/// and DELETED. A zero is dim, so the numbers that did something stand out.
/// The callers have at least one row.
fn pieces_table(rows: &[(String, [u64; 3])], env: &Env) -> Vec<String> {
    const GAP: &str = "   ";
    const NUMBER: usize = 7;
    let name_width = rows
        .iter()
        .map(|(name, _)| name.width())
        .chain([5])
        .max()
        .expect("the header is a cell");
    let header = format!(
        "{}{GAP}{}{GAP}{}{GAP}{}",
        pad("PIECE", name_width),
        numeric_header("CREATED", NUMBER),
        numeric_header("UPDATED", NUMBER),
        numeric_header("DELETED", NUMBER)
    );
    let mut lines = vec![paint(env, theme::DIM, &header)];
    let number = |count: u64| {
        let text = count.to_string();
        let fill = " ".repeat(NUMBER - text.len());
        if count == 0 {
            format!("{fill}{}", paint(env, theme::DIM, &text))
        } else {
            format!("{fill}{text}")
        }
    };
    for (name, row) in rows {
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

/// What a terminal gets after `push --init` has claimed the target: the
/// totals, what the marker is for, and a table of what was sent per piece.
fn claimed(report: &PushReport, env: &Env) -> Vec<String> {
    let rows = change_rows(report);
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
    if !rows.is_empty() {
        lines.push(String::new());
        lines.extend(pieces_table(&rows, env));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn second_line(operation: Operation, done: u64, total: u64) -> String {
        let [first, second] = progress::card_of(COUNTS, operation, done, total);
        let word = if operation == Operation::Storing {
            "storing"
        } else {
            "removing"
        };
        assert!(first.ends_with(&format!("  {word}")), "{first:?}");
        second
    }

    #[test]
    fn storing_counts_entries_against_the_total_of_rsync_without_a_percentage() {
        let second = second_line(Operation::Storing, 2, 3);
        assert!(second.ends_with("  2 of 3 entries"), "{second:?}");
        assert!(!second.contains('%'), "{second:?}");
        assert!(second.starts_with("    ━"), "{second:?}");
    }

    #[test]
    fn a_total_that_is_unknown_or_behind_the_count_leaves_a_moving_bar_and_the_count() {
        for (done, total, text) in [(2, 0, "2 entries"), (5, 3, "5 entries"), (1, 0, "1 entry")] {
            let second = second_line(Operation::Storing, done, total);
            assert!(second.ends_with(&format!("  {text}")), "{second:?}");
            assert!(!second.contains('%'), "{second:?}");
        }
    }

    #[test]
    fn counts_group_their_thousands_with_a_space() {
        let second = second_line(Operation::Storing, 10_012, 17_304);
        assert!(second.ends_with("  10 012 of 17 304 entries"), "{second:?}");
        let second = second_line(Operation::Storing, 1_203, 0);
        assert!(second.ends_with("  1 203 entries"), "{second:?}");
    }

    #[test]
    fn removing_counts_the_entries_deleted() {
        let second = second_line(Operation::Removing, 3, 0);
        assert!(second.ends_with("  3 entries"), "{second:?}");
    }
}
