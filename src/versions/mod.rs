// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Versions of archived pieces: stamps, the names of store files, the store
//! layout, damage and the naming of a version on the command line.

use crate::error::HasKind;

pub mod damage;
pub mod names;
pub mod naming;
pub mod stamp;
pub mod store;

/// Why an operation on versions failed.
#[derive(Debug, thiserror::Error)]
pub enum VersionsError {
    #[error("`{0}` is not a version stamp")]
    InvalidStamp(String),
    #[error("cannot {action} {path} in the store: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: std::io::Error,
    },
}

impl HasKind for VersionsError {}
