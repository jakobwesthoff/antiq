// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` and `unpin` commands.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::Error;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::index::Index;
use crate::lock::Holder;
use crate::pin;
use crate::plan::{Plan, PlanAction};
use crate::reference::{resolve, resolve_path};
use crate::relpath::{Name, RelPath};
use crate::report::{CommandReport, Warning};
use crate::run::{self, Started};
use crate::select::{Rules, select};
use crate::store::Store;
use crate::tracking::{self, PieceState, derive_state};
use crate::tree::EntryKind;
use serde::Serialize;
use std::ffi::OsStr;
use std::io;
use std::path::Path;

/// Errors of the `pin` and `unpin` commands.
#[derive(Debug, thiserror::Error)]
pub enum PinOpError {
    #[error("the folder of {path} is not present")]
    NotPresent { path: RelPath },
    #[error("{subject} cannot be pinned: {reason}")]
    Unusable { subject: String, reason: String },
}

impl crate::error::HasKind for PinOpError {}

/// What the command does to the pin file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Pin,
    Unpin,
}

/// One step of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PinAction {
    Pin { id: Option<ObjectId>, path: RelPath },
    Unpin { id: Option<ObjectId>, path: RelPath },
}

impl PlanAction for PinAction {
    fn describe(&self) -> String {
        match self {
            PinAction::Pin { path, .. } => format!("pin {path}"),
            PinAction::Unpin { path, .. } => format!("unpin {path}"),
        }
    }
}

/// What a run of `pin` or `unpin` did.
#[derive(Debug, Serialize)]
pub struct PinReport {
    /// `None` for a new piece, which has no ID yet.
    pub id: Option<ObjectId>,
    pub path: RelPath,
    /// Whether the piece is pinned now.
    pub pinned: bool,
    /// Whether the run changed the pin file.
    pub changed: bool,
    pub warnings: Vec<Warning>,
}

impl CommandReport for PinReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// The result of [`run`].
#[derive(Debug)]
pub enum Pinned {
    /// A dry run: what would be done, and the run that holds the lock.
    Planned {
        plan: Plan<PinAction>,
        holder: Option<Holder>,
    },
    Done(PinReport),
}

/// Pins or unpins the piece that `given` names, relative to `cwd`.
pub fn run(
    collection: &Collection,
    clock: &dyn Clock,
    cwd: &Path,
    given: &OsStr,
    direction: Direction,
    dry_run: bool,
) -> Result<Pinned, Error> {
    // The repairs of a real run come first: they can change which object
    // holds a folder.
    let started = run::start(collection, clock, dry_run)?;
    let index = Index::compute(collection)?;
    let root = TreeRoot::open(collection.root())
        .map_err(crate::fsx::io_error("open", collection.root()))?;
    let Target { id, path } = target(collection, &index, &root, cwd, given)?;

    let pinned = current_pin(&root, &path)?;
    let wanted = direction == Direction::Pin;
    let changed = pinned != wanted;

    let guard = match started {
        Started::DryRun { holder } => {
            let mut plan = Plan::new();
            if changed {
                plan.push(match direction {
                    Direction::Pin => PinAction::Pin { id, path },
                    Direction::Unpin => PinAction::Unpin { id, path },
                });
            }
            return Ok(Pinned::Planned { plan, holder });
        }
        Started::Guarded(guard) => guard,
    };
    if changed {
        match direction {
            Direction::Pin => pin::create(&root, &path)?,
            Direction::Unpin => pin::remove(&root, &path)?,
        }
    }
    Ok(Pinned::Done(PinReport {
        id,
        path,
        pinned: wanted,
        changed,
        warnings: guard.warnings().to_vec(),
    }))
}

// =========================================================
// Which piece is meant
// =========================================================

struct Target {
    /// `None` for a new piece.
    id: Option<ObjectId>,
    path: RelPath,
}

