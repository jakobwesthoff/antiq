// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The live pieces of a collection: the folders its patterns select.
//!
//! `status`, `find` and `grep` all start from this selection, so a piece is
//! live for each of them by the same rule.

use crate::collection::Collection;
use crate::error::HasKind;
use crate::filesystem::tree::TreeRoot;
use crate::relpath::RelPath;
use crate::report::Warning;
use crate::select::{Rules, SelectError, Selection, select};
use crate::shown::Shown;
use crate::versions::VersionsError;
use crate::versions::store::{is_absent, lookup_exact};
use std::collections::BTreeSet;
use std::path::Path;

/// Which pieces a search covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The live pieces and the store.
    Both,
    /// The live pieces only.
    Live,
    /// The store only.
    Archived,
}

impl Scope {
    pub fn includes_live(self) -> bool {
        self != Scope::Archived
    }

    pub fn includes_store(self) -> bool {
        self != Scope::Live
    }
}

/// Why the live pieces could not be selected.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum LiveError {
    #[error("cannot open the collection {path}: {source}")]
    Root {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Select(
        #[from]
        #[diagnostic_source]
        SelectError,
    ),
}

impl HasKind for LiveError {}

/// The pieces the patterns of a collection select, with the open collection
/// root and the select rules they were found by.
pub struct LivePieces {
    root: TreeRoot,
    rules: Rules,
    selection: Selection,
}

impl LivePieces {
    pub fn select(collection: &Collection) -> Result<LivePieces, LiveError> {
        let config = &collection.config().collection;
        let root = TreeRoot::open(collection.root()).map_err(|source| LiveError::Root {
            path: Shown(&collection.root()).to_string(),
            source,
        })?;
        let rules = Rules::new(&config.patterns, &config.exclude)?.bind(&root)?;
        let selection = select(&root, &rules)?;
        Ok(LivePieces {
            root,
            rules,
            selection,
        })
    }

    /// The pieces, sorted by path bytes.
    pub fn pieces(&self) -> &[RelPath] {
        self.selection.pieces()
    }

    /// The directories selection could not read, so pieces below them are
    /// not among [`LivePieces::pieces`].
    pub fn unreadable(&self) -> &[RelPath] {
        self.selection.unreadable()
    }

    /// One warning for each directory selection could not read.
    pub fn unreadable_warnings(&self) -> Vec<Warning> {
        self.unreadable()
            .iter()
            .map(|directory| Warning::new(format!("unreadable: {directory}")))
            .collect()
    }

    pub(crate) fn root(&self) -> &TreeRoot {
        &self.root
    }

    pub(crate) fn rules(&self) -> &Rules {
        &self.rules
    }
}

/// A live piece with the spelling of the store folder it shares a name with.
///
/// The volume of the collection can treat two spellings as one name, so a
/// live `Notes` is the piece the store keeps under `notes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spelled {
    /// The path of the live folder, as it is walked.
    pub live: RelPath,
    /// The path of the store folder, or the live path when there is none.
    pub stored: RelPath,
}

impl Spelled {
    /// A live piece that keeps its own spelling.
    pub fn alone(live: &RelPath) -> Spelled {
        Spelled {
            live: live.clone(),
            stored: live.clone(),
        }
    }

    /// The spelling of the live folder, when it differs from the store's.
    pub fn other_spelling(&self) -> Option<&RelPath> {
        (self.live != self.stored).then_some(&self.live)
    }
}

/// What looks a path up below the store: the path of the store folder it
/// names on this volume, `None` when there is none.
type Lookup<'a> = &'a dyn Fn(&RelPath) -> Result<Option<RelPath>, VersionsError>;

/// How a search spells its live pieces: the live paths and the store folders
/// that exist go in, the spellings and the warnings for the paths that could
/// not be looked up come out.
pub(crate) type Spelling<'a> =
    &'a dyn Fn(&[RelPath], &BTreeSet<&RelPath>) -> (Vec<Spelled>, Vec<Warning>);

