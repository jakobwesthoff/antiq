// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a command tells next to its result: warnings, and notes that are
//! no warning, such as a repair that ran.

use super::console::block::Block;
use super::console::rich::Rich;
use antiq::report::Warning;
use miette::{Diagnostic, Severity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Warning(Warning),
    /// Something that happened and is no cause for concern. A pipe reads
    /// it as a `note:` line.
    Note(String),
    /// A line in its final wording, the same on a terminal and a pipe,
    /// such as the report of a failure that did not stop the run.
    Line(String),
}

impl Notice {
    /// The block for a terminal, with the line a pipe gets. A line has
    /// none.
    pub fn block(&self) -> Option<Block> {
        match self {
            Notice::Note(text) => {
                Some(Block::info(Rich::new().text(text)).pipe(&format!("note: {text}")))
            }
            Notice::Warning(warning) => Some(warning_block(warning)),
            Notice::Line(_) => None,
        }
    }
}

/// A warning as a block: the message of its kind is the title and the
/// help of the kind is the fix. A kind with the severity of advice is an
/// info, as nothing is wrong. A warning without a kind shows its message.
fn warning_block(warning: &Warning) -> Block {
    let message = &warning.message;
    let advice = warning
        .kind
        .as_ref()
        .is_some_and(|kind| kind.severity() == Some(Severity::Advice));
    let block = if advice {
        Block::info(Rich::parse(message)).pipe(&format!("note: {message}"))
    } else if warning.kind.is_some() {
        Block::warning(Rich::parse(message)).pipe(&format!("warning: {message}"))
    } else {
        Block::warning(Rich::new().text(message)).pipe(&format!("warning: {message}"))
    };
    match warning.kind.as_ref().and_then(|kind| kind.help()) {
        Some(help) => block.fix(Rich::parse(&help.to_string())),
        None => block,
    }
}

/// The warnings of a report as notices.
pub fn warnings(warnings: &[Warning]) -> Vec<Notice> {
    warnings.iter().cloned().map(Notice::Warning).collect()
}

/// The lines a pipe gets for `notices`, for the tests of the commands.
#[cfg(test)]
pub fn pipe_lines(notices: &[Notice]) -> Vec<String> {
    use super::console::env::Env;
    use super::console::render::render_blocks;
    notices
        .iter()
        .flat_map(|notice| match notice.block() {
            Some(block) => render_blocks(&[block], &Env::pipe()),
            None => match notice {
                Notice::Line(line) => vec![line.clone()],
                _ => Vec::new(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::console::env::Env;
    use super::super::console::render::render_blocks;
    use super::Notice;
    use antiq::report::{Warning, WarningKind};

    fn rich(notice: Notice) -> Vec<String> {
        render_blocks(
            &[notice.block().expect("a block")],
            &Env::new(true, false, true, 100),
        )
    }

    fn pipe(notice: Notice) -> Vec<String> {
        render_blocks(&[notice.block().expect("a block")], &Env::pipe())
    }

    fn unknown_key() -> Notice {
        Notice::Warning(Warning::of(WarningKind::UnknownKey {
            table: "collection",
            key: "idle_dyas".into(),
        }))
    }

    #[test]
    fn a_warning_without_a_kind_is_its_message_as_the_title() {
        let notice = Notice::Warning(Warning::new("cannot open notes/a: denied"));
        assert_eq!(
            rich(notice.clone()),
            [" ⚠ WARNING  Cannot open notes/a: denied"]
        );
        assert_eq!(pipe(notice), ["warning: cannot open notes/a: denied"]);
    }

    #[test]
    fn an_unknown_key_says_it_is_ignored_and_how_to_fix_it() {
        assert_eq!(
            rich(unknown_key()),
            [
                " ⚠ WARNING  Unknown setting idle_dyas under [collection], which antiq ignores",
                "▌ → Correct the name or remove the line in .antiq/config.toml.",
            ]
        );
        assert_eq!(
            pipe(unknown_key()),
            ["warning: unknown setting `idle_dyas` under [collection], which antiq ignores"]
        );
    }

    #[test]
    fn a_network_mount_is_named() {
        let notice = Notice::Warning(Warning::of(WarningKind::NetworkMount { filesystem: "SMB" }));
        assert_eq!(
            rich(notice),
            [" ⚠ WARNING  The antiq collection is on an SMB mount"]
        );
    }

    #[test]
    fn a_filesystem_or_lock_that_cannot_be_read_gives_its_reason() {
        let filesystem = Notice::Warning(Warning::of(WarningKind::FilesystemUnknown {
            reason: "cannot stat /work: denied".into(),
        }));
        assert_eq!(
            rich(filesystem),
            [
                " ⚠ WARNING  Cannot tell what kind of filesystem the antiq collection is on: cannot stat /work:",
                "            denied",
            ]
        );
        let lock = Notice::Warning(Warning::of(WarningKind::LockUnreadable {
            reason: "denied".into(),
        }));
        assert_eq!(
            rich(lock),
            [" ⚠ WARNING  Cannot read the lock file: denied"]
        );
    }

    #[test]
    fn a_run_that_holds_the_lock_is_a_note_not_a_warning() {
        let notice = Notice::Warning(Warning::of(WarningKind::LockHeld {
            pid: 4711,
            machine: "imperius".into(),
            started: "2026-11-04T09:00:00Z".into(),
        }));
        assert_eq!(
            rich(notice.clone()),
            [
                " ℹ INFO  Another antiq run holds the lock: its pid is 4711 on machine imperius, started",
                "         2026-11-04T09:00:00Z",
            ]
        );
        assert_eq!(
            pipe(notice),
            [
                "note: another antiq run holds the lock: its pid is 4711 on machine imperius, started 2026-11-04T09:00:00Z"
            ]
        );
    }

    #[test]
    fn a_note_is_an_info_with_a_note_line_for_a_pipe() {
        let notice = Notice::Note("repaired: removed an empty folder".into());
        assert_eq!(
            rich(notice.clone()),
            [" ℹ INFO  Repaired: removed an empty folder"]
        );
        assert_eq!(pipe(notice), ["note: repaired: removed an empty folder"]);
    }
}
