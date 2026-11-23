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
}
