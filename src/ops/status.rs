// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `status` command.

/// Errors of the `status` command.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {}

impl crate::error::HasKind for StatusError {}
