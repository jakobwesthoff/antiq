// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The brake on bulk deletions.

use crate::error::{ErrorKind, HasKind};
use crate::reference::Candidate;
use serde::Serialize;

/// Where the brake holds: how many deletions are planned out of how many
/// objects, and the first few of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrakeHold {
    pub planned: usize,
    pub set_size: usize,
    items: Vec<Candidate>,
}

impl BrakeHold {
    /// A report names at most this many items.
    pub const MAX_ITEMS: usize = 10;

    pub fn new(planned: usize, set_size: usize, mut items: Vec<Candidate>) -> Self {
        items.truncate(Self::MAX_ITEMS);
        BrakeHold {
            planned,
            set_size,
            items,
        }
    }

    pub fn items(&self) -> &[Candidate] {
        &self.items
    }
}

/// Errors of the brake.
#[derive(Debug, thiserror::Error)]
pub enum BrakeError {
    #[error("the brake holds {} of {} objects", .0.planned, .0.set_size)]
    Held(BrakeHold),
}

impl HasKind for BrakeError {
    fn kind(&self) -> ErrorKind {
        match self {
            BrakeError::Held(_) => ErrorKind::Brake,
        }
    }
}
