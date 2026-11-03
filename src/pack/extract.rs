// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Extracting a package into staging.

use super::{PackError, TargetRules, Unpacked};
use crate::fsx::tree::TreeRoot;
use std::path::Path;

/// Unpacks `package` below `staging`.
pub fn unpack(
    _package: &Path,
    _staging: &TreeRoot,
    _rules: &TargetRules,
) -> Result<Unpacked, PackError> {
    Err(PackError::NotImplemented("unpacking a package"))
}
