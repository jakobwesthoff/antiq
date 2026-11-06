// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Errors of the library and their mapping to exit codes.
//!
//! Every module has its own error enum next to its code. [`Error`] wraps
//! them all, and [`Error::kind`] names the JSON error kind and the exit
//! code of whichever one it holds.

use crate::clock::whole_seconds;
use crate::collection::CollectionError;
use crate::config::ConfigError;
use crate::fsx::FsxError;
use crate::id::IdError;
use crate::idle::IdleError;
use crate::listing::ListingError;
use crate::lock::LockError;
use crate::machine::MachineError;
use crate::ops::init::InitError;
use crate::pack::PackError;
use crate::pin::PinError;
use crate::reference::ReferenceError;
use crate::relpath::RelPathError;
use crate::repair::RepairError;
use crate::run::RunError;
use crate::select::SelectError;
use crate::sysmem::SysmemError;
use crate::walk::WalkError;
use serde_json::{Map, Value};

/// The kind of an error as scripts see it in the JSON error object, and
/// the exit code that goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Usage,
    /// A command that has no implementation yet.
    NotImplemented,
    NotACollection,
    AmbiguousReference,
    UnknownReference,
    /// A path given as a piece, `--to` or `--path` lies outside the collection.
    OutsideCollection,
    LockHeld,
    FormatTooNew,
    /// Every failure without a kind of its own.
    Other,
}

impl ErrorKind {
    pub const ALL: [ErrorKind; 9] = [
        ErrorKind::Usage,
        ErrorKind::NotImplemented,
        ErrorKind::NotACollection,
        ErrorKind::AmbiguousReference,
        ErrorKind::UnknownReference,
        ErrorKind::OutsideCollection,
        ErrorKind::LockHeld,
        ErrorKind::FormatTooNew,
        ErrorKind::Other,
    ];

    /// The name in the `kind` key of the JSON error object.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::NotImplemented => "not_implemented",
            ErrorKind::NotACollection => "not_a_collection",
            ErrorKind::AmbiguousReference => "ambiguous_reference",
            ErrorKind::UnknownReference => "unknown_reference",
            ErrorKind::OutsideCollection => "outside_collection",
            ErrorKind::LockHeld => "lock_held",
            ErrorKind::FormatTooNew => "format_too_new",
            ErrorKind::Other => "other",
        }
    }

    /// Exit codes 0 and 3 belong to runs that did not fail; see the
    /// exit-code table of the spec.
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorKind::Usage => 2,
            _ => 1,
        }
    }
}

