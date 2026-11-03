// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading members and listings from a package.

use super::PackError;
use crate::listing::ListingEntry;
use std::path::Path;

/// The listing entries of the members of `package`.
pub fn list(_package: &Path) -> Result<Vec<ListingEntry>, PackError> {
    Err(PackError::NotImplemented("listing a package"))
}
