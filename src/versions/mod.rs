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
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum VersionsError {
    #[error("`{0}` is not a version stamp")]
    InvalidStamp(String),
    #[error("a stamp for {piece} is not free up to the counter {limit}")]
    CounterTooHigh { piece: String, limit: u32 },
    #[error("{path} exists already")]
    Taken { path: String },
    /// A move of the files of a version failed after some of them had
    /// moved and could not be undone. The files that moved stay where they
    /// are and the staging stays too, whatever the cause was, a full disk
    /// included: repair 1 needs the staging to finish the move.
    #[error("the files of {piece} {stamp} are partly moved: {source}")]
    PartlyMoved {
        piece: String,
        stamp: String,
        source: Box<VersionsError>,
    },
    /// The `.tar.zst` is in the store, but making the store folder durable
    /// failed. The version exists, and it may not survive a crash. The
    /// piece stops there: its live folder stays where it is and is not
    /// checked or removed.
    #[error("{piece} {stamp} is in the store, but it may not be durable: {source}")]
    StoredNotDurable {
        piece: String,
        stamp: String,
        source: crate::filesystem::FilesystemError,
    },
    #[error(transparent)]
    #[diagnostic(transparent)]
    Filesystem(#[from] crate::filesystem::FilesystemError),
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: std::io::Error,
    },
}

impl HasKind for VersionsError {}
