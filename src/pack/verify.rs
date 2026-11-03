// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Verifying a package against a folder or a listing.

use super::{PackError, Verified, WalkRules};
use crate::fsx::tree::TreeRoot;
use std::path::Path;

/// Compares `package` with the tree below `root`.
pub fn verify_strict(
    _package: &Path,
    _root: &TreeRoot,
    _rules: &WalkRules,
) -> Result<Verified, PackError> {
    Err(PackError::NotImplemented("the strict verify"))
}
