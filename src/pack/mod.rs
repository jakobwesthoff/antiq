// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Packing, verifying and unpacking archives.

pub mod extract;
pub mod frames;
pub mod hashing;
pub mod live;
pub mod manifest;
pub mod names;
pub mod pool;
pub mod read;
pub mod tar_write;
pub mod verify;
pub mod zstd;

/// Errors of packing and unpacking.
#[derive(Debug, thiserror::Error)]
pub enum PackError {}

impl crate::error::HasKind for PackError {}
