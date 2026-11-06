// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `push` operation: mirrors the collection to the target.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::plan::HeldDryRun;
use crate::run::{self, RunError, Started};
use crate::target::url::{TargetUrl, UrlError};
use std::path::{Path, PathBuf};

/// Why push failed.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error(
        "no target is configured: set `[target] url` in the config, then run `antiq push --init` on an empty directory of the target"
    )]
    NoTarget,
    #[error("the `[target] url` is not usable")]
    Url(#[source] UrlError),
    #[error(
        "the collection path {} is not valid UTF-8, which the target marker cannot record",
        .path.display()
    )]
    RootNotUtf8 { path: PathBuf },
    // The wrapped error is a source and not transparent, so that the
    // holder of a held lock stays visible to the error details.
    #[error("cannot start the run")]
    Run(#[source] RunError),
    /// The steps after the ones built so far.
    #[error("push is not implemented beyond its first checks")]
    Unfinished,
}

impl HasKind for PushError {
    fn kind(&self) -> ErrorKind {
        match self {
            PushError::NoTarget => ErrorKind::NoTarget,
            PushError::Run(error) => error.kind(),
            PushError::Unfinished => ErrorKind::NotImplemented,
            PushError::Url(_) | PushError::RootNotUtf8 { .. } => ErrorKind::Other,
        }
    }
}

/// What the command line asked `push` to do.
#[derive(Debug, Clone, Copy)]
pub struct PushRequest {
    pub dry_run: bool,
}

/// How a push ended, when it did not fail.
#[derive(Debug)]
pub enum PushOutcome {
    /// A dry run that found another run holding the lock.
    HeldDryRun(HeldDryRun),
}

/// Pushes the collection to its target.
pub fn push(
    collection: &Collection,
    request: &PushRequest,
    clock: &dyn Clock,
) -> Result<PushOutcome, PushError> {
    // Everything that needs no lock is checked first, so that a push without
    // a usable target says so also while another run holds the lock.
    let _url = target_url(collection)?;
    utf8_root(collection.root())?;

    let started = run::start(collection, clock, request.dry_run).map_err(PushError::Run)?;
    match started {
        Started::DryRun {
            holder: Some(holder),
            ..
        } => Ok(PushOutcome::HeldDryRun(HeldDryRun::new(&holder))),
        Started::DryRun { holder: None, .. } | Started::Guarded(_) => Err(PushError::Unfinished),
    }
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
