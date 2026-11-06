// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live packages during sync.

use super::{SyncContext, SyncError};

/// Brings the live packages of the remote in line with what the pulled
/// metadata names. Runs after the pull and before the uploads.
// TODO(I32): replace this stub with the refresh of live packages.
pub(super) fn refresh(_ctx: &SyncContext<'_>) -> Result<(), SyncError> {
    Ok(())
}

/// Deletes the live packages on the remote whose archive is committed.
/// Runs after the commits.
// TODO(I32): replace this stub with the deletion of committed live packages.
pub(super) fn delete_committed(_ctx: &SyncContext<'_>) -> Result<(), SyncError> {
    Ok(())
}
