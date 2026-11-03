// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archive listings.

/// Errors of archive listings.
#[derive(Debug, thiserror::Error)]
pub enum ListingError {}

impl crate::error::HasKind for ListingError {}
