// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `reconcile` command, as far as it needs no remote: it rebuilds the
//! index, and a real run starts with the repairs of every run.

use crate::brake::{self, BrakeInput};
use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::Error;
use crate::id::ObjectId;
use crate::index::{self, Index};
use crate::lock::Holder;
use crate::plan::{Plan, PlanAction};
use crate::remote::away::AwayError;
use crate::remote::cleanup::{LeftAlone, Step, What};
use crate::remote::{Remote, RemoteError};
use crate::report::{self, CommandReport, Warning};
use crate::run::{self, Started};
use serde::Serialize;

mod prune;
mod remote_part;

/// Errors of the `reconcile` command.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Away(#[from] AwayError),
}

impl crate::error::HasKind for ReconcileError {}

/// One step of a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReconcileAction {
    /// Empty the index and build it again from the stored listings.
    RebuildIndex { member_lists: usize },
    /// Note that the archive of a piece is gone from the remote.
    MarkMissing { id: ObjectId, path: String },
    /// Clear that note, as the archive is back.
    ClearMissing { id: ObjectId, path: String },
    /// Drop a piece from this machine.
    Drop { id: ObjectId, path: String },
    /// Remove a leftover from the remote.
    Remove {
        id: ObjectId,
        name: String,
        what: What,
    },
    /// Remove a leftover from the remote if it is old enough, which a dry
    /// run cannot tell.
    AgeUnknown {
        id: ObjectId,
        name: String,
        what: What,
    },
}

impl ReconcileAction {
    fn from_mark(piece: PieceRef) -> ReconcileAction {
        ReconcileAction::MarkMissing {
            id: piece.id,
            path: piece.path,
        }
    }

    fn from_unmark(piece: PieceRef) -> ReconcileAction {
        ReconcileAction::ClearMissing {
            id: piece.id,
            path: piece.path,
        }
    }

    fn from_step(step: Step) -> ReconcileAction {
        match step {
            Step::Remove { id, name, what } => ReconcileAction::Remove { id, name, what },
            Step::AgeUnknown { id, name, what } => ReconcileAction::AgeUnknown { id, name, what },
        }
    }
}

impl PlanAction for ReconcileAction {
    fn describe(&self) -> String {
        match self {
            ReconcileAction::MarkMissing { path, .. } => {
                format!("mark {path} as missing on the remote")
            }
            ReconcileAction::Drop { path, .. } => format!("drop {path}"),
            ReconcileAction::ClearMissing { path, .. } => {
                format!("clear the missing mark of {path}")
            }
            ReconcileAction::Remove { id, name, what } => Step::Remove {
                id: *id,
                name: name.clone(),
                what: *what,
            }
            .describe(),
            ReconcileAction::AgeUnknown { id, name, what } => Step::AgeUnknown {
                id: *id,
                name: name.clone(),
                what: *what,
            }
            .describe(),
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

/// What the remote part did to the remote.
#[derive(Debug, Default, Serialize)]
pub struct RemoteReport {
    pub pulled: usize,
    pub removed: usize,
    pub skipped: usize,
    pub left_alone: Vec<LeftAlone>,
}

/// A piece named in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PieceRef {
    pub id: ObjectId,
    pub path: String,
}

/// A piece whose newest committed version another machine wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedElsewhere {
    pub id: ObjectId,
    pub path: String,
    pub version: u32,
    pub machine: String,
}

/// A committed version that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnreadableVersion {
    pub id: ObjectId,
    pub version: u32,
    pub reason: String,
}

/// What a run of `reconcile` did.
#[derive(Debug, Serialize)]
pub struct ReconcileReport {
    pub index: IndexReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteReport>,
    pub marked: Vec<PieceRef>,
    pub pruned: Vec<PieceRef>,
    pub unmarked: Vec<PieceRef>,
    pub changed_elsewhere: Vec<ChangedElsewhere>,
    pub unreadable_versions: Vec<UnreadableVersion>,
    pub warnings: Vec<Warning>,
    /// Why the remote part stopped, after the local part was done.
    #[serde(skip)]
    pub stop: Option<Error>,
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
        /// Why the remote part stopped, after the local part was listed.
        stop: Option<Error>,
    },
    Done(ReconcileReport),
}

