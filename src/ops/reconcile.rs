// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `reconcile` command, as far as it needs no remote: it rebuilds the
//! index, and a real run starts with the repairs of every run.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::Error;
use crate::id::ObjectId;
use crate::index::{self, Index};
use crate::lock::Holder;
use crate::plan::{Plan, PlanAction};
use crate::report::{self, CommandReport, Warning};
use crate::run::{self, Started};
use serde::Serialize;

/// Errors of the `reconcile` command.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {}

impl crate::error::HasKind for ReconcileError {}

/// One step of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReconcileAction {
    /// Empty the index and build it again from the stored listings.
    RebuildIndex { member_lists: usize },
}

impl PlanAction for ReconcileAction {
    fn describe(&self) -> String {
        match self {
            ReconcileAction::RebuildIndex { member_lists: 1 } => {
                "rebuild the index (1 member list)".to_string()
            }
            ReconcileAction::RebuildIndex { member_lists } => {
                format!("rebuild the index ({member_lists} member lists)")
            }
        }
    }
}

/// An object the index could not be built for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedObject {
    pub id: ObjectId,
    pub reason: String,
}

/// What the rebuild of the index did.
#[derive(Debug, Default, Serialize)]
pub struct IndexReport {
    /// Cache files written.
    pub written: usize,
    /// Items removed from the cache directory.
    pub removed: usize,
    /// Objects whose listing could not be read.
    pub skipped: Vec<SkippedObject>,
}

/// What a run of `reconcile` did.
#[derive(Debug, Serialize)]
pub struct ReconcileReport {
    pub index: IndexReport,
    pub warnings: Vec<Warning>,
}

impl CommandReport for ReconcileReport {
    /// An object whose listing could not be read stays out of the index.
    fn partial(&self) -> bool {
        !self.index.skipped.is_empty()
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// The result of [`reconcile`].
#[derive(Debug)]
pub enum Reconciled {
    /// A dry run: what would be done, what to tell about the collection, and
    /// the run that holds the lock.
    Planned {
        plan: Plan<ReconcileAction>,
        notes: Vec<Warning>,
        holder: Option<Holder>,
    },
    Done(ReconcileReport),
}

/// What a collection with a remote is told, since the remote's share of
/// the work is not done by this command.
pub const REMOTE_PART_NOT_DONE: &str = "the remote part of reconcile is not done yet";

/// What `reconcile` tells about the collection: the notes of every command
/// and, with a remote attached, that its part is left undone.
fn collection_notes(collection: &Collection) -> Vec<Warning> {
    let mut notes = report::setting_notes(collection);
    if !collection.config().remotes.is_empty() {
        notes.push(Warning::new(REMOTE_PART_NOT_DONE));
    }
    notes
}

/// Brings the index in line with the stored metadata. `prune` asks for the
/// removal of entries that are missing on the remote, which this command
/// cannot do yet.
///
/// Returns `crate::Error`, not an error of its own, so that the errors
/// nested in it keep the details their JSON form carries.
pub fn reconcile(
    collection: &Collection,
    clock: &dyn Clock,
    prune: bool,
    dry_run: bool,
) -> Result<Reconciled, Error> {
    if prune {
        return Err(Error::NotImplemented {
            command: "reconcile --prune".to_string(),
        });
    }
    match run::start(collection, clock, dry_run)? {
        Started::DryRun { holder } => {
            let index = Index::compute(collection)?;
            let member_lists = index
                .entries()
                .iter()
                .filter_map(|entry| entry.listing.as_ref().map(|listing| (entry.id, listing)))
                .filter(|(id, listing)| collection.object_dir(*id).join(listing.as_str()).is_file())
                .count();
            let mut plan = Plan::new();
            plan.push(ReconcileAction::RebuildIndex { member_lists });
            Ok(Reconciled::Planned {
                plan,
                notes: collection_notes(collection),
                holder,
            })
        }
        Started::Guarded(guard) => {
            let rebuilt = index::rebuild(collection, guard.lock())?;
            let mut warnings = collection_notes(collection);
            warnings.extend_from_slice(guard.warnings());
            Ok(Reconciled::Done(ReconcileReport {
                index: IndexReport {
                    written: rebuilt.written,
                    removed: rebuilt.removed,
                    skipped: rebuilt
                        .skipped
                        .into_iter()
                        .map(|(id, reason)| SkippedObject { id, reason })
                        .collect(),
                },
                warnings,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(skipped: Vec<SkippedObject>) -> ReconcileReport {
        ReconcileReport {
            index: IndexReport {
                written: 0,
                removed: 0,
                skipped,
            },
            warnings: vec![Warning::new("a repair did something")],
        }
    }

    #[test]
    fn a_run_is_partial_when_an_object_was_skipped() {
        let skipped = SkippedObject {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("a ULID"),
            reason: "its listing is damaged".to_string(),
        };
        assert!(report(vec![skipped]).partial());
        assert!(!report(Vec::new()).partial());
    }

    #[test]
    fn a_report_hands_out_its_warnings() {
        assert_eq!(
            report(Vec::new()).warnings(),
            [Warning::new("a repair did something")]
        );
    }
}