fn target(
    collection: &Collection,
    index: &Index,
    root: &TreeRoot,
    cwd: &Path,
    given: &OsStr,
) -> Result<Target, Error> {
    // A folder that no object holds is a new piece, also where archived
    // objects are recorded at its path: `resolve` would name one of those.
    // A reference that is no path inside the collection falls through to
    // `resolve`, which reports why.
    if let Ok(path) = resolve_path(collection, cwd, Path::new(given))
        && is_new_piece(collection, index, root, &path)?
    {
        return Ok(Target { id: None, path });
    }

    let id = resolve(collection, index, cwd, given)?;
    let Some(path) = index.path_of(id).cloned() else {
        return Err(PinOpError::Unusable {
            subject: id.to_string(),
            reason: "it has neither metadata nor a tracking entry with a path".to_string(),
        }
        .into());
    };
    require_folder(collection, root, id, &path)?;
    Ok(Target { id: Some(id), path })
}

/// Whether a folder is at `path`, not followed if it is a symlink.
fn folder_exists(root: &TreeRoot, path: &RelPath) -> Result<bool, Error> {
    match root.lstat(path) {
        Ok(entry) => Ok(entry.kind == EntryKind::Directory),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => {
            Err(crate::fsx::io_error("look at", Path::new(&path.to_string()))(error).into())
        }
    }
}

/// Whether the folder at `path` is a piece that has no object: it exists,
/// no object holds it and the patterns select it.
fn is_new_piece(
    collection: &Collection,
    index: &Index,
    root: &TreeRoot,
    path: &RelPath,
) -> Result<bool, Error> {
    if !folder_exists(root, path)? || holder_of(collection, index, path)?.is_some() {
        return Ok(false);
    }
    let config = &collection.config().collection;
    let rules = Rules::new(&config.patterns, &config.exclude)?.bind(root)?;
    Ok(select(root, &rules)?.pieces().contains(path))
}

/// The object whose folder is at `path`: the one the tracking entries say
/// holds it, else an object recorded there that is live or unearthed once
/// a folder is assumed. A `gone` entry of live metadata owns a folder that
/// is back that way, which the entries alone do not show.
fn holder_of(
    collection: &Collection,
    index: &Index,
    path: &RelPath,
) -> Result<Option<ObjectId>, Error> {
    let at = index.at_path(path);
    if at.claimant.is_some() {
        return Ok(at.claimant);
    }
    let store = Store::new(collection);
    for id in at.others {
        let entry = tracking::read(collection, id)?;
        let current = store.current(id)?;
        let state = derive_state(
            entry.as_ref(),
            current.as_ref().map(|current| &current.metadata),
            true,
            collection.id(),
        );
        if matches!(state, Ok(PieceState::Live | PieceState::Unearthed)) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Refuses an object that has no folder to put the pin in.
fn require_folder(
    collection: &Collection,
    root: &TreeRoot,
    id: ObjectId,
    path: &RelPath,
) -> Result<(), Error> {
    let entry = tracking::read(collection, id)?;
    let current = Store::new(collection).current(id)?;
    let state = derive_state(
        entry.as_ref(),
        current.as_ref().map(|current| &current.metadata),
        folder_exists(root, path)?,
        collection.id(),
    );
    match state {
        Ok(PieceState::Live | PieceState::Unearthed) => Ok(()),
        Ok(_) => Err(PinOpError::NotPresent { path: path.clone() }.into()),
        Err(error) => Err(PinOpError::Unusable {
            subject: path.to_string(),
            reason: error.to_string(),
        }
        .into()),
    }
}

/// Whether the piece at `piece` has a pin. A directory of the pin file's
/// name is neither a pin nor something `unpin` may remove, so both
/// commands refuse it here, before they decide what to do.
fn current_pin(root: &TreeRoot, piece: &RelPath) -> Result<bool, Error> {
    let name = Name::from_bytes(pin::PIN_FILE.as_bytes().to_vec())
        .expect("the pin file name is a valid name");
    let file = piece.join(&name);
    match root.lstat(&file) {
        Ok(entry) if entry.kind == EntryKind::Directory => {
            Err(pin::PinError::NotAFile { path: file }.into())
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(pin::PinError::Stat { path: file, source }.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_is_never_partial_and_hands_out_its_warnings() {
        let report = PinReport {
            id: None,
            path: RelPath::parse("a").expect("a path"),
            pinned: true,
            changed: true,
            warnings: vec![Warning::new("a repair did something")],
        };
        assert!(!report.partial());
        assert_eq!(report.warnings(), [Warning::new("a repair did something")]);
    }
}
