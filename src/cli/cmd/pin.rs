// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` and `unpin` commands: create or remove the `.antiq-pin` of a piece.

use crate::cli::console::block::Block;
use crate::cli::console::rich::Rich;
use crate::cli::notice::{self, Notice};
use crate::cli::output::Output;
use crate::cli::table::sanitize;
use crate::cli::{Context, PieceArgs};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::commands::pin::{
    Action, Direction, PinPlan, PinReport, PinRequest, pin as pin_piece, plan,
};
use antiq::report::CommandReport;
use anyhow::Context as _;

pub fn pin(ctx: &Context, args: &PieceArgs, out: &mut Output) -> anyhow::Result<u8> {
    run(ctx, args, out, Direction::Pin)
}

pub fn unpin(ctx: &Context, args: &PieceArgs, out: &mut Output) -> anyhow::Result<u8> {
    run(ctx, args, out, Direction::Unpin)
}

fn run(
    ctx: &Context,
    args: &PieceArgs,
    out: &mut Output,
    direction: Direction,
) -> anyhow::Result<u8> {
    let name = match direction {
        Direction::Pin => "pin",
        Direction::Unpin => "unpin",
    };
    let collection = Collection::find(&ctx.start)
        .map_err(antiq::Error::from)
        .context("find the antiq collection")?;
    let request = PinRequest {
        collection: &collection,
        cwd: &ctx.start,
        piece: &args.piece,
        direction,
    };

    let (lines, dry) = if args.dry_run {
        let planned = plan(&request, &SystemClock)
            .map_err(antiq::Error::from)
            .with_context(|| format!("plan to {name} the piece"))?;
        if ctx.json {
            out.json(Some(name), &planned).context("print the plan")?;
            return Ok(0);
        }
        (render_plan(&planned, direction), true)
    } else {
        let report = pin_piece(&request, &SystemClock)
            .map_err(antiq::Error::from)
            .with_context(|| format!("{name} the piece"))?;
        if ctx.json {
            out.json(Some(name), &report).context("print the result")?;
            return Ok(0);
        }
        (render(&report, direction), false)
    };
    print(&lines, direction, dry, out).context("print the result")?;
    out.notices(&lines.stderr).context("print the notes")?;
    Ok(0)
}

/// The result on stdout. A terminal gets the info in place of the sentence
/// when the piece is in the state asked for, and under the lines otherwise.
/// A dry run ends with the footer, behind the info.
fn print(lines: &Lines, direction: Direction, dry: bool, out: &mut Output) -> std::io::Result<()> {
    if lines.instead
        && let Some(info) = &lines.info
        && out.info_only(info)?
    {
        return Ok(());
    }
    let actions = if dry {
        out.dry_lines(&lines.stdout)?
    } else {
        for line in &lines.stdout {
            out.line(line)?;
        }
        0
    };
    if let Some(info) = &lines.info {
        out.info_result(info)?;
    }
    if actions > 0 {
        let (nothing_was, rerun) = match direction {
            Direction::Pin => ("pinned", "pin the piece"),
            Direction::Unpin => ("unpinned", "unpin the piece"),
        };
        out.dry_footer(nothing_was, rerun)?;
    }
    Ok(())
}

/// What a run prints.
#[derive(Debug, Default, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<Notice>,
    /// What a pin is, for a terminal.
    info: Option<Block>,
    /// The info stands in place of the stdout lines on a terminal: the
    /// piece is in the state asked for and nothing happens.
    instead: bool,
}

