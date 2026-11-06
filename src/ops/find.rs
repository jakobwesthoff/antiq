// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `find` command: lists and searches the objects of a collection.

use crate::clock::whole_seconds;
use crate::collection::Collection;
use crate::error::Error;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::index::{self, Index, IndexEntry, Match, Query};
use crate::ops::pin::folder_exists;
use crate::relpath::{Name, RelPath, escape};
use crate::report::{self, CommandReport, Warning};
use crate::store::Store;
use crate::tracking::{self, PieceState, derive_state};
use jiff::Timestamp;
use serde::{Serialize, Serializer};
use std::cmp::Reverse;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Errors of the `find` command.
#[derive(Debug, thiserror::Error)]
pub enum FindError {}

impl crate::error::HasKind for FindError {}

/// The state of a found object: one of the states of spec "States", or
/// `error` for an object whose entry, metadata or object cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundState {
    Piece(PieceState),
    Error,
}

impl Serialize for FoundState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            FoundState::Piece(state) => state.serialize(serializer),
            FoundState::Error => serializer.serialize_str("error"),
        }
    }
}

/// One object that matched.
#[derive(Debug, Clone, Serialize)]
pub struct FoundPiece {
    pub id: ObjectId,
    pub path: RelPath,
    /// The folder the object has or would be unearthed to, in the escaped
    /// form.
    pub abs_path: String,
    pub name: Name,
    pub state: FoundState,
    pub created: Timestamp,
    /// The size of the current payload, `None` without one.
    pub size: Option<u64>,
    /// The folder as the filesystem spells it, for `--print`.
    #[serde(skip)]
    pub folder: PathBuf,
    /// The shortest prefix of the ID that names no other known object.
    #[serde(skip)]
    pub short_id: String,
}

/// What `find` found.
#[derive(Debug, Serialize)]
pub struct FindReport {
    pub query: Option<String>,
    pub matches: Vec<FoundPiece>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for FindReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// The objects that match `query`, or all of them without one. Nothing is
/// written and no lock is taken, so `find` runs while another run holds
/// the collection.
///
/// A query matches the path of a piece, its name and the member paths of
/// its listing. Path matches come first, then matches through the name
/// alone, then matches through a member alone. The name only differs from
/// the path's last component for an imported archive.
///
/// Returns `crate::Error`, not an error of its own, so that the errors
/// nested in it keep the details their JSON form carries.
pub fn find(collection: &Collection, query: Option<&str>) -> Result<FindReport, Error> {
    let text = query.unwrap_or("");
    let index = Index::compute(collection)?;
    let searched = index::search(collection, &index, text)?;

    let mut warnings = report::notes(collection);
    for object in index.unreadable() {
        warnings.push(Warning::new(format!(
            "the object {} cannot be read: {}",
            object.id, object.reason
        )));
    }
    for (id, reason) in &searched.skipped {
        warnings.push(Warning::new(format!(
            "the contents of {id} cannot be searched: {reason}"
        )));
    }

    let root = TreeRoot::open(collection.root())
        .map_err(crate::fsx::io_error("open", collection.root()))?;
    let store = Store::new(collection);
    let mut matches = Vec::new();
    for id in in_order(&index, text, searched.matches) {
        let entry = index
            .get(id)
            .expect("the search returns objects of the index");
        matches.push(found(collection, &index, &store, &root, entry));
    }
    Ok(FindReport {
        query: query.map(str::to_string),
        matches,
        warnings,
    })
}

/// The IDs of the matches: through the path, then through the name alone,
/// then through a member alone. With an empty query every object is a path
/// match.
fn in_order(index: &Index, text: &str, found: Vec<Match>) -> Vec<ObjectId> {
    let (by_path, by_member): (Vec<Match>, Vec<Match>) = found
        .into_iter()
        .partition(|found| found.members.is_empty());
    let mut ids: Vec<ObjectId> = by_path.iter().map(|found| found.id).collect();

    let mut query = Query::parse(text);
    if !query.is_empty() {
        let known: HashSet<ObjectId> = by_path
            .iter()
            .chain(&by_member)
            .map(|found| found.id)
            .collect();
        let mut by_name: Vec<(Reverse<u32>, Reverse<Timestamp>, ObjectId)> = index
            .entries()
            .iter()
            .filter(|entry| !known.contains(&entry.id))
            .filter_map(|entry| {
                query
                    .score(&entry.name.to_string())
                    .map(|score| (Reverse(score), Reverse(entry.created), entry.id))
            })
            .collect();
        by_name.sort();
        ids.extend(by_name.into_iter().map(|(.., id)| id));
    }

    ids.extend(by_member.iter().map(|found| found.id));
    ids
}

fn found(
    collection: &Collection,
    index: &Index,
    store: &Store<'_>,
    root: &TreeRoot,
    entry: &IndexEntry,
) -> FoundPiece {
    let folder = collection
        .root()
        .join(OsStr::from_bytes(entry.path.as_bytes()));
    FoundPiece {
        id: entry.id,
        path: entry.path.clone(),
        abs_path: escape(folder.as_os_str().as_bytes()),
        name: entry.name.clone(),
        state: state_of(collection, store, root, entry),
        created: whole_seconds(entry.created),
        size: entry.size,
        folder,
        short_id: short_id(index, entry.id),
    }
}

/// The state of the object, as `status` derives it. A folder counts as
/// present when a directory is at the path of the tracking entry, else of
/// the metadata.
fn state_of(
    collection: &Collection,
    store: &Store<'_>,
    root: &TreeRoot,
    entry: &IndexEntry,
) -> FoundState {
    let Ok(tracked) = tracking::read(collection, entry.id) else {
        return FoundState::Error;
    };
    let path = tracked
        .as_ref()
        .map_or(&entry.path, |tracked| &tracked.path);
    // A location that cannot be looked at does not stop the listing: the
    // object is in error, as one with a damaged entry is.
    let Ok(present) = folder_exists(root, path) else {
        return FoundState::Error;
    };
    // Metadata that cannot be read now makes the derivation fail, which
    // is the state `error`.
    let current = store.current(entry.id).ok().flatten();
    let derived = derive_state(
        tracked.as_ref(),
        current.as_ref().map(|current| &current.metadata),
        present,
        collection.id(),
    );
    derived.map_or(FoundState::Error, FoundState::Piece)
}

/// The shortest prefix of the ID, at least 6 characters, that no other
/// known object shares.
fn short_id(index: &Index, id: ObjectId) -> String {
    let full = id.to_string();
    (6..=full.len())
        .map(|length| &full[..length])
        .find(|prefix| index.ids_with_prefix(prefix).len() == 1)
        .expect("a whole ID is shared by no other object")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_is_written_as_the_word_of_the_spec_or_error() {
        let states = [
            (FoundState::Piece(PieceState::RemoteOnly), "\"remote-only\""),
            (
                FoundState::Piece(PieceState::LiveOnAnotherMachine),
                "\"live-on-another-machine\"",
            ),
            (FoundState::Error, "\"error\""),
        ];
        for (state, json) in states {
            assert_eq!(serde_json::to_string(&state).expect("serialises"), json);
        }
    }

    #[test]
    fn a_search_is_never_partial_and_hands_out_its_warnings() {
        let report = FindReport {
            query: None,
            matches: Vec::new(),
            warnings: vec![Warning::new("an object could not be read")],
        };
        assert!(!report.partial());
        assert_eq!(
            report.warnings(),
            [Warning::new("an object could not be read")]
        );
    }
}
