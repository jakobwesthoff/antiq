// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `resolve` command.

pub mod conflict;
pub mod diverged;
pub mod replace;
pub mod superseded;

use crate::error::{ErrorKind, HasKind};

/// Errors of the `resolve` command.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("{0} is not supported yet")]
    Unsupported(String),
}

impl HasKind for ResolveError {
    fn kind(&self) -> ErrorKind {
        match self {
            ResolveError::Unsupported(_) => ErrorKind::Unsupported,
        }
    }
}
