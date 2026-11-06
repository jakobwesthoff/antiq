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
use crate::rules::RulesError;
use crate::run::RunError;
use crate::select::SelectError;
use crate::sysmem::SysmemError;
use crate::versions::VersionsError;
use crate::walk::WalkError;
use jiff::Timestamp;
use miette::Diagnostic;
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
    FormatTooNew,
    /// Every failure without a kind of its own.
    Other,
}

impl ErrorKind {
    pub const ALL: [ErrorKind; 10] = [
        ErrorKind::Usage,
        ErrorKind::NotACollection,
        ErrorKind::AmbiguousReference,
        ErrorKind::UnknownReference,
        ErrorKind::OutsideCollection,
        ErrorKind::LockHeld,
        ErrorKind::TargetUnreachable,
        ErrorKind::NoTarget,
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
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Collection(#[from] CollectionError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Filesystem(#[from] FilesystemError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Id(#[from] IdError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Idle(#[from] IdleError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Init(#[from] InitError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Listing(#[from] ListingError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Pin(#[from] PinError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Reference(#[from] ReferenceError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    RelPath(#[from] RelPathError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Select(#[from] SelectError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Sysmem(#[from] SysmemError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Forget(#[from] ForgetError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Fork(#[from] ForkError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Grep(#[from] GrepError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Import(#[from] ImportError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    List(#[from] ListError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    PinCommand(#[from] PinCommandError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Push(#[from] PushError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Rebuild(#[from] RebuildError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Restore(#[from] RestoreError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Status(#[from] StatusError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Survey(#[from] SurveyError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Unearth(#[from] UnearthError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Marker(#[from] MarkerError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Query(#[from] QueryError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Rules(#[from] RulesError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Versions(#[from] VersionsError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Walk(#[from] WalkError),
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Usage(_) => ErrorKind::Usage,
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

    /// The error this one wraps, or this error itself for `Usage`. Every
    /// wrapper is `#[error(transparent)]`, which forwards `source()` past
    /// the wrapped error, so a search for a typed cause has to start here
    /// to see it.
    pub fn wrapped(&self) -> &(dyn std::error::Error + 'static) {
        match self {
            Error::Usage(_) => self,
            Error::Collection(inner) => inner,
            Error::Config(inner) => inner,
            Error::Filesystem(inner) => inner,
            Error::Id(inner) => inner,
            Error::Idle(inner) => inner,
            Error::Init(inner) => inner,
            Error::Listing(inner) => inner,
            Error::Lock(inner) => inner,
            Error::Machine(inner) => inner,
            Error::Pack(inner) => inner,
            Error::Pin(inner) => inner,
            Error::Reference(inner) => inner,
            Error::RelPath(inner) => inner,
            Error::Run(inner) => inner,
            Error::Select(inner) => inner,
            Error::Sysmem(inner) => inner,
            Error::Archive(inner) => inner,
            Error::Forget(inner) => inner,
            Error::Fork(inner) => inner,
            Error::Grep(inner) => inner,
            Error::Import(inner) => inner,
            Error::List(inner) => inner,
            Error::PinCommand(inner) => inner,
            Error::Push(inner) => inner,
            Error::Rebuild(inner) => inner,
            Error::Restore(inner) => inner,
            Error::Status(inner) => inner,
            Error::Survey(inner) => inner,
            Error::Unearth(inner) => inner,
            Error::Marker(inner) => inner,
            Error::Query(inner) => inner,
            Error::Rules(inner) => inner,
            Error::Versions(inner) => inner,
            Error::Walk(inner) => inner,
        }
    }

    /// The error the walks for details start at. `Reference`, `Lock` and
    /// `Run` are transparent, so their walks start at what they wrap.
    fn walk_start(&self) -> &(dyn std::error::Error + 'static) {
        match self {
            Error::Reference(_) | Error::Lock(_) | Error::Run(_) => self.wrapped(),
            other => other,
        }
    }

    /// The candidates of the ambiguous reference this error holds, in the
    /// order the JSON error object lists them.
    pub fn candidates(&self) -> Option<&[Candidate]> {
        find_candidates(self.walk_start())
    }

    /// The keys the JSON error object carries next to `kind` and
    /// `message` for this error. Empty for errors without details.
    pub fn details(&self) -> Map<String, Value> {
        let mut details = Map::new();
        let candidates = self.candidates();
        let held = find_held_lock(self.walk_start());
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

/// The link of the diagnostic chain of `error` that explains it: the
/// innermost one that has a code.
///
/// Wrappers such as `cannot start the run` have no code, so a lock held
/// under a push is explained as the lock. An error without any code has no
/// explanation, and its message is all there is to show.
pub fn explanation(error: &dyn Diagnostic) -> Option<&dyn Diagnostic> {
    let mut explained = None;
    let mut link = Some(error);
    while let Some(current) = link {
        if current.code().is_some() {
            explained = Some(current);
        }
        link = current.diagnostic_source();
    }
    explained
}

/// The error and its sources in one line, each cause once: see
/// [`join_distinct`].
pub fn chain_text(error: &(dyn std::error::Error + 'static)) -> String {
    join_distinct(std::iter::successors(Some(error), |link| link.source()).map(ToString::to_string))
}

/// Joins the texts of an error chain, outermost first, with ": ".
///
/// A `Display` that includes its source (`"cannot read {path}: {source}"`)
/// already shows the text of the link below it, so a text the link directly
/// above it contains is left out. Only the adjacent link is compared: a short
/// cause that merely occurs in an earlier link still shows.
pub fn join_distinct(texts: impl IntoIterator<Item = String>) -> String {
    let mut joined = String::new();
    let mut above: Option<String> = None;
    for text in texts {
        match &above {
            None => joined.push_str(&text),
            Some(above) if above.contains(&text) => {}
            Some(_) => {
                joined.push_str(": ");
                joined.push_str(&text);
            }
        }
        above = Some(text);
    }
    joined
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
    use crate::commands::restore::RestoreError;
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
    fn a_wrapper_hands_out_the_error_it_wraps() {
        let collection: Error = CollectionError::NotACollection {
            start: "/tmp/x".into(),
        }
        .into();
        assert!(
            collection
                .wrapped()
                .downcast_ref::<CollectionError>()
                .is_some()
        );
        let restore: Error = RestoreError::from(ambiguous()).into();
        assert!(restore.wrapped().downcast_ref::<RestoreError>().is_some());
        let usage = Error::Usage("bad".into());
        assert!(usage.wrapped().downcast_ref::<Error>().is_some());
    }

    /// Asserts that `Error` wraps `inner` and hands the same type back.
    fn wraps<T>(inner: T)
    where
        T: std::error::Error + Send + Sync + 'static,
        Error: From<T>,
    {
        let error = Error::from(inner);
        assert!(
            error.wrapped().downcast_ref::<T>().is_some(),
            "{} is not handed out as {}",
            error,
            std::any::type_name::<T>()
        );
    }

    #[test]
    fn every_wrapper_hands_out_its_own_inner_error() {
        use crate::relpath::RelPath;
        let path = || RelPath::parse("projects/scanner").expect("a piece path");
        let io = || std::io::Error::other("disk");
        let invalid_stamp = || VersionsError::InvalidStamp("x".into());

        wraps(ConfigError::Read {
            path: "c".into(),
            source: io(),
        });
        wraps(FilesystemError::ForeignDevice { path: "p".into() });
        wraps(IdError::Invalid("x".into()));
        wraps(IdleError::Problems);
        wraps(MachineError::Unavailable { reason: "r".into() });
        wraps(PackError::NoTopDirectory);
        wraps(PinError::Stat {
            path: path(),
            source: io(),
        });
        wraps(RelPathError::InvalidName("x"));
        wraps(SelectError::InvalidPattern {
            pattern: "[".into(),
            reason: "r".into(),
        });
        wraps(SysmemError::Unavailable);
        wraps(ListError::Versions(invalid_stamp()));
        wraps(StatusError::Survey(SurveyError::Root {
            path: "p".into(),
            source: io(),
        }));
        wraps(SurveyError::Root {
            path: "p".into(),
            source: io(),
        });
        wraps(MarkerError::Invalid {
            path: path(),
            reason: "r".into(),
        });
        wraps(RulesError::NotUtf8);
        wraps(invalid_stamp());
        wraps(WalkError::OutOfDescriptors {
            piece: path(),
            path: path(),
            source: io(),
        });
        // `QueryError` has no value, so `Error::Query` cannot be built.
    }

    #[test]
    fn usage_carries_its_kind() {
        assert_eq!(Error::Usage("bad".into()).kind(), ErrorKind::Usage);
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
                    closest: Vec::new(),
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
            closest: Vec::new(),
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
    fn the_candidates_of_an_error_are_those_of_its_ambiguous_reference() {
        let wrapped = Error::Reference(ambiguous());
        assert_eq!(
            wrapped.candidates(),
            Some(&[candidate("projects/a"), candidate("projects/b")][..])
        );
        let other = Error::Reference(ReferenceError::Unknown {
            reference: "alpha".into(),
            closest: Vec::new(),
        });
        assert_eq!(other.candidates(), None);
    }

    #[test]
    fn a_cause_the_text_above_it_contains_is_joined_once() {
        let texts = [
            "cannot find the piece: `scan` matches 3 pieces",
            "`scan` matches 3 pieces",
        ];
        assert_eq!(
            join_distinct(texts.map(String::from)),
            "cannot find the piece: `scan` matches 3 pieces"
        );
    }

    #[test]
    fn a_contained_cause_in_the_middle_of_a_chain_does_not_hide_the_ones_below() {
        let texts = [
            "cannot start",
            "cannot start: no room",
            "no room",
            "disk full",
        ];
        assert_eq!(
            join_distinct(texts.map(String::from)),
            "cannot start: cannot start: no room: disk full"
        );
    }

    #[test]
    fn a_short_cause_that_only_an_earlier_link_contains_still_shows() {
        let texts = [
            "cannot open the file: Permission denied",
            "cannot read the listing",
            "Permission denied",
        ];
        assert_eq!(
            join_distinct(texts.map(String::from)),
            "cannot open the file: Permission denied: cannot read the listing: Permission denied"
        );
    }

    #[test]
    fn a_chain_that_embeds_nothing_is_joined_as_it_is() {
        let texts = ["cannot change the pin", "disk full"];
        assert_eq!(
            join_distinct(texts.map(String::from)),
            "cannot change the pin: disk full"
        );
        assert_eq!(join_distinct(Vec::<String>::new()), "");
    }

    #[test]
    fn a_pin_error_that_embeds_its_io_error_shows_the_io_error_once() {
        let error = crate::pin::PinError::Stat {
            path: crate::relpath::RelPath::parse("projects/alpha/.antiq-pin").expect("a path"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert_eq!(
            chain_text(&error),
            "cannot look for the pin file projects/alpha/.antiq-pin: permission denied"
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

    // =====================================================
    // The explanation of an error
    // =====================================================

    fn held_lock() -> LockError {
        held()
    }

    fn version_ambiguous() -> crate::versions::naming::VersionLookupError {
        crate::versions::naming::VersionLookupError::Ambiguous {
            piece: "projects/a".into(),
            input: "2026".into(),
            stamps: vec!["20261103T100000Z".into()],
        }
    }

    fn marker_too_new() -> crate::target::marker::MarkerError {
        crate::target::marker::MarkerError::TooNew { found: 9 }
    }

    /// Every error that carries a code, reached directly and through each
    /// wrapper that can hold it, with the code its explanation has and the
    /// kind the JSON error object names.
    fn coded_errors() -> Vec<(&'static str, Error, &'static str, ErrorKind)> {
        use crate::commands::{
            archive, forget, fork, import, pin, push, rebuild, restore, unearth,
        };
        let lock = || RunError::Lock(held_lock());
        let piece = || String::from("projects/a");
        let stamp = || String::from("20261103T100000Z");
        let unknown = || ReferenceError::Unknown {
            reference: "alpha".into(),
            closest: Vec::new(),
        };
        let outside = || ReferenceError::Outside {
            path: "/elsewhere".into(),
            root: "/collection".into(),
        };
        let restore_too_new = || restore::RestoreError::FormatTooNew {
            piece: piece(),
            stamp: stamp(),
        };
        let cases: Vec<(&'static str, Error, &'static str, ErrorKind)> = vec![
            (
                "collection",
                CollectionError::NotACollection { start: "/x".into() }.into(),
                "antiq::collection::not_a_collection",
                ErrorKind::NotACollection,
            ),
            (
                "ambiguous reference",
                ambiguous().into(),
                "antiq::reference::ambiguous",
                ErrorKind::AmbiguousReference,
            ),
            (
                "unknown reference",
                unknown().into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "only incomplete",
                ReferenceError::OnlyIncomplete {
                    reference: "alpha".into(),
                    piece: piece(),
                }
                .into(),
                "antiq::reference::only_incomplete",
                ErrorKind::UnknownReference,
            ),
            (
                "outside",
                outside().into(),
                "antiq::reference::outside",
                ErrorKind::OutsideCollection,
            ),
            (
                "lock held",
                held_lock().into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "lock held unknown",
                LockError::HeldUnknown.into(),
                "antiq::lock::held_unknown",
                ErrorKind::LockHeld,
            ),
            (
                "run over lock",
                lock().into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "archive over lock",
                archive::ArchiveError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "forget over lock",
                forget::ForgetError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "forget without force",
                forget::ForgetError::AllVersionsNeedForce {
                    piece: "tools/scan".to_string(),
                }
                .into(),
                "antiq::forget::all_versions_need_force",
                ErrorKind::Usage,
            ),
            (
                "rebuild over lock",
                rebuild::RebuildError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "pin over lock",
                pin::PinCommandError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "import over lock",
                import::ImportError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "push over lock",
                push::PushError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "unearth over lock",
                unearth::UnearthError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "fork over lock",
                fork::ForkError::Run(lock()).into(),
                "antiq::lock::held",
                ErrorKind::LockHeld,
            ),
            (
                "archive over reference",
                archive::ArchiveError::Reference(unknown()).into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "forget over reference",
                forget::ForgetError::Reference(unknown()).into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "rebuild over reference",
                rebuild::RebuildError::Reference(unknown()).into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "pin over reference",
                pin::PinCommandError::Reference(outside()).into(),
                "antiq::reference::outside",
                ErrorKind::OutsideCollection,
            ),
            (
                "import over reference",
                import::ImportError::Reference(unknown()).into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "restore over reference",
                restore::RestoreError::Reference(unknown()).into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "unearth over restore over reference",
                unearth::UnearthError::Restore(restore::RestoreError::Reference(unknown())).into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "fork over restore over reference",
                fork::ForkError::Restore(restore::RestoreError::Reference(unknown())).into(),
                "antiq::reference::unknown",
                ErrorKind::UnknownReference,
            ),
            (
                "listing",
                ListingError::FormatTooNew { found: 2 }.into(),
                "antiq::listing::format_too_new",
                ErrorKind::FormatTooNew,
            ),
            (
                "restore over listing",
                restore::RestoreError::Listing(ListingError::FormatTooNew { found: 2 }).into(),
                "antiq::listing::format_too_new",
                ErrorKind::FormatTooNew,
            ),
            (
                "restore",
                restore_too_new().into(),
                "antiq::restore::format_too_new",
                ErrorKind::FormatTooNew,
            ),
            (
                "unearth over restore",
                unearth::UnearthError::Restore(restore_too_new()).into(),
                "antiq::restore::format_too_new",
                ErrorKind::FormatTooNew,
            ),
            (
                "fork over restore",
                fork::ForkError::Restore(restore_too_new()).into(),
                "antiq::restore::format_too_new",
                ErrorKind::FormatTooNew,
            ),
            (
                "rebuild",
                rebuild::RebuildError::FormatTooNew {
                    piece: piece(),
                    stamp: stamp(),
                }
                .into(),
                "antiq::rebuild::format_too_new",
                ErrorKind::FormatTooNew,
            ),
            (
                "version",
                version_ambiguous().into_error_in_restore(),
                "antiq::version::ambiguous",
                ErrorKind::Other,
            ),
            (
                "no target",
                push::PushError::NoTarget.into(),
                "antiq::push::no_target",
                ErrorKind::NoTarget,
            ),
            (
                "unreachable",
                push::PushError::Unreachable {
                    reason: "down".into(),
                }
                .into(),
                "antiq::push::unreachable",
                ErrorKind::TargetUnreachable,
            ),
            (
                "marker mismatch",
                push::PushError::MarkerMismatch {
                    marker_host: "a".into(),
                    marker_path: "/a".into(),
                    host: "b".into(),
                    path: "/b".into(),
                }
                .into(),
                "antiq::push::marker_mismatch",
                ErrorKind::Other,
            ),
            (
                "damaged store",
                push::PushError::DamagedStore {
                    lines: vec!["a".into()],
                }
                .into(),
                "antiq::push::damaged_store",
                ErrorKind::Other,
            ),
            (
                "push over marker",
                push::PushError::MarkerFormat(marker_too_new()).into(),
                "antiq::target::marker_too_new",
                ErrorKind::FormatTooNew,
            ),
        ];
        cases
    }

    trait IntoErrorInRestore {
        fn into_error_in_restore(self) -> Error;
    }

    impl IntoErrorInRestore for crate::versions::naming::VersionLookupError {
        fn into_error_in_restore(self) -> Error {
            RestoreError::Version(self).into()
        }
    }

    #[test]
    fn every_coded_error_is_explained_with_its_code_and_kind() {
        for (name, error, code, kind) in coded_errors() {
            let explained = explanation(&error)
                .unwrap_or_else(|| panic!("{name}: no explanation in `{error}`"));
            assert_eq!(
                explained.code().map(|code| code.to_string()).as_deref(),
                Some(code),
                "code of {name}"
            );
            assert_eq!(error.kind(), kind, "kind of {name}");
        }
    }

    #[test]
    fn an_explanation_is_the_innermost_coded_link() {
        let error: Error = crate::commands::push::PushError::Run(RunError::Lock(held())).into();
        let explained = explanation(&error).expect("the lock explains it");
        assert!(explained.to_string().contains("4242"), "{explained}");
    }

    #[test]
    fn an_error_without_a_code_has_no_explanation() {
        assert!(explanation(&Error::Usage("bad".into())).is_none());
        let io: Error = crate::config::ConfigError::Read {
            path: "c".into(),
            source: std::io::Error::other("disk"),
        }
        .into();
        assert!(explanation(&io).is_none());
        assert!(io.code().is_none());
    }

    #[test]
    fn the_library_can_name_itself_by_its_crate_name() {
        assert_eq!(antiq::error::ErrorKind::Usage, ErrorKind::Usage);
    }
}
