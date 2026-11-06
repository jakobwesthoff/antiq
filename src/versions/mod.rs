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

/// Why an operation on versions failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error)]
pub enum VersionsError {}

impl HasKind for VersionsError {}
