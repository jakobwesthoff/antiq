// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `list` operation.

use crate::error::HasKind;

/// Why list failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error)]
pub enum ListError {}

impl HasKind for ListError {}
