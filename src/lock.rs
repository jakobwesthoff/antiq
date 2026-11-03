// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The run lock in .antiq/lock.

use crate::error::{ErrorKind, HasKind};
use jiff::Timestamp;

/// Errors of the run lock.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another run holds the lock (pid {pid}, started {started})")]
    Held { pid: u32, started: Timestamp },
}

impl HasKind for LockError {
    fn kind(&self) -> ErrorKind {
        match self {
            LockError::Held { .. } => ErrorKind::LockHeld,
        }
    }
}
