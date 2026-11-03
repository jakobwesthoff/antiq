// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `reconcile` command.

/// Errors of the `reconcile` command.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {}

impl crate::error::HasKind for ReconcileError {}
