// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a run of `archive` and its dry run report.

use crate::commands::survey::Refusal;
use crate::plan::{HeldDryRun, PlanAction, Step};
use crate::relpath::RelPath;
use crate::report::{CommandReport, Warning};
use jiff::Timestamp;
use serde::Serialize;

/// The outcome of `archive`.
#[derive(Debug)]
pub enum Outcome {
    /// A real run.
    Archived(ArchiveReport),
    /// A dry run.
    Planned(ArchivePlan),
    /// A dry run that found the lock held.
    Held(HeldDryRun),
}

/// A piece whose folder is gone and whose version is in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchivedPiece {
    pub piece: String,
    pub stamp: String,
    /// The time of the version, which the stamp spells.
    pub version: Timestamp,
    pub archive_bytes: u64,
    pub files: u64,
    pub bytes: u64,
}

/// Why a piece that was due is not archived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The folder changed while it was packed or after the verify.
    Differs,
    /// The store had no room for the piece.
    NoRoom,
    /// Any other failure; the message names the step and the place.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedPiece {
    pub piece: String,
    pub reason: SkipReason,
    pub message: String,
}

/// One reason `archive` leaves a piece alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub kind: &'static str,
    /// Relative to the collection root.
    pub path: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

impl Reason {
    /// A piece named on the command line carries a pin.
    pub(super) fn pinned(pin: &RelPath) -> Reason {
        Reason {
            kind: "pinned",
            path: pin.to_string(),
            text: format!("pinned: {pin}"),
            line: None,
        }
    }

    /// A piece named on the command line has no file that counts.
    pub(super) fn empty(piece: &RelPath) -> Reason {
        Reason {
            kind: "empty",
            path: piece.to_string(),
            text: format!("empty: {piece}"),
            line: None,
        }
    }
}

impl From<&Refusal> for Reason {
    fn from(refusal: &Refusal) -> Reason {
        Reason {
            kind: refusal.kind.as_str(),
            path: refusal.path.to_string(),
            text: refusal.to_string(),
            line: refusal.line,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefusedPiece {
    pub piece: String,
    pub reasons: Vec<Reason>,
}

impl RefusedPiece {
    /// `reason, reason`
    pub fn reasons_text(&self) -> String {
        let texts: Vec<&str> = self.reasons.iter().map(|r| r.text.as_str()).collect();
        texts.join(", ")
    }
}

/// What a real run did.
#[derive(Debug, Serialize)]
pub struct ArchiveReport {
    pub archived: Vec<ArchivedPiece>,
    pub skipped: Vec<SkippedPiece>,
    pub refused: Vec<RefusedPiece>,
    /// The repairs at the start of the run that were done.
    pub repairs: Vec<Step<ArchiveAction>>,
    pub warnings: Vec<Warning>,
    #[serde(skip)]
    pub(super) named: bool,
}

impl ArchiveReport {
    /// 3 when a piece was skipped, 1 when the piece named on the command
    /// line is refused, else 0. Without a named piece, refusals never
    /// change the code.
    pub fn exit_code(&self) -> u8 {
        exit_code(self.partial(), self.named, &self.refused)
    }
}

impl CommandReport for ArchiveReport {
    fn partial(&self) -> bool {
        !self.skipped.is_empty()
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

fn exit_code(partial: bool, named: bool, refused: &[RefusedPiece]) -> u8 {
    if partial {
        3
    } else if named && !refused.is_empty() {
        1
    } else {
        0
    }
}

/// A piece a dry run would archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArchiveAction {
    Archive { piece: String },
}

impl PlanAction for ArchiveAction {
    fn describe(&self) -> String {
        match self {
            ArchiveAction::Archive { piece } => format!("archive {piece}"),
        }
    }
}

/// What a dry run would do: the repairs first, then the pieces it would
/// archive, then the ones it refuses.
#[derive(Debug, Serialize)]
pub struct ArchivePlan {
    dry_run: bool,
    pub actions: Vec<Step<ArchiveAction>>,
    pub refused: Vec<RefusedPiece>,
    pub warnings: Vec<Warning>,
    #[serde(skip)]
    named: bool,
}

impl ArchivePlan {
    pub(super) fn new(
        actions: Vec<Step<ArchiveAction>>,
        refused: Vec<RefusedPiece>,
        warnings: Vec<Warning>,
        named: bool,
    ) -> ArchivePlan {
        ArchivePlan {
            dry_run: true,
            actions,
            refused,
            warnings,
            named,
        }
    }

    /// The lines the console prints: a `Would ...` line per action, a
    /// `Refused ...` line per refused piece, or `Nothing to do.` for none.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .actions
            .iter()
            .map(|action| format!("Would {}", action.describe()))
            .collect();
        lines.extend(
            self.refused
                .iter()
                .map(|piece| format!("Refused {}: {}", piece.piece, piece.reasons_text())),
        );
        if lines.is_empty() {
            lines.push("Nothing to do.".to_string());
        }
        lines
    }

    /// 1 when the piece named on the command line is refused, else 0.
    pub fn exit_code(&self) -> u8 {
        exit_code(false, self.named, &self.refused)
    }
}

impl CommandReport for ArchivePlan {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}
