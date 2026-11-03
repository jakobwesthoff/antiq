// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The local object store in .antiq/objects/.

/// Errors of the local object store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {}

impl crate::error::HasKind for StoreError {}
