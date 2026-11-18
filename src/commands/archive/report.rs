// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a run of `archive` and its dry run report.

use crate::commands::survey::Refusal;
use crate::pin::PIN_FILE;
use crate::plan::{HeldDryRun, PlanAction, Step};
use crate::relpath::{Name, RelPath};
use crate::report::{CommandReport, Warning};
use jiff::Timestamp;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::fmt;

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

/// One reason `archive` or `import` leaves a folder alone. `Display` is
/// the wording every view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The walk or a marker file refuses the folder.
    Walk(Refusal),
    /// The folder carries a pin. `pin` is the pin file, relative to the
    /// collection root; for `import`, to the folder.
    Pinned { pin: RelPath },
    /// No file in the folder counts. `piece` is relative to the collection
    /// root; for `import`, it is the folder itself, which is the root.
    Empty { piece: RelPath },
}

impl Reason {
    /// The pin of the piece at `top`.
    pub fn pinned(top: &RelPath) -> Reason {
        let name =
            Name::from_bytes(PIN_FILE.as_bytes().to_vec()).expect("the pin file name is a name");
        Reason::Pinned {
            pin: top.join(&name),
        }
    }

    /// The piece at `piece` has no file that counts.
    pub fn empty(piece: &RelPath) -> Reason {
        Reason::Empty {
            piece: piece.clone(),
        }
    }

    /// The name of the reason in JSON.
    pub fn kind(&self) -> &'static str {
        match self {
            Reason::Walk(refusal) => refusal.kind.as_str(),
            Reason::Pinned { .. } => "pinned",
            Reason::Empty { .. } => "empty",
        }
    }

    /// The path the reason is about, relative to the collection root; for
    /// `import`, to the folder, which is `.` itself.
    pub fn path(&self) -> String {
        match self {
            Reason::Walk(refusal) => refusal.path.to_string(),
            Reason::Pinned { pin } => pin.to_string(),
            Reason::Empty { piece } if piece.is_root() => ".".to_string(),
            Reason::Empty { piece } => piece.to_string(),
        }
    }

    /// The line of the rule file at fault.
    pub fn line(&self) -> Option<usize> {
        match self {
            Reason::Walk(refusal) => refusal.line,
            Reason::Pinned { .. } | Reason::Empty { .. } => None,
        }
    }
}

impl From<&Refusal> for Reason {
    fn from(refusal: &Refusal) -> Reason {
        Reason::Walk(refusal.clone())
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Walk(refusal) => refusal.fmt(f),
            Reason::Pinned { pin } => write!(f, "pinned: {pin}"),
            Reason::Empty { piece } if piece.is_root() => f.write_str("empty: no file counts"),
            Reason::Empty { piece } => write!(f, "empty: {piece}"),
        }
    }
}

impl Serialize for Reason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let line = self.line();
        let mut found = serializer.serialize_struct("Reason", 3 + usize::from(line.is_some()))?;
        found.serialize_field("kind", self.kind())?;
        found.serialize_field("path", &self.path())?;
        found.serialize_field("text", &self.to_string())?;
        if let Some(line) = line {
            found.serialize_field("line", &line)?;
        }
        found.end()
    }
}

/// The reasons of one folder as one piece of text.
pub struct Reasons<'a>(pub &'a [Reason]);

impl fmt::Display for Reasons<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, reason) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{reason}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefusedPiece {
    pub piece: String,
    pub reasons: Vec<Reason>,
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
    Archive {
        piece: String,
        /// What the version leaves out: ignored entries (a folder counts
        /// once, with a trailing `/`) and the `.antiq-restored` marker.
        left_out: Vec<String>,
    },
}

impl PlanAction for ArchiveAction {
    fn describe(&self) -> String {
        match self {
            ArchiveAction::Archive { piece, .. } => format!("archive {piece}"),
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

    /// The lines the console prints: a `Would ...` line per action, a line
    /// per left-out path under each piece it would archive, a
    /// `Refused ...` line per refused piece, or `Nothing to do.` for none.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for action in &self.actions {
            lines.push(format!("Would {}", action.describe()));
            if let Step::Command(ArchiveAction::Archive { left_out, .. }) = action {
                lines.extend(
                    left_out
                        .iter()
                        .map(|path| format!("Would leave out {path}")),
                );
            }
        }
        lines.extend(
            self.refused
                .iter()
                .map(|piece| format!("Refused {}: {}", piece.piece, Reasons(&piece.reasons))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::survey::RefusalKind;
    use serde_json::json;

    fn path(text: &str) -> RelPath {
        RelPath::from_bytes(text.as_bytes().to_vec()).expect("a test path is a path")
    }

    #[test]
    fn a_walk_reason_serialises_with_its_line_after_the_text() {
        let reason = Reason::from(&Refusal {
            kind: RefusalKind::RuleFile,
            path: path("piece/.antiqignore"),
            line: Some(3),
        });

        let text = serde_json::to_string(&reason).expect("serialises");

        assert_eq!(
            text,
            r#"{"kind":"rule_file","path":"piece/.antiqignore","text":"rule file: piece/.antiqignore:3","line":3}"#
        );
    }

    #[test]
    fn a_reason_without_a_line_serialises_without_the_key() {
        let pinned = serde_json::to_value(Reason::pinned(&path("piece"))).expect("serialises");
        let empty = serde_json::to_value(Reason::empty(&path("piece"))).expect("serialises");

        assert_eq!(
            pinned,
            json!({"kind": "pinned", "path": "piece/.antiq-pin", "text": "pinned: piece/.antiq-pin"})
        );
        assert_eq!(
            empty,
            json!({"kind": "empty", "path": "piece", "text": "empty: piece"})
        );
    }

    #[test]
    fn several_reasons_read_as_one_text() {
        let reasons = [
            Reason::pinned(&path("piece")),
            Reason::empty(&path("piece")),
        ];

        assert_eq!(
            Reasons(&reasons).to_string(),
            "pinned: piece/.antiq-pin, empty: piece"
        );
    }

    #[test]
    fn the_folder_itself_is_shown_as_a_dot() {
        let reason = Reason::empty(&RelPath::root());

        assert_eq!(reason.path(), ".");
    }
}
