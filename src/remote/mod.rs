// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Remotes.

pub mod away;
pub mod cleanup;
pub mod commit;
pub mod marker;
pub mod path;
pub mod policy;
pub mod rsync;
pub mod ssh;
#[cfg(test)]
pub mod testing;
pub mod url;

use crate::error::{ErrorKind, HasKind};

/// Errors of remotes.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("no remote is attached")]
    NoRemote,
    /// The remote cannot be reached right now. `overdue` says it has been
    /// away for longer than `away_warn_days` while remote work is pending.
    #[error("{message}")]
    Away { message: String, overdue: bool },
    #[error("{0}")]
    Broken(String),
    #[error("{0} is not supported yet")]
    Unsupported(String),
}

impl HasKind for RemoteError {
    fn kind(&self) -> ErrorKind {
        match self {
            RemoteError::NoRemote => ErrorKind::NoRemote,
            RemoteError::Away { .. } => ErrorKind::RemoteAway,
            RemoteError::Broken(_) => ErrorKind::RemoteBroken,
            RemoteError::Unsupported(_) => ErrorKind::Unsupported,
        }
    }
}