/// The info a terminal gets. `changed` is false when the piece is in the
/// state asked for already.
fn explanation(piece: &str, direction: Direction, changed: bool, dry: bool) -> Block {
    let piece = sanitize(piece);
    let file = format!("{piece}/.antiq-pin");
    let status = Rich::new()
        .cmd("antiq status")
        .text(" shows which pieces are pinned.");
    match (direction, changed) {
        (Direction::Pin, false) => {
            Block::info(
                Rich::new()
                    .text("Nothing to pin: ")
                    .path(&piece)
                    .text(" is already pinned."),
            )
            .text(Rich::new().text("The file ").path(".antiq-pin").text(
                " at the top of the piece pins it, and this piece has it. Nothing was changed.",
            ))
            .fix(status)
        }
        (Direction::Unpin, false) => Block::info(
            Rich::new()
                .text("Nothing to unpin: ")
                .path(&piece)
                .text(" is not pinned."),
        )
        .text(
            Rich::new()
                .text("A pin is the file ")
                .path(".antiq-pin")
                .text(" at the top of a piece, and this piece has none. Nothing was changed."),
        )
        .fix(status),
        (Direction::Pin, true) if dry => {
            Block::info(Rich::new().text("A pinned piece is never archived.")).text(
                Rich::new()
                    .text("Pinning creates the file ")
                    .path(&file)
                    .text(". ")
                    .cmd("push")
                    .text(" still mirrors a pinned piece, and ")
                    .cmd("status")
                    .text(" shows it as pinned."),
            )
        }
        (Direction::Pin, true) => {
            Block::info(Rich::new().text("A pinned piece is never archived."))
                .text(
                    Rich::new()
                        .text("The file ")
                        .path(&file)
                        .text(" marks it. ")
                        .cmd("push")
                        .text(" still mirrors the piece, and ")
                        .cmd("status")
                        .text(" shows it as pinned."),
                )
                .fix(
                    Rich::new()
                        .cmd(&format!("antiq unpin {piece}"))
                        .text(" removes the file again."),
                )
        }
        (Direction::Unpin, true) if dry => {
            Block::info(Rich::new().text("An unpinned piece can be archived again.")).text(
                Rich::new()
                    .text("Unpinning removes the file ")
                    .path(&file)
                    .text(". The piece is archived once it has been idle long enough."),
            )
        }
        (Direction::Unpin, true) => {
            Block::info(Rich::new().text("The piece can be archived again.")).text(
                Rich::new()
                    .text("The file ")
                    .path(&file)
                    .text(" is removed. The piece is archived once it has been idle long enough."),
            )
        }
    }
}

/// What a run, or a dry run in the state asked for, says about the piece.
fn sentence(piece: &str, direction: Direction, changed: bool) -> String {
    let piece = sanitize(piece);
    match (direction, changed) {
        (Direction::Pin, true) => format!("Pinned {piece}"),
        (Direction::Pin, false) => format!("{piece} is already pinned"),
        (Direction::Unpin, true) => format!("Unpinned {piece}"),
        (Direction::Unpin, false) => format!("{piece} is not pinned"),
    }
}

fn render(report: &PinReport, direction: Direction) -> Lines {
    let mut stderr = Vec::new();
    for action in &report.repairs {
        if let Action::Repair(repair) = action {
            stderr.push(Notice::Note(format!("repaired: {}", repair.describe())));
        }
    }
    stderr.extend(notice::warnings(report.warnings()));
    let piece = report.piece.to_string();
    Lines {
        stdout: vec![sentence(&piece, direction, report.changed)],
        stderr,
        info: Some(explanation(&piece, direction, report.changed, false)),
        instead: !report.changed,
    }
}

