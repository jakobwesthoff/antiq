// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Relative paths and names as bytes, and their escaped text form.

/// Errors of paths and names.
#[derive(Debug, thiserror::Error)]
pub enum RelPathError {}

impl crate::error::HasKind for RelPathError {}
