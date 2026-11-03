// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Piece references.

use crate::error::{ErrorKind, HasKind};
use serde::Serialize;

/// A piece a reference could mean: its full ULID and its path in the
/// escaped form. Brake reports list their items the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub id: String,
    pub path: String,
}

/// Errors of resolving a piece reference.
#[derive(Debug, thiserror::Error)]
pub enum ReferenceError {
    #[error("`{reference}` matches {} pieces", candidates.len())]
    Ambiguous {
        reference: String,
        candidates: Vec<Candidate>,
    },
    #[error("`{reference}` matches no piece")]
    Unknown { reference: String },
}

impl HasKind for ReferenceError {
    fn kind(&self) -> ErrorKind {
        match self {
            ReferenceError::Ambiguous { .. } => ErrorKind::AmbiguousReference,
            ReferenceError::Unknown { .. } => ErrorKind::UnknownReference,
        }
    }
}
