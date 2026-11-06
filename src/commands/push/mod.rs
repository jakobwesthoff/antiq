// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `push` operation: mirrors the collection to the target.

use crate::clock::Clock;
use crate::code_span::CodeSpan;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::machine::MachineError;
use crate::plan::{HeldDryRun, Plan};
use crate::relpath::RelPath;
use crate::report::setting_notes;
use crate::run::{self, RunError, Started};
use crate::select::SelectError;
use crate::target::marker::{self, Marker, MarkerError};
use crate::target::rsync::{Options, Rsync, RsyncError, quiet_option};
use crate::target::url::{TargetUrl, UrlError};
use crate::versions::VersionsError;
use std::path::{Path, PathBuf};

pub use outcome::{PushAction, PushOutcome, PushReport};
use summary::Changes;

mod mirror;
mod outcome;
mod precheck;
mod store;
mod summary;

/// Why push failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PushError {
    #[diagnostic(
        code(antiq::push::no_target),
        help(
            "Add this to `.antiq/config.toml`:\n\n  [target]\n  url = \"/Volumes/nas/antiq/projects\"\n\nThen run `antiq push --init` once on an empty directory of the target to claim it."
        )
    )]
    #[error("no push target is configured, so push has nowhere to mirror the antiq collection to")]
    NoTarget,
    #[error("the `[target] url` is not usable")]
    Url(
        #[source]
        #[diagnostic_source]
        UrlError,
    ),
    #[error(
        "the collection path {} is not valid UTF-8, which the target marker cannot record",
        .path.display()
    )]
    RootNotUtf8 { path: PathBuf },
    // The wrapped error is a source and not transparent, so that the
    // holder of a held lock stays visible to the error details.
    #[error("cannot use the local rsync")]
    Rsync(
        #[source]
        #[diagnostic_source]
        RsyncError,
    ),
    #[error("cannot start the run")]
    Run(
        #[source]
        #[diagnostic_source]
        RunError,
    ),
    #[error("cannot read the store")]
    Versions(
        #[source]
        #[diagnostic_source]
        VersionsError,
    ),
    #[error("cannot select the pieces")]
    Select(
        #[source]
        #[diagnostic_source]
        SelectError,
    ),
    #[error("cannot open the collection {}", .path.display())]
    Root {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read {}", .path.display())]
    Trash {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[diagnostic(
        code(antiq::push::damaged_store),
        help("Repair or remove these versions first:\n\n{}", indented(.lines))
    )]
    #[error(
        "push is refused: the antiq collection holds versions that are damaged or incomplete, and push copies them as they are"
    )]
    DamagedStore { lines: Vec<String> },
    #[error(
        "`{first}` and `{second}` differ only in case or Unicode normalization, which the target's volume treats as one name"
    )]
    NameClash { first: RelPath, second: RelPath },
    #[error(".antiq/trash/ still holds {}, which a repair could not put back", .path.display())]
    TrashNotEmpty { path: PathBuf },
    #[error(
        "`[target] rsync_args` holds `{option}`, which silences the listing that tells antiq what the target holds; remove it"
    )]
    QuietRsyncArgs { option: String },
    /// The target cannot be used; the text says what is wrong with it.
    #[error("{problem}")]
    Target { problem: String },
    /// The target cannot be reached, which `--if-reachable` accepts.
    #[diagnostic(code(antiq::push::unreachable))]
    #[error("{reason}")]
    Unreachable { reason: String },
    #[diagnostic(
        code(antiq::push::marker_mismatch),
        help("Push with `antiq push --force` to overwrite the target once, or edit the marker.")
    )]
    #[error(
        "the target's marker {} names another host or path: it names host {} and path {}, and this run is host {} and path {}",
        CodeSpan(marker::FILE_NAME),
        CodeSpan(.marker_host),
        CodeSpan(.marker_path),
        CodeSpan(.host),
        CodeSpan(.path)
    )]
    MarkerMismatch {
        marker_host: String,
        marker_path: String,
        host: String,
        path: String,
    },
    #[error("cannot use the target's marker")]
    MarkerFormat(
        #[source]
        #[diagnostic_source]
        MarkerError,
    ),
    #[error("cannot prepare the target's marker in {}", .path.display())]
    Scratch {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the mirror failed: {detail}")]
    Mirror { detail: String },
    #[error("sending the marker to the target failed: {detail}")]
    MarkerRun { detail: String },
    #[error("cannot name this machine")]
    Machine(
        #[source]
        #[diagnostic_source]
        MachineError,
    ),
}

