// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` and `unpin` operations.

use crate::error::HasKind;

/// Why pin or unpin failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error)]
pub enum PinCommandError {}

impl HasKind for PinCommandError {}
