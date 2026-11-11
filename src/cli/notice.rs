// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a command tells next to its result: warnings, and notes that are
//! no warning, such as a repair that ran.

use super::console::block::Block;
use super::console::rich::Rich;
use super::table::sanitize;
use antiq::report::{Warning, WarningKind};
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
                let text = sanitize(text);
                Some(Block::info(Rich::new().text(&text)).pipe(&format!("note: {text}")))
            }
            Notice::Warning(warning) => Some(warning_block(warning)),
            Notice::Line(_) => None,
        }
    }

    /// The text of a line, which a pipe and a terminal show alike.
    pub fn line(&self) -> Option<String> {
        match self {
            Notice::Line(line) => Some(sanitize(line)),
            _ => None,
        }
    }
}

/// A warning as a block: the message of its kind is the title and the
/// help of the kind is the fix. A kind with the severity of advice is an
/// info, as nothing is wrong. A warning without a kind shows its message,
/// with the value it starts with as a code span when it has one.
///
/// A message names paths and values from the user's files, so a control
/// character in it is escaped for every reader, as in the lines of a table.
fn warning_block(warning: &Warning) -> Block {
    let message = &sanitize(&warning.message);
    let advice = warning
        .kind
        .as_ref()
        .is_some_and(|kind| kind.severity() == Some(Severity::Advice));
    // The reasons of these two kinds come from the system and can hold
    // backticks that are no code spans.
    let free_text = matches!(
        warning.kind,
        Some(WarningKind::FilesystemUnknown { .. } | WarningKind::LockUnreadable { .. })
    );
    let block = if advice {
        Block::info(Rich::parse(message)).pipe(&format!("note: {message}"))
    } else if warning.kind.is_some() && !free_text {
        Block::warning(Rich::parse(message)).pipe(&format!("warning: {message}"))
    } else {
        // The lead is a prefix of the raw message, so each part is escaped on
        // its own and the split stays where it was.
        let title = match warning.lead() {
            Some((lead, rest)) => Rich::new().cmd(&sanitize(lead)).text(&sanitize(rest)),
            None => Rich::new().text(message),
        };
        Block::warning(title).pipe(&format!("warning: {message}"))
    };
    match warning.kind.as_ref().and_then(|kind| kind.help()) {
        Some(help) => block.fix(Rich::parse(&sanitize(&help.to_string()))),
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
            None => notice.line().into_iter().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::console::env::Env;
    use super::super::console::render::render_blocks;
    use super::{Notice, pipe_lines};
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
            ["▌ ⚠ WARNING", "▌ Cannot open notes/a: denied",]
        );
        assert_eq!(pipe(notice), ["warning: cannot open notes/a: denied"]);
    }

    #[test]
    fn a_warning_that_starts_with_a_piece_keeps_the_case_of_its_name() {
        let marked = Notice::Warning(Warning::marked(
            "beta/notes",
            ": its files add up to 9 bytes",
        ));
        assert_eq!(
            rich(marked.clone()),
            ["▌ ⚠ WARNING", "▌ beta/notes: its files add up to 9 bytes",]
        );
        assert_eq!(
            pipe(marked),
            ["warning: `beta/notes`: its files add up to 9 bytes"]
        );
        let plain = Notice::Warning(Warning::new("beta/notes: its files add up to 9 bytes"));
        assert_eq!(
            rich(plain),
            ["▌ ⚠ WARNING", "▌ Beta/notes: its files add up to 9 bytes",]
        );
    }

    #[test]
    fn a_marked_warning_with_backticks_in_its_value_and_text_keeps_both_whole() {
        let marked = Notice::Warning(Warning::marked("a`b", ": cannot read `x` or `y`"));
        assert_eq!(
            rich(marked.clone()),
            ["▌ ⚠ WARNING", "▌ a`b: cannot read `x` or `y`",]
        );
        assert_eq!(pipe(marked), ["warning: ``a`b``: cannot read `x` or `y`"]);
    }

    #[test]
    fn an_unknown_key_says_it_is_ignored_and_how_to_fix_it() {
        assert_eq!(
            rich(unknown_key()),
            [
                "▌ ⚠ WARNING",
                "▌ Unknown setting idle_dyas under [collection], which antiq ignores",
                "▌",
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
            ["▌ ⚠ WARNING", "▌ The antiq collection is on an SMB mount",]
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
                "▌ ⚠ WARNING",
                "▌ Cannot tell what kind of filesystem the antiq collection is on: cannot stat /work: denied",
            ]
        );
        let lock = Notice::Warning(Warning::of(WarningKind::LockUnreadable {
            reason: "denied".into(),
        }));
        assert_eq!(
            rich(lock),
            ["▌ ⚠ WARNING", "▌ Cannot read the lock file: denied",]
        );
    }

    #[test]
    fn a_reason_with_backticks_stays_plain_text_in_the_title() {
        let filesystem = Notice::Warning(Warning::of(WarningKind::FilesystemUnknown {
            reason: "cannot stat /work/`a`/`b`: denied".into(),
        }));
        assert_eq!(
            rich(filesystem),
            [
                "▌ ⚠ WARNING",
                "▌ Cannot tell what kind of filesystem the antiq collection is on: cannot stat /work/`a`/`b`: denied",
            ]
        );
        let lock = Notice::Warning(Warning::of(WarningKind::LockUnreadable {
            reason: "`a` and `b`".into(),
        }));
        assert_eq!(
            rich(lock),
            ["▌ ⚠ WARNING", "▌ Cannot read the lock file: `a` and `b`",]
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
                "▌ ℹ INFO",
                "▌ Another antiq run holds the lock: its pid is 4711 on machine imperius, started",
                "▌ 2026-11-04T09:00:00Z",
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
    fn a_control_character_of_a_marked_warning_is_escaped_in_the_value_and_the_text() {
        let marked = Notice::Warning(Warning::marked("a\nb", ": cannot read \u{1b}[0m"));
        assert_eq!(
            pipe(marked.clone()),
            ["warning: `a%0Ab`: cannot read %1B[0m"]
        );
        assert_eq!(rich(marked), ["▌ ⚠ WARNING", "▌ a%0Ab: cannot read %1B[0m"]);
    }

    #[test]
    fn a_control_character_of_a_warning_with_a_kind_is_escaped() {
        let notice = Notice::Warning(Warning::of(WarningKind::UnknownKey {
            table: "collection",
            key: "idle\ndays".into(),
        }));
        assert_eq!(
            pipe(notice),
            ["warning: unknown setting `idle%0Adays` under [collection], which antiq ignores"]
        );
    }

    #[test]
    fn a_control_character_in_the_reason_of_a_warning_is_escaped() {
        let notice = Notice::Warning(Warning::of(WarningKind::FilesystemUnknown {
            reason: "cannot stat /work\n/x: denied".into(),
        }));
        assert_eq!(
            pipe(notice),
            [
                "warning: cannot tell what kind of filesystem the antiq collection is on: cannot stat /work%0A/x: denied"
            ]
        );
    }

    #[test]
    fn a_control_character_in_an_advice_is_escaped() {
        let notice = Notice::Warning(Warning::of(WarningKind::LockHeld {
            pid: 1,
            machine: "mac\nbook".into(),
            started: "2026-11-04T09:00:00Z".into(),
        }));
        assert_eq!(
            pipe(notice),
            [
                "note: another antiq run holds the lock: its pid is 1 on machine mac%0Abook, started 2026-11-04T09:00:00Z"
            ]
        );
    }

    #[test]
    fn a_control_character_in_a_note_is_escaped() {
        let notice = Notice::Note("repaired: delete .antiq/tmp/a\nb".into());
        assert_eq!(
            pipe(notice.clone()),
            ["note: repaired: delete .antiq/tmp/a%0Ab"]
        );
        assert_eq!(
            rich(notice),
            ["▌ ℹ INFO", "▌ Repaired: delete .antiq/tmp/a%0Ab"]
        );
    }

    #[test]
    fn a_control_character_in_a_line_is_escaped_for_every_reader_of_it() {
        let notice = Notice::Line("error: could not delete a\nb".into());
        assert_eq!(
            notice.line(),
            Some("error: could not delete a%0Ab".to_string())
        );
        assert_eq!(pipe_lines(&[notice]), ["error: could not delete a%0Ab"]);
    }

    #[test]
    fn a_note_and_a_warning_are_no_line() {
        assert_eq!(Notice::Note("repaired".into()).line(), None);
        assert_eq!(unknown_key().line(), None);
    }

    #[test]
    fn a_note_is_an_info_with_a_note_line_for_a_pipe() {
        let notice = Notice::Note("repaired: removed an empty folder".into());
        assert_eq!(
            rich(notice.clone()),
            ["▌ ℹ INFO", "▌ Repaired: removed an empty folder",]
        );
        assert_eq!(pipe(notice), ["note: repaired: removed an empty folder"]);
    }
}
