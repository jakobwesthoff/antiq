// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The markers antiq writes into a collection.

use crate::error::HasKind;

/// The marker at the top of a piece that records its unearthing. It is
/// antiq's own bookkeeping and does not go into the archive.
pub const RESTORED_FILE: &str = ".antiq-restored";

/// Why a marker operation failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error)]
pub enum MarkerError {}

impl HasKind for MarkerError {}
