// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The extraction that `unearth` and `fork` share.

use crate::error::HasKind;

/// Why a restore failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {}

impl HasKind for RestoreError {}
