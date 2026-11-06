// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The block a failed run is reported as.

use super::Description;
use super::console::block::{Block, Para};
use super::console::rich::Rich;
use super::unresolved::pasteable_line;
use antiq::code_span::CodeSpan;
use antiq::commands::push::PushError;
use antiq::error::explanation;
use antiq::versions::naming::VersionLookupError;
use miette::Diagnostic;
use std::path::Path;

/// The error block for `description`: the message of the diagnostic that
/// explains the error as the title, its help as the fix, the candidates
/// of an ambiguous reference, and the steps the run was in.
///
/// An error without an explanation shows the text of its whole chain.
pub fn block_for(description: &Description, directory: Option<&Path>) -> Block {
    let explained = description
        .library_error
        .and_then(|error| explanation(error));
    let free_text = description
        .library_error
        .and_then(find::<PushError>)
        .is_some_and(|error| matches!(error, PushError::Unreachable { .. }));
    let mut block = match explained {
        Some(explained) if free_text => plain_block(explained),
        Some(explained) => {
            let help = explained.help().map(|help| {
                pasteable_commands(help.to_string(), description.library_error, directory)
            });
            titled_block(explained, Rich::parse(&explained.to_string()), help)
        }
        None => Block::error(Rich::new().text(&description.root)),
    };
    let candidates = description
        .library_error
        .and_then(antiq::Error::candidates)
        .unwrap_or_default();
    for candidate in candidates {
        block = block.candidate(&candidate.path);
    }
    // The stamps an ambiguous version input matches are candidates too,
    // although the library error that carries them is no reference error.
    if let Some(VersionLookupError::Ambiguous { stamps, .. }) = description
        .library_error
        .and_then(find::<VersionLookupError>)
    {
        for stamp in stamps {
            block = block.candidate(stamp);
        }
    }
    for step in &description.contexts {
        block = block.doing(step);
    }
    block
}

/// The error block for a diagnostic: its message as the title and its
/// help as the fix. The message is read for code spans.
pub(super) fn explained_block(explained: &dyn Diagnostic) -> Block {
    titled_block(
        explained,
        Rich::parse(&explained.to_string()),
        explained.help().map(|help| help.to_string()),
    )
}

/// [`explained_block`] with the message as plain text. The reason of an
/// unreachable target comes from the system, such as the output of ssh,
/// and can hold backticks that are no code spans.
fn plain_block(explained: &dyn Diagnostic) -> Block {
    titled_block(
        explained,
        Rich::new().text(&explained.to_string()),
        explained.help().map(|help| help.to_string()),
    )
}

fn titled_block(explained: &dyn Diagnostic, title: Rich, help: Option<String>) -> Block {
    let mut block = Block::error(title).message(&explained.to_string());
    if let Some(help) = help {
        for paragraph in help_paragraphs(&help) {
            block = block.how(paragraph);
        }
    }
    block
}

/// `help` with each command that the damaged versions of a refused push
/// name written the way to paste it: quoted for a shell and with the `-C`
/// of the run. Only the CLI knows how antiq was started, so the library
/// writes the commands without it.
fn pasteable_commands(
    help: String,
    error: Option<&antiq::Error>,
    directory: Option<&Path>,
) -> String {
    let Some(PushError::DamagedStore { commands, .. }) = error.and_then(find::<PushError>) else {
        return help;
    };
    commands.iter().fold(help, |help, command| {
        let pasteable = CodeSpan(pasteable_line(directory, &command.words())).to_string();
        help.replace(&command.written(), &pasteable)
    })
}

/// The paragraphs of a help: blank lines separate them, and a paragraph
/// whose lines all start with two spaces is a literal block, such as a
/// piece of a config file, kept as written.
fn help_paragraphs(help: &str) -> Vec<Para> {
    let mut paragraphs = Vec::new();
    for (index, block) in help.split("\n\n").enumerate() {
        if index > 0 {
            paragraphs.push(Para::Blank);
        }
        if block.lines().all(|line| line.starts_with("  ")) {
            paragraphs.extend(block.lines().map(Para::pre));
        } else {
            paragraphs.push(Para::Text(Rich::parse(&block.replace('\n', " "))));
        }
    }
    paragraphs
}