/// The plan of a dry run: one `Would ...` line per repair, then the change
/// or the sentence that the piece is in the state asked for, or the line
/// about the holder of the lock.
fn render_plan(plan: &PinPlan, direction: Direction) -> Lines {
    let (piece, changed, actions, warnings) = match plan {
        PinPlan::Held { lock_held, .. } => {
            return Lines {
                stdout: vec![format!(
                    "The lock is held by pid {}, started {}. No repairs are listed.",
                    lock_held.pid, lock_held.started
                )],
                stderr: Vec::new(),
                info: None,
                instead: false,
            };
        }
        PinPlan::Listed {
            piece,
            changed,
            actions,
            warnings,
            ..
        } => (piece.to_string(), *changed, actions, warnings),
    };
    let mut stdout: Vec<String> = actions
        .iter()
        .map(|action| match action {
            Action::Repair(repair) => format!("Would {}", repair.describe()),
            Action::Pin { piece } => format!("Would pin {}", sanitize(&piece.to_string())),
            Action::Unpin { piece } => format!("Would unpin {}", sanitize(&piece.to_string())),
        })
        .collect();
    if !changed {
        stdout.push(sentence(&piece, direction, false));
    }
    Lines {
        stdout,
        stderr: notice::warnings(warnings),
        // With repairs listed above it, the sentence of the state that holds
        // says it already.
        info: (changed || actions.is_empty())
            .then(|| explanation(&piece, direction, changed, true)),
        instead: !changed && actions.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::notice::pipe_lines;
    use antiq::commands::pin::Action;
    use antiq::plan::LockHeld;
    use antiq::relpath::{Name, RelPath};
    use antiq::repair::Repair;
    use antiq::report::Warning;

    fn piece(path: &str) -> RelPath {
        RelPath::from_bytes(path.as_bytes().to_vec()).expect("a valid path")
    }

    fn report(path: &str, pinned: bool, changed: bool) -> PinReport {
        PinReport {
            piece: piece(path),
            pinned,
            changed,
            repairs: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn leftover() -> Action {
        Action::Repair(Repair::DeleteTemporary {
            entry: Name::from_bytes(b"leftover".to_vec()).expect("a name"),
        })
    }

    fn listed(path: &str, pinned: bool, changed: bool, direction: Direction) -> PinPlan {
        let mut actions = vec![leftover()];
        if changed {
            let piece = piece(path);
            actions.push(match direction {
                Direction::Pin => Action::Pin { piece },
                Direction::Unpin => Action::Unpin { piece },
            });
        }
        PinPlan::Listed {
            dry_run: true,
            piece: piece(path),
            pinned,
            changed,
            actions,
            warnings: vec![Warning::new("watch out")],
        }
    }

    #[test]
    fn a_run_says_what_it_did_to_the_piece() {
        let cases = [
            (Direction::Pin, true, true, "Pinned tools/scan"),
            (Direction::Pin, true, false, "tools/scan is already pinned"),
            (Direction::Unpin, false, true, "Unpinned tools/scan"),
            (Direction::Unpin, false, false, "tools/scan is not pinned"),
        ];
        for (direction, pinned, changed, expected) in cases {
            let lines = render(&report("tools/scan", pinned, changed), direction);
            assert_eq!(lines.stdout, [expected], "{direction:?} {changed}");
            assert!(lines.stderr.is_empty());
        }
    }

    #[test]
    fn a_name_cannot_move_the_cursor() {
        let lines = render(&report("a\u{1b}[31mb", true, true), Direction::Pin);
        assert_eq!(lines.stdout, ["Pinned a%1B[31mb"]);
    }

    #[test]
    fn repairs_and_warnings_go_to_stderr_in_this_order() {
        let mut done = report("tools/scan", true, true);
        done.repairs = vec![leftover()];
        done.warnings = vec![Warning::new("watch out")];
        let lines = render(&done, Direction::Pin);
        assert_eq!(
            pipe_lines(&lines.stderr),
            [
                "note: repaired: delete .antiq/tmp/leftover",
                "warning: watch out"
            ]
        );
    }

    #[test]
    fn a_plan_lists_the_repairs_and_then_the_change() {
        for (direction, expected) in [
            (Direction::Pin, "Would pin tools/scan"),
            (Direction::Unpin, "Would unpin tools/scan"),
        ] {
            let pinned = direction == Direction::Pin;
            let lines = render_plan(&listed("tools/scan", pinned, true, direction), direction);
            assert_eq!(lines.stdout, ["Would delete .antiq/tmp/leftover", expected]);
            assert_eq!(pipe_lines(&lines.stderr), ["warning: watch out"]);
        }
    }

    #[test]
    fn a_plan_of_the_state_that_holds_says_so_after_the_repairs() {
        let lines = render_plan(
            &listed("tools/scan", true, false, Direction::Pin),
            Direction::Pin,
        );
        assert_eq!(
            lines.stdout,
            [
                "Would delete .antiq/tmp/leftover",
                "tools/scan is already pinned"
            ]
        );
        let lines = render_plan(
            &listed("tools/scan", false, false, Direction::Unpin),
            Direction::Unpin,
        );
        assert_eq!(
            lines.stdout,
            [
                "Would delete .antiq/tmp/leftover",
                "tools/scan is not pinned"
            ]
        );
    }

    #[test]
    fn a_plan_of_a_held_lock_is_the_line_about_the_holder() {
        let plan = PinPlan::Held {
            dry_run: true,
            lock_held: LockHeld {
                pid: 4711,
                started: "2026-11-04T09:02:11Z".parse().expect("a timestamp"),
            },
            actions: Vec::new(),
        };
        let lines = render_plan(&plan, Direction::Pin);
        assert_eq!(
            lines.stdout,
            ["The lock is held by pid 4711, started 2026-11-04T09:02:11Z. No repairs are listed."]
        );
        assert!(lines.stderr.is_empty());
    }
}
