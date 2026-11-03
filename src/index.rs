// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The derived, rebuildable index and search.

/// Errors of the index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {}

impl crate::error::HasKind for IndexError {}
