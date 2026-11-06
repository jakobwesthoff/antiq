// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The block a failed run is reported as.

use super::Description;
use super::console::block::{Block, Para};
use super::console::rich::Rich;
use antiq::collection::CollectionError;
use antiq::commands::push::PushError;
use antiq::listing::ListingError;
use antiq::lock::LockError;
use antiq::reference::ReferenceError;
use antiq::run::RunError;
use antiq::target::marker;
use antiq::versions::naming::VersionLookupError;

/// The error block for `description`: the title, the sections an
/// explanation adds, the candidates of an ambiguous reference, and the
/// steps the run was in.
///
/// Texts that come from a file or the command line go in as plain text,
/// never as markup, because they can hold backticks.
pub fn block_for(description: &Description) -> Block {
    let explanation = description.library_error.and_then(explain);
    let mut block = match &explanation {
        Some(explanation) => Block::error(explanation.title.clone()),
        None => Block::error(Rich::new().text(&description.root)),
    };
    if let Some(explanation) = explanation {
        for paragraph in explanation.why {
            block = block.why(paragraph);
        }
        for paragraph in explanation.how {
            block = block.how(paragraph);
        }
    }
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

/// What a terminal reader needs beyond the message of the library: a
/// title in the words of the console, why it happened and how to fix it.
/// The library message stays what `--json` carries.
struct Explanation {
    title: Rich,
    why: Vec<Para>,
    how: Vec<Para>,
}

impl Explanation {
    fn new(title: Rich) -> Explanation {
        Explanation {
            title,
            why: Vec::new(),
            how: Vec::new(),
        }
    }

    fn why(mut self, text: Rich) -> Explanation {
        self.why.push(text.into());
        self
    }

    fn how(mut self, text: Rich) -> Explanation {
        self.how.push(text.into());
        self
    }
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

/// The held lock behind `error`, whether a run error wraps it or not.
fn held_lock(error: &antiq::Error) -> Option<&LockError> {
    find::<LockError>(error).or_else(|| match find::<RunError>(error) {
        Some(RunError::Lock(lock)) => Some(lock),
        _ => None,
    })
}

fn explain(error: &antiq::Error) -> Option<Explanation> {
    if let Some(CollectionError::NotACollection { start }) = find(error) {
        return Some(not_a_collection(&start.display().to_string()));
    }
    // A push error that has no explanation of its own may still wrap one
    // that has, such as the held lock of the run it could not start.
    if let Some(explanation) = find::<PushError>(error).and_then(explain_push) {
        return Some(explanation);
    }
    if let Some(ReferenceError::Outside { path, root }) = find(error) {
        return Some(outside(path, root));
    }
    if let Some(ListingError::FormatTooNew { found }) = find(error) {
        return Some(
            Explanation::new(Rich::new().text(&format!(
                "A listing has format {found}, which this antiq cannot read"
            )))
            .how(Rich::new().text(&format!(
                "Use a version of antiq that reads format {found}."
            ))),
        );
    }
    match held_lock(error) {
        Some(LockError::Held { pid, started }) => {
            Some(lock_held(Some((*pid, started.to_string()))))
        }
        Some(LockError::HeldUnknown) => Some(lock_held(None)),
        _ => None,
    }
}

fn not_a_collection(start: &str) -> Explanation {
    Explanation::new(Rich::new().path(start).text(" is not inside an antiq collection"))
        .why(Rich::parse(
            "antiq works in a directory that holds an antiq collection, which has an `.antiq/` folder. None of the parent directories has one.",
        ))
        .how(Rich::parse(
            "Change into your collection, or create one here with `antiq init`.",
        ))
}

fn outside(path: &str, root: &str) -> Explanation {
    Explanation::new(
        Rich::new()
            .path(path)
            .text(" is outside the antiq collection"),
    )
    .why(
        Rich::new()
            .text("The antiq collection is at ")
            .path(root)
            .text(", and ")
            .path(path)
            .text(" is not below it."),
    )
    .how(Rich::new().text("Name a path below ").path(root).text("."))
}

fn lock_held(holder: Option<(u32, String)>) -> Explanation {
    let turns = "Runs that change an antiq collection take turns. ";
    let why = match holder {
        Some((pid, started)) => Rich::new()
            .text(turns)
            .text(&format!(
                "The run with pid {pid}, started {started}, holds the lock in "
            ))
            .cmd(".antiq/lock")
            .text("."),
        None => Rich::new()
            .text(turns)
            .text("Another run holds the lock in ")
            .cmd(".antiq/lock")
            .text(" but has not written who it is yet."),
    };
    Explanation::new(Rich::new().text("Another antiq run holds the lock"))
        .why(why)
        .how(Rich::new().text("Wait until that run ends, then run the command again."))
}

fn explain_push(error: &PushError) -> Option<Explanation> {
    match error {
        PushError::NoTarget => Some(no_target()),
        PushError::MarkerMismatch {
            marker_host,
            marker_path,
            host,
            path,
        } => Some(
            Explanation::new(Rich::new().text("The target's marker names another host or path"))
                .why(
                    Rich::new()
                        .text("The marker ")
                        .cmd(marker::FILE_NAME)
                        .text(" on the target names host ")
                        .path(marker_host)
                        .text(" and path ")
                        .path(marker_path)
                        .text(". This run is host ")
                        .path(host)
                        .text(" and path ")
                        .path(path)
                        .text("."),
                )
                .how(Rich::parse(
                    "Push with `antiq push --force` to overwrite the target once, or edit the marker.",
                )),
        ),
        PushError::DamagedStore { lines } => {
            let mut explanation = Explanation::new(Rich::new().text(
                "Push refused: the store holds versions that are damaged or incomplete",
            ))
            .why(Rich::new().text(
                "push copies the store as it is, so it only runs on a store without damage.",
            ))
            .how(Rich::new().text("Repair or remove these versions first:"));
            explanation.how.push(Para::Blank);
            for line in lines {
                explanation.how.push(Para::pre(&format!("  {line}")));
            }
            Some(explanation)
        }
        _ => None,
    }
}

fn no_target() -> Explanation {
    let mut explanation = Explanation::new(Rich::new().text("No push target is configured"))
        .why(Rich::parse(
            "`antiq push` mirrors the antiq collection to a target, a local path or an ssh host, and the config names none.",
        ))
        .how(Rich::parse("Add this to `.antiq/config.toml`:"));
    explanation.how.push(Para::Blank);
    explanation.how.push(Para::pre("  [target]"));
    explanation
        .how
        .push(Para::pre("  url = \"/Volumes/nas/antiq/projects\""));
    explanation.how.push(Para::Blank);
    explanation.how(Rich::parse(
        "Then run `antiq push --init` once on an empty directory of the target to claim it.",
    ))
}

#[cfg(test)]
mod tests {
    use super::super::console::env::Env;
    use super::super::describe;
    use super::block_for;
    use antiq::collection::CollectionError;
    use antiq::commands::push::PushError;
    use antiq::listing::ListingError;
    use antiq::lock::LockError;
    use antiq::reference::ReferenceError;
    use antiq::run::RunError;

    /// The rich lines of the block for `error`, run through the steps
    /// `doing` (innermost first), on a terminal 100 columns wide.
    fn rich(error: impl Into<antiq::Error>, doing: &[&str]) -> Vec<String> {
        let mut error = anyhow::Error::new(error.into());
        for step in doing {
            error = error.context(step.to_string());
        }
        block_for(&describe(&error)).render(&Env::new(true, false, true, 100))
    }

    fn plain(error: impl Into<antiq::Error>, doing: &[&str]) -> Vec<String> {
        let mut error = anyhow::Error::new(error.into());
        for step in doing {
            error = error.context(step.to_string());
        }
        block_for(&describe(&error)).render_plain()
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
                "▌ /Users/jakob/Downloads is not inside an antiq collection",
                "▌",
                "▌ Why it happened",
                "▌   antiq works in a directory that holds an antiq collection, which has an .antiq/ folder. None of",
                "▌   the parent directories has one.",
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
                "▌ No push target is configured",
                "▌",
                "▌ Why it happened",
                "▌   antiq push mirrors the antiq collection to a target, a local path or an ssh host, and the config",
                "▌   names none.",
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
                "error: No push target is configured",
                "  why: antiq push mirrors the antiq collection to a target, a local path or an ssh host, and the config names none.",
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
                "▌ Another antiq run holds the lock",
                "▌",
                "▌ Why it happened",
                "▌   Runs that change an antiq collection take turns. The run with pid 4242, started",
                "▌   2026-11-03T10:00:00Z, holds the lock in .antiq/lock.",
                "▌",
                "▌ How to fix it",
                "▌   Wait until that run ends, then run the command again.",
            ]
        );
        assert_eq!(
            rich(LockError::HeldUnknown, &[]),
            [
                " ✖ ERROR",
                "▌ Another antiq run holds the lock",
                "▌",
                "▌ Why it happened",
                "▌   Runs that change an antiq collection take turns. Another run holds the lock in .antiq/lock but",
                "▌   has not written who it is yet.",
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
                "▌ A listing has format 2, which this antiq cannot read",
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
                "▌ /elsewhere/proj is outside the antiq collection",
                "▌",
                "▌ Why it happened",
                "▌   The antiq collection is at /home/jakob/projects, and /elsewhere/proj is not below it.",
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
                "▌ The target's marker names another host or path",
                "▌",
                "▌ Why it happened",
                "▌   The marker .antiq-target on the target names host imperius and path /home/a/coll. This run is",
                "▌   host workbench and path /home/b/coll.",
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
        };
        assert_eq!(
            rich(error, &["push the antiq collection"]),
            [
                " ✖ ERROR",
                "▌ Push refused: the store holds versions that are damaged or incomplete",
                "▌",
                "▌ Why it happened",
                "▌   push copies the store as it is, so it only runs on a store without damage.",
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
    fn an_error_without_an_explanation_keeps_its_text_as_the_title() {
        let error = antiq::Error::Usage("bad input".into());
        assert_eq!(
            rich(error, &["read the config file"]),
            [
                " ✖ ERROR",
                "▌ bad input",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to read the config file.",
            ]
        );
    }
}