/// The typed error of kind `T` that `error` is or wraps. The wrappers of
/// the library are transparent, so the walk starts at what they wrap.
pub(super) fn find<T: std::error::Error + 'static>(error: &antiq::Error) -> Option<&T> {
    let mut link = Some(error.wrapped());
    while let Some(current) = link {
        if let Some(found) = current.downcast_ref::<T>() {
            return Some(found);
        }
        link = current.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::console::block::Para;
    use super::super::console::env::Env;
    use super::super::console::rich::Rich;
    use super::super::describe;
    use super::block_for;
    use antiq::collection::CollectionError;
    use antiq::commands::push::{PushError, VersionCommand};
    use antiq::commands::rebuild::RebuildError;
    use antiq::commands::restore::RestoreError;
    use antiq::listing::ListingError;
    use antiq::lock::LockError;
    use antiq::reference::ReferenceError;
    use antiq::relpath::RelPath;
    use antiq::run::RunError;
    use antiq::versions::naming::VersionLookupError;
    use antiq::versions::stamp::Stamp;
    use std::path::Path;

    /// The rich lines of the block for `error`, run through the steps
    /// `doing` (innermost first), on a terminal 100 columns wide.
    fn rich(error: impl Into<antiq::Error>, doing: &[&str]) -> Vec<String> {
        let mut error = anyhow::Error::new(error.into());
        for step in doing {
            error = error.context(step.to_string());
        }
        block_for(&describe(&error), None).render(&Env::new(true, false, true, 100))
    }

    fn plain(error: impl Into<antiq::Error>, doing: &[&str]) -> Vec<String> {
        let mut error = anyhow::Error::new(error.into());
        for step in doing {
            error = error.context(step.to_string());
        }
        block_for(&describe(&error), None).render_plain()
    }

    #[test]
    fn outside_a_collection_says_what_a_collection_is_and_how_to_get_into_one() {
        let error = CollectionError::NotACollection {
            start: "/Users/jakob/Downloads".into(),
        };
        assert_eq!(
            rich(error, &["find the antiq collection"]),
            [
                " ✖ ERROR",
                "▌ /Users/jakob/Downloads is not inside an antiq collection, which has an .antiq/ folder and none of",
                "▌ its parent directories has one",
                "▌",
                "▌ How to fix it",
                "▌   Change into your collection, or create one here with antiq init.",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to find the antiq collection.",
            ]
        );
    }

    #[test]
    fn push_without_a_target_shows_the_config_to_add() {
        assert_eq!(
            rich(PushError::NoTarget, &["push the antiq collection"]),
            [
                " ✖ ERROR",
                "▌ No push target is configured, so push has nowhere to mirror the antiq collection to",
                "▌",
                "▌ How to fix it",
                "▌   Add this to .antiq/config.toml:",
                "▌",
                "▌     [target]",
                "▌     url = \"/Volumes/nas/antiq/projects\"",
                "▌",
                "▌   Then run antiq push --init once on an empty directory of the target to claim it.",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to push the antiq collection.",
            ]
        );
    }

    #[test]
    fn a_pipe_gets_the_same_sections_as_labelled_lines() {
        assert_eq!(
            plain(PushError::NoTarget, &["push the antiq collection"]),
            [
                "error: no push target is configured, so push has nowhere to mirror the antiq collection to",
                "  fix: Add this to .antiq/config.toml:",
                "         [target]",
                "         url = \"/Volumes/nas/antiq/projects\"",
                "       Then run antiq push --init once on an empty directory of the target to claim it.",
                "  while attempting to push the antiq collection",
            ]
        );
    }

    #[test]
    fn a_push_that_found_the_lock_held_explains_the_lock() {
        let held = PushError::Run(RunError::Lock(LockError::HeldUnknown));
        assert_eq!(
            rich(held, &[]),
            rich(LockError::HeldUnknown, &[]),
            "a lock held under a push is the lock, not a push error"
        );
    }

    #[test]
    fn a_held_lock_names_the_run_and_says_to_wait() {
        let held = LockError::Held {
            pid: 4242,
            started: "2026-11-03T10:00:00Z".parse().expect("valid literal"),
        };
        assert_eq!(
            rich(held, &[]),
            [
                " ✖ ERROR",
                "▌ Another antiq run holds the lock (pid 4242, started 2026-11-03T10:00:00Z), and runs that change an",
                "▌ antiq collection take turns",
                "▌",
                "▌ How to fix it",
                "▌   Wait until that run ends, then run the command again.",
            ]
        );
        assert_eq!(
            rich(LockError::HeldUnknown, &[]),
            [
                " ✖ ERROR",
                "▌ Another antiq run holds the lock but has not written who it is yet, and runs that change an antiq",
                "▌ collection take turns",
                "▌",
                "▌ How to fix it",
                "▌   Wait until that run ends, then run the command again.",
            ]
        );
    }

    #[test]
    fn a_listing_of_a_newer_format_asks_for_a_newer_antiq() {
        assert_eq!(
            rich(ListingError::FormatTooNew { found: 2 }, &[]),
            [
                " ✖ ERROR",
                "▌ The listing has format 2, which this antiq cannot read",
                "▌",
                "▌ How to fix it",
                "▌   Use a version of antiq that reads format 2.",
            ]
        );
    }

    #[test]
    fn a_path_outside_the_collection_names_the_root() {
        let error = ReferenceError::Outside {
            path: "/elsewhere/proj".into(),
            root: "/home/jakob/projects".into(),
        };
        assert_eq!(
            rich(error, &["pin the piece"]),
            [
                " ✖ ERROR",
                "▌ /elsewhere/proj is outside the antiq collection at /home/jakob/projects",
                "▌",
                "▌ How to fix it",
                "▌   Name a path below /home/jakob/projects.",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to pin the piece.",
            ]
        );
    }

    #[test]
    fn a_marker_of_another_host_names_both_and_the_way_past_it() {
        let error = PushError::MarkerMismatch {
            marker_host: "imperius".into(),
            marker_path: "/home/a/coll".into(),
            host: "workbench".into(),
            path: "/home/b/coll".into(),
        };
        assert_eq!(
            rich(error, &[]),
            [
                " ✖ ERROR",
                "▌ The target's marker .antiq-target names another host or path: it names host imperius and path",
                "▌ /home/a/coll, and this run is host workbench and path /home/b/coll",
                "▌",
                "▌ How to fix it",
                "▌   Push with antiq push --force to overwrite the target once, or edit the marker.",
            ]
        );
    }

    #[test]
    fn a_damaged_store_lists_the_versions_with_their_repair() {
        let error = PushError::DamagedStore {
            lines: vec![
                "tools/scan-old 2026-11-05T120211: the listing is missing; run `antiq rebuild tools/scan-old 2026-11-05T120211`".into(),
                "experiments/synth-toy 2026-11-05T120211: the archive is missing".into(),
            ],
            commands: Vec::new(),
        };
        assert_eq!(
            rich(error, &["push the antiq collection"]),
            [
                " ✖ ERROR",
                "▌ Push is refused: the antiq collection holds versions that are damaged or incomplete, and push",
                "▌ copies them as they are",
                "▌",
                "▌ How to fix it",
                "▌   Repair or remove these versions first:",
                "▌",
                "▌     tools/scan-old 2026-11-05T120211: the listing is missing; run `antiq rebuild tools/scan-old 2026-11-05T120211`",
                "▌     experiments/synth-toy 2026-11-05T120211: the archive is missing",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to push the antiq collection.",
            ]
        );
    }

    #[test]
    fn a_target_that_refuses_modes_names_the_option_and_the_first_lines() {
        let error = PushError::TargetRefusesModes {
            shown: vec![
                "rsync: [receiver] mkstemp \"/t/a\" failed: Operation not permitted (1)".into(),
                "rsync: [receiver] mkstemp \"/t/b\" failed: Operation not permitted (1)".into(),
            ],
            more: 37,
        };
        assert_eq!(
            rich(error, &["push the antiq collection"]),
            [
                " ✖ ERROR",
                "▌ The mirror failed: the target does not let rsync set file modes (rsync exit 23)",
                "▌",
                "▌ How to fix it",
                "▌   The target does not let rsync set file modes. Add this to .antiq/config.toml:",
                "▌",
                "▌     [target]",
                "▌     rsync_args = [\"--no-perms\"]",
                "▌",
                "▌   rsync said:",
                "▌",
                "▌     rsync: [receiver] mkstemp \"/t/a\" failed: Operation not permitted (1)",
                "▌     rsync: [receiver] mkstemp \"/t/b\" failed: Operation not permitted (1)",
                "▌     and 37 more lines",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to push the antiq collection.",
            ]
        );
    }

    #[test]
    fn the_commands_of_a_damaged_store_are_pasteable_from_another_directory() {
        let piece = RelPath::parse("my docs/scan").expect("a path");
        let stamp = Stamp::parse("2026-11-05T120211").expect("a stamp");
        let command = VersionCommand {
            verb: "rebuild",
            piece: piece.clone(),
            stamp,
        };
        let error = antiq::Error::from(PushError::DamagedStore {
            lines: vec![format!(
                "my docs/scan 2026-11-05T120211: the listing is missing. Run {}.",
                command.written()
            )],
            commands: vec![command],
        });
        let error = anyhow::Error::new(error);
        let description = describe(&error);

        let lines = block_for(&description, Some(Path::new("/work/my coll"))).render_plain();
        assert_eq!(lines[1], "  fix: Repair or remove these versions first:");
        assert_eq!(
            lines[2],
            "         my docs/scan 2026-11-05T120211: the listing is missing. Run `antiq -C '/work/my coll' rebuild 'my docs/scan' 2026-11-05T120211`."
        );
        let plain = block_for(&description, None).render_plain();
        assert!(
            plain[2].ends_with("Run `antiq rebuild 'my docs/scan' 2026-11-05T120211`."),
            "{plain:?}"
        );
    }

    #[test]
    fn an_error_without_an_explanation_keeps_its_text_as_the_title() {
        let error = antiq::Error::Usage("bad input".into());
        assert_eq!(
            rich(error, &["read the config file"]),
            [
                " ✖ ERROR",
                "▌ Bad input",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to read the config file.",
            ]
        );
    }

    #[test]
    fn the_rich_title_of_an_explained_error_starts_with_a_capital_and_the_pipe_keeps_the_message() {
        let lines = rich(PushError::NoTarget, &[]);
        assert!(lines[1].starts_with("▌ No push target"), "{lines:?}");
        let piped = plain(PushError::NoTarget, &[]);
        assert!(piped[0].starts_with("error: no push target"), "{piped:?}");
    }

    #[test]
    fn a_pipe_keeps_the_backticks_of_the_message_that_json_carries() {
        let error = ReferenceError::Outside {
            path: "/a`b".into(),
            root: "/root".into(),
        };
        assert_eq!(
            plain(error, &[])[0],
            "error: ``/a`b`` is outside the antiq collection at `/root`"
        );
        assert_eq!(
            rich(
                ReferenceError::Outside {
                    path: "/a`b".into(),
                    root: "/root".into(),
                },
                &[]
            )[1],
            "▌ /a`b is outside the antiq collection at /root"
        );
    }

    #[test]
    fn the_reason_of_an_unreachable_target_stays_plain_text_in_the_title() {
        let error = PushError::Unreachable {
            reason: "ssh said `no route` to `host`".into(),
        };
        assert_eq!(rich(error, &[])[1], "▌ Ssh said `no route` to `host`");
    }

    /// A piece whose name holds two backticks, which a plain pair of
    /// backticks around it would pair up with the ones around the input.
    const TICKED_PIECE: &str = "projects/a`b`c";

    #[test]
    fn a_version_lookup_keeps_a_piece_with_backticks_whole_in_console_and_pipe() {
        let error = || VersionLookupError::NoSuch {
            piece: TICKED_PIECE.into(),
            input: "2020".into(),
            stamps: vec!["2026-11-04T090211".into()],
        };
        assert_eq!(
            rich(RestoreError::Version(error()), &[])[1],
            "▌ 2020 matches no version of projects/a`b`c; there are 2026-11-04T090211"
        );
        assert_eq!(
            plain(RestoreError::Version(error()), &[])[0],
            "error: `2020` matches no version of ``projects/a`b`c``; there are 2026-11-04T090211"
        );
        let ambiguous = VersionLookupError::Ambiguous {
            piece: TICKED_PIECE.into(),
            input: "2026-11-04".into(),
            stamps: vec!["2026-11-04T100000".into(), "2026-11-04T090211".into()],
        };
        assert!(
            rich(RestoreError::Version(ambiguous), &[])[1]
                .starts_with("▌ 2026-11-04 names no single version of projects/a`b`c, as 2"),
        );
    }

    #[test]
    fn a_newer_format_keeps_a_piece_with_backticks_whole_in_console_and_pipe() {
        let restore = || RestoreError::FormatTooNew {
            piece: TICKED_PIECE.into(),
            stamp: "2026-11-04T090211".into(),
        };
        assert_eq!(
            rich(restore(), &[])[1],
            "▌ Version 2026-11-04T090211 of projects/a`b`c has a listing of a newer format than this antiq reads"
        );
        let rebuild = RebuildError::FormatTooNew {
            piece: TICKED_PIECE.into(),
            stamp: "2026-11-04T090211".into(),
        };
        assert_eq!(
            rich(rebuild, &[])[1],
            "▌ The listing of version 2026-11-04T090211 of projects/a`b`c is of a newer format than antiq reads"
        );
    }

    #[test]
    fn a_help_with_a_literal_block_keeps_the_lines_of_the_block() {
        let paragraphs =
            super::help_paragraphs("Add this:\n\n  [target]\n  url = \"x\"\n\nThen run `antiq`.");
        assert_eq!(
            paragraphs,
            [
                Para::Text(Rich::new().text("Add this:")),
                Para::Blank,
                Para::pre("  [target]"),
                Para::pre("  url = \"x\""),
                Para::Blank,
                Para::Text(Rich::parse("Then run `antiq`.")),
            ]
        );
    }
}
