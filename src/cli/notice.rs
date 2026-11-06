// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a command tells next to its result: warnings, and notes that are
//! no warning, such as a repair that ran.

use super::console::block::Block;
use super::console::rich::Rich;
use antiq::report::{Warning, WarningKind};

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

/// A warning in the words of the console. A kind without wording shows
/// the message of the library as its title.
fn warning_block(warning: &Warning) -> Block {
    let warn = |title: Rich| Block::warning(title).pipe(&format!("warning: {}", warning.message));
    match &warning.kind {
        Some(WarningKind::UnknownKey { table, key }) => warn(
            Rich::new()
                .text("Unknown setting ")
                .path(key)
                .text(&format!(" under [{table}]")),
        )
        .text(Rich::new().text("antiq ignores it."))
        .fix(Rich::parse(
            "Correct the name or remove the line in `.antiq/config.toml`.",
        )),
        Some(WarningKind::NetworkMount { filesystem }) => {
            warn(Rich::new().text(&format!("The antiq collection is on an {filesystem} mount")))
        }
        Some(WarningKind::FilesystemUnknown { reason }) => {
            warn(Rich::new().text("Cannot tell what kind of filesystem the antiq collection is on"))
                .text(Rich::new().text(reason))
        }
        Some(WarningKind::LockUnreadable { reason }) => {
            warn(Rich::new().text("Cannot read the lock file")).text(Rich::new().text(reason))
        }
        Some(WarningKind::LockHeld {
            pid,
            machine,
            started,
        }) => Block::info(Rich::new().text("Another antiq run holds the lock"))
            .text(Rich::new().text(&format!(
                "Its pid is {pid} on machine {machine}, started {started}."
            )))
            .pipe(&format!("note: {}", warning.message)),
        None => warn(Rich::new().text(&warning.message)),
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
        Notice::Warning(Warning::of(
            WarningKind::UnknownKey {
                table: "collection",
                key: "idle_dyas".into(),
            },
            "the key `idle_dyas` under [collection] is unknown and ignored",
        ))
    }

    #[test]
    fn a_warning_without_a_kind_is_its_message_as_the_title() {
        let notice = Notice::Warning(Warning::new("cannot open notes/a: denied"));
        assert_eq!(
            rich(notice.clone()),
            [" ⚠ WARNING  cannot open notes/a: denied"]
        );
        assert_eq!(pipe(notice), ["warning: cannot open notes/a: denied"]);
    }

    #[test]
    fn an_unknown_key_says_it_is_ignored_and_how_to_fix_it() {
        assert_eq!(
            rich(unknown_key()),
            [
                " ⚠ WARNING  Unknown setting idle_dyas under [collection]",
                "▌   antiq ignores it.",
                "▌ → Correct the name or remove the line in .antiq/config.toml.",
            ]
        );
        assert_eq!(
            pipe(unknown_key()),
            ["warning: the key `idle_dyas` under [collection] is unknown and ignored"]
        );
    }

    #[test]
    fn a_network_mount_is_named() {
        let notice = Notice::Warning(Warning::of(
            WarningKind::NetworkMount { filesystem: "SMB" },
            "the collection is on an SMB mount",
        ));
        assert_eq!(
            rich(notice),
            [" ⚠ WARNING  The antiq collection is on an SMB mount"]
        );
    }

    #[test]
    fn a_filesystem_or_lock_that_cannot_be_read_gives_its_reason() {
        let filesystem = Notice::Warning(Warning::of(
            WarningKind::FilesystemUnknown {
                reason: "cannot stat /work: denied".into(),
            },
            "cannot tell what kind of filesystem the collection is on: cannot stat /work: denied",
        ));
        assert_eq!(
            rich(filesystem),
            [
                " ⚠ WARNING  Cannot tell what kind of filesystem the antiq collection is on",
                "▌   cannot stat /work: denied",
            ]
        );
        let lock = Notice::Warning(Warning::of(
            WarningKind::LockUnreadable {
                reason: "denied".into(),
            },
            "cannot read the lock file: denied",
        ));
        assert_eq!(
            rich(lock),
            [" ⚠ WARNING  Cannot read the lock file", "▌   denied"]
        );
    }

    #[test]
    fn a_run_that_holds_the_lock_is_a_note_not_a_warning() {
        let notice = Notice::Warning(Warning::of(
            WarningKind::LockHeld {
                pid: 4711,
                machine: "imperius".into(),
                started: "2026-11-04T09:00:00Z".into(),
            },
            "another run holds the lock (pid 4711, machine imperius, started 2026-11-04T09:00:00Z)",
        ));
        assert_eq!(
            rich(notice.clone()),
            [
                " ℹ INFO  Another antiq run holds the lock",
                "▌   Its pid is 4711 on machine imperius, started 2026-11-04T09:00:00Z.",
            ]
        );
        assert_eq!(
            pipe(notice),
            [
                "note: another run holds the lock (pid 4711, machine imperius, started 2026-11-04T09:00:00Z)"
            ]
        );
    }

    #[test]
    fn a_note_is_an_info_with_a_note_line_for_a_pipe() {
        let notice = Notice::Note("repaired: removed an empty folder".into());
        assert_eq!(
            rich(notice.clone()),
            [" ℹ INFO  repaired: removed an empty folder"]
        );
        assert_eq!(pipe(notice), ["note: repaired: removed an empty folder"]);
    }
}
