// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` operation.

mod seams;
mod store;

use crate::archive::PackError;
use crate::error::HasKind;
use crate::filesystem::FilesystemError;
use crate::listing::ListingError;
use crate::versions::VersionsError;

/// Why archiving a piece failed. Each variant names the step and the
/// place, and carries the cause as its source.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("{piece} needs {needed} bytes in .antiq/ and {available} are free")]
    NoRoom {
        piece: String,
        needed: u64,
        available: u64,
    },
    #[error("cannot read the free space of .antiq/")]
    Space(#[source] FilesystemError),
    #[error("cannot choose a version stamp for {piece}")]
    Stamp {
        piece: String,
        #[source]
        source: VersionsError,
    },
    #[error("cannot prepare tmp/pack/{stamp}/{piece}")]
    Stage {
        piece: String,
        stamp: String,
        #[source]
        source: FilesystemError,
    },
    #[error("cannot pack {piece} into tmp/pack/{stamp}")]
    Pack {
        piece: String,
        stamp: String,
        #[source]
        source: PackError,
    },
    #[error("cannot write {file} in tmp/pack/{stamp}/{piece}")]
    Write {
        piece: String,
        stamp: String,
        file: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write the listing of {piece} in tmp/pack/{stamp}")]
    Listing {
        piece: String,
        stamp: String,
        #[source]
        source: ListingError,
    },
    #[error("cannot make {file} in tmp/pack/{stamp}/{piece} durable")]
    Flush {
        piece: String,
        stamp: String,
        file: String,
        #[source]
        source: FilesystemError,
    },
    #[error("cannot verify the packing of {piece} in tmp/pack/{stamp}")]
    Verify {
        piece: String,
        stamp: String,
        #[source]
        source: PackError,
    },
    #[error("cannot move the version {stamp} of {piece} into the store")]
    MoveIn {
        piece: String,
        stamp: String,
        #[source]
        source: VersionsError,
    },
}

impl HasKind for ArchiveError {}
