// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fork` command.

/// Errors of the `fork` command.
#[derive(Debug, thiserror::Error)]
pub enum ForkError {}

impl crate::error::HasKind for ForkError {}