/// The lines of the damaged versions as the preformatted lines of a help.
fn indented(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl HasKind for PushError {
    fn kind(&self) -> ErrorKind {
        match self {
            PushError::NoTarget => ErrorKind::NoTarget,
            PushError::Run(error) => error.kind(),
            PushError::Rsync(error) => error.kind(),
            PushError::Unreachable { .. } => ErrorKind::TargetUnreachable,
            PushError::MarkerFormat(MarkerError::TooNew { .. }) => ErrorKind::FormatTooNew,
            PushError::Machine(error) => error.kind(),
            PushError::Url(_)
            | PushError::RootNotUtf8 { .. }
            | PushError::QuietRsyncArgs { .. }
            | PushError::Versions(_)
            | PushError::Select(_)
            | PushError::Root { .. }
            | PushError::Trash { .. }
            | PushError::DamagedStore { .. }
            | PushError::NameClash { .. }
            | PushError::TrashNotEmpty { .. }
            | PushError::Target { .. }
            | PushError::Scratch { .. }
            | PushError::MarkerRun { .. }
            | PushError::Mirror { .. }
            | PushError::MarkerMismatch { .. }
            | PushError::MarkerFormat(MarkerError::Invalid { .. }) => ErrorKind::Other,
        }
    }
}

/// What the command line asked `push` to do.
#[derive(Debug, Clone, Copy)]
pub struct PushRequest {
    /// The target is an empty directory that is the right one: write its
    /// marker.
    pub init: bool,
    /// Push although the marker names another host or path.
    pub force: bool,
    /// A target that cannot be reached is no failure.
    pub if_reachable: bool,
    pub dry_run: bool,
}

/// Pushes the collection to its target.
pub fn push(
    collection: &Collection,
    request: &PushRequest,
    clock: &dyn Clock,
) -> Result<PushOutcome, PushError> {
    // Everything that needs no lock is checked first, so that a push without
    // a usable target says so also while another run holds the lock.
    let url = target_url(collection)?;
    utf8_root(collection.root())?;
    if let Some(option) = quiet_option(&collection.config().target.rsync_args) {
        return Err(PushError::QuietRsyncArgs {
            option: option.to_string(),
        });
    }

    let started = run::start(collection, clock, request.dry_run).map_err(PushError::Run)?;
    // The guard, when there is one, holds the lock until push returns.
    let (_guard, repairs, warnings, folders) = match started {
        Started::DryRun {
            holder: Some(holder),
            ..
        } => return Ok(PushOutcome::HeldDryRun(HeldDryRun::new(&holder))),
        Started::DryRun {
            holder: None,
            repairs,
            warnings,
        } => {
            let planned = store::Planned::from_repairs(collection, &repairs);
            let folders = store::check(collection, &store::Mode::DryRun(planned))?;
            (None, repairs, warnings, folders)
        }
        Started::Guarded(guard) => {
            let warnings = guard.warnings().to_vec();
            let folders = store::check(
                collection,
                &store::Mode::Real {
                    warnings: &warnings,
                },
            )?;
            let done = guard.repaired().to_vec();
            (Some(guard), done, warnings, folders)
        }
    };

    let mut notes = setting_notes(collection);
    notes.extend(warnings);
    let warnings = notes;

    let rsync = local_rsync(collection)?;
    let identity = identity(collection, crate::machine::detect())?;
    let flags = precheck::Flags {
        init: request.init,
        force: request.force,
    };
    let shown = collection
        .config()
        .target
        .url
        .clone()
        .expect("the url parsed before");
    let options = Options::from_config(&collection.config().target);
    let (probe, call_url, directory) = match &url {
        TargetUrl::Path { root } => {
            let (probe, canonical, directory) = precheck::inspect_path(root, collection.root())?;
            (probe, TargetUrl::Path { root: canonical }, directory)
        }
        TargetUrl::Ssh { .. } => {
            // A dry run changes nothing in the collection, so the fetched
            // marker goes to the system's temp directory.
            let scratch = if request.dry_run {
                std::env::temp_dir()
            } else {
                collection.tmp_dir()
            };
            let probe = precheck::inspect_ssh(
                &precheck::Remote {
                    rsync: &rsync,
                    options: &options,
                    url: &url,
                    shown: &shown,
                    scratch: &scratch,
                },
                &|call| call.run(),
            )?;
            (probe, url.clone(), None)
        }
    };
    let ssh = matches!(url, TargetUrl::Ssh { .. });
    let mut report = PushReport {
        dry_run: request.dry_run,
        reason: None,
        target: shown.clone(),
        initialized: false,
        forced: request.force,
        changes: Changes::default(),
        vanished: Vec::new(),
        second_pass: false,
        notes: Vec::new(),
        warnings,
        // A dry run lists the repairs as actions of its plan, a real run
        // reports the ones it did.
        repairs: if request.dry_run {
            Vec::new()
        } else {
            repairs.clone()
        },
        plan: request.dry_run.then(|| Plan::after_repairs(repairs)),
    };
    let proceed = match precheck::decide(probe, ssh, &identity, flags, &shown) {
        precheck::Decision::Unreachable(reason) if request.if_reachable => {
            report.reason = Some(reason);
            return Ok(PushOutcome::Report(Box::new(report)));
        }
        precheck::Decision::Unreachable(reason) => {
            return Err(PushError::Unreachable { reason });
        }
        precheck::Decision::Refuse(error) => return Err(error),
        precheck::Decision::Proceed(proceed) => proceed,
    };
    report.notes.extend(proceed.note);
    report.initialized = proceed.write_marker;

    if proceed.write_marker && !request.dry_run {
        let marker = Marker::new(identity.id, identity.host, identity.path);
        mirror::send_marker(
            &mirror::Sender {
                rsync: &rsync,
                options: &options,
                url: &call_url,
                scratch: &collection.tmp_dir(),
                write: &|path, bytes| std::fs::write(path, bytes),
                before: &|| recheck_directory(&call_url, directory),
            },
            &marker,
            &|call| call.run(),
        )?;
    }

    recheck_directory(&call_url, directory)?;
    let mirrored = mirror::run_mirror(
        &mirror::Mirror {
            rsync: &rsync,
            options: &options,
            collection: collection.root(),
            url: &call_url,
        },
        &|call| call.run(),
        request.dry_run,
    )?;
    report.vanished = mirrored.vanished;
    report.second_pass = mirrored.second_pass;
    let items: Vec<summary::Item> = mirrored
        .stdouts
        .iter()
        .flat_map(|stdout| summary::parse_itemized(stdout))
        .collect();
    report.changes = summary::group(&items, &folders);

    if let Some(plan) = report.plan.as_mut() {
        if proceed.write_marker {
            plan.push_command(PushAction::WriteMarker);
        }
        plan.push_command(PushAction::Mirror {
            changes: report.changes.clone(),
        });
    }
    Ok(PushOutcome::Report(Box::new(report)))
}

/// Before anything is sent to a path target, checks that its directory is
/// still the one that was inspected.
fn recheck_directory(url: &TargetUrl, inspected: Option<(u64, u64)>) -> Result<(), PushError> {
    use std::os::unix::fs::MetadataExt;
    if let (TargetUrl::Path { root }, Some(inspected)) = (url, inspected) {
        precheck::still_the_same(root, inspected, &|path| {
            let meta = std::fs::metadata(path)?;
            Ok((meta.dev(), meta.ino()))
        })?;
    }
    Ok(())
}

/// What this run puts into a marker or compares a marker with.
fn identity(
    collection: &Collection,
    host: Result<String, MachineError>,
) -> Result<precheck::Identity, PushError> {
    Ok(precheck::Identity {
        id: collection.id(),
        host: host.map_err(PushError::Machine)?,
        path: utf8_root(collection.root())?.to_string(),
    })
}

/// The rsync that runs the push: `[target] rsync`, else the first good one
/// on `PATH` or in the usual install directories. The error names the
/// executable it refused and the key that overrides the search.
fn local_rsync(collection: &Collection) -> Result<Rsync, PushError> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    Rsync::resolve(collection.config().target.rsync.as_deref(), &path_var).map_err(PushError::Rsync)
}

