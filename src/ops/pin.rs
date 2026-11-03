// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` and `unpin` commands.

/// Errors of the `pin` and `unpin` commands.
#[derive(Debug, thiserror::Error)]
pub enum PinOpError {}

impl crate::error::HasKind for PinOpError {}
