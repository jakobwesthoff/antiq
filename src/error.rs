// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Errors of the library and their mapping to exit codes.
//!
//! Every module has its own error enum next to its code. [`Error`] wraps
//! them all, and [`Error::kind`] names the JSON error kind and the exit
//! code of whichever one it holds.

use crate::archive::PackError;
use crate::clock::whole_seconds;
use crate::collection::CollectionError;
use crate::commands::archive::ArchiveError;
use crate::commands::forget::ForgetError;
use crate::commands::fork::ForkError;
use crate::commands::grep::GrepError;
use crate::commands::import::ImportError;
use crate::commands::init::InitError;
use crate::commands::list::ListError;
use crate::commands::pin::PinCommandError;
use crate::commands::push::PushError;
use crate::commands::rebuild::RebuildError;
use crate::commands::restore::RestoreError;
use crate::commands::status::StatusError;
use crate::commands::survey::SurveyError;
use crate::commands::unearth::UnearthError;
use crate::config::ConfigError;
use crate::filesystem::FilesystemError;
use crate::id::IdError;
use crate::idle::IdleError;
use crate::listing::ListingError;
use crate::lock::LockError;
use crate::machine::MachineError;
use crate::markers::MarkerError;
use crate::pin::PinError;
use crate::query::QueryError;
use crate::reference::{Candidate, ReferenceError};
use crate::relpath::RelPathError;
use crate::repair::RepairError;
use crate::rules::RulesError;
use crate::run::RunError;
use crate::select::SelectError;
use crate::sysmem::SysmemError;
use crate::versions::VersionsError;
use crate::walk::WalkError;
use jiff::Timestamp;
use serde_json::{Map, Value};

/// The kind of an error as scripts see it in the JSON error object, and
/// the exit code that goes with it. The variants follow the order of the
/// spec's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Usage,
    NotACollection,
    AmbiguousReference,
    UnknownReference,
    /// A path given as a piece, `--as` or `--name` lies outside the collection.
    OutsideCollection,
    LockHeld,
    /// The target of `push` cannot be reached.
    TargetUnreachable,
    /// `push` runs without a target url.
    NoTarget,
    /// A command that has no implementation yet.
    NotImplemented,
    FormatTooNew,
    /// Every failure without a kind of its own.
    Other,
}

impl ErrorKind {
    pub const ALL: [ErrorKind; 11] = [
        ErrorKind::Usage,
        ErrorKind::NotACollection,
        ErrorKind::AmbiguousReference,
        ErrorKind::UnknownReference,
        ErrorKind::OutsideCollection,
        ErrorKind::LockHeld,
        ErrorKind::TargetUnreachable,
        ErrorKind::NoTarget,
        ErrorKind::NotImplemented,
        ErrorKind::FormatTooNew,
        ErrorKind::Other,
    ];

