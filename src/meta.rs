// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Object metadata.

use crate::error::{ErrorKind, HasKind};

/// Errors of object metadata.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("the metadata has format {found}, which this version of antiq cannot write")]
    FormatTooNew { found: u32 },
}

impl HasKind for MetaError {
    fn kind(&self) -> ErrorKind {
        match self {
            MetaError::FormatTooNew { .. } => ErrorKind::FormatTooNew,
        }
    }
}