/// What `reconcile` tells about the collection: the notes of every
/// command.
fn collection_notes(collection: &Collection) -> Vec<Warning> {
    report::setting_notes(collection)
}

/// Opens the remote of a collection; tests pass their own.
pub type Opener<'a> = &'a dyn Fn(&Collection) -> Result<Box<dyn Remote>, RemoteError>;

/// What a run of `reconcile` was asked to do.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileRequest {
    pub prune: bool,
    pub force: bool,
    pub dry_run: bool,
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
    opener: Opener,
    brake: BrakeInput<'_>,
    request: ReconcileRequest,
) -> Result<Reconciled, Error> {
    let ReconcileRequest { prune, dry_run, .. } = request;
    let percent = collection.config().collection.brake_percent;
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
            let mut kept_notes = Vec::new();
            plan.push(ReconcileAction::RebuildIndex { member_lists });
            let part = remote_part::run(collection, clock, opener, None);
            for action in &part.actions {
                plan.push(action.clone());
            }
            if prune {
                let dropping = prune::plan(collection, &part)?;
                let hold = brake::evaluate(percent, dropping.candidates(), dropping.set_size);
                plan.brake = hold.filter(|_| !brake.force);
                kept_notes = dropping.notes;
                for piece in dropping.pieces {
                    plan.push(ReconcileAction::Drop {
                        id: piece.id,
                        path: piece.path,
                    });
                }
            }
            let mut notes = collection_notes(collection);
            notes.extend(part.warnings);
            notes.extend(kept_notes);
            Ok(Reconciled::Planned {
                plan,
                notes,
                holder,
                stop: part.stop,
            })
        }
        Started::Guarded(guard) => {
            let rebuilt = index::rebuild(collection, guard.lock())?;
            let part = remote_part::run(collection, clock, opener, Some(guard.lock()));
            let mut pruned = Vec::new();
            let mut kept_notes = Vec::new();
            if prune {
                let dropping = prune::plan(collection, &part)?;
                let hold = brake::evaluate(percent, dropping.candidates(), dropping.set_size);
                brake::gate(
                    hold,
                    brake.force,
                    &|hold| {
                        format!(
                            "Drop {} of {} pieces from this machine?",
                            hold.planned, hold.set_size
                        )
                    },
                    brake.interaction,
                )?;
                pruned = prune::execute(collection, guard.lock(), &part, &dropping.pieces)?;
                kept_notes = dropping.notes;
            }
            let mut warnings = collection_notes(collection);
            warnings.extend_from_slice(guard.warnings());
            warnings.extend(part.warnings);
            warnings.extend(kept_notes);
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
                remote: part.remote,
                marked: part.marked,
                pruned,
                unmarked: part.unmarked,
                changed_elsewhere: part.changed_elsewhere,
                unreadable_versions: part.unreadable_versions,
                warnings,
                stop: part.stop,
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
            remote: None,
            marked: Vec::new(),
            pruned: Vec::new(),
            unmarked: Vec::new(),
            changed_elsewhere: Vec::new(),
            unreadable_versions: Vec::new(),
            warnings: vec![Warning::new("a repair did something")],
            stop: None,
        }
    }

    #[test]
    fn the_actions_of_a_cleanup_step_read_as_the_step_does() {
        let id: ObjectId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("a ULID");
        for step in [
            Step::Remove {
                id,
                name: ".meta.2.studio.part".into(),
                what: What::TempFile,
            },
            Step::AgeUnknown {
                id,
                name: ".meta.2.studio.part".into(),
                what: What::TempFile,
            },
        ] {
            assert_eq!(
                ReconcileAction::from_step(step.clone()).describe(),
                step.describe()
            );
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