    /// The name in the `kind` key of the JSON error object.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::NotACollection => "not_a_collection",
            ErrorKind::AmbiguousReference => "ambiguous_reference",
            ErrorKind::UnknownReference => "unknown_reference",
            ErrorKind::OutsideCollection => "outside_collection",
            ErrorKind::LockHeld => "lock_held",
            ErrorKind::TargetUnreachable => "target_unreachable",
            ErrorKind::NoTarget => "no_target",
            ErrorKind::NotImplemented => "not_implemented",
            ErrorKind::FormatTooNew => "format_too_new",
            ErrorKind::Other => "other",
        }
    }

    /// Exit code 0 belongs to runs that did not fail and 3 to runs that
    /// finished with some pieces failed; see the exit-code table of the
    /// spec.
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorKind::Usage => 2,
            ErrorKind::TargetUnreachable => 4,
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
    Filesystem(#[from] FilesystemError),
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
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Forget(#[from] ForgetError),
    #[error(transparent)]
    Fork(#[from] ForkError),
    #[error(transparent)]
    Grep(#[from] GrepError),
    #[error(transparent)]
    Import(#[from] ImportError),
    #[error(transparent)]
    List(#[from] ListError),
    #[error(transparent)]
    PinCommand(#[from] PinCommandError),
    #[error(transparent)]
    Push(#[from] PushError),
    #[error(transparent)]
    Rebuild(#[from] RebuildError),
    #[error(transparent)]
    Restore(#[from] RestoreError),
    #[error(transparent)]
    Status(#[from] StatusError),
    #[error(transparent)]
    Survey(#[from] SurveyError),
    #[error(transparent)]
    Unearth(#[from] UnearthError),
    #[error(transparent)]
    Marker(#[from] MarkerError),
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error(transparent)]
    Rules(#[from] RulesError),
    #[error(transparent)]
    Versions(#[from] VersionsError),
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
            Error::Filesystem(e) => e.kind(),
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
            Error::Archive(e) => e.kind(),
            Error::Forget(e) => e.kind(),
            Error::Fork(e) => e.kind(),
            Error::Grep(e) => e.kind(),
            Error::Import(e) => e.kind(),
            Error::List(e) => e.kind(),
            Error::PinCommand(e) => e.kind(),
            Error::Push(e) => e.kind(),
            Error::Rebuild(e) => e.kind(),
            Error::Restore(e) => e.kind(),
            Error::Status(e) => e.kind(),
            Error::Survey(e) => e.kind(),
            Error::Unearth(e) => e.kind(),
            Error::Marker(e) => e.kind(),
            Error::Query(e) => e.kind(),
            Error::Rules(e) => e.kind(),
            Error::Versions(e) => e.kind(),
            Error::Walk(e) => e.kind(),
        }
    }

    /// The keys the JSON error object carries next to `kind` and
    /// `message` for this error. Empty for errors without details.
    pub fn details(&self) -> Map<String, Value> {
        let mut details = Map::new();
        // `Reference`, `Lock` and `Run` are transparent, so the walks start
        // at what they wrap.
        let (candidates, held) = match self {
            Error::Reference(inner) => (find_candidates(inner), find_held_lock(inner)),
            Error::Lock(inner) => (find_candidates(inner), find_held_lock(inner)),
            Error::Run(inner) => (find_candidates(inner), find_held_lock(inner)),
            other => (find_candidates(other), find_held_lock(other)),
        };
        if let Some(candidates) = candidates {
            details.insert(
                "candidates".into(),
                serde_json::to_value(candidates)
                    .expect("candidates are plain strings and always serialise"),
            );
        }
        if let Some((pid, started)) = held {
            details.insert("pid".into(), Value::from(pid));
            details.insert(
                "started".into(),
                Value::from(whole_seconds(started).to_string()),
            );
        }
        details
    }
}

/// The candidates of the ambiguous reference that `error` or one of its
/// sources is.
///
/// The same rule as for [`find_held_lock`] holds: a command error that wraps
/// a [`ReferenceError`] names it as its source and is not
/// `#[error(transparent)]`.
fn find_candidates<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a [Candidate]> {
    let mut link = Some(error);
    while let Some(current) = link {
        if let Some(ReferenceError::Ambiguous { candidates, .. }) = current.downcast_ref() {
            return Some(candidates);
        }
        link = current.source();
    }
    None
}

/// The pid and start of the held lock that `error` or one of its sources
/// is.
///
/// A command error that wraps a [`LockError`] or a [`RunError`] must name
/// it as its source and must not be `#[error(transparent)]`: a transparent
/// error forwards `source()` to the source of the wrapped error, which
/// skips the wrapped error itself and hides the lock from this walk.
fn find_held_lock(error: &(dyn std::error::Error + 'static)) -> Option<(u32, Timestamp)> {
    let mut link = Some(error);
    while let Some(current) = link {
        match (current.downcast_ref(), current.downcast_ref()) {
            (Some(LockError::Held { pid, started }), _)
            | (_, Some(RunError::Lock(LockError::Held { pid, started }))) => {
                return Some((*pid, *started));
            }
            _ => {}
        }
        link = current.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::CollectionError;
    use crate::listing::ListingError;
    use crate::lock::LockError;
    use crate::run::RunError;
    use jiff::Timestamp;
    use serde_json::json;

    fn candidate(path: &str) -> Candidate {
        Candidate {
            path: path.to_string(),
        }
    }

    #[test]
    fn every_kind_has_its_name_and_exit_code() {
        let table = [
            (ErrorKind::Usage, "usage", 2),
            (ErrorKind::NotACollection, "not_a_collection", 1),
            (ErrorKind::AmbiguousReference, "ambiguous_reference", 1),
            (ErrorKind::UnknownReference, "unknown_reference", 1),
            (ErrorKind::OutsideCollection, "outside_collection", 1),
            (ErrorKind::LockHeld, "lock_held", 1),
            (ErrorKind::TargetUnreachable, "target_unreachable", 4),
            (ErrorKind::NoTarget, "no_target", 1),
            (ErrorKind::NotImplemented, "not_implemented", 1),
            (ErrorKind::FormatTooNew, "format_too_new", 1),
            (ErrorKind::Other, "other", 1),
        ];
        assert_eq!(table.len(), ErrorKind::ALL.len());
        let listed: Vec<ErrorKind> = table.iter().map(|(kind, _, _)| *kind).collect();
        assert_eq!(listed, ErrorKind::ALL, "ALL follows the order of the table");
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
            candidates: vec![candidate("projects/a"), candidate("projects/b")],
        }
        .into();
        assert_eq!(
            serde_json::Value::Object(error.details()),
            json!({"candidates": [
                {"path": "projects/a"},
                {"path": "projects/b"},
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

    /// What a command error that wraps a lock error looks like: its own
    /// message, the lock error as its source.
    #[derive(Debug, thiserror::Error)]
    enum Wrapping {
        #[error("cannot archive")]
        Lock(#[from] LockError),
        #[error("cannot start the run")]
        Run(#[from] RunError),
    }

    #[derive(Debug, thiserror::Error)]
    enum Hiding {
        #[error(transparent)]
        Lock(#[from] LockError),
    }

    #[derive(Debug, thiserror::Error)]
    enum WrappingReference {
        #[error("cannot unearth")]
        Reference(#[from] ReferenceError),
    }

    #[derive(Debug, thiserror::Error)]
    enum HidingReference {
        #[error(transparent)]
        Reference(#[from] ReferenceError),
    }

    fn ambiguous() -> ReferenceError {
        ReferenceError::Ambiguous {
            reference: "alpha".into(),
            candidates: vec![candidate("projects/a"), candidate("projects/b")],
        }
    }

    #[test]
    fn the_chain_walker_finds_candidates_below_a_wrapping_error() {
        let wrapped = WrappingReference::Reference(ambiguous());
        assert_eq!(
            find_candidates(&wrapped),
            Some(&[candidate("projects/a"), candidate("projects/b")][..])
        );
    }

    #[test]
    fn the_chain_walker_ignores_references_that_are_not_ambiguous() {
        let wrapped = WrappingReference::Reference(ReferenceError::Unknown {
            reference: "alpha".into(),
        });
        assert_eq!(find_candidates(&wrapped), None);
    }

    #[test]
    fn the_chain_walker_does_not_see_candidates_through_a_transparent_wrapper() {
        assert_eq!(
            find_candidates(&HidingReference::Reference(ambiguous())),
            None
        );
    }

    fn held() -> LockError {
        LockError::Held {
            pid: 4242,
            started: "2026-11-03T10:00:00Z".parse().expect("valid literal"),
        }
    }

    #[test]
    fn the_chain_walker_finds_a_held_lock_below_a_wrapping_error() {
        let (pid, started) = find_held_lock(&Wrapping::Lock(held())).expect("a held lock");
        assert_eq!(pid, 4242);
        assert_eq!(started.to_string(), "2026-11-03T10:00:00Z");
    }

    #[test]
    fn the_chain_walker_finds_a_held_lock_inside_a_run_error() {
        let error = Wrapping::Run(RunError::Lock(held()));
        assert_eq!(find_held_lock(&error).map(|(pid, _)| pid), Some(4242));
    }

    #[test]
    fn the_chain_walker_ignores_other_lock_errors() {
        assert_eq!(
            find_held_lock(&Wrapping::Lock(LockError::HeldUnknown)),
            None
        );
    }

    #[test]
    fn the_chain_walker_does_not_see_through_a_transparent_wrapper() {
        // `transparent` forwards `source()` to the inner error's source,
        // which is why a command error must not use it for a lock error.
        assert_eq!(find_held_lock(&Hiding::Lock(held())), None);
    }

    #[test]
    fn the_details_of_a_run_error_carry_the_held_lock() {
        let error: Error = RunError::Lock(held()).into();
        assert_eq!(
            serde_json::Value::Object(error.details()),
            json!({"pid": 4242, "started": "2026-11-03T10:00:00Z"})
        );
    }

    #[test]
    fn every_module_and_command_error_converts_and_has_a_kind() {
        fn wraps<E: Into<Error> + HasKind>() {}
        wraps::<crate::versions::VersionsError>();
        wraps::<crate::rules::RulesError>();
        wraps::<crate::markers::MarkerError>();
        wraps::<crate::query::QueryError>();
        wraps::<crate::commands::archive::ArchiveError>();
        wraps::<crate::commands::unearth::UnearthError>();
        wraps::<crate::commands::fork::ForkError>();
        wraps::<crate::commands::forget::ForgetError>();
        wraps::<crate::commands::rebuild::RebuildError>();
        wraps::<crate::commands::import::ImportError>();
        wraps::<crate::commands::list::ListError>();
        wraps::<crate::commands::grep::GrepError>();
        wraps::<crate::commands::status::StatusError>();
        wraps::<crate::commands::pin::PinCommandError>();
        wraps::<crate::commands::push::PushError>();
        wraps::<crate::commands::survey::SurveyError>();
        wraps::<crate::commands::restore::RestoreError>();
    }

    #[test]
    fn the_library_can_name_itself_by_its_crate_name() {
        assert_eq!(antiq::error::ErrorKind::Usage, ErrorKind::Usage);
    }
}
