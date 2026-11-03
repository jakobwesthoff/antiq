// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `unearth` command.

pub mod live;
pub mod local;
pub mod remote;

/// Errors of the `unearth` command.
#[derive(Debug, thiserror::Error)]
pub enum UnearthError {}

impl crate::error::HasKind for UnearthError {}