/// Spells each live path as the store folder that `lookup` finds for it. A
/// path that is a store folder stays; one for which `lookup` finds a store
/// folder takes its spelling; any other keeps its own. A lookup that fails
/// leaves the live spelling and is a warning, never read as "no folder".
pub(crate) fn stored_spellings(
    live: &[RelPath],
    folders: &BTreeSet<&RelPath>,
    lookup: Lookup<'_>,
) -> (Vec<Spelled>, Vec<Warning>) {
    let mut spelled = Vec::with_capacity(live.len());
    let mut warnings = Vec::new();
    for path in live {
        if folders.contains(path) {
            spelled.push(Spelled::alone(path));
            continue;
        }
        match lookup(path) {
            Ok(Some(found)) if folders.contains(&found) => spelled.push(Spelled {
                live: path.clone(),
                stored: found,
            }),
            Ok(_) => spelled.push(Spelled::alone(path)),
            Err(error) => {
                warnings.push(Warning::new(format!(
                    "cannot compare the spelling of {path}: {error}"
                )));
                spelled.push(Spelled::alone(path));
            }
        }
    }
    (spelled, warnings)
}

/// [`stored_spellings`] against the store below `pieces_dir`.
///
/// One `lstat` of the whole live path tells most live pieces apart from a
/// store folder: they have none on any volume, so the lookup that lists the
/// parents of the path is left to the paths that exist. A store that cannot
/// be opened is a store that is not there, and no live piece has a spelling
/// in it.
pub(crate) fn spell_as_stored(
    pieces_dir: &Path,
    live: &[RelPath],
    folders: &BTreeSet<&RelPath>,
) -> (Vec<Spelled>, Vec<Warning>) {
    let Ok(store) = TreeRoot::open(pieces_dir) else {
        return (live.iter().map(Spelled::alone).collect(), Vec::new());
    };
    let lookup = |path: &RelPath| match store.lstat(path) {
        Err(error) if is_absent(&error) => Ok(None),
        Err(source) => Err(VersionsError::Io {
            action: "look at",
            path: Shown(path).to_string(),
            source,
        }),
        Ok(_) => lookup_exact(&store, path),
    };
    stored_spellings(live, folders, &lookup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use std::fs;

    fn collection(temp: &TempCollection, config_lines: &str) -> Collection {
        fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{config_lines}\n"),
        )
        .expect("a config can be written");
        Collection::find(temp.root()).expect("a collection")
    }

    fn names(live: &LivePieces) -> Vec<String> {
        live.pieces().iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_pieces_are_the_folders_the_patterns_select() {
        let mut temp = TempCollection::minimal();
        temp.tree().file("notes/a.txt", "a");
        temp.tree().file("projects/scanner/main.rs", "m");
        let found = LivePieces::select(&collection(&temp, "patterns = [\"*/*\", \"notes\"]"))
            .expect("the pieces are selected");
        assert_eq!(names(&found), ["notes", "projects/scanner"]);
    }

    #[test]
    fn a_directory_that_cannot_be_read_is_named_in_a_warning() {
        let mut temp = TempCollection::minimal();
        temp.tree().file("notes/a.txt", "a");
        temp.tree().unreadable_dir("projects");
        let found = LivePieces::select(&collection(&temp, "patterns = [\"*/*\", \"notes\"]"))
            .expect("the pieces are selected");
        assert_eq!(names(&found), ["notes"]);
        let warnings: Vec<String> = found
            .unreadable_warnings()
            .into_iter()
            .map(|warning| warning.message)
            .collect();
        assert_eq!(warnings, ["unreadable: projects"]);
    }

    #[test]
    fn a_pattern_that_cannot_be_used_is_the_error_of_the_selection_alone() {
        let temp = TempCollection::minimal();
        let error = LivePieces::select(&collection(&temp, "patterns = [\"[\"]"))
            .err()
            .expect("the pattern is invalid");
        assert!(
            error.to_string().starts_with("pattern [ is invalid: "),
            "{error}"
        );
    }

    #[test]
    fn a_scope_says_which_sides_it_covers() {
        assert!(Scope::Both.includes_live() && Scope::Both.includes_store());
        assert!(Scope::Live.includes_live() && !Scope::Live.includes_store());
        assert!(!Scope::Archived.includes_live() && Scope::Archived.includes_store());
    }

    // =========================================================
    // Spelling
    // =========================================================

    use crate::testing::tree::{Tree, folds_case};
    use crate::versions::VersionsError;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    fn pairs(spelled: &[Spelled]) -> Vec<(String, String)> {
        spelled
            .iter()
            .map(|piece| (piece.live.to_string(), piece.stored.to_string()))
            .collect()
    }

    fn pair(live: &str, stored: &str) -> (String, String) {
        (live.to_string(), stored.to_string())
    }

    /// A lookup that finds the store folder `found` for every path.
    fn finds(found: &str) -> impl Fn(&RelPath) -> Result<Option<RelPath>, VersionsError> {
        let found = path(found);
        move |_| Ok(Some(found.clone()))
    }

    #[test]
    fn a_path_that_is_a_store_folder_is_not_looked_up() {
        let notes = path("notes");
        let other = path("other");
        let folders = BTreeSet::from([&notes, &other]);

        // A lookup that finds `other` would move `notes` there.
        let (spelled, warnings) = stored_spellings(&[path("notes")], &folders, &finds("other"));

        assert_eq!(pairs(&spelled), [pair("notes", "notes")]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_path_for_which_the_lookup_finds_a_store_folder_takes_its_spelling() {
        let notes = path("notes");
        let folders = BTreeSet::from([&notes]);

        let (spelled, warnings) = stored_spellings(&[path("Notes")], &folders, &finds("notes"));

        assert_eq!(pairs(&spelled), [pair("Notes", "notes")]);
        assert_eq!(spelled[0].other_spelling(), Some(&path("Notes")));
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_path_without_a_store_folder_keeps_its_spelling() {
        let notes = path("notes");
        let folders = BTreeSet::from([&notes]);

        // The lookup finds nothing, or a directory that holds no version.
        for answer in [None, Some(path("parent"))] {
            let (spelled, warnings) =
                stored_spellings(&[path("other")], &folders, &|_| Ok(answer.clone()));
            assert_eq!(pairs(&spelled), [pair("other", "other")]);
            assert_eq!(spelled[0].other_spelling(), None);
            assert!(warnings.is_empty());
        }
    }

    #[test]
    fn a_lookup_that_fails_is_a_warning_and_the_live_spelling_stays() {
        let folders = BTreeSet::new();

        let (spelled, warnings) = stored_spellings(&[path("Notes")], &folders, &|_| {
            Err(VersionsError::Io {
                action: "look at",
                path: "Notes".to_string(),
                source: std::io::Error::other("boom"),
            })
        });

        assert_eq!(pairs(&spelled), [pair("Notes", "Notes")]);
        assert_eq!(warnings.len(), 1);
        let message = &warnings[0].message;
        assert!(message.starts_with("cannot compare the spelling of Notes: "));
    }

    #[test]
    fn a_live_path_that_is_absent_from_the_store_keeps_its_spelling() {
        let mut store = Tree::new();
        store.dir("notes/old");
        let notes = path("notes/old");
        let folders = BTreeSet::from([&notes]);

        let (spelled, warnings) = spell_as_stored(store.root(), &[path("other")], &folders);

        assert_eq!(pairs(&spelled), [pair("other", "other")]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn on_a_folding_volume_a_live_path_takes_the_spelling_of_the_store_folder() {
        let mut store = Tree::new();
        store.dir("projects/client");
        let folds = folds_case(store.root());
        let client = path("projects/client");
        let folders = BTreeSet::from([&client]);

        let (spelled, warnings) =
            spell_as_stored(store.root(), &[path("Projects/Client")], &folders);

        let stored = ["Projects/Client", "projects/client"][usize::from(folds)];
        assert_eq!(pairs(&spelled), [pair("Projects/Client", stored)]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_store_that_cannot_be_opened_gives_no_spelling() {
        let missing = Tree::new();
        let gone = missing.root().join("no-such-store");
        let notes = path("notes");
        let folders = BTreeSet::from([&notes]);

        let (spelled, warnings) = spell_as_stored(&gone, &[path("Notes")], &folders);

        assert_eq!(pairs(&spelled), [pair("Notes", "Notes")]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_path_below_a_folder_that_cannot_be_looked_into_is_a_warning() {
        let mut store = Tree::new();
        store.unreadable_dir("locked");
        assert!(
            std::fs::read_dir(store.root().join("locked")).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let old = path("locked/old");
        let folders = BTreeSet::from([&old]);

        let (spelled, warnings) = spell_as_stored(store.root(), &[path("locked/new")], &folders);

        assert_eq!(pairs(&spelled), [pair("locked/new", "locked/new")]);
        assert_eq!(warnings.len(), 1);
        let message = &warnings[0].message;
        assert!(message.starts_with("cannot compare the spelling of locked/new: "));
    }
}