/// Implemented by every module error. The default is for errors that have
/// no kind of their own.
pub trait HasKind {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// Any error of the library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error("{command} is not implemented")]
    NotImplemented { command: String },
    #[error(transparent)]
    Collection(#[from] CollectionError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Fsx(#[from] FsxError),
    #[error(transparent)]
    Id(#[from] IdError),
    #[error(transparent)]
    Idle(#[from] IdleError),
    #[error(transparent)]
    Init(#[from] InitError),
    #[error(transparent)]
    Listing(#[from] ListingError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    Pin(#[from] PinError),
    #[error(transparent)]
    Reference(#[from] ReferenceError),
    #[error(transparent)]
    RelPath(#[from] RelPathError),
    #[error(transparent)]
    Repair(#[from] RepairError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    Sysmem(#[from] SysmemError),
    #[error(transparent)]
    Walk(#[from] WalkError),
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Usage(_) => ErrorKind::Usage,
            Error::NotImplemented { .. } => ErrorKind::NotImplemented,
            Error::Collection(e) => e.kind(),
            Error::Config(e) => e.kind(),
            Error::Fsx(e) => e.kind(),
            Error::Id(e) => e.kind(),
            Error::Idle(e) => e.kind(),
            Error::Init(e) => e.kind(),
            Error::Listing(e) => e.kind(),
            Error::Lock(e) => e.kind(),
            Error::Machine(e) => e.kind(),
            Error::Pack(e) => e.kind(),
            Error::Pin(e) => e.kind(),
            Error::Reference(e) => e.kind(),
            Error::RelPath(e) => e.kind(),
            Error::Repair(e) => e.kind(),
            Error::Run(e) => e.kind(),
            Error::Select(e) => e.kind(),
            Error::Sysmem(e) => e.kind(),
            Error::Walk(e) => e.kind(),
        }
    }

    /// The keys the JSON error object carries next to `kind` and
    /// `message` for this error. Empty for errors without details.
    pub fn details(&self) -> Map<String, Value> {
        let mut details = Map::new();
        match self {
            Error::Reference(ReferenceError::Ambiguous { candidates, .. }) => {
                details.insert(
                    "candidates".into(),
                    serde_json::to_value(candidates)
                        .expect("candidates are plain strings and always serialise"),
                );
            }
            Error::Lock(LockError::Held { pid, started })
            | Error::Run(RunError::Lock(LockError::Held { pid, started })) => {
                details.insert("pid".into(), Value::from(*pid));
                details.insert(
                    "started".into(),
                    Value::from(whole_seconds(*started).to_string()),
                );
            }
            _ => {}
        }
        details
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::CollectionError;
    use crate::listing::ListingError;
    use crate::lock::LockError;
    use crate::reference::{Candidate, ReferenceError};
    use jiff::Timestamp;
    use serde_json::json;

    fn candidate(id: &str, path: &str) -> Candidate {
        Candidate {
            id: id.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn every_kind_has_its_name_and_exit_code() {
        let table = [
            (ErrorKind::Usage, "usage", 2),
            (ErrorKind::NotImplemented, "not_implemented", 1),
            (ErrorKind::NotACollection, "not_a_collection", 1),
            (ErrorKind::AmbiguousReference, "ambiguous_reference", 1),
            (ErrorKind::UnknownReference, "unknown_reference", 1),
            (ErrorKind::OutsideCollection, "outside_collection", 1),
            (ErrorKind::LockHeld, "lock_held", 1),
            (ErrorKind::FormatTooNew, "format_too_new", 1),
            (ErrorKind::Other, "other", 1),
        ];
        assert_eq!(table.len(), ErrorKind::ALL.len());
        for (kind, name, code) in table {
            assert!(ErrorKind::ALL.contains(&kind), "{kind:?} is not in ALL");
            assert_eq!(kind.as_str(), name);
            assert_eq!(kind.exit_code(), code, "exit code of {name}");
        }
    }

    #[test]
    fn usage_and_not_implemented_carry_their_kinds() {
        assert_eq!(Error::Usage("bad".into()).kind(), ErrorKind::Usage);
        let error = Error::NotImplemented {
            command: "status".into(),
        };
        assert_eq!(error.kind(), ErrorKind::NotImplemented);
        assert_eq!(error.to_string(), "status is not implemented");
    }

    #[test]
    fn wrapped_errors_report_their_own_kind() {
        let cases: Vec<(Error, ErrorKind)> = vec![
            (
                CollectionError::NotACollection {
                    start: "/tmp/x".into(),
                }
                .into(),
                ErrorKind::NotACollection,
            ),
            (
                ReferenceError::Ambiguous {
                    reference: "01ABCD".into(),
                    candidates: vec![],
                }
                .into(),
                ErrorKind::AmbiguousReference,
            ),
            (
                ReferenceError::Unknown {
                    reference: "01ABCD".into(),
                }
                .into(),
                ErrorKind::UnknownReference,
            ),
            (
                ReferenceError::Outside {
                    path: "/elsewhere/proj".into(),
                    root: "/collection".into(),
                }
                .into(),
                ErrorKind::OutsideCollection,
            ),
            (
                LockError::Held {
                    pid: 7,
                    started: Timestamp::UNIX_EPOCH,
                }
                .into(),
                ErrorKind::LockHeld,
            ),
            (
                ListingError::FormatTooNew { found: 2 }.into(),
                ErrorKind::FormatTooNew,
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(error.kind(), kind, "kind of `{error}`");
        }
    }

    #[test]
    fn a_kind_without_an_override_is_other() {
        #[derive(Debug)]
        struct Plain;
        impl HasKind for Plain {}
        assert_eq!(Plain.kind(), ErrorKind::Other);
    }

    #[test]
    fn ambiguous_reference_lists_candidates() {
        let error: Error = ReferenceError::Ambiguous {
            reference: "01ABCD".into(),
            candidates: vec![
                candidate("01ABCDXXXXXXXXXXXXXXXXXXXX", "projects/a"),
                candidate("01ABCDYYYYYYYYYYYYYYYYYYYY", "projects/b"),
            ],
        }
        .into();
        assert_eq!(
            serde_json::Value::Object(error.details()),
            json!({"candidates": [
                {"id": "01ABCDXXXXXXXXXXXXXXXXXXXX", "path": "projects/a"},
                {"id": "01ABCDYYYYYYYYYYYYYYYYYYYY", "path": "projects/b"},
            ]})
        );
    }

    #[test]
    fn lock_details_carry_pid_and_whole_second_start() {
        let started: Timestamp = "2026-11-03T10:00:00.123456789Z"
            .parse()
            .expect("literal is valid RFC 3339");
        let error: Error = LockError::Held { pid: 4242, started }.into();
        assert_eq!(
            serde_json::Value::Object(error.details()),
            json!({"pid": 4242, "started": "2026-11-03T10:00:00Z"})
        );
    }

    #[test]
    fn errors_without_details_give_an_empty_map() {
        assert!(Error::Usage("bad".into()).details().is_empty());
    }
}