/// The parsed `[target] url`.
fn target_url(collection: &Collection) -> Result<TargetUrl, PushError> {
    let text = collection
        .config()
        .target
        .url
        .as_deref()
        .ok_or(PushError::NoTarget)?;
    TargetUrl::parse(text).map_err(PushError::Url)
}

/// The collection root as text. The marker stores the root as a JSON string,
/// which has no spelling for bytes that are not UTF-8.
fn utf8_root(root: &Path) -> Result<&str, PushError> {
    root.to_str().ok_or_else(|| PushError::RootNotUtf8 {
        path: root.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn errors_of_the_checks_have_their_kinds() {
        let unavailable = MachineError::Unavailable {
            reason: "no name".into(),
        };
        assert_eq!(PushError::Machine(unavailable).kind(), ErrorKind::Other);
    }

    #[test]
    fn only_a_path_target_with_an_inspected_directory_is_rechecked() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let url = TargetUrl::Path {
            root: temp.path().to_path_buf(),
        };
        let ssh = TargetUrl::parse("host:/srv/antiq").expect("a target");
        assert!(recheck_directory(&url, None).is_ok());
        assert!(recheck_directory(&ssh, Some((0, 0))).is_ok());
        let error = recheck_directory(&url, Some((0, 0))).expect_err("another directory");
        assert!(matches!(error, PushError::Target { .. }), "{error}");
    }

    #[test]
    fn a_machine_without_a_name_is_an_error_naming_it() {
        let collection = Collection::for_tests();
        let unavailable = MachineError::Unavailable {
            reason: "no name".into(),
        };
        let error = identity(&collection, Err(unavailable)).expect_err("no name");
        assert!(matches!(error, PushError::Machine(_)), "{error}");

        let named = identity(&collection, Ok("Studio".into())).expect("named");
        assert_eq!(named.host, "Studio");
        assert_eq!(named.id, collection.id());
    }

    #[test]
    fn a_root_that_is_not_utf8_is_refused_and_one_that_is_passes() {
        let bad = Path::new(OsStr::from_bytes(b"/srv/caf\xe9"));
        let error = utf8_root(bad).expect_err("not UTF-8");
        assert!(matches!(error, PushError::RootNotUtf8 { .. }), "{error}");

        assert_eq!(
            utf8_root(Path::new("/srv/café")).expect("UTF-8"),
            "/srv/café"
        );
    }
}
