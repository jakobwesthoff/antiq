// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Finding a collection and its local layout.

use crate::error::{ErrorKind, HasKind};
use std::path::PathBuf;

/// Errors of finding a collection.
#[derive(Debug, thiserror::Error)]
pub enum CollectionError {
    #[error("{} is not inside a collection", start.display())]
    NotACollection { start: PathBuf },
}

impl HasKind for CollectionError {
    fn kind(&self) -> ErrorKind {
        match self {
            CollectionError::NotACollection { .. } => ErrorKind::NotACollection,
        }
    }
}
