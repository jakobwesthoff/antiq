// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Moving archives between tiers.

use super::{SyncContext, SyncError};

/// Applies the tiers: drops the local copy of an archive whose remote copy
/// is verified, and verifies copies. Runs last.
// TODO(I33): replace this stub with the tier phase. It owns dropping a
// local archive and writing `verified`, through `ConfirmedRemoteCopy`.
pub(super) fn apply(_ctx: &SyncContext<'_>) -> Result<(), SyncError> {
    Ok(())
}
